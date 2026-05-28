use anyhow::{Context, Result};
use scap::{
    capturer::{Capturer, Options, Resolution},
    frame::Frame,
};
use std::thread;
use tokio::sync::mpsc;

/// One captured video frame in BGRA8 layout.
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

        let opts = Options {
            fps: target_fps,
            show_cursor: true,
            show_highlight: false,
            output_type: scap::frame::FrameType::BGRAFrame,
            output_resolution: Resolution::_1080p,
            ..Default::default()
        };
        let mut capturer = Capturer::new(opts);
        capturer.start_capture();

        let (tx, rx) = mpsc::channel::<VideoFrame>(8);

        let join = thread::spawn(move || {
            let start = std::time::Instant::now();
            loop {
                let frame = match capturer.get_next_frame() {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!("capture frame error: {e}");
                        break;
                    }
                };
                let ts_us = start.elapsed().as_micros() as u64;
                let parsed = match frame {
                    Frame::BGRA(b) => Some(VideoFrame {
                        width: b.width as u32,
                        height: b.height as u32,
                        stride: (b.width as u32) * 4,
                        data: b.data,
                        timestamp_us: ts_us,
                    }),
                    other => {
                        tracing::warn!("unexpected frame type: {:?}", std::mem::discriminant(&other));
                        None
                    }
                };
                if let Some(vf) = parsed {
                    // drop if downstream is behind — better to skip than build latency
                    if tx.blocking_send(vf).is_err() {
                        break;
                    }
                }
            }
            capturer.stop_capture();
        });

        Ok(Self { rx, _join: join })
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
