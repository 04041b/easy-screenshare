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
/// touching call sites. Kept inline here to avoid an extra module.
///
/// We call libvpx directly through `env-libvpx-sys` (re-exported as the
/// `vpx_sys` crate) instead of pulling in the `vpx-decode` wrapper crate:
/// the wrapper is barely maintained, our encoder already links libvpx, and
/// the decode-side surface we need is ~4 FFI calls.
mod vpx_decode_shim {
    use std::os::raw::c_int;
    use std::ptr;

    pub struct Decoder {
        ctx: vpx_sys::vpx_codec_ctx_t,
        // `ctx` is initialised once; we set this on first successful init so
        // Drop knows whether to call `vpx_codec_destroy`. libvpx tolerates
        // destroy on an uninit ctx but only because the zeroed `iface` short-
        // circuits — relying on that is fragile, so we track it ourselves.
        initialised: bool,
    }

    // SAFETY: a libvpx decoder context is owned exclusively by this struct
    // and only touched through `&mut self`. libvpx itself has no thread-local
    // state for VP8 decode that would care which thread does the work.
    unsafe impl Send for Decoder {}

    pub struct Decoded {
        pub width: u32,
        pub height: u32,
        pub rgba: Vec<u8>,
    }

    impl Decoder {
        pub fn new() -> Self {
            // Zero-init is fine — libvpx writes every field on a successful
            // `vpx_codec_dec_init_ver`. We defer the actual init to the first
            // decode call so we don't fight the borrow checker about taking
            // `&mut ctx` from a partially constructed `Self`.
            Self {
                ctx: unsafe { std::mem::zeroed() },
                initialised: false,
            }
        }

        fn ensure_init(&mut self) -> bool {
            if self.initialised {
                return true;
            }
            // Single-threaded decode: the viewer is on a soft path (relay
            // fallback or debug native window) and frames already arrive
            // serialised. Multi-threaded VP8 decode adds latency at the
            // resolutions we ship.
            let cfg = vpx_sys::vpx_codec_dec_cfg_t {
                threads: 1,
                w: 0,
                h: 0,
            };
            let rc = unsafe {
                vpx_sys::vpx_codec_dec_init_ver(
                    &mut self.ctx,
                    vpx_sys::vpx_codec_vp8_dx(),
                    &cfg,
                    0,
                    vpx_sys::VPX_DECODER_ABI_VERSION as c_int,
                )
            };
            if rc != vpx_sys::vpx_codec_err_t::VPX_CODEC_OK {
                tracing::error!(?rc, "vpx_codec_dec_init_ver failed");
                return false;
            }
            self.initialised = true;
            true
        }

        pub fn decode(&mut self, data: &[u8]) -> Option<Decoded> {
            if data.is_empty() || !self.ensure_init() {
                return None;
            }
            let rc = unsafe {
                vpx_sys::vpx_codec_decode(
                    &mut self.ctx,
                    data.as_ptr(),
                    data.len() as u32,
                    ptr::null_mut(),
                    0,
                )
            };
            if rc != vpx_sys::vpx_codec_err_t::VPX_CODEC_OK {
                // Pre-keyframe deltas hit this when the viewer joins mid-
                // stream; logging every one drowns the console.
                tracing::debug!(?rc, "vpx_codec_decode rejected packet");
                return None;
            }
            // Drain queued frames and keep only the newest. libvpx can hand
            // back more than one image per `decode` call (rare for VP8, but
            // the API contract says iterate until null).
            let mut iter: vpx_sys::vpx_codec_iter_t = ptr::null();
            let mut latest_rgba: Option<Decoded> = None;
            loop {
                let img = unsafe { vpx_sys::vpx_codec_get_frame(&mut self.ctx, &mut iter) };
                if img.is_null() {
                    break;
                }
                // SAFETY: `img` is owned by libvpx and valid until the next
                // decode call; we only read from it within this loop body.
                latest_rgba = Some(unsafe { i420_image_to_rgba(&*img) });
            }
            latest_rgba
        }
    }

    impl Drop for Decoder {
        fn drop(&mut self) {
            if self.initialised {
                unsafe {
                    vpx_sys::vpx_codec_destroy(&mut self.ctx);
                }
            }
        }
    }

    /// I420 → RGBA8 with BT.601 limited-range coefficients. Inline integer
    /// math (no extra crate): the native viewer is the debug path and a
    /// 1080p frame is ~2M pixels — fast enough at egui's repaint cadence.
    ///
    /// `img.stride[i]` is per-plane and almost always larger than the plane
    /// width — libvpx aligns rows for SIMD. We index by stride, not width.
    unsafe fn i420_image_to_rgba(img: &vpx_sys::vpx_image_t) -> Decoded {
        // We only ever ask VP8 for I420 output; guard so a future codec
        // swap (VP9 profile 1 etc.) fails loudly instead of producing
        // green frames.
        debug_assert_eq!(img.fmt, vpx_sys::vpx_img_fmt::VPX_IMG_FMT_I420);

        let w = img.d_w as usize;
        let h = img.d_h as usize;
        let y_plane = img.planes[vpx_sys::VPX_PLANE_Y as usize];
        let u_plane = img.planes[vpx_sys::VPX_PLANE_U as usize];
        let v_plane = img.planes[vpx_sys::VPX_PLANE_V as usize];
        let y_stride = img.stride[vpx_sys::VPX_PLANE_Y as usize] as usize;
        let u_stride = img.stride[vpx_sys::VPX_PLANE_U as usize] as usize;
        let v_stride = img.stride[vpx_sys::VPX_PLANE_V as usize] as usize;

        let mut rgba = vec![0u8; w * h * 4];
        for y in 0..h {
            let y_row = y_plane.add(y * y_stride);
            let u_row = u_plane.add((y / 2) * u_stride);
            let v_row = v_plane.add((y / 2) * v_stride);
            let dst_row = rgba.as_mut_ptr().add(y * w * 4);
            for x in 0..w {
                // BT.601 limited-range YUV → RGB. Coefficients scaled by
                // 1024 for integer math; saturating-cast handles overshoot
                // at the colour-space corners.
                let yv = *y_row.add(x) as i32 - 16;
                let uv = *u_row.add(x / 2) as i32 - 128;
                let vv = *v_row.add(x / 2) as i32 - 128;
                let y298 = 298 * yv;
                let r = (y298 + 409 * vv + 128) >> 8;
                let g = (y298 - 100 * uv - 208 * vv + 128) >> 8;
                let b = (y298 + 516 * uv + 128) >> 8;
                let dst = dst_row.add(x * 4);
                *dst = r.clamp(0, 255) as u8;
                *dst.add(1) = g.clamp(0, 255) as u8;
                *dst.add(2) = b.clamp(0, 255) as u8;
                *dst.add(3) = 255;
            }
        }
        Decoded {
            width: w as u32,
            height: h as u32,
            rgba,
        }
    }
}
