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
    pub fn start(target_fps: u32, target_resolution: Resolution) -> Result<Self> {
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
        let reader_res = target_resolution;
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
                output_resolution: reader_res,
                ..Default::default()
            };
            let mut capturer = Capturer::new(opts);
            capturer.start_capture();
            let start = Instant::now();
            let mut first_logged = false;
            let mut scaled_log_done = false;
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
                        // scap's Windows backend (windows-capture) silently
                        // ignores `output_resolution`, so we get the native
                        // display size (e.g. 2560×1600 on a 16:10 laptop).
                        // Encoding that at our preset bitrate is starvation
                        // and triggers a lag/keyframe thrash spiral. Downscale
                        // here to the requested resolution; on backends that
                        // honored the request this becomes a no-op.
                        // `Resolution::value()` is private in scap, so duplicate
                        // its width table. Height tracks the source aspect ratio
                        // and is even-aligned because VP8 requires even dims.
                        let target_w: u32 = match reader_res {
                            Resolution::_480p => 640,
                            Resolution::_720p => 1280,
                            Resolution::_1080p => 1920,
                            Resolution::_1440p => 2560,
                            Resolution::_2160p => 3840,
                            Resolution::_4320p => 7680,
                            _ => 0,
                        };
                        let target_h: u32 = if target_w == 0 || w == 0 {
                            0
                        } else {
                            let h_calc = (target_w as u64 * h as u64 / w as u64) as u32;
                            h_calc & !1
                        };
                        let tw = target_w & !1;
                        let th = target_h;
                        let (final_w, final_h, final_data) = if tw > 0 && th > 0 && tw < w && th < h && tw >= 16 && th >= 16 {
                            let scaled = downscale_bgra(&b.data, w, h, tw, th);
                            if !scaled_log_done {
                                tracing::info!(src_w = w, src_h = h, dst_w = tw, dst_h = th, "downscaling capture in-process (scap backend did not honor output_resolution)");
                                scaled_log_done = true;
                            }
                            (tw, th, scaled)
                        } else {
                            (w, h, b.data)
                        };
                        *latest_writer.lock() = Some(VideoFrame {
                            width: final_w,
                            height: final_h,
                            stride: final_w * 4,
                            data: final_data,
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

/// Nearest-neighbor BGRA downscale. Chosen for speed over visual quality —
/// screen content is mostly low-frequency (UI, text glyphs) so the artefacts
/// are tolerable, and the VP8 encoder further smooths what's left. A box
/// filter would be ~4× slower per pixel and we're already on the hot path
/// at ~30 fps × multi-megapixel frames. `stride_bytes` lets us skip any row
/// padding the capturer adds; pass `src_w * 4` when there's none.
fn downscale_bgra(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let stride_bytes = (src_w * 4) as usize;
    let dst_row_bytes = (dst_w * 4) as usize;
    let mut dst = vec![0u8; dst_row_bytes * dst_h as usize];
    for dy in 0..dst_h {
        // Sample-center mapping: dst pixel center at (dy + 0.5) / dst_h of the
        // source range; equivalent to ((dy * 2 + 1) * src_h) / (dst_h * 2).
        let sy = ((dy as u64 * 2 + 1) * src_h as u64) / (dst_h as u64 * 2);
        let src_row = sy as usize * stride_bytes;
        let dst_row = dy as usize * dst_row_bytes;
        for dx in 0..dst_w {
            let sx = ((dx as u64 * 2 + 1) * src_w as u64) / (dst_w as u64 * 2);
            let s = src_row + sx as usize * 4;
            let d = dst_row + dx as usize * 4;
            dst[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    dst
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
