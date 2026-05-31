#[cfg(not(target_os = "macos"))]
use anyhow::Result;
#[cfg(target_os = "linux")]
use parking_lot::Mutex;
#[cfg(target_os = "linux")]
use scap::{
    capturer::{Capturer, Options},
    frame::Frame,
};
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::thread;
#[cfg(target_os = "linux")]
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;
use tokio::sync::mpsc;

#[cfg(not(target_os = "macos"))]
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
    // Kept alive for the lifetime of the capture. Each platform parks its
    // capture-thread / capture-session handle here so dropping the
    // VideoCapture tears the platform-specific resources down.
    #[cfg(target_os = "linux")]
    _join: thread::JoinHandle<()>,
    #[cfg(target_os = "windows")]
    _capture_control: windows_capture::capture::CaptureControl<
        windows_impl::Handler,
        anyhow::Error,
    >,
    // The macOS SCStream is shared with the paired AudioCapture so the
    // stream stays alive until both halves drop; see `macos_impl::start_av`.
    #[cfg(target_os = "macos")]
    pub(crate) _session: std::sync::Arc<macos_impl::MacAvSession>,
}

impl VideoCapture {
    /// Direct constructor for Linux/Windows. macOS callers use
    /// [`crate::capture::start_av`], which builds the video and audio
    /// captures from a single SCStream — there's no standalone video-only
    /// SCKit path here because the wrapper session is shared.
    #[cfg(not(target_os = "macos"))]
    pub fn start(target_fps: u32, target_resolution: Resolution) -> Result<Self> {
        #[cfg(target_os = "windows")]
        {
            windows_impl::start(target_fps, target_resolution)
        }
        #[cfg(target_os = "linux")]
        {
            Self::start_scap(target_fps, target_resolution)
        }
    }

    #[cfg(target_os = "linux")]
    fn start_scap(target_fps: u32, target_resolution: Resolution) -> Result<Self> {
        if !scap::is_supported() {
            anyhow::bail!("screen capture is not supported on this platform/version");
        }
        if !scap::has_permission() {
            if !scap::request_permission() {
                anyhow::bail!("screen recording permission denied");
            }
        }

        // Pre-flight the shareable content. scap's `Capturer::new` does
        // `.find(main_display).unwrap()` on the display list, so an empty
        // list panics deep inside the crate — and since the release profile
        // is `panic = "abort"`, that panic takes down the whole process.
        // Convert it into an actionable error here.
        let display_count = scap::get_all_targets()
            .into_iter()
            .filter(|t| matches!(t, scap::Target::Display(_)))
            .count();
        if display_count == 0 {
            anyhow::bail!("no capturable displays found — screen capture cannot start");
        }

        let (tx, rx) = mpsc::channel::<VideoFrame>(8);

        // Latest valid frame, shared between the scap reader thread (writer)
        // and the emitter thread (reader). When scap goes idle and starts
        // emitting w=0/h=0 sentinels, the emitter keeps publishing the last
        // good frame at the target rate so the encoder sees a steady stream.
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
/// smooths what's left.
#[cfg(target_os = "windows")]
fn downscale_bgra(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let stride_bytes = (src_w * 4) as usize;
    let dst_row_bytes = (dst_w * 4) as usize;
    let mut dst = vec![0u8; dst_row_bytes * dst_h as usize];
    for dy in 0..dst_h {
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

            let was_first = !self.first_logged;
            let was_first_scaled = !self.scaled_log_done;

            let tw = target_width & !1;
            let th = if tw > 0 && w > 0 {
                ((tw as u64 * h as u64 / w as u64) as u32) & !1
            } else {
                0
            };
            let should_scale =
                tw > 0 && th > 0 && tw < w && th < h && tw >= 16 && th >= 16;

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

        // MinimumUpdateInterval caps WGC's own delivery rate so it can skip
        // GPU→CPU copies between frames when the encoder is already saturating.
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
                // None (Resolution::Native) → 0, which the handler's
                // should_scale check treats as "don't downscale".
                target_width: target_resolution.width().unwrap_or(0),
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

#[cfg(target_os = "macos")]
pub mod macos_impl {
    //! macOS screen + system-audio capture via the `screencapturekit` crate
    //! directly, replacing scap. One `SCStream` emits both video frames
    //! (`SCStreamOutputType::Screen`) and system audio
    //! (`SCStreamOutputType::Audio`). The stream is wrapped in
    //! `Arc<MacAvSession>` and shared between the paired `VideoCapture` and
    //! `AudioCapture` so the underlying stream stays alive until both halves
    //! drop. `Drop` on the session calls `stop_capture` synchronously.
    //!
    //! The mac audio path here is the reason this entry point exists at all
    //! — `cpal` can't tap the render-side audio engine on macOS, so the
    //! previous build sent the microphone instead of system audio. SCKit
    //! delivers exactly what's playing through the speakers via the same
    //! stream as the video, which is the path Apple intends for screen-share.
    use super::super::audio::{AudioCapture, AudioFrame};
    use super::{VideoCapture, VideoFrame};
    use crate::capture::Quality;
    use anyhow::{Context, Result};
    // screencapturekit 1.5.x exposes `image_buffer()` / `audio_buffer_list()`
    // directly on `CMSampleBuffer` (no extension trait needed), and
    // `CVPixelBufferLockFlags` lives in `screencapturekit::cv`.
    use screencapturekit::cv::CVPixelBufferLockFlags;
    use screencapturekit::prelude::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::sync::mpsc;

    // CoreGraphics direct FFI for the one symbol we need; adding the whole
    // `core-graphics` crate just to identify the main display would be a
    // disproportionate dep. CGDirectDisplayID is a u32.
    extern "C" {
        fn CGMainDisplayID() -> u32;
    }

    /// Owns the `SCStream` so the same session can be shared by the paired
    /// `VideoCapture` and `AudioCapture`. The last `Arc` drop tears the
    /// stream down through `stop_capture`.
    pub struct MacAvSession {
        stream: SCStream,
    }

    impl Drop for MacAvSession {
        fn drop(&mut self) {
            // stop_capture blocks on a Swift completion. That's fine here:
            // the only callers reach this via Drop on capture handles that
            // are themselves dropped synchronously from the sender thread.
            if let Err(e) = self.stream.stop_capture() {
                tracing::warn!("SCStream stop failed: {e}");
            } else {
                tracing::info!("SCStream stopped cleanly");
            }
        }
    }

    pub fn start_av(quality: Quality) -> Result<(VideoCapture, AudioCapture)> {
        // 1. Pick the primary display. `SCShareableContent::get()` is the
        //    permission gate — it errors out if Screen Recording isn't
        //    granted, which we surface verbatim so the GUI can show it.
        let content = SCShareableContent::get()
            .map_err(|e| anyhow::anyhow!("SCShareableContent::get: {e}"))?;
        let displays = content.displays();
        // `displays()` returns SCDisplays in CoreGraphics enumeration order,
        // which is NOT guaranteed to put the main display first on a
        // multi-monitor setup (the common MacBook + external monitor case).
        // Match against CGMainDisplayID so the shared screen is the one with
        // the menu bar; fall back to the first display only when the lookup
        // misses (e.g. unusual list after a TCC permission flap).
        let main_id = unsafe { CGMainDisplayID() };
        let main_idx = displays
            .iter()
            .position(|d| d.display_id() == main_id)
            .unwrap_or(0);
        let display = displays
            .into_iter()
            .nth(main_idx)
            .context("no capturable displays — open System Settings ▸ Privacy & Security ▸ Screen Recording, enable this binary, and relaunch")?;

        let src_w = display.width().max(1);
        let src_h = display.height().max(1);

        // Derive height from the source's actual aspect so we don't squash.
        // VP8 also requires even dimensions, so mask off the low bit.
        // None (Resolution::Native) → use the display's own width.
        let target_w = quality
            .resolution()
            .width()
            .map(|w| w.min(src_w))
            .unwrap_or(src_w);
        let target_h = (target_w as u64 * src_h as u64 / src_w as u64) as u32;
        let out_w = target_w & !1;
        let out_h = target_h.max(2) & !1;

        // 2. Build the filter (single display, no excluded windows) and the
        //    capture configuration. captures_audio + sample_rate + channel_count
        //    are macOS 13.0+ on SCKit — feature `macos_13_0` is enabled in
        //    Cargo.toml so these methods are present.
        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build();

        let sample_rate: u32 = 48_000;
        let channels: u16 = 2;

        let config = SCStreamConfiguration::new()
            .with_width(out_w)
            .with_height(out_h)
            .with_pixel_format(PixelFormat::BGRA)
            .with_shows_cursor(true)
            .with_minimum_frame_interval(&CMTime::new(1, quality.fps().max(1) as i32))
            .with_captures_audio(true)
            .with_sample_rate(sample_rate as i32)
            .with_channel_count(channels as i32);

        // 3. Channels and shared session state.
        let (video_tx, video_rx) = mpsc::channel::<VideoFrame>(8);
        let (audio_tx, audio_rx) = mpsc::channel::<AudioFrame>(32);

        let start = Instant::now();
        let video_first = Arc::new(AtomicBool::new(false));
        let audio_first = Arc::new(AtomicBool::new(false));

        let mut stream = SCStream::new(&filter, &config);

        // 4. Video output handler. Runs on an SCKit dispatch queue, NOT
        //    a tokio worker — `blocking_send` here would deadlock the whole
        //    GCD queue. Use `try_send` and drop frames when the encoder
        //    can't keep up; the sender's drain-to-latest already absorbs
        //    burst delivery in the same direction.
        {
            let video_tx = video_tx.clone();
            let video_first = Arc::clone(&video_first);
            stream.add_output_handler(
                move |sample: CMSampleBuffer, of_type: SCStreamOutputType| {
                    if of_type != SCStreamOutputType::Screen {
                        return;
                    }
                    let Some(pixel_buffer) = sample.image_buffer() else {
                        return;
                    };
                    let guard = match pixel_buffer.lock(CVPixelBufferLockFlags::READ_ONLY) {
                        Ok(g) => g,
                        Err(rc) => {
                            tracing::warn!(rc, "CVPixelBuffer lock failed");
                            return;
                        }
                    };
                    let w = guard.width() as u32;
                    let h = guard.height() as u32;
                    let stride = guard.bytes_per_row();
                    let raw = guard.as_slice();
                    if w == 0 || h == 0 || raw.is_empty() {
                        return;
                    }

                    // SCKit BGRA buffers are usually padded to a 64-byte row
                    // alignment, so we always copy row-by-row to a tight Vec
                    // — the encoder downstream expects stride == width*4.
                    let row_bytes = (w * 4) as usize;
                    let mut data = Vec::with_capacity(row_bytes * h as usize);
                    if stride == row_bytes {
                        data.extend_from_slice(&raw[..row_bytes * h as usize]);
                    } else {
                        for y in 0..h as usize {
                            let off = y * stride;
                            data.extend_from_slice(&raw[off..off + row_bytes]);
                        }
                    }
                    drop(guard);

                    if !video_first.swap(true, Ordering::Relaxed) {
                        tracing::info!(
                            w, h, stride, bytes = data.len(),
                            "first BGRA frame from screencapturekit"
                        );
                    }

                    let ts_us = start.elapsed().as_micros() as u64;
                    let vf = VideoFrame {
                        width: w,
                        height: h,
                        stride: w * 4,
                        data,
                        timestamp_us: ts_us,
                    };
                    let _ = video_tx.try_send(vf);
                },
                SCStreamOutputType::Screen,
            );
        }

        // 5. Audio output handler — f32 stereo at 48 kHz.
        //    SCKit delivers Float32 audio as a non-interleaved
        //    AudioBufferList (one AudioBuffer per channel) by default. The
        //    body of the handler interleaves into a single Vec<f32> so the
        //    opus encoder downstream sees the same {L,R,L,R,…} layout the
        //    cpal/wasapi paths produce.
        {
            let audio_tx = audio_tx.clone();
            let audio_first = Arc::clone(&audio_first);
            stream.add_output_handler(
                move |sample: CMSampleBuffer, of_type: SCStreamOutputType| {
                    if of_type != SCStreamOutputType::Audio {
                        return;
                    }
                    let Some(abl) = sample.audio_buffer_list() else {
                        return;
                    };
                    // SCKit Float32 stereo arrives as a non-interleaved
                    // AudioBufferList by default: one AudioBuffer per channel
                    // (kAudioFormatFlagIsNonInterleaved). If we concatenate
                    // them end-to-end the downstream opus encoder gets all of
                    // L followed by all of R within a single 20ms frame, which
                    // sounds like garbled mono. Detect the planar layout
                    // (> 1 non-empty buffer) and interleave by sample index.
                    // A single-buffer ABL is already interleaved and we just
                    // copy it through.
                    let bufs: Vec<&[u8]> = abl
                        .iter()
                        .map(|b| b.data())
                        .filter(|d| !d.is_empty())
                        .collect();
                    if bufs.is_empty() {
                        return;
                    }
                    let read_f32 = |bytes: &[u8], i: usize| -> f32 {
                        let off = i * 4;
                        f32::from_ne_bytes([
                            bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3],
                        ])
                    };
                    let samples: Vec<f32> = if bufs.len() == 1 {
                        bufs[0]
                            .chunks_exact(4)
                            .map(|c| f32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                            .collect()
                    } else {
                        // Planar: each buffer holds one channel. Walk by
                        // sample index across all channels. Cap at the
                        // shortest buffer in case SCKit ever hands us a
                        // ragged ABL.
                        let n_ch = bufs.len();
                        let per_ch = bufs.iter().map(|b| b.len() / 4).min().unwrap_or(0);
                        let mut out = Vec::with_capacity(per_ch * n_ch);
                        for s in 0..per_ch {
                            for ch in 0..n_ch {
                                out.push(read_f32(bufs[ch], s));
                            }
                        }
                        out
                    };
                    if samples.is_empty() {
                        return;
                    }
                    if !audio_first.swap(true, Ordering::Relaxed) {
                        tracing::info!(
                            samples = samples.len(),
                            sample_rate,
                            channels,
                            "first audio packet from screencapturekit"
                        );
                    }
                    let ts_us = start.elapsed().as_micros() as u64;
                    match audio_tx.try_send(AudioFrame {
                        samples,
                        channels,
                        sample_rate,
                        timestamp_us: ts_us,
                    }) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => {}
                        Err(mpsc::error::TrySendError::Closed(_)) => {}
                    }
                },
                SCStreamOutputType::Audio,
            );
        }

        // 6. Start the stream (blocking; returns when SCKit's start completion
        //    fires). Any error here means SCKit refused to start — usually
        //    revoked permission or another process holding the display.
        stream
            .start_capture()
            .map_err(|e| anyhow::anyhow!("SCStream start: {e}"))?;
        tracing::info!(out_w, out_h, fps = quality.fps(), "SCStream started");

        let session = Arc::new(MacAvSession { stream });

        let video = VideoCapture {
            rx: video_rx,
            _session: Arc::clone(&session),
        };
        let audio = AudioCapture {
            rx: audio_rx,
            sample_rate,
            channels,
            _session: Arc::clone(&session),
        };
        Ok((video, audio))
    }
}
