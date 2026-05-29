use std::sync::Arc;

use eframe::egui;
use parking_lot::Mutex;
use tokio::runtime::Runtime;

use crate::webrtc_client;
use crate::webrtc_client::sender::ShareInfo;

#[derive(Default)]
struct UiState {
    share_url: Option<String>,
    share_pin: Option<String>,
    share_qr: Option<egui::TextureHandle>,
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
        Box::new(move |_cc| Box::new(App { state, backend, rt })),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}

struct App {
    state: Arc<Mutex<UiState>>,
    backend: String,
    rt: Arc<Runtime>,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("screenshare");
            ui.add_space(8.0);

            let mut state = self.state.lock();
            if state.sharing {
                ui.label("Sharing your screen.");
                if let Some(url) = &state.share_url {
                    ui.add_space(8.0);
                    ui.label("Viewer URL:");
                    ui.code(url);
                    if ui.button("Copy URL").clicked() {
                        ctx.output_mut(|o| o.copied_text = url.clone());
                    }
                }
                if let Some(pin) = &state.share_pin {
                    ui.add_space(10.0);
                    ui.label("PIN (share with viewer):");
                    ui.label(
                        egui::RichText::new(pin)
                            .monospace()
                            .size(28.0)
                            .strong(),
                    );
                    if ui.button("Copy PIN").clicked() {
                        ctx.output_mut(|o| o.copied_text = pin.clone());
                    }
                }
                if let Some(tex) = &state.share_qr {
                    ui.add_space(12.0);
                    ui.image((tex.id(), egui::vec2(220.0, 220.0)));
                }
                if let Some(err) = &state.error {
                    ui.add_space(8.0);
                    ui.colored_label(egui::Color32::LIGHT_RED, err);
                }
            } else {
                ui.label("Share your screen, or view someone else's.");
                ui.add_space(12.0);
                if ui.button("Share my screen").clicked() {
                    let backend = self.backend.clone();
                    let state = self.state.clone();
                    let ctx = ctx.clone();
                    state.lock().sharing = true;
                    self.rt.spawn(async move {
                        match webrtc_client::sender::run_with_callbacks(&backend, {
                            let state = state.clone();
                            let ctx = ctx.clone();
                            move |info: ShareInfo| {
                                let qr = render_qr(&ctx, &info.viewer_url);
                                let mut s = state.lock();
                                s.share_url = Some(info.viewer_url);
                                s.share_pin = Some(info.pin);
                                s.share_qr = qr;
                                ctx.request_repaint();
                            }
                        })
                        .await
                        {
                            Ok(()) => {}
                            Err(e) => {
                                state.lock().error = Some(format!("{e:#}"));
                                ctx.request_repaint();
                            }
                        }
                    });
                }

                ui.add_space(20.0);
                ui.label("Or join a share by code + PIN:");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut state.view_code_input)
                            .hint_text("CODE")
                            .desired_width(120.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut state.view_pin_input)
                            .hint_text("PIN")
                            .desired_width(80.0),
                    );
                    let go = ui.button("Watch").clicked()
                        || ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let code_ok = !state.view_code_input.trim().is_empty();
                    let pin_ok = state.view_pin_input.trim().len() == 6
                        && state.view_pin_input.trim().chars().all(|c| c.is_ascii_digit());
                    if go && code_ok && pin_ok {
                        let code = state.view_code_input.trim().to_uppercase();
                        let pin = state.view_pin_input.trim().to_string();
                        let backend = self.backend.clone();
                        let ctx = ctx.clone();
                        self.rt.spawn(async move {
                            if let Err(e) = webrtc_client::viewer::run_native(&backend, &code, &pin).await {
                                tracing::error!("viewer failed: {e:#}");
                            }
                            ctx.request_repaint();
                        });
                    }
                });
            }
        });
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
