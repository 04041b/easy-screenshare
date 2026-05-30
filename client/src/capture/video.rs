use anyhow::{Context, Result};
use parking_lot::Mutex;
use scap::{
    capturer::{Capturer, Options, Resolution},
    frame::Frame,
};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// One captured video frame in BGRA8 layout.
#[derive(Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: Vec<u8>,
    pub timestamp_us: u64,
}

pub struct VideoCapture {
    pub rx: mpsc::Receiver<VideoFrame>,
    _join: thread::JoinHandle<()>,
}

impl VideoCapture {
    pub fn start(target_fps: u32) -> Result<Self> {
        if !scap::is_supported() {
            anyhow::bail!("screen capture is not supported on this platform/version");
        }
        if !scap::has_permission() {
            // Try to request — on macOS this opens System Settings if denied.
            if !scap::request_permission() {
                anyhow::bail!("screen recording permission denied");
            }
        }

        // Pre-flight the shareable content. On macOS, `has_permission()` can
        // report a stale `true` (the TCC grant is keyed to the executable, so a
        // rebuilt binary loses it even though a cached check says otherwise).
        // In that state `SCShareableContent::current()` returns zero displays,
        // and scap's `Capturer::new` does `.find(main_display).unwrap()` on the
        // empty list — panicking deep inside the crate. Because the release
        // profile sets `panic = "abort"`, that panic takes down the whole
        // process, the relay/WebRTC WS closes, and the viewer sees a black
        // screen. Convert that abort into an actionable error here, before scap
        // can reach its unwrap.
        let display_count = scap::get_all_targets()
            .into_iter()
            .filter(|t| matches!(t, scap::Target::Display(_)))
            .count();
        if display_count == 0 {
            #[cfg(target_os = "macos")]
            anyhow::bail!(
                "no capturable displays — macOS Screen Recording permission is not \
                 effectively granted to this binary. Open System Settings ▸ Privacy & \
                 Security ▸ Screen Recording, toggle this executable off then on (or \
                 remove and re-add it), and relaunch."
            );
            #[cfg(not(target_os = "macos"))]
            anyhow::bail!("no capturable displays found — screen capture cannot start");
        }

        let (tx, rx) = mpsc::channel::<VideoFrame>(8);

        // Latest valid frame, shared between the scap reader thread (writer)
        // and the emitter thread (reader). When scap goes idle and starts
        // emitting w=0/h=0 sentinels (idle screen, headless display, etc.),
        // the emitter keeps publishing the last good frame at the target rate
        // so the downstream encoder sees a steady stream.
        let latest: Arc<Mutex<Option<VideoFrame>>> = Arc::new(Mutex::new(None));

        let latest_writer = latest.clone();
        let reader_fps = target_fps;
        let reader = thread::spawn(move || {
            // Build the Capturer inside the thread. On Windows, scap's
            // Options contains Option<Target> where Target::Window holds an
            // HWND(*mut c_void) — making Options unconditionally !Send.
            // Constructing here keeps the !Send value confined to one thread.
            let opts = Options {
                fps: reader_fps,
                show_cursor: true,
                show_highlight: false,
                output_type: scap::frame::FrameType::BGRAFrame,
                output_resolution: Resolution::_1080p,
                ..Default::default()
            };
            let mut capturer = Capturer::new(opts);
            capturer.start_capture();
            let start = Instant::now();
            let mut first_logged = false;
            let mut empties_in_a_row = 0u32;
            loop {
                let frame = match capturer.get_next_frame() {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!("capture frame error: {e}");
                        break;
                    }
                };
                let ts_us = start.elapsed().as_micros() as u64;
                match frame {
                    Frame::BGRA(b) => {
                        let w = b.width as u32;
                        let h = b.height as u32;
                        if w == 0 || h == 0 || b.data.is_empty() {
                            empties_in_a_row = empties_in_a_row.saturating_add(1);
                            if empties_in_a_row == 5 {
                                tracing::warn!(
                                    "scap is delivering empty frames — likely no display attached or screen is fully idle. Last good frame will be repeated."
                                );
                            }
                            continue;
                        }
                        empties_in_a_row = 0;
                        if !first_logged {
                            tracing::info!(w, h, bytes = b.data.len(), "first BGRA frame from scap");
                            first_logged = true;
                        }
                        *latest_writer.lock() = Some(VideoFrame {
                            width: w,
                            height: h,
                            stride: w * 4,
                            data: b.data,
                            timestamp_us: ts_us,
                        });
                    }
                    other => {
                        tracing::warn!("unexpected frame type: {:?}", std::mem::discriminant(&other));
                    }
                }
            }
            capturer.stop_capture();
        });

        // Emitter: republish the latest captured frame at the target rate.
        let frame_interval = Duration::from_micros(1_000_000 / target_fps as u64);
        let latest_reader = latest.clone();
        let emitter = thread::spawn(move || {
            let mut next_tick = Instant::now();
            let start = Instant::now();
            loop {
                let now = Instant::now();
                if now < next_tick {
                    thread::sleep(next_tick - now);
                }
                next_tick += frame_interval;
                if let Some(mut frame) = latest_reader.lock().clone() {
                    frame.timestamp_us = start.elapsed().as_micros() as u64;
                    if tx.blocking_send(frame).is_err() {
                        break;
                    }
                }
            }
        });
        // Keep the reader handle to drop together; we only return one join handle.
        std::mem::drop(reader);

        Ok(Self { rx, _join: emitter })
    }

}

pub fn primary_resolution() -> Result<(u32, u32)> {
    // scap doesn't expose target enumeration here uniformly; pick 1080p as default request.
    // Real resolution comes from the first frame.
    Ok((1920, 1080))
}

pub fn touch() -> Result<()> {
    // Cheap sanity-check helper for tests.
    if !scap::is_supported() {
        anyhow::bail!("capture not supported");
    }
    Ok(())
}

#[allow(dead_code)]
fn _unused() -> Result<()> {
    let _ = primary_resolution().context("res")?;
    Ok(())
}
