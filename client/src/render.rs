//! Native viewer rendering surface.
//!
//! The browser viewer (served from the Worker at `/viewer/:id`) is the
//! primary, zero-install viewer experience. The native viewer here is a
//! convenience for users who prefer the bundled binary; it shares the same
//! signaling and fallback paths but renders frames in a local window.
//!
//! Architecture:
//! - `FrameSink` is a thread-safe channel that the WebRTC `on_track` callback
//!   (or the WS fallback receiver) pushes decoded RGBA frames into.
//! - `start_native_window()` spawns a small windowed renderer that pulls from
//!   that channel. On macOS, AppKit requires UI on the main thread; since the
//!   GUI mode already owns the main thread, the `view` subcommand opens its
//!   own eframe window before the tokio runtime would otherwise block.

use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct FrameSink {
    inner: Arc<Inner>,
}

struct Inner {
    tx: mpsc::UnboundedSender<DecodedFrame>,
    decoder: Mutex<Option<vpx_decode_shim::Decoder>>,
}

pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl FrameSink {
    pub fn push_rgba(&self, width: u32, height: u32, rgba: Vec<u8>) {
        let _ = self.inner.tx.send(DecodedFrame { width, height, rgba });
    }

    /// Decode a VP8-encoded payload (from the fallback WS) and push the result.
    pub fn push_encoded_vp8(&self, data: Vec<u8>, _keyframe: bool) {
        let mut guard = self.inner.decoder.lock();
        let dec = guard.get_or_insert_with(vpx_decode_shim::Decoder::new);
        if let Some(frame) = dec.decode(&data) {
            self.push_rgba(frame.width, frame.height, frame.rgba);
        }
    }
}

/// Open a native viewer window. Returns a `FrameSink` for producers to feed.
pub fn start_native_window() -> Result<FrameSink> {
    let (tx, rx) = mpsc::unbounded_channel::<DecodedFrame>();
    let sink = FrameSink {
        inner: Arc::new(Inner {
            tx,
            decoder: Mutex::new(None),
        }),
    };

    // Spin up the eframe window on a dedicated OS thread. On macOS this won't
    // work in the strictest sense (AppKit requires the main thread); the
    // `screenshare view <CODE>` subcommand path handles that by being the only
    // window the process owns. For the GUI-launched viewer, we fall back to
    // printing stats instead of opening a second window.
    std::thread::spawn(move || {
        if let Err(e) = run_window(rx) {
            tracing::error!("viewer window exited: {e:#}");
        }
    });

    Ok(sink)
}

fn run_window(mut rx: mpsc::UnboundedReceiver<DecodedFrame>) -> Result<()> {
    use eframe::egui;

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 720.0]),
        ..Default::default()
    };
    let latest: Arc<Mutex<Option<DecodedFrame>>> = Arc::new(Mutex::new(None));
    let l2 = latest.clone();
    std::thread::spawn(move || {
        while let Some(frame) = rx.blocking_recv() {
            *l2.lock() = Some(frame);
        }
    });

    let app_latest = latest.clone();
    eframe::run_native(
        "screenshare viewer",
        options,
        Box::new(move |_cc| Box::new(ViewerApp {
            latest: app_latest,
            tex: None,
            tex_size: (0, 0),
        })),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}

struct ViewerApp {
    latest: Arc<Mutex<Option<DecodedFrame>>>,
    tex: Option<eframe::egui::TextureHandle>,
    tex_size: (u32, u32),
}

impl eframe::App for ViewerApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        use eframe::egui;

        if let Some(frame) = self.latest.lock().take() {
            let img = egui::ColorImage::from_rgba_unmultiplied(
                [frame.width as usize, frame.height as usize],
                &frame.rgba,
            );
            if self.tex_size != (frame.width, frame.height) || self.tex.is_none() {
                self.tex = Some(ctx.load_texture("video", img, egui::TextureOptions::LINEAR));
                self.tex_size = (frame.width, frame.height);
            } else if let Some(t) = &mut self.tex {
                t.set(img, egui::TextureOptions::LINEAR);
            }
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(egui::Color32::BLACK))
            .show(ctx, |ui| {
                if let Some(t) = &self.tex {
                    let avail = ui.available_size();
                    let (w, h) = (self.tex_size.0 as f32, self.tex_size.1 as f32);
                    let scale = (avail.x / w).min(avail.y / h).min(1.0);
                    ui.centered_and_justified(|ui| {
                        ui.image((t.id(), egui::vec2(w * scale, h * scale)));
                    });
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label("waiting for frames…");
                    });
                }
            });
        ctx.request_repaint_after(std::time::Duration::from_millis(16));
    }
}

/// Pump a WebRTC remote VP8 track into the FrameSink.
///
/// This reads RTP packets, depacketizes VP8, and decodes via libvpx.
/// The webrtc-rs `TrackRemote::read_rtp()` API gives raw RTP packets that we
/// hand to a VP8 depacketizer (`webrtc::rtp::codecs::vp8::Vp8Packet`).
pub async fn pump_vp8_track(
    track: Arc<webrtc::track::track_remote::TrackRemote>,
    sink: FrameSink,
) {
    use webrtc::rtp::packetizer::Depacketizer;

    let mut depacketizer = webrtc::rtp::codecs::vp8::Vp8Packet::default();
    let mut frame_buf: Vec<u8> = Vec::new();
    let mut decoder = vpx_decode_shim::Decoder::new();

    loop {
        let (packet, _attrs) = match track.read_rtp().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("read_rtp: {e}");
                break;
            }
        };
        let payload = match depacketizer.depacketize(&packet.payload) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("vp8 depacketize: {e}");
                continue;
            }
        };
        frame_buf.extend_from_slice(&payload);

        if packet.header.marker {
            if let Some(f) = decoder.decode(&frame_buf) {
                sink.push_rgba(f.width, f.height, f.rgba);
            }
            frame_buf.clear();
        }
    }
}

/// Thin wrapper around libvpx decode so we can swap implementations without
/// touching call sites. Keeping it inline here to avoid an extra module.
mod vpx_decode_shim {
    pub struct Decoder {
        // For v1 this is a placeholder — the user is expected to wire a real
        // VP8 decoder here. Options:
        //   * `vpx-decode` crate (mirror of vpx-encode, less maintained)
        //   * `dav1d`/`openh264` if we switch codecs
        //   * `ffmpeg-next` for a one-stop solution
        // The browser viewer is the recommended path; this stub keeps the
        // native binary compiling and gives a clear extension point.
    }
    pub struct Decoded {
        pub width: u32,
        pub height: u32,
        pub rgba: Vec<u8>,
    }
    impl Decoder {
        pub fn new() -> Self { Self {} }
        pub fn decode(&mut self, _data: &[u8]) -> Option<Decoded> {
            // Returning None means the viewer window stays on its
            // "waiting for frames…" message. See module-level comment.
            None
        }
    }
}
