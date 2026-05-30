use anyhow::Result;
#[cfg(not(target_os = "windows"))]
use anyhow::Context;
#[cfg(not(target_os = "windows"))]
use parking_lot::Mutex;
#[cfg(not(target_os = "windows"))]
use scap::{
    capturer::{Capturer, Options},
    frame::Frame,
};
#[cfg(not(target_os = "windows"))]
use std::sync::Arc;
use std::thread;
#[cfg(not(target_os = "windows"))]
use std::time::Duration;
use std::time::Instant;
use tokio::sync::mpsc;

use super::Resolution;

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
    // Kept alive for the lifetime of the capture. On scap-backed builds it's
    // the emitter thread's handle (the reader thread runs free-form and
    // self-exits when the mpsc sender drops). On the windows-capture build
    // it's the CaptureControl handle returned by start_free_threaded; the
    // handler's on_frame_arrived stops the capture when its mpsc Sender goes.
    #[cfg(not(target_os = "windows"))]
    _join: thread::JoinHandle<()>,
    #[cfg(target_os = "windows")]
    _capture_control: windows_capture::capture::CaptureControl<
        windows_impl::Handler,
        anyhow::Error,
    >,
}

impl VideoCapture {
    pub fn start(target_fps: u32, target_resolution: Resolution) -> Result<Self> {
        #[cfg(target_os = "windows")]
        {
            windows_impl::start(target_fps, target_resolution)
        }
        #[cfg(not(target_os = "windows"))]
        {
            Self::start_scap(target_fps, target_resolution)
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn start_scap(target_fps: u32, target_resolution: Resolution) -> Result<Self> {
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
        let reader_res: scap::capturer::Resolution = target_resolution.into();
        let reader = thread::spawn(move || {
            super::lower_thread_priority_for_background_work();
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
            super::lower_thread_priority_for_background_work();
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

/// Nearest-neighbor BGRA downscale. Used by the Windows path because
/// `windows-capture`'s Direct3D-backed capture surface doesn't natively
/// scale output, so we shrink in software here. Chosen for speed over
/// visual quality — screen content is mostly low-frequency (UI, text
/// glyphs) so the artefacts are tolerable, and the VP8 encoder further
/// smooths what's left. A box filter would be ~4× slower per pixel and
/// we're already on the hot path at ~30 fps × multi-megapixel frames.
#[cfg(target_os = "windows")]
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

#[cfg(target_os = "windows")]
mod windows_impl {
    //! Windows screen capture via the `windows-capture` crate directly,
    //! bypassing scap (whose Windows wrapper silently ignored
    //! `output_resolution` and was version-pinned to an old release).
    //!
    //! The handler trait runs on a dedicated thread managed by
    //! windows-capture; we push BGRA frames into the same `tokio::mpsc`
    //! channel the scap path uses, so the rest of the pipeline is unchanged.

    use super::{downscale_bgra, Resolution, VideoCapture, VideoFrame};
    use anyhow::Result;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;
    use windows_capture::{
        capture::{Context, GraphicsCaptureApiHandler},
        frame::Frame,
        graphics_capture_api::InternalCaptureControl,
        monitor::Monitor,
        settings::{
            ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
            MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
        },
    };

    pub struct HandlerFlags {
        pub tx: mpsc::Sender<VideoFrame>,
        pub target_width: u32,
    }

    pub struct Handler {
        tx: mpsc::Sender<VideoFrame>,
        target_width: u32,
        start: Instant,
        first_logged: bool,
        scaled_log_done: bool,
        nopad_buf: Vec<u8>,
    }

    impl GraphicsCaptureApiHandler for Handler {
        type Flags = HandlerFlags;
        type Error = anyhow::Error;

        fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
            crate::capture::lower_thread_priority_for_background_work();
            Ok(Self {
                tx: ctx.flags.tx,
                target_width: ctx.flags.target_width,
                start: Instant::now(),
                first_logged: false,
                scaled_log_done: false,
                nopad_buf: Vec::new(),
            })
        }

        fn on_frame_arrived(
            &mut self,
            frame: &mut Frame,
            control: InternalCaptureControl,
        ) -> Result<(), Self::Error> {
            let w = frame.width();
            let h = frame.height();
            let ts_us = self.start.elapsed().as_micros() as u64;
            let target_width = self.target_width;

            // Capture the decisions we want to make about logging up-front
            // so we don't need to mutate self while the frame buffer is
            // borrowing self.nopad_buf below.
            let was_first = !self.first_logged;
            let was_first_scaled = !self.scaled_log_done;

            // VP8 requires even dimensions. windows-capture has no native
            // output_resolution scaling, so we shrink in software here.
            let tw = target_width & !1;
            let th = if tw > 0 && w > 0 {
                ((tw as u64 * h as u64 / w as u64) as u32) & !1
            } else {
                0
            };
            let should_scale =
                tw > 0 && th > 0 && tw < w && th < h && tw >= 16 && th >= 16;

            // Borrow scope: get the BGRA bytes via FrameBuffer, then either
            // downscale into a fresh Vec or copy into one. The FrameBuffer
            // and its returned slice both go out of scope at the end of the
            // block, releasing the borrow on self.nopad_buf so we can mutate
            // self.first_logged / scaled_log_done after.
            let (final_w, final_h, data): (u32, u32, Vec<u8>) = {
                let mut fb = frame
                    .buffer()
                    .map_err(|e| anyhow::anyhow!("frame.buffer: {e:?}"))?;
                let raw = fb.as_nopadding_buffer(&mut self.nopad_buf);
                if should_scale {
                    (tw, th, downscale_bgra(raw, w, h, tw, th))
                } else {
                    (w, h, raw.to_vec())
                }
            };

            if was_first {
                tracing::info!(
                    w = final_w, h = final_h, bytes = data.len(),
                    "first BGRA frame from windows-capture"
                );
                self.first_logged = true;
            }
            if should_scale && was_first_scaled {
                tracing::info!(
                    src_w = w, src_h = h, dst_w = tw, dst_h = th,
                    "downscaling capture in-process"
                );
                self.scaled_log_done = true;
            }

            let vf = VideoFrame {
                width: final_w,
                height: final_h,
                stride: final_w * 4,
                data,
                timestamp_us: ts_us,
            };
            // blocking_send back-pressures naturally: when the encoder is
            // behind, the WGC thread stalls here, which makes WGC drop
            // older frames internally rather than queueing them on us.
            if self.tx.blocking_send(vf).is_err() {
                tracing::info!("video mpsc closed — stopping windows-capture");
                control.stop();
            }
            Ok(())
        }

        fn on_closed(&mut self) -> Result<(), Self::Error> {
            tracing::info!("windows-capture session closed");
            Ok(())
        }
    }

    pub fn start(target_fps: u32, target_resolution: Resolution) -> Result<VideoCapture> {
        let (tx, rx) = mpsc::channel::<VideoFrame>(8);

        let monitor = Monitor::primary()
            .map_err(|e| anyhow::anyhow!("get primary monitor: {e:?}"))?;

        // MinimumUpdateInterval caps WGC's *own* delivery rate. We've been
        // burning CPU+GPU bandwidth doing 60 fps GPU→CPU copies that the
        // encoder threw away. Telling WGC the floor lets it skip the copy
        // entirely between frames. Custom interval = 1 / target_fps.
        let interval = Duration::from_micros(1_000_000 / target_fps.max(1) as u64);

        let settings = Settings::new(
            monitor,
            CursorCaptureSettings::Default,
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Custom(interval),
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            HandlerFlags {
                tx,
                target_width: target_resolution.width(),
            },
        );

        let control = Handler::start_free_threaded(settings)
            .map_err(|e| anyhow::anyhow!("windows-capture start: {e:?}"))?;

        Ok(VideoCapture {
            rx,
            _capture_control: control,
        })
    }
}
