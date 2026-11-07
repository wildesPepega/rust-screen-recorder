//! Full-screen, translucent in-game overlay (toggled with Alt+Z by default).
//! Everything – including all settings – is reachable from here without leaving the game.

use super::settings::{SettingsEditor, Tab};
use super::theme::{self, ACCENT, PANEL, PANEL_2, RED, TEXT_DIM};
use super::{fmt_duration, send_async, StatusPoller};
use crate::config::{CaptureMode, Config};
use crate::ipc::{Request, Status};
use egui::{Align, Color32, CornerRadius, Frame, Layout, RichText, Stroke, Vec2};
use std::time::{Duration, Instant};

pub fn run(tab: Option<&str>) -> anyhow::Result<()> {
    let panel = tab.and_then(super::settings::parse_tab);
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(crate::kwin::OVERLAY_TITLE)
            .with_app_id("rust-screen-recorder-overlay")
            .with_decorations(false)
            .with_transparent(true)
            .with_fullscreen(true)
            .with_always_on_top(),
        ..Default::default()
    };
    eframe::run_native(
        crate::kwin::OVERLAY_TITLE,
        opts,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(Overlay {
                poller: StatusPoller::start(cc.egui_ctx.clone(), Duration::from_millis(350)),
                cfg: Config::load(),
                show_windows: false,
                panel,
                editor: SettingsEditor::new(panel.unwrap_or(Tab::General)),
                opened: Instant::now(),
                panel_opened: Instant::now(),
                started: false,
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

struct Overlay {
    poller: StatusPoller,
    cfg: Config,
    show_windows: bool,
    /// Settings panel shown inside the overlay.
    panel: Option<Tab>,
    editor: SettingsEditor,
    /// Start of the slide-in animation of the main view / of the settings panel.
    opened: Instant,
    panel_opened: Instant,
    /// The animation starts once the first daemon status has arrived.
    started: bool,
}

const SLIDE: f32 = 0.28; // seconds
const STAGGER: f32 = 0.06; // delay between elements
const SLIDE_PX: f32 = 70.0;

/// Ease-out progress (0..1) of an element that starts `delay` seconds after `since`.
fn progress(since: Instant, delay: f32) -> f32 {
    let t = ((since.elapsed().as_secs_f32() - delay) / SLIDE).clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Run `add` slid down by (1-p)*SLIDE_PX and faded by p.
fn slide_in<R>(ui: &mut egui::Ui, p: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.vertical(|ui| {
        ui.add_space((1.0 - p) * SLIDE_PX);
        ui.multiply_opacity(p);
        add(ui)
    })
    .inner
}

const TILE_W: f32 = 300.0;
const GAP: f32 = 24.0;

fn big_button(ui: &mut egui::Ui, text: &str, color: Color32, enabled: bool) -> bool {
    let b = egui::Button::new(RichText::new(text).size(16.0).strong().color(if enabled { Color32::BLACK } else { TEXT_DIM }))
        .fill(if enabled { color } else { PANEL_2 })
        .corner_radius(CornerRadius::same(8))
        .min_size(Vec2::new(TILE_W - 40.0, 42.0));
    ui.add_enabled(enabled, b).clicked()
}

fn wide_button(ui: &mut egui::Ui, text: &str) -> bool {
    ui.add(egui::Button::new(text).min_size(Vec2::new(TILE_W - 40.0, 30.0))).clicked()
}

fn tile(ui: &mut egui::Ui, title: &str, icon: &str, add: impl FnOnce(&mut egui::Ui)) {
    Frame::new()
        .fill(PANEL.gamma_multiply(0.97))
        .stroke(Stroke::new(1.0_f32, Color32::from_white_alpha(18)))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(egui::Margin::same(20))
        .show(ui, |ui| {
            ui.set_width(TILE_W - 40.0);
            ui.set_min_height(370.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(icon).size(30.0));
                ui.label(RichText::new(title).size(20.0).strong());
                ui.add_space(6.0);
                add(ui);
            });
        });
}

impl Overlay {
    fn open_panel(&mut self, t: Tab) {
        self.editor.reload();
        self.editor.set_tab(t);
        self.panel = Some(t);
        self.panel_opened = Instant::now();
    }

    fn close_panel(&mut self) {
        self.panel = None;
        self.cfg = Config::load();
        self.opened = Instant::now() - Duration::from_secs_f32(SLIDE * 0.4); // quick re-entry of tiles
    }

    fn tiles(&mut self, ui: &mut egui::Ui, st: &Status) {
        let total = 3.0 * TILE_W + 2.0 * GAP;
        ui.horizontal(|ui| {
            ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
            ui.spacing_mut().item_spacing.x = GAP;

            let p0 = progress(self.opened, 0.05);
            let p1 = progress(self.opened, 0.05 + STAGGER);
            let p2 = progress(self.opened, 0.05 + 2.0 * STAGGER);
            slide_in(ui, p0, |ui| tile(ui, "Instant Replay", "⟲", |ui| {
                let (txt, col) = if st.replay_active { ("ON", ACCENT) } else { ("OFF", TEXT_DIM) };
                ui.label(RichText::new(txt).size(15.0).color(col).strong());
                ui.label(RichText::new(format!("Last {} seconds", st.replay_secs)).color(TEXT_DIM));
                ui.add_space(10.0);
                if big_button(ui, &format!("Save last {} s", st.replay_secs), ACCENT, st.replay_active) {
                    send_async(Request::SaveReplay);
                }
                if wide_button(ui, if st.replay_active { "Turn off" } else { "Turn on" }) {
                    send_async(Request::ToggleReplay);
                }
                if wide_button(ui, "⚙ Replay settings") {
                    self.open_panel(Tab::Replay);
                }
                if wide_button(ui, "Open clips folder") {
                    super::open_folder(&crate::config::expand_path(&self.cfg.replay.output_dir));
                }
                if st.fireshare_enabled && st.uploading {
                    let (label, frac) = match st.upload_phase {
                        0 => ("Preparing…".to_string(), 0.0),
                        2 => ("Finishing…".to_string(), 1.0),
                        _ => (
                            format!("{:.0} %  ·  {:.1} / {:.1} MB", st.upload_progress * 100.0, st.upload_sent as f64 / 1e6, st.upload_total as f64 / 1e6),
                            st.upload_progress,
                        ),
                    };
                    ui.label(RichText::new(format!("⇪ {}", st.upload_name)).small().color(TEXT_DIM));
                    ui.add(
                        egui::ProgressBar::new(frac)
                            .desired_width(TILE_W - 40.0)
                            .fill(Color32::from_rgb(0x4d, 0xa3, 0xff))
                            // White, bold text with a dark outline-ish background stays readable on
                            // both the blue fill and the dark track.
                            .text(RichText::new(label).small().strong().color(Color32::WHITE).background_color(Color32::from_black_alpha(110)))
                            .animate(st.upload_phase != 1),
                    );
                    if st.upload_queue > 0 {
                        ui.label(RichText::new(format!("+{} more queued", st.upload_queue)).small().color(TEXT_DIM));
                    }
                }
                if st.fireshare_enabled {
                    ui.horizontal(|ui| {
                        let b = egui::Button::new("⇪ Upload last clip").min_size(Vec2::new((TILE_W - 48.0) / 2.0, 30.0));
                        if ui.add_enabled(st.has_last_clip, b).on_hover_text("Upload to FireShare (queued if an upload is running)").clicked() {
                            send_async(Request::UploadLastClip);
                        }
                        let b = egui::Button::new("Copy link").min_size(Vec2::new((TILE_W - 48.0) / 2.0, 30.0));
                        if ui.add_enabled(!st.last_link.is_empty(), b).on_hover_text(&st.last_link).clicked() {
                            send_async(Request::CopyLastLink);
                        }
                    });
                    if st.fireshare_auto {
                        ui.label(RichText::new("FireShare: clips upload automatically").small().color(TEXT_DIM));
                    }
                }
                ui.label(RichText::new(format!("Save: {}", self.cfg.hotkeys.save_replay)).small().color(TEXT_DIM));
            }));

            slide_in(ui, p1, |ui| tile(ui, "Record", "⏺", |ui| {
                if st.recording_active {
                    ui.label(RichText::new(format!("● REC  {}", fmt_duration(st.recording_secs))).size(15.0).color(RED).strong());
                } else {
                    ui.label(RichText::new("Not recording").size(15.0).color(TEXT_DIM));
                }
                ui.label(RichText::new(" ").color(TEXT_DIM));
                ui.add_space(10.0);
                let label = if st.recording_active { "Stop recording" } else { "Start recording" };
                if big_button(ui, label, RED, true) {
                    send_async(Request::ToggleRecording);
                }
                if wide_button(ui, "⚙ Recording settings") {
                    self.open_panel(Tab::Recording);
                }
                if wide_button(ui, "Open recordings folder") {
                    super::open_folder(&crate::config::expand_path(&self.cfg.recording.output_dir));
                }
                ui.label(RichText::new(format!("Toggle: {}", self.cfg.hotkeys.toggle_recording)).small().color(TEXT_DIM));
            }));

            slide_in(ui, p2, |ui| tile(ui, "Capture", "🖵", |ui| {
                let mode = st.capture_mode.unwrap_or(CaptureMode::Auto);
                ui.horizontal_wrapped(|ui| {
                    for m in CaptureMode::ALL {
                        if ui.selectable_label(mode == m, m.short()).on_hover_text(m.label()).clicked() && mode != m {
                            send_async(Request::SetCaptureMode(m));
                            self.show_windows = m == CaptureMode::Window;
                        }
                    }
                });
                ui.label(RichText::new(mode.label()).small().color(TEXT_DIM));
                ui.add_space(6.0);
                if mode == CaptureMode::Window && wide_button(ui, "Choose window…") {
                    self.show_windows = !self.show_windows;
                }
                if let Some(g) = &st.game {
                    ui.add_space(4.0);
                    ui.label(RichText::new(format!("🎮 {g}")).color(ACCENT));
                }
                if !st.video_path.is_empty() {
                    ui.label(RichText::new(&st.video_path).small().color(TEXT_DIM));
                }
            }));
        });

        if self.show_windows {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                Frame::new().fill(PANEL).corner_radius(CornerRadius::same(12)).inner_margin(egui::Margin::same(14)).show(ui, |ui| {
                    ui.set_width(total - 28.0);
                    ui.label(RichText::new("Select the window to capture").strong());
                    egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                        for w in &st.windows {
                            let sel = w.id == st.selected_window;
                            let txt = format!("{}   —   {} ({}×{})", w.caption, w.class, w.w, w.h);
                            if ui.selectable_label(sel, txt).clicked() {
                                send_async(Request::SelectWindow(w.id.clone()));
                                self.show_windows = false;
                            }
                        }
                    });
                });
            });
        }
    }

    fn settings_panel(&mut self, ui: &mut egui::Ui, st: &Status) {
        let w = (ui.available_width() - 80.0).clamp(700.0, 1100.0);
        let h = (ui.available_height() - 60.0).clamp(420.0, 760.0);
        let p = progress(self.panel_opened, 0.0);
        ui.add_space((1.0 - p) * SLIDE_PX);
        ui.multiply_opacity(p);
        let section = self.panel.map(|t| t.section()).unwrap_or("Settings");
        ui.vertical_centered(|ui| {
            Frame::new()
                .fill(PANEL)
                .stroke(Stroke::new(1.0_f32, Color32::from_white_alpha(24)))
                .corner_radius(CornerRadius::same(14))
                .inner_margin(egui::Margin::same(12))
                .show(ui, |ui| {
                    ui.set_width(w);
                    ui.set_height(h);
                    ui.horizontal(|ui| {
                        if ui.button("< Back").clicked() {
                            self.close_panel();
                        }
                        ui.label(RichText::new(section).size(18.0).strong());
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(&st.target).color(TEXT_DIM));
                        });
                    });
                    ui.separator();
                    self.editor.show(ui, Some(st));
                });
        });
    }
}

impl eframe::App for Overlay {
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !self.editor.is_capturing_key() {
            if self.panel.is_some() {
                self.close_panel();
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        let st = self.poller.get();
        if st.is_some() && !self.started {
            self.started = true;
            self.opened = Instant::now();
            self.panel_opened = Instant::now();
        }
        // Keep repainting while something slides in (the dim background is there from frame 1).
        let anim_end = SLIDE + 0.05 + 3.0 * STAGGER;
        if self.opened.elapsed().as_secs_f32() < anim_end || self.panel_opened.elapsed().as_secs_f32() < SLIDE {
            ctx.request_repaint();
        }

        egui::CentralPanel::default().frame(Frame::new().fill(Color32::from_black_alpha(165))).show(ctx, |ui| {
            let Some(st) = st else {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new("Connecting to the recorder daemon…").size(22.0));
                });
                return;
            };
            // Always reachable: Global Settings / close, top right.
            let top = ui.max_rect().right_top();
            egui::Area::new(egui::Id::new("overlay-topbar"))
                .fixed_pos(top + Vec2::new(-360.0, 18.0))
                .show(ui.ctx(), |ui| {
                    ui.horizontal(|ui| {
                        let b = egui::Button::new(RichText::new("⚙ Global Settings").size(15.0)).min_size(Vec2::new(170.0, 32.0));
                        if ui.add(b).clicked() {
                            if self.panel.is_some_and(|t| t.is_global()) {
                                self.close_panel();
                            } else {
                                self.open_panel(Tab::General);
                            }
                        }
                        if ui.add(egui::Button::new(RichText::new("Close").size(15.0)).min_size(Vec2::new(90.0, 32.0))).clicked() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                });
            ui.add_space(if self.panel.is_some() { 24.0 } else { ui.available_height() * 0.10 });
            let ph = progress(self.opened, 0.0);
            ui.add_space((1.0 - ph) * SLIDE_PX * 0.5);
            let header_opacity = ph;
            ui.vertical_centered(|ui| {
                ui.multiply_opacity(header_opacity);
                ui.label(RichText::new("RUST SCREEN RECORDER").size(14.0).color(ACCENT).strong());
                ui.horizontal(|ui| {
                    // centred status line
                    let txt = RichText::new(&st.target).size(24.0).strong();
                    let mut chips = vec![];
                    if st.replay_active {
                        chips.push(RichText::new("  ⟲ REPLAY").color(ACCENT).strong());
                    }
                    if st.recording_active {
                        chips.push(RichText::new(format!("  ● REC {}", fmt_duration(st.recording_secs))).color(RED).strong());
                    }
                    let w_est = st.target.len() as f32 * 12.0 + chips.len() as f32 * 110.0;
                    ui.add_space(((ui.available_width() - w_est) / 2.0).max(0.0));
                    ui.label(txt);
                    for c in chips {
                        ui.label(c);
                    }
                });
                if !st.screencast_available {
                    ui.label(RichText::new("KWin has not granted screen capture yet – restart the app once after installing.").color(RED));
                }
                ui.add_space(16.0);
            });

            if self.panel.is_some() {
                self.settings_panel(ui, &st);
            } else {
                self.tiles(ui, &st);
                ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
                    ui.add_space(30.0);
                    if !st.last_message.is_empty() {
                        ui.label(RichText::new(&st.last_message).color(TEXT_DIM));
                    }
                    ui.label(RichText::new(format!("{} or Esc to close", self.cfg.hotkeys.toggle_overlay)).color(TEXT_DIM));
                });
            }
        });
    }
}
