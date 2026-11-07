//! Settings editor – shown inside the in-game overlay (there is no separate settings window).

use super::theme::{ACCENT, RED, TEXT_DIM};
use super::{fmt_duration, send_async};
use crate::config::*;
use crate::ipc::{Request, Status};
use egui::{ComboBox, DragValue, Grid, RichText, Slider};
use std::sync::mpsc::{channel, Receiver};

pub fn parse_tab(s: &str) -> Option<Tab> {
    Some(match s {
        "general" | "global" => Tab::General,
        "overlay" | "overlays" => Tab::Overlay,
        "replay" => Tab::Replay,
        "recording" => Tab::Recording,
        "hotkeys" => Tab::Hotkeys,
        "games" => Tab::Games,
        "status" => Tab::Status,
        _ => return None,
    })
}

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum Tab {
    General,
    Overlay,
    Replay,
    Recording,
    Hotkeys,
    Games,
    Status,
}

impl Tab {
    /// Pages of the "Global Settings" section.
    pub const GLOBAL: [Tab; 5] = [Tab::General, Tab::Overlay, Tab::Hotkeys, Tab::Games, Tab::Status];
    pub fn is_global(self) -> bool {
        Tab::GLOBAL.contains(&self)
    }
    pub fn title(self) -> &'static str {
        match self {
            Tab::General => "General",
            Tab::Overlay => "Overlay",
            Tab::Replay => "Instant Replay Settings",
            Tab::Recording => "Recording Settings",
            Tab::Hotkeys => "Hotkeys",
            Tab::Games => "Games",
            Tab::Status => "Status",
        }
    }
    /// Heading of the panel this tab belongs to.
    pub fn section(self) -> &'static str {
        if self.is_global() {
            "Global Settings"
        } else {
            self.title()
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum PathField {
    ReplayOut,
    ReplayBuffer,
    RecordingOut,
}

#[derive(Clone, Copy, PartialEq)]
enum KeyField {
    Overlay,
    Save,
    Replay,
    Record,
}

pub struct SettingsEditor {
    pub cfg: Config,
    saved: Config,
    tab: Tab,
    audio_sources: Vec<AudioDevice>,
    browse: Option<(PathField, Receiver<Option<String>>)>,
    capture_key: Option<KeyField>,
    info: String,
    /// Result of "Test connection" (running / done).
    fs_test: Option<Receiver<Result<String, String>>>,
    fs_result: Option<Result<String, String>>,
    show_token: bool,
    /// Overlay tab state.
    ov_sel: Corner,
    ov_input_tex: Option<(InputOverlay, egui::TextureHandle, [usize; 2])>,
    ov_web_job: Option<(Corner, Receiver<Result<crate::overlays::Image, String>>)>,
    ov_web_tex: Option<(Corner, String, egui::TextureHandle, [usize; 2])>,
    ov_web_err: Option<String>,
    ov_drag: Option<egui::Pos2>,
    ov_access: bool,
}

impl SettingsEditor {
    pub fn new(tab: Tab) -> SettingsEditor {
        let cfg = Config::load();
        SettingsEditor {
            saved: cfg.clone(),
            cfg,
            tab,
            audio_sources: crate::audio::list_devices(),
            browse: None,
            capture_key: None,
            info: String::new(),
            fs_test: None,
            fs_result: None,
            show_token: false,
            ov_sel: Corner::BottomLeft,
            ov_input_tex: None,
            ov_web_job: None,
            ov_web_tex: None,
            ov_web_err: None,
            ov_drag: None,
            ov_access: crate::overlays::input::has_access(),
        }
    }

    pub fn set_tab(&mut self, t: Tab) {
        self.tab = t;
    }

    /// True while a hotkey is being recorded (Esc must not close the overlay then).
    pub fn is_capturing_key(&self) -> bool {
        self.capture_key.is_some()
    }

    pub fn is_dirty(&self) -> bool {
        self.cfg != self.saved
    }

    /// Reload from disk (e.g. when the overlay opens the panel again).
    pub fn reload(&mut self) {
        if !self.is_dirty() {
            self.cfg = Config::load();
            self.saved = self.cfg.clone();
        }
    }

    /// Draw tabs + content + save bar into `ui`.
    pub fn show(&mut self, ui: &mut egui::Ui, st: Option<&Status>) {
        self.poll_browse();
        let ctx = ui.ctx().clone();
        if let Some(field) = self.capture_key {
            if let Some(k) = key_string(&ctx) {
                if k != "Escape" {
                    let target = match field {
                        KeyField::Overlay => &mut self.cfg.hotkeys.toggle_overlay,
                        KeyField::Save => &mut self.cfg.hotkeys.save_replay,
                        KeyField::Replay => &mut self.cfg.hotkeys.toggle_replay,
                        KeyField::Record => &mut self.cfg.hotkeys.toggle_recording,
                    };
                    *target = k;
                }
                self.capture_key = None;
            }
        }

        egui::TopBottomPanel::bottom("settings-bottom").frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 6))).show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                let dirty = self.is_dirty();
                if ui.add_enabled(dirty, egui::Button::new(RichText::new("Save & apply").strong())).clicked() {
                    self.save();
                }
                if ui.add_enabled(dirty, egui::Button::new("Revert")).clicked() {
                    self.cfg = self.saved.clone();
                }
                if ui.button("Restore defaults").clicked() {
                    self.cfg = Config::default();
                }
                if dirty {
                    ui.label(RichText::new("Unsaved changes").color(super::theme::ACCENT));
                } else {
                    ui.label(RichText::new(&self.info).color(TEXT_DIM));
                }
            });
        });

        // Replay / Recording are single pages (opened from their tiles); the global section has
        // its own sub-pages. Nothing appears in two places.
        if self.tab.is_global() {
            egui::SidePanel::left("settings-tabs").resizable(false).exact_width(170.0).show_inside(ui, |ui| {
                ui.add_space(8.0);
                for t in Tab::GLOBAL {
                    let r = ui.add_sized([150.0, 30.0], egui::Button::selectable(self.tab == t, t.title()));
                    if r.clicked() {
                        self.tab = t;
                    }
                }
            });
        }

        egui::CentralPanel::default().frame(egui::Frame::new().inner_margin(egui::Margin::same(10))).show_inside(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| match self.tab {
                Tab::General => self.general_tab(ui, st),
                Tab::Replay => self.replay_tab(ui, st),
                Tab::Recording => self.recording_tab(ui, st),
                Tab::Hotkeys => self.hotkeys_tab(ui),
                Tab::Games => self.games_tab(ui, st),
                Tab::Status => self.status_tab(ui, st),
                Tab::Overlay => self.overlay_tab(ui),
            });
        });
    }
}

fn browse_dir(start: &str) -> Receiver<Option<String>> {
    let (tx, rx) = channel();
    let start = expand_path(start);
    std::thread::spawn(move || {
        let r = std::process::Command::new("kdialog")
            // The overlay's KWin script keeps windows titled "Rust Screen Recorder - …" on top.
            .args(["--title", "Rust Screen Recorder - Choose folder"])
            .arg("--getexistingdirectory")
            .arg(&start)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty());
        let _ = tx.send(r);
    });
    rx
}

fn combo<T: PartialEq + Copy>(ui: &mut egui::Ui, id: &str, v: &mut T, all: &[T], label: impl Fn(T) -> String) {
    ComboBox::from_id_salt(id).width(300.0).selected_text(label(*v)).show_ui(ui, |ui| {
        for &o in all {
            ui.selectable_value(v, o, label(o));
        }
    });
}

impl SettingsEditor {
    fn path_row(&mut self, ui: &mut egui::Ui, label: &str, field: PathField) {
        ui.label(label);
        ui.horizontal(|ui| {
            let val = match field {
                PathField::ReplayOut => &mut self.cfg.replay.output_dir,
                PathField::ReplayBuffer => &mut self.cfg.replay.disk_buffer_dir,
                PathField::RecordingOut => &mut self.cfg.recording.output_dir,
            };
            ui.add(egui::TextEdit::singleline(val).desired_width(420.0));
            let start = val.clone();
            if ui.button("Browse…").clicked() && self.browse.is_none() {
                self.browse = Some((field, browse_dir(&start)));
            }
            if ui.button("Open").clicked() {
                super::open_folder(&expand_path(&start));
            }
        });
        ui.end_row();
    }

    fn poll_browse(&mut self) {
        if let Some((field, rx)) = &self.browse {
            if let Ok(r) = rx.try_recv() {
                if let Some(p) = r {
                    match field {
                        PathField::ReplayOut => self.cfg.replay.output_dir = p,
                        PathField::ReplayBuffer => self.cfg.replay.disk_buffer_dir = p,
                        PathField::RecordingOut => self.cfg.recording.output_dir = p,
                    }
                }
                self.browse = None;
            }
        }
    }

    fn save(&mut self) {
        match self.cfg.save() {
            Ok(()) => {
                self.saved = self.cfg.clone();
                send_async(Request::ReloadConfig);
                self.info = "Settings saved and applied.".into();
            }
            Err(e) => self.info = format!("Could not save: {e:#}"),
        }
    }
}

fn video_ui(ui: &mut egui::Ui, id: &str, v: &mut VideoSettings, st: Option<&Status>) {
    let avail: Vec<(Codec, Backend)> = st.map(|s| s.encoders.clone()).unwrap_or_default();
    Grid::new(format!("{id}-preset")).num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
        ui.label("Quality preset");
        combo(ui, &format!("{id}-p"), &mut v.preset, &Preset::ALL, |p| p.label().into());
        ui.end_row();
    });
    if v.preset != Preset::Custom {
        let p = v.effective();
        let backend = crate::encoders::resolve_backend(p.codec, p.backend, &avail);
        ui.label(
            RichText::new(format!(
                "{} · {} · up to {}×{} @ {} fps · {} kbit/s VBR · {}",
                p.codec.label(),
                backend.map(|b| b.label()).unwrap_or("no encoder found"),
                p.width,
                p.height,
                p.fps,
                p.bitrate_kbps,
                p.container.ext().to_uppercase()
            ))
            .color(TEXT_DIM),
        );
        if ui.button("Customize this preset…").clicked() {
            v.custom = p;
            v.preset = Preset::Custom;
        }
        return;
    }
    let c = &mut v.custom;
    Grid::new(format!("{id}-custom")).num_columns(2).spacing([16.0, 10.0]).striped(true).show(ui, |ui| {
        ui.label("Codec");
        ComboBox::from_id_salt(format!("{id}-codec")).width(300.0).selected_text(c.codec.label()).show_ui(ui, |ui| {
            for codec in Codec::ALL {
                let ok = avail.is_empty() || avail.iter().any(|(cc, _)| *cc == codec);
                let txt = if ok { codec.label().to_string() } else { format!("{} (no encoder)", codec.label()) };
                ui.selectable_value(&mut c.codec, codec, txt);
            }
        });
        ui.end_row();

        ui.label("Encoder");
        ComboBox::from_id_salt(format!("{id}-backend")).width(300.0).selected_text(c.backend.label()).show_ui(ui, |ui| {
            for b in Backend::ALL {
                let ok = b == Backend::Auto || avail.is_empty() || avail.contains(&(c.codec, b));
                let txt = if ok { b.label().to_string() } else { format!("{} – unavailable", b.label()) };
                ui.add_enabled_ui(ok, |ui| {
                    ui.selectable_value(&mut c.backend, b, txt);
                });
            }
        });
        ui.end_row();

        ui.label("Resolution");
        ui.horizontal(|ui| {
            let native = c.width == 0;
            if ui.selectable_label(native, "Native").clicked() {
                c.width = 0;
                c.height = 0;
            }
            for (w, h) in [(854, 480), (1280, 720), (1920, 1080), (2560, 1440), (3840, 2160)] {
                if ui.selectable_label(c.width == w && c.height == h, format!("{h}p")).clicked() {
                    c.width = w;
                    c.height = h;
                }
            }
        });
        ui.end_row();
        ui.label("");
        ui.horizontal(|ui| {
            ui.add(DragValue::new(&mut c.width).range(0..=7680).suffix(" w"));
            ui.label("×");
            ui.add(DragValue::new(&mut c.height).range(0..=4320).suffix(" h"));
            ui.label(RichText::new("max. size, aspect ratio is kept, 0 = native").color(TEXT_DIM));
        });
        ui.end_row();

        ui.label("Frame rate");
        ui.horizontal(|ui| {
            for f in [30, 60, 120, 144] {
                ui.selectable_value(&mut c.fps, f, format!("{f}"));
            }
            ui.add(DragValue::new(&mut c.fps).range(1..=240).suffix(" fps"));
        });
        ui.end_row();

        ui.label("Rate control");
        combo(ui, &format!("{id}-rc"), &mut c.rate_control, &RateControl::ALL, |r| r.label().into());
        ui.end_row();

        if c.rate_control == RateControl::Quality {
            ui.label("Quality (lower = better)");
            ui.add(Slider::new(&mut c.quality, 0..=51));
        } else {
            ui.label("Bitrate");
            ui.add(Slider::new(&mut c.bitrate_kbps, 500..=100_000).logarithmic(true).suffix(" kbit/s"));
        }
        ui.end_row();

        ui.label("Encoder speed");
        combo(ui, &format!("{id}-speed"), &mut c.speed, &Speed::ALL, |s| s.label().into());
        ui.end_row();

        ui.label("Keyframe interval");
        ui.add(Slider::new(&mut c.keyframe_interval_secs, 0.5..=10.0).suffix(" s"));
        ui.end_row();

        ui.label("10-bit color");
        ui.add_enabled(c.codec != Codec::H264, egui::Checkbox::new(&mut c.ten_bit, "HEVC / AV1 / VP9 only"));
        ui.end_row();

        ui.label("Container");
        combo(ui, &format!("{id}-cont"), &mut c.container, &Container::ALL, |x| x.ext().to_uppercase());
        ui.end_row();
    });
    if c.container == Container::Webm && matches!(c.codec, Codec::H264 | Codec::Hevc) {
        ui.label(RichText::new("WebM cannot hold H.264/HEVC – MKV will be used instead.").color(RED));
    }
}

fn audio_ui(ui: &mut egui::Ui, id: &str, a: &mut AudioSettings, devices: &[AudioDevice]) {
    ui.label(RichText::new("Record from these devices – add as many outputs and inputs as you like.").color(TEXT_DIM));
    let mut remove = None;
    Grid::new(format!("{id}-devs")).num_columns(4).spacing([12.0, 8.0]).striped(true).show(ui, |ui| {
        for (i, d) in a.devices.iter_mut().enumerate() {
            let icon = match d.kind {
                AudioKind::Output => "🔊",
                AudioKind::Input => "🎤",
            };
            ui.label(format!("{icon}  {}", d.label)).on_hover_text(&d.name);
            ui.add(Slider::new(&mut d.volume, 0..=200).suffix(" %").text("volume"));
            if !a.merge {
                ui.label(RichText::new(format!("Track {}", i + 1)).color(TEXT_DIM));
            } else {
                ui.label("");
            }
            if ui.button("Remove").clicked() {
                remove = Some(i);
            }
            ui.end_row();
        }
    });
    if let Some(i) = remove {
        a.devices.remove(i);
    }
    if a.devices.is_empty() {
        ui.label(RichText::new("No audio will be recorded.").color(TEXT_DIM));
    }
    ui.horizontal(|ui| {
        for (kind, title) in [(AudioKind::Output, "+ Add output (what you hear)"), (AudioKind::Input, "+ Add input (microphone)")] {
            ui.menu_button(title, |ui| {
                for d in devices.iter().filter(|d| d.kind == kind) {
                    let already = a.devices.iter().any(|x| x.name == d.name);
                    if ui.add_enabled(!already, egui::Button::new(&d.label)).on_hover_text(&d.name).clicked() {
                        a.devices.push(d.clone());
                        ui.close();
                    }
                }
            });
        }
    });
    Grid::new(format!("{id}-audio")).num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
        ui.label("Tracks");
        ui.horizontal(|ui| {
            ui.radio_value(&mut a.merge, true, "Mix into one track");
            ui.radio_value(&mut a.merge, false, "One track per device");
        });
        ui.end_row();
        ui.label("Audio codec");
        combo(ui, &format!("{id}-ac"), &mut a.codec, &AudioCodec::ALL, |c| c.label().into());
        ui.end_row();
        if a.codec != AudioCodec::Flac {
            ui.label("Audio bitrate");
            ui.add(Slider::new(&mut a.bitrate_kbps, 64..=320).suffix(" kbit/s"));
            ui.end_row();
        }
    });
}

fn key_string(ctx: &egui::Context) -> Option<String> {
    ctx.input(|i| {
        for ev in &i.events {
            if let egui::Event::Key { key, pressed: true, modifiers, .. } = ev {
                let mut parts = vec![];
                if modifiers.ctrl {
                    parts.push("Ctrl".to_string());
                }
                if modifiers.alt {
                    parts.push("Alt".to_string());
                }
                if modifiers.shift {
                    parts.push("Shift".to_string());
                }
                if modifiers.mac_cmd {
                    parts.push("Meta".to_string());
                }
                parts.push(key.name().to_string());
                return Some(parts.join("+"));
            }
        }
        None
    })
}

impl SettingsEditor {
    fn general_tab(&mut self, ui: &mut egui::Ui, st: Option<&Status>) {
        ui.heading("General");
        ui.add_space(6.0);
        let g = &mut self.cfg.general;
        Grid::new("general").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
            ui.label("Monitor");
            let outputs = st.map(|s| s.outputs.clone()).unwrap_or_default();
            let cur = if g.monitor.is_empty() { "Primary / first monitor".to_string() } else { g.monitor.clone() };
            ComboBox::from_id_salt("mon").width(300.0).selected_text(cur).show_ui(ui, |ui| {
                ui.selectable_value(&mut g.monitor, String::new(), "Primary / first monitor");
                for o in outputs {
                    ui.selectable_value(&mut g.monitor, o.name.clone(), format!("{} – {} ({}×{})", o.name, o.description, o.width, o.height));
                }
            });
            ui.end_row();

            if g.capture_mode == CaptureMode::Region {
                ui.label("Region");
                ui.horizontal(|ui| {
                    ui.add(DragValue::new(&mut g.region_x).prefix("x "));
                    ui.add(DragValue::new(&mut g.region_y).prefix("y "));
                    ui.add(DragValue::new(&mut g.region_w).range(16..=16384).prefix("w "));
                    ui.add(DragValue::new(&mut g.region_h).range(16..=16384).prefix("h "));
                });
                ui.end_row();
            }

            ui.label("Mouse cursor");
            ui.checkbox(&mut g.show_cursor, "Show the cursor in captures");
            ui.end_row();
            ui.label("Notifications");
            ui.vertical(|ui| {
                ui.checkbox(&mut g.overlay_notifications, "In-game overlay notifications (replay on/off, clip saved, recording …)");
                ui.checkbox(&mut g.notifications, "Also send desktop notifications (Plasma notification centre)");
            });
            ui.end_row();
            ui.label("GPU");
            ui.checkbox(&mut g.zero_copy, "Zero-copy GPU capture & encoding (VA-API) – lowest CPU load");
            ui.end_row();
            ui.label("Updates");
            ui.checkbox(&mut g.auto_update, format!("Check for updates at every start and install them automatically (current: v{})", crate::updater::CURRENT));
            ui.end_row();
            ui.label("Autostart");
            ui.checkbox(&mut g.autostart, "Start in the background when I log in");
            ui.end_row();
        });
        ui.add_space(12.0);
        ui.label(
            RichText::new(
                "Screen capture uses KWin's own screencast interface directly – the KDE \"Select what to share\" dialog \
                 (xdg-desktop-portal) is never shown and no permissions or tokens are stored in any KDE configuration.",
            )
            .color(TEXT_DIM),
        );
    }

    fn replay_tab(&mut self, ui: &mut egui::Ui, st: Option<&Status>) {
        ui.add_space(2.0);
        Grid::new("replay").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
            let r = &mut self.cfg.replay;
            ui.label("Replay length");
            ui.add(Slider::new(&mut r.duration_secs, 5..=1200).suffix(" s").logarithmic(true));
            ui.end_row();
            ui.label("Auto start");
            ui.checkbox(&mut r.auto_start, "Turn on automatically (Auto/Desktop: at start · Game: when a game starts)");
            ui.end_row();
            ui.label("Buffer storage");
            ui.horizontal(|ui| {
                ui.radio_value(&mut r.storage, ReplayStorage::Ram, "RAM");
                ui.radio_value(&mut r.storage, ReplayStorage::Disk, "Disk");
            });
            ui.end_row();
        });
        Grid::new("replay-paths").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
            self.path_row(ui, "Save clips to", PathField::ReplayOut);
            if self.cfg.replay.storage == ReplayStorage::Disk {
                self.path_row(ui, "Disk buffer folder", PathField::ReplayBuffer);
            }
        });
        let v = self.cfg.replay.video.effective();
        let mb = (v.bitrate_kbps as u64 + self.cfg.replay.audio.bitrate_kbps as u64) * self.cfg.replay.duration_secs as u64 / 8 / 1000;
        ui.label(
            RichText::new(format!(
                "≈ {mb} MB {} used for the buffer at the configured bitrate.",
                if self.cfg.replay.storage == ReplayStorage::Ram { "of RAM" } else { "of disk space" }
            ))
            .color(TEXT_DIM),
        );
        ui.separator();
        ui.heading("Video");
        video_ui(ui, "replay-v", &mut self.cfg.replay.video, st);
        ui.separator();
        ui.heading("Audio");
        audio_ui(ui, "replay-a", &mut self.cfg.replay.audio, &self.audio_sources);
        ui.separator();
        self.fireshare_ui(ui);
    }

    fn fireshare_ui(&mut self, ui: &mut egui::Ui) {
        if let Some(rx) = &self.fs_test {
            if let Ok(r) = rx.try_recv() {
                self.fs_result = Some(r);
                self.fs_test = None;
            } else {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
        }
        ui.heading("FireShare");
        ui.label(
            RichText::new("Upload clips to your own FireShare server and get a share link right away. Create an upload token in FireShare under Settings → Security → Upload Tokens – no password is stored.")
                .color(TEXT_DIM),
        );
        let fs = &mut self.cfg.fireshare;
        ui.checkbox(&mut fs.enabled, "Enable FireShare");
        if !fs.enabled {
            return;
        }
        let mut test = false;
        Grid::new("fireshare").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
            ui.label("Server URL");
            ui.add(egui::TextEdit::singleline(&mut fs.url).hint_text("https://clips.example.com").desired_width(420.0));
            ui.end_row();
            ui.label("Upload token");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut fs.token).password(!self.show_token).hint_text("fsk_…").desired_width(330.0));
                if ui.button(if self.show_token { "Hide" } else { "Show" }).clicked() {
                    self.show_token = !self.show_token;
                }
            });
            ui.end_row();
            ui.label("");
            ui.horizontal(|ui| {
                test = ui.add_enabled(self.fs_test.is_none(), egui::Button::new("Test connection")).clicked();
                match (&self.fs_test, &self.fs_result) {
                    (Some(_), _) => {
                        ui.spinner();
                    }
                    (None, Some(Ok(user))) => {
                        ui.label(RichText::new(format!("✓ Connected as {user}")).color(ACCENT));
                    }
                    (None, Some(Err(e))) => {
                        ui.label(RichText::new(e).color(RED));
                    }
                    _ => {}
                }
            });
            ui.end_row();
            ui.label("Upload");
            ui.vertical(|ui| {
                ui.checkbox(&mut fs.auto_upload_replays, "Upload every saved clip automatically");
                ui.checkbox(&mut fs.upload_recordings, "Also upload finished recordings");
                ui.checkbox(&mut fs.copy_link, "Copy the share link to the clipboard");
            });
            ui.end_row();
            ui.label("Games");
            ui.vertical(|ui| {
                ui.checkbox(&mut fs.link_game, "Link clips to the game in FireShare's library");
                ui.checkbox(&mut fs.folder_per_game, "Use a FireShare folder per game");
            });
            ui.end_row();
            if !fs.folder_per_game {
                ui.label("Folder");
                ui.add(egui::TextEdit::singleline(&mut fs.folder).hint_text("FireShare's default upload folder").desired_width(300.0));
                ui.end_row();
            }
        });
        ui.label(
            RichText::new("MKV clips are converted to MP4 (lossless) for the upload. Whether the link is public depends on FireShare's privacy defaults.")
                .small()
                .color(TEXT_DIM),
        );
        if test {
            let cfg = fs.clone();
            let (tx, rx) = channel();
            std::thread::spawn(move || {
                let _ = tx.send(crate::fireshare::check(&cfg).map_err(|e| format!("{e:#}")));
            });
            self.fs_test = Some(rx);
            self.fs_result = None;
        }
    }

    fn overlay_tab(&mut self, ui: &mut egui::Ui) {
        // Finished website screenshot?
        if let Some((corner, rx)) = &self.ov_web_job {
            match rx.try_recv() {
                Ok(Ok(img)) => {
                    let url = self.cfg.overlays.slot(*corner).website.url.clone();
                    let tex = ui.ctx().load_texture("web-shot", to_color_image(&img), egui::TextureOptions::LINEAR);
                    self.ov_web_tex = Some((*corner, url, tex, [img.w as usize, img.h as usize]));
                    self.ov_web_err = None;
                    self.ov_web_job = None;
                }
                Ok(Err(e)) => {
                    self.ov_web_err = Some(e);
                    self.ov_web_job = None;
                }
                Err(_) => ui.ctx().request_repaint_after(std::time::Duration::from_millis(150)),
            }
        }

        ui.heading("Overlay");
        ui.label(
            RichText::new("Overlays are burned into your recordings and Instant Replay clips – they are not shown on your screen. Click a corner to configure it.")
                .color(TEXT_DIM),
        );
        ui.checkbox(&mut self.cfg.overlays.enabled, "Show overlays in recordings and clips");
        ui.add_space(6.0);

        // ---- 16:9 monitor with the four corners -------------------------------------------------
        let mon_w = ui.available_width().min(620.0);
        let mon_h = mon_w * 9.0 / 16.0;
        let (resp, painter) = ui.allocate_painter(egui::vec2(mon_w, mon_h + 26.0), egui::Sense::click());
        let bezel = egui::Rect::from_min_size(resp.rect.min, egui::vec2(mon_w, mon_h));
        let screen = bezel.shrink(8.0);
        painter.rect_filled(bezel, 10.0, egui::Color32::from_rgb(0x0c, 0x0d, 0x10));
        painter.rect_filled(screen, 4.0, egui::Color32::from_rgb(0x2a, 0x33, 0x44));
        painter.text(screen.center(), egui::Align2::CENTER_CENTER, "Your recording", egui::FontId::proportional(16.0), egui::Color32::from_white_alpha(40));
        // stand
        let stand = egui::Rect::from_center_size(egui::pos2(bezel.center().x, bezel.max.y + 10.0), egui::vec2(mon_w * 0.18, 8.0));
        painter.rect_filled(stand, 3.0, egui::Color32::from_rgb(0x0c, 0x0d, 0x10));

        self.ensure_input_preview(ui.ctx());
        let enabled = self.cfg.overlays.enabled;
        for corner in Corner::ALL {
            let slot = self.cfg.overlays.slot(corner).clone();
            let (iw, ih) = self.estimated_size(corner, &slot);
            let (sw, sh) = (screen.width() as u32 * 4, screen.height() as u32 * 4); // sub-pixel precision
            let (x, y, w, h) = if slot.kind == OverlayKind::None {
                // empty corner: small placeholder box
                crate::overlays::place(corner, 18.0, 3.0, 16, 9, sw, sh)
            } else {
                crate::overlays::place(corner, slot.size_percent, slot.margin_percent, iw, ih, sw, sh)
            };
            let r = egui::Rect::from_min_size(
                screen.min + egui::vec2(x as f32 / 4.0, y as f32 / 4.0),
                egui::vec2(w as f32 / 4.0, h as f32 / 4.0),
            );
            let selected = self.ov_sel == corner;
            let hovered = resp.hover_pos().is_some_and(|p| r.contains(p));
            if slot.kind == OverlayKind::None {
                painter.rect_stroke(r, 6.0, egui::Stroke::new(1.5_f32, egui::Color32::from_white_alpha(if hovered { 140 } else { 60 })), egui::StrokeKind::Inside);
                painter.text(r.center(), egui::Align2::CENTER_CENTER, "+ Add", egui::FontId::proportional(13.0), egui::Color32::from_white_alpha(140));
            } else {
                let tint = egui::Color32::from_white_alpha(((if enabled { 255.0 } else { 110.0 }) * slot.opacity) as u8);
                let textured = match slot.kind {
                    OverlayKind::Input => self.ov_input_tex.as_ref().map(|(_, t, _)| t.id()),
                    OverlayKind::Website => self.ov_web_tex.as_ref().filter(|(c, u, _, _)| *c == corner && *u == slot.website.url).map(|(_, _, t, _)| t.id()),
                    _ => None,
                };
                match textured {
                    Some(id) => {
                        let uv = if slot.kind == OverlayKind::Website { self.crop_uv(&slot) } else { egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)) };
                        painter.image(id, r, uv, tint);
                    }
                    None => {
                        painter.rect_filled(r, 6.0, egui::Color32::from_rgba_unmultiplied(20, 22, 28, 200));
                        let icon = match slot.kind {
                            OverlayKind::Webcam => "📷 Webcam",
                            OverlayKind::Website => "🌐 Website",
                            _ => slot.kind.label(),
                        };
                        painter.text(r.center(), egui::Align2::CENTER_CENTER, icon, egui::FontId::proportional(13.0), egui::Color32::WHITE);
                    }
                }
            }
            if selected {
                painter.rect_stroke(r.expand(2.0), 7.0, egui::Stroke::new(2.0_f32, ACCENT), egui::StrokeKind::Outside);
            } else if hovered && slot.kind != OverlayKind::None {
                painter.rect_stroke(r.expand(1.0), 7.0, egui::Stroke::new(1.0_f32, egui::Color32::from_white_alpha(160)), egui::StrokeKind::Outside);
            }
            if resp.clicked() && hovered {
                self.ov_sel = corner;
            }
        }
        // clicking anywhere selects the nearest corner
        if resp.clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                let left = p.x < screen.center().x;
                let top = p.y < screen.center().y;
                self.ov_sel = match (top, left) {
                    (true, true) => Corner::TopLeft,
                    (true, false) => Corner::TopRight,
                    (false, true) => Corner::BottomLeft,
                    (false, false) => Corner::BottomRight,
                };
            }
        }

        // ---- settings of the selected corner -------------------------------------------------
        ui.add_space(8.0);
        let corner = self.ov_sel;
        ui.label(RichText::new(format!("{} corner", corner.label())).strong().size(16.0));
        let slot = self.cfg.overlays.slot_mut(corner);
        ui.horizontal(|ui| {
            for k in OverlayKind::ALL {
                ui.selectable_value(&mut slot.kind, k, k.label());
            }
        });
        if slot.kind == OverlayKind::None {
            return;
        }
        Grid::new("ov-common").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
            ui.label("Size");
            ui.add(Slider::new(&mut slot.size_percent, 5.0..=60.0).suffix(" % of the video width"));
            ui.end_row();
            ui.label("Opacity");
            let mut op = slot.opacity * 100.0;
            if ui.add(Slider::new(&mut op, 10.0..=100.0).suffix(" %")).changed() {
                slot.opacity = op / 100.0;
            }
            ui.end_row();
            ui.label("Distance to the edge");
            ui.add(Slider::new(&mut slot.margin_percent, 0.0..=10.0).suffix(" %"));
            ui.end_row();
        });
        ui.add_space(6.0);
        match slot.kind {
            OverlayKind::Input => {
                Grid::new("ov-input").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                    ui.label("Keyboard");
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut slot.input.layout, KeyboardLayout::Gaming, "Gaming (left half)");
                        ui.radio_value(&mut slot.input.layout, KeyboardLayout::Full, "Whole keyboard");
                    });
                    ui.end_row();
                    ui.label("Mouse");
                    ui.checkbox(&mut slot.input.show_mouse, "Show the mouse (buttons, wheel, scrolling, side buttons)");
                    ui.end_row();
                    ui.label("Key labels");
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut slot.input.labels, KeyLabels::Auto, "Automatic");
                        ui.radio_value(&mut slot.input.labels, KeyLabels::De, "German (QWERTZ)");
                        ui.radio_value(&mut slot.input.labels, KeyLabels::Us, "US (QWERTY)");
                    });
                    ui.end_row();
                });
                if !self.ov_access {
                    ui.add_space(6.0);
                    egui::Frame::new().fill(egui::Color32::from_rgb(0x3a, 0x2a, 0x12)).corner_radius(6).inner_margin(10).show(ui, |ui| {
                        ui.label(RichText::new("Keyboard & mouse access needed").strong().color(egui::Color32::from_rgb(0xff, 0xc0, 0x60)));
                        ui.label("Wayland does not let apps see global key presses. The overlay reads your keyboard and mouse directly, which needs membership in the 'input' group. Run this once, then log out and back in:");
                        let mut cmd = "sudo usermod -aG input $USER".to_string();
                        ui.add(egui::TextEdit::singleline(&mut cmd).code_editor().desired_width(320.0));
                        ui.label(RichText::new("Note: every program you run can then read keyboard input. Rust Screen Recorder only uses the current key state for drawing and never stores it.").small().color(TEXT_DIM));
                    });
                }
            }
            OverlayKind::Webcam => {
                let cams = crate::overlays::webcam::list();
                Grid::new("ov-cam").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                    ui.label("Camera");
                    let cur = if slot.webcam.device.is_empty() {
                        "First camera".to_string()
                    } else {
                        cams.iter().find(|(d, _)| *d == slot.webcam.device).map(|(d, n)| format!("{n} ({d})")).unwrap_or(slot.webcam.device.clone())
                    };
                    ComboBox::from_id_salt("ov-cam-dev").width(320.0).selected_text(cur).show_ui(ui, |ui| {
                        ui.selectable_value(&mut slot.webcam.device, String::new(), "First camera");
                        for (d, n) in &cams {
                            ui.selectable_value(&mut slot.webcam.device, d.clone(), format!("{n} ({d})"));
                        }
                    });
                    ui.end_row();
                    ui.label("Mirror");
                    ui.checkbox(&mut slot.webcam.mirror, "Mirror the image (like a selfie view)");
                    ui.end_row();
                });
                if cams.is_empty() {
                    ui.label(RichText::new("No webcam is connected right now.").color(RED));
                }
            }
            OverlayKind::Website => {
                let mut load = false;
                Grid::new("ov-web").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                    ui.label("URL");
                    ui.add(egui::TextEdit::singleline(&mut slot.website.url).hint_text("https://… (alerts, chat, widgets)").desired_width(420.0));
                    ui.end_row();
                    ui.label("Page size");
                    ui.horizontal(|ui| {
                        ui.add(DragValue::new(&mut slot.website.page_width).range(200..=3840).suffix(" px"));
                        ui.label("×");
                        ui.add(DragValue::new(&mut slot.website.page_height).range(100..=2160).suffix(" px"));
                    });
                    ui.end_row();
                    ui.label("Background");
                    ui.checkbox(&mut slot.website.transparent, "Transparent page background");
                    ui.end_row();
                    ui.label("Editor");
                    ui.horizontal(|ui| {
                        load = ui.add_enabled(self.ov_web_job.is_none() && !slot.website.url.trim().is_empty(), egui::Button::new("Load page preview")).clicked();
                        if self.ov_web_job.is_some() {
                            ui.spinner();
                            ui.label("loading…");
                        }
                    });
                    ui.end_row();
                });
                if crate::overlays::web::find_browser().is_none() {
                    ui.label(RichText::new("No Chromium-based browser found (Chromium, Chrome, Brave, Edge or Vivaldi is needed).").color(RED));
                }
                if let Some(e) = &self.ov_web_err {
                    ui.label(RichText::new(e).color(RED));
                }
                if load {
                    let cfg = slot.website.clone();
                    let (tx, rx) = channel();
                    std::thread::spawn(move || {
                        let _ = tx.send(crate::overlays::web::screenshot(&cfg).map_err(|e| format!("{e:#}")));
                    });
                    self.ov_web_job = Some((corner, rx));
                }
                self.website_editor(ui, corner);
            }
            OverlayKind::None => {}
        }
    }

    /// Image size used for the preview boxes.
    fn estimated_size(&self, _corner: Corner, slot: &OverlaySlot) -> (u32, u32) {
        match slot.kind {
            OverlayKind::Input => self.ov_input_tex.as_ref().map(|(_, _, s)| (s[0] as u32, s[1] as u32)).unwrap_or((700, 260)),
            OverlayKind::Webcam => (16, 9),
            OverlayKind::Website => {
                let w = &slot.website;
                if w.crop_w > 0 && w.crop_h > 0 {
                    (w.crop_w, w.crop_h)
                } else {
                    (w.page_width.max(1), w.page_height.max(1))
                }
            }
            OverlayKind::None => (16, 9),
        }
    }

    fn crop_uv(&self, slot: &OverlaySlot) -> egui::Rect {
        let w = &slot.website;
        let (pw, ph) = (w.page_width.max(1) as f32, w.page_height.max(1) as f32);
        if w.crop_w == 0 || w.crop_h == 0 {
            return egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        }
        egui::Rect::from_min_max(
            egui::pos2(w.crop_x as f32 / pw, w.crop_y as f32 / ph),
            egui::pos2(((w.crop_x + w.crop_w) as f32 / pw).min(1.0), ((w.crop_y + w.crop_h) as f32 / ph).min(1.0)),
        )
    }

    fn ensure_input_preview(&mut self, ctx: &egui::Context) {
        let Some(slot) = Corner::ALL.iter().map(|c| self.cfg.overlays.slot(*c)).find(|s| s.kind == OverlayKind::Input) else { return };
        if self.ov_input_tex.as_ref().is_some_and(|(c, _, _)| *c == slot.input) {
            return;
        }
        if let Some(img) = crate::overlays::input::preview(&slot.input) {
            let tex = ctx.load_texture("input-preview", to_color_image(&img), egui::TextureOptions::LINEAR);
            self.ov_input_tex = Some((slot.input.clone(), tex, [img.w as usize, img.h as usize]));
        }
    }

    /// Screenshot of the page with a draggable crop rectangle.
    fn website_editor(&mut self, ui: &mut egui::Ui, corner: Corner) {
        let Some((c, url, tex, size)) = &self.ov_web_tex else {
            ui.label(RichText::new("Load the page preview to choose the visible part with the mouse.").small().color(TEXT_DIM));
            return;
        };
        let slot = self.cfg.overlays.slot_mut(corner);
        if *c != corner || *url != slot.website.url {
            ui.label(RichText::new("Load the page preview again to edit this overlay.").small().color(TEXT_DIM));
            return;
        }
        ui.add_space(4.0);
        ui.label(RichText::new("Drag on the page to cut out the part that should be visible.").small().color(TEXT_DIM));
        let (pw, ph) = (size[0] as f32, size[1] as f32);
        let disp_w = ui.available_width().min(620.0);
        let disp_h = disp_w * ph / pw;
        let (resp, painter) = ui.allocate_painter(egui::vec2(disp_w, disp_h), egui::Sense::drag());
        let r = resp.rect;
        // checkerboard so transparent parts are visible
        let cell = 12.0;
        let (nx, ny) = ((disp_w / cell).ceil() as i32, (disp_h / cell).ceil() as i32);
        for iy in 0..ny {
            for ix in 0..nx {
                let col = if (ix + iy) % 2 == 0 { egui::Color32::from_gray(58) } else { egui::Color32::from_gray(42) };
                let cr = egui::Rect::from_min_size(r.min + egui::vec2(ix as f32 * cell, iy as f32 * cell), egui::vec2(cell, cell)).intersect(r);
                painter.rect_filled(cr, 0.0, col);
            }
        }
        painter.image(tex.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
        let to_page = |p: egui::Pos2| (((p.x - r.min.x) / disp_w * pw).clamp(0.0, pw), ((p.y - r.min.y) / disp_h * ph).clamp(0.0, ph));
        if resp.drag_started() {
            self.ov_drag = resp.interact_pointer_pos();
        }
        if let (Some(a), Some(b)) = (self.ov_drag, resp.interact_pointer_pos()) {
            if resp.dragged() || resp.drag_stopped() {
                let (ax, ay) = to_page(a);
                let (bx, by) = to_page(b);
                let (x0, y0, x1, y1) = (ax.min(bx), ay.min(by), ax.max(bx), ay.max(by));
                if x1 - x0 > 8.0 && y1 - y0 > 8.0 {
                    slot.website.crop_x = x0 as u32;
                    slot.website.crop_y = y0 as u32;
                    slot.website.crop_w = (x1 - x0) as u32;
                    slot.website.crop_h = (y1 - y0) as u32;
                }
            }
        }
        if resp.drag_stopped() {
            self.ov_drag = None;
        }
        // crop rectangle + dimmed outside
        let w = &slot.website;
        if w.crop_w > 0 && w.crop_h > 0 {
            let cr = egui::Rect::from_min_size(
                r.min + egui::vec2(w.crop_x as f32 / pw * disp_w, w.crop_y as f32 / ph * disp_h),
                egui::vec2(w.crop_w as f32 / pw * disp_w, w.crop_h as f32 / ph * disp_h),
            );
            let dim = egui::Color32::from_black_alpha(150);
            painter.rect_filled(egui::Rect::from_min_max(r.min, egui::pos2(r.max.x, cr.min.y)), 0.0, dim);
            painter.rect_filled(egui::Rect::from_min_max(egui::pos2(r.min.x, cr.max.y), r.max), 0.0, dim);
            painter.rect_filled(egui::Rect::from_min_max(egui::pos2(r.min.x, cr.min.y), egui::pos2(cr.min.x, cr.max.y)), 0.0, dim);
            painter.rect_filled(egui::Rect::from_min_max(egui::pos2(cr.max.x, cr.min.y), egui::pos2(r.max.x, cr.max.y)), 0.0, dim);
            painter.rect_stroke(cr, 0.0, egui::Stroke::new(2.0_f32, ACCENT), egui::StrokeKind::Outside);
        }
        let slot = self.cfg.overlays.slot_mut(corner);
        ui.horizontal(|ui| {
            ui.label("Cut-out");
            ui.add(DragValue::new(&mut slot.website.crop_x).prefix("x "));
            ui.add(DragValue::new(&mut slot.website.crop_y).prefix("y "));
            ui.add(DragValue::new(&mut slot.website.crop_w).prefix("w "));
            ui.add(DragValue::new(&mut slot.website.crop_h).prefix("h "));
            if ui.button("Whole page").clicked() {
                slot.website.crop_x = 0;
                slot.website.crop_y = 0;
                slot.website.crop_w = 0;
                slot.website.crop_h = 0;
            }
        });
    }

    fn recording_tab(&mut self, ui: &mut egui::Ui, st: Option<&Status>) {
        ui.add_space(2.0);
        Grid::new("rec-paths").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
            self.path_row(ui, "Save recordings to", PathField::RecordingOut);
        });
        ui.separator();
        ui.heading("Video");
        video_ui(ui, "rec-v", &mut self.cfg.recording.video, st);
        ui.separator();
        ui.heading("Audio");
        audio_ui(ui, "rec-a", &mut self.cfg.recording.audio, &self.audio_sources);
    }

    fn hotkeys_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Hotkeys");
        ui.label(RichText::new("Global shortcuts are registered with KDE's shortcut service and work inside games.").color(TEXT_DIM));
        ui.add_space(6.0);
        Grid::new("hk").num_columns(3).spacing([16.0, 10.0]).show(ui, |ui| {
            for (field, label) in [
                (KeyField::Overlay, "Show / hide overlay"),
                (KeyField::Save, "Save instant replay"),
                (KeyField::Replay, "Instant replay on / off"),
                (KeyField::Record, "Start / stop recording"),
            ] {
                ui.label(label);
                let v = match field {
                    KeyField::Overlay => &mut self.cfg.hotkeys.toggle_overlay,
                    KeyField::Save => &mut self.cfg.hotkeys.save_replay,
                    KeyField::Replay => &mut self.cfg.hotkeys.toggle_replay,
                    KeyField::Record => &mut self.cfg.hotkeys.toggle_recording,
                };
                ui.add(egui::TextEdit::singleline(v).desired_width(200.0));
                let valid = crate::hotkeys::parse_key(v).is_ok();
                ui.horizontal(|ui| {
                    let rec = self.capture_key == Some(field);
                    if ui.button(if rec { "Press keys…" } else { "Record" }).clicked() {
                        self.capture_key = if rec { None } else { Some(field) };
                    }
                    if ui.button("Clear").clicked() {
                        v.clear();
                    }
                    if !valid {
                        ui.label(RichText::new("invalid").color(RED));
                    }
                });
                ui.end_row();
            }
        });
        ui.add_space(12.0);
        ui.label(RichText::new("Command line alternatives (e.g. for custom shortcuts):").strong());
        for c in ["save-replay", "toggle-replay", "toggle-recording", "toggle-overlay"] {
            ui.monospace(format!("rust-screen-recorder ctl {c}"));
        }
    }

    fn games_tab(&mut self, ui: &mut egui::Ui, st: Option<&Status>) {
        ui.heading("Games");
        ui.add_space(6.0);
        let gs = &mut self.cfg.games;
        Grid::new("games").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
            ui.label("Folders");
            ui.checkbox(&mut gs.folder_per_game, "Save clips in a folder named after the game");
            ui.end_row();
            ui.label("Game exit");
            ui.checkbox(&mut gs.stop_recording_on_exit, "Stop and save the recording when the game closes");
            ui.end_row();
            ui.label("Fullscreen");
            ui.checkbox(&mut gs.fullscreen_is_game, "Treat any fullscreen window as a game");
            ui.end_row();
            ui.label("Minimum game window");
            ui.horizontal(|ui| {
                ui.add(DragValue::new(&mut gs.min_width).range(0..=4000).suffix(" px"));
                ui.label("×");
                ui.add(DragValue::new(&mut gs.min_height).range(0..=4000).suffix(" px"));
            });
            ui.end_row();
        });
        ui.label(
            RichText::new("Games are detected through Steam, Lutris, Heroic, Faugus, Bottles and umu/Proton. Their names come from the launcher's library.")
                .color(TEXT_DIM),
        );
        ui.add_space(8.0);
        ui.columns(2, |cols| {
            cols[0].label(RichText::new("Launcher blocklist (never recorded)").strong());
            cols[0].label(RichText::new("One per line. Exact exe name, or *text to match a part of the name.").small().color(TEXT_DIM));
            let mut text = gs.launcher_blocklist.join("\n");
            if cols[0].add(egui::TextEdit::multiline(&mut text).desired_rows(12).code_editor()).changed() {
                gs.launcher_blocklist = text.lines().map(|s| s.to_string()).collect();
            }
            if cols[0].button("Reset to defaults").clicked() {
                gs.launcher_blocklist = crate::games::DEFAULT_LAUNCHER_BLOCKLIST.iter().map(|s| s.to_string()).collect();
            }
            cols[1].label(RichText::new("Extra games (always treated as a game)").strong());
            cols[1].label(RichText::new("One exe name per line, e.g. minecraft-launcher-less-game.").small().color(TEXT_DIM));
            let mut text = gs.extra_games.join("\n");
            if cols[1].add(egui::TextEdit::multiline(&mut text).desired_rows(12).code_editor()).changed() {
                gs.extra_games = text.lines().map(|s| s.to_string()).collect();
            }
        });
        ui.separator();
        ui.label(RichText::new("Open windows").strong());
        if let Some(st) = st {
            Grid::new("wins").num_columns(3).striped(true).spacing([16.0, 6.0]).show(ui, |ui| {
                ui.label(RichText::new("Window").strong());
                ui.label(RichText::new("Size").strong());
                ui.label(RichText::new("Detected as").strong());
                ui.end_row();
                for w in &st.windows {
                    ui.label(&w.caption);
                    ui.label(format!("{}×{}", w.w, w.h));
                    match crate::games::detect(w, &self.cfg.games) {
                        Some(g) => ui.label(RichText::new(format!("🎮 {} ({})", g.name, g.source)).color(ACCENT)),
                        None => ui.label(RichText::new("—").color(TEXT_DIM)),
                    };
                    ui.end_row();
                }
            });
        }
    }

    fn status_tab(&mut self, ui: &mut egui::Ui, st: Option<&Status>) {
        ui.heading("Status");
        let Some(st) = st else {
            ui.label(RichText::new("The daemon is not running.").color(RED));
            return;
        };
        Grid::new("status").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
            ui.label("Version");
            ui.label(format!("v{}", crate::updater::CURRENT));
            ui.end_row();
            ui.label("KWin screencast");
            if st.screencast_available {
                ui.label(RichText::new("available (portal-free)").color(ACCENT));
            } else {
                ui.label(RichText::new("not granted – restart the app once").color(RED));
            }
            ui.end_row();
            ui.label("Target");
            ui.label(&st.target);
            ui.end_row();
            ui.label("Video path");
            ui.label(if st.video_path.is_empty() { "idle" } else { &st.video_path });
            ui.end_row();
            ui.label("Instant replay");
            ui.label(if st.replay_active { "on" } else { "off" });
            ui.end_row();
            ui.label("Recording");
            ui.label(if st.recording_active { fmt_duration(st.recording_secs) } else { "no".into() });
            ui.end_row();
            ui.label("Last message");
            ui.label(&st.last_message);
            ui.end_row();
            ui.label("Last saved");
            ui.label(&st.last_saved);
            ui.end_row();
        });
        ui.horizontal(|ui| {
            if ui.button(if st.replay_active { "Stop instant replay" } else { "Start instant replay" }).clicked() {
                send_async(Request::ToggleReplay);
            }
            if ui.add_enabled(st.replay_active, egui::Button::new("Save replay")).clicked() {
                send_async(Request::SaveReplay);
            }
            if ui.button(if st.recording_active { "Stop recording" } else { "Start recording" }).clicked() {
                send_async(Request::ToggleRecording);
            }
        });
        ui.separator();
        ui.label(RichText::new("Working encoders on this system").strong());
        Grid::new("enc").num_columns(2).striped(true).show(ui, |ui| {
            for codec in Codec::ALL {
                ui.label(codec.label());
                let list: Vec<&str> = st
                    .encoders
                    .iter()
                    .filter(|(c, _)| *c == codec)
                    .map(|(_, b)| match b {
                        Backend::Vaapi => "VA-API",
                        Backend::Nvenc => "NVENC",
                        Backend::Amf => "AMF",
                        Backend::Qsv => "QSV",
                        Backend::Vulkan => "Vulkan",
                        Backend::Cpu => "CPU",
                        Backend::Auto => "",
                    })
                    .collect();
                ui.label(if list.is_empty() { "—".to_string() } else { list.join(", ") });
                ui.end_row();
            }
        });
        ui.add_space(6.0);
        ui.label(RichText::new(format!("FFmpeg logs: {}", crate::session::log_dir().display())).color(TEXT_DIM));
    }
}

/// Straight-alpha BGRA → egui image.
fn to_color_image(img: &crate::overlays::Image) -> egui::ColorImage {
    let mut rgba = img.bgra.clone();
    for p in rgba.chunks_exact_mut(4) {
        p.swap(0, 2);
    }
    egui::ColorImage::from_rgba_unmultiplied([img.w as usize, img.h as usize], &rgba)
}
