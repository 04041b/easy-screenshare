use std::sync::Arc;

use eframe::egui;
use parking_lot::Mutex;
use tokio::runtime::Runtime;

use crate::webrtc_client;
use crate::webrtc_client::sender::ShareInfo;

#[derive(Default, Clone)]
struct UiState {
    share_url: Option<String>,
    share_pin: Option<String>,
    error: Option<String>,
    sharing: bool,
    view_code_input: String,
    view_pin_input: String,
}

pub fn run_gui(rt: Runtime, backend: String) -> anyhow::Result<()> {
    let state = Arc::new(Mutex::new(UiState::default()));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([520.0, 460.0]),
        ..Default::default()
    };
    let rt = Arc::new(rt);
    eframe::run_native(
        "screenshare",
        options,
        Box::new(move |_cc| {
            Box::new(App {
                state,
                backend,
                rt,
                qr_tex: None,
                qr_url: String::new(),
            })
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}

struct App {
    state: Arc<Mutex<UiState>>,
    backend: String,
    rt: Arc<Runtime>,
    qr_tex: Option<egui::TextureHandle>,
    qr_url: String,
}

impl App {
    /// Snapshot state for read-only rendering, releasing the lock before any
    /// click handler runs. parking_lot::Mutex is non-reentrant, so click
    /// handlers must NOT call `self.state.lock()` while a guard is still
    /// held in the surrounding scope — that's an instant deadlock and
    /// freezes the UI thread.
    fn snapshot(&self) -> UiState {
        self.state.lock().clone()
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let snap = self.snapshot();

        // Build / refresh QR texture from the snapshot (cheap when URL unchanged).
        if let Some(url) = &snap.share_url {
            if self.qr_url != *url {
                self.qr_tex = render_qr(ctx, url);
                self.qr_url = url.clone();
            }
        }

        let mut next_code = snap.view_code_input.clone();
        let mut next_pin = snap.view_pin_input.clone();
        let mut want_share = false;
        let mut want_watch = false;
        let mut want_copy_url = false;
        let mut want_copy_pin = false;

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("screenshare");
            ui.add_space(8.0);

            if snap.sharing {
                ui.label("Sharing your screen.");
                if let Some(url) = &snap.share_url {
                    ui.add_space(8.0);
                    ui.label("Viewer URL:");
                    ui.code(url);
                    if ui.button("Copy URL").clicked() {
                        want_copy_url = true;
                    }
                }
                if let Some(pin) = &snap.share_pin {
                    ui.add_space(10.0);
                    ui.label("PIN (share with viewer):");
                    ui.label(egui::RichText::new(pin).monospace().size(28.0).strong());
                    if ui.button("Copy PIN").clicked() {
                        want_copy_pin = true;
                    }
                }
                if let Some(tex) = &self.qr_tex {
                    ui.add_space(12.0);
                    ui.image((tex.id(), egui::vec2(220.0, 220.0)));
                }
                if snap.share_url.is_none() && snap.error.is_none() {
                    ui.add_space(8.0);
                    ui.label("Creating session…");
                }
                if let Some(err) = &snap.error {
                    ui.add_space(8.0);
                    ui.colored_label(egui::Color32::LIGHT_RED, err);
                }
            } else {
                ui.label("Share your screen, or view someone else's.");
                ui.add_space(12.0);
                if ui.button("Share my screen").clicked() {
                    want_share = true;
                }

                ui.add_space(20.0);
                ui.label("Or join a share by code + PIN:");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut next_code)
                            .hint_text("CODE")
                            .desired_width(120.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut next_pin)
                            .hint_text("PIN")
                            .desired_width(80.0),
                    );
                    if ui.button("Watch").clicked() {
                        want_watch = true;
                    }
                });
            }
        });

        // ---- apply pending state mutations OUTSIDE any held guard ----

        // Cheap writes: lock briefly, mutate, drop.
        {
            let mut s = self.state.lock();
            if s.view_code_input != next_code {
                s.view_code_input = next_code;
            }
            if s.view_pin_input != next_pin {
                s.view_pin_input = next_pin;
            }
        }

        if want_copy_url {
            if let Some(url) = snap.share_url.as_ref() {
                ctx.output_mut(|o| o.copied_text = url.clone());
            }
        }
        if want_copy_pin {
            if let Some(pin) = snap.share_pin.as_ref() {
                ctx.output_mut(|o| o.copied_text = pin.clone());
            }
        }

        if want_share && !snap.sharing {
            self.state.lock().sharing = true;
            let backend = self.backend.clone();
            let state = self.state.clone();
            let ctx2 = ctx.clone();
            self.rt.spawn(async move {
                let cb_state = state.clone();
                let cb_ctx = ctx2.clone();
                match webrtc_client::sender::run_with_callbacks(&backend, move |info: ShareInfo| {
                    let mut s = cb_state.lock();
                    s.share_url = Some(info.viewer_url);
                    s.share_pin = Some(info.pin);
                    drop(s);
                    cb_ctx.request_repaint();
                })
                .await
                {
                    Ok(()) => {}
                    Err(e) => {
                        state.lock().error = Some(format!("{e:#}"));
                        ctx2.request_repaint();
                    }
                }
            });
        }

        if want_watch {
            let code = snap.view_code_input.trim().to_uppercase();
            let pin = snap.view_pin_input.trim().to_string();
            let pin_ok = pin.len() == 6 && pin.chars().all(|c| c.is_ascii_digit());
            if !code.is_empty() && pin_ok {
                let backend = self.backend.clone();
                let ctx2 = ctx.clone();
                self.rt.spawn(async move {
                    if let Err(e) = webrtc_client::viewer::run_native(&backend, &code, &pin).await {
                        tracing::error!("viewer failed: {e:#}");
                    }
                    ctx2.request_repaint();
                });
            }
        }
    }
}

fn render_qr(ctx: &egui::Context, url: &str) -> Option<egui::TextureHandle> {
    let code = qrcode::QrCode::new(url).ok()?;
    let img = code.render::<image::Luma<u8>>().min_dimensions(220, 220).build();
    let (w, h) = (img.width() as usize, img.height() as usize);
    let mut rgba = Vec::with_capacity(w * h * 4);
    for p in img.pixels() {
        let v = p.0[0];
        rgba.extend_from_slice(&[v, v, v, 255]);
    }
    let color = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
    Some(ctx.load_texture("share-qr", color, egui::TextureOptions::NEAREST))
}
