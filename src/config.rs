//! Persistent user configuration (stored in ~/.config/rust-screen-recorder/config.toml).
//!
//! This is the application's *own* settings file. Nothing is ever written to KDE / KWin /
//! xdg-desktop-portal configuration files.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
    Vp9,
}

impl Codec {
    pub const ALL: [Codec; 4] = [Codec::H264, Codec::Hevc, Codec::Av1, Codec::Vp9];
    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "H.264 / AVC",
            Codec::Hevc => "H.265 / HEVC",
            Codec::Av1 => "AV1",
            Codec::Vp9 => "VP9",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum Backend {
    Auto,
    Vaapi,
    Nvenc,
    Amf,
    Qsv,
    Vulkan,
    Cpu,
}

impl Backend {
    pub const ALL: [Backend; 7] = [
        Backend::Auto,
        Backend::Vaapi,
        Backend::Nvenc,
        Backend::Amf,
        Backend::Qsv,
        Backend::Vulkan,
        Backend::Cpu,
    ];
    /// Order used when resolving `Auto`.
    pub const AUTO_ORDER: [Backend; 6] = [
        Backend::Nvenc,
        Backend::Vaapi,
        Backend::Qsv,
        Backend::Amf,
        Backend::Vulkan,
        Backend::Cpu,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Backend::Auto => "Auto (best available)",
            Backend::Vaapi => "GPU – VA-API (AMD/Intel)",
            Backend::Nvenc => "GPU – NVENC (NVIDIA)",
            Backend::Amf => "GPU – AMF (AMD proprietary)",
            Backend::Qsv => "GPU – Quick Sync (Intel)",
            Backend::Vulkan => "GPU – Vulkan Video",
            Backend::Cpu => "CPU (software)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RateControl {
    /// Constant bitrate
    Cbr,
    /// Variable bitrate (target + peak)
    Vbr,
    /// Constant quality (CQP / CRF)
    Quality,
}

impl RateControl {
    pub const ALL: [RateControl; 3] = [RateControl::Vbr, RateControl::Cbr, RateControl::Quality];
    pub fn label(self) -> &'static str {
        match self {
            RateControl::Cbr => "CBR (constant bitrate)",
            RateControl::Vbr => "VBR (variable bitrate)",
            RateControl::Quality => "Constant quality (CQP/CRF)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Container {
    Mp4,
    Mkv,
    Mov,
    Webm,
}

impl Container {
    pub const ALL: [Container; 4] = [Container::Mp4, Container::Mkv, Container::Mov, Container::Webm];
    pub fn ext(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Mkv => "mkv",
            Container::Mov => "mov",
            Container::Webm => "webm",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Speed {
    Fastest,
    Fast,
    Balanced,
    Quality,
}

impl Speed {
    pub const ALL: [Speed; 4] = [Speed::Fastest, Speed::Fast, Speed::Balanced, Speed::Quality];
    pub fn label(self) -> &'static str {
        match self {
            Speed::Fastest => "Fastest (lowest load)",
            Speed::Fast => "Fast",
            Speed::Balanced => "Balanced",
            Speed::Quality => "Best quality (highest load)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Preset {
    P480,
    P720,
    P1080,
    Custom,
}

impl Preset {
    pub const ALL: [Preset; 4] = [Preset::P480, Preset::P720, Preset::P1080, Preset::Custom];
    pub fn label(self) -> &'static str {
        match self {
            Preset::P480 => "480p (854×480, 60 fps, 3.5 Mbit/s)",
            Preset::P720 => "720p (1280×720, 60 fps, 8 Mbit/s)",
            Preset::P1080 => "1080p (1920×1080, 60 fps, 16 Mbit/s)",
            Preset::Custom => "Custom",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoParams {
    pub codec: Codec,
    pub backend: Backend,
    /// 0 = keep native (source) size. Otherwise the source is scaled down to fit into
    /// width × height, keeping the aspect ratio (never upscaled).
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub rate_control: RateControl,
    pub bitrate_kbps: u32,
    /// 0..=51 (lower = better), mapped to the encoder specific scale.
    pub quality: u32,
    pub keyframe_interval_secs: f32,
    pub container: Container,
    pub speed: Speed,
    pub ten_bit: bool,
}

impl Default for VideoParams {
    fn default() -> Self {
        Preset::P1080.params().unwrap()
    }
}

impl Preset {
    /// The well-tuned built-in presets. `None` for Custom.
    pub fn params(self) -> Option<VideoParams> {
        let base = VideoParams {
            codec: Codec::H264,
            backend: Backend::Auto,
            width: 1920,
            height: 1080,
            fps: 60,
            rate_control: RateControl::Vbr,
            bitrate_kbps: 16_000,
            quality: 23,
            keyframe_interval_secs: 2.0,
            container: Container::Mp4,
            speed: Speed::Balanced,
            ten_bit: false,
        };
        match self {
            Preset::P480 => Some(VideoParams { width: 854, height: 480, bitrate_kbps: 3_500, ..base }),
            Preset::P720 => Some(VideoParams { width: 1280, height: 720, bitrate_kbps: 8_000, ..base }),
            Preset::P1080 => Some(base),
            Preset::Custom => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoSettings {
    pub preset: Preset,
    pub custom: VideoParams,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self { preset: Preset::P1080, custom: VideoParams::default() }
    }
}

impl VideoSettings {
    pub fn effective(&self) -> VideoParams {
        self.preset.params().unwrap_or_else(|| self.custom.clone())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioKind {
    /// Something you hear (the monitor of a sink).
    Output,
    /// A microphone / line input.
    Input,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioDevice {
    /// PulseAudio/PipeWire source name (sink monitors end in `.monitor`).
    pub name: String,
    pub label: String,
    pub kind: AudioKind,
    /// Volume in percent (100 = unchanged).
    pub volume: u32,
}

impl Default for AudioDevice {
    fn default() -> Self {
        AudioDevice::default_output()
    }
}

impl AudioDevice {
    pub fn default_output() -> Self {
        AudioDevice { name: "@DEFAULT_MONITOR@".into(), label: "Default output".into(), kind: AudioKind::Output, volume: 100 }
    }
    pub fn default_input() -> Self {
        AudioDevice { name: "@DEFAULT_SOURCE@".into(), label: "Default microphone".into(), kind: AudioKind::Input, volume: 100 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodec {
    Aac,
    Opus,
    Flac,
}

impl AudioCodec {
    pub const ALL: [AudioCodec; 3] = [AudioCodec::Aac, AudioCodec::Opus, AudioCodec::Flac];
    pub fn label(self) -> &'static str {
        match self {
            AudioCodec::Aac => "AAC",
            AudioCodec::Opus => "Opus",
            AudioCodec::Flac => "FLAC (lossless)",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    /// Any number of outputs and inputs. Empty = no audio.
    pub devices: Vec<AudioDevice>,
    /// true = mix everything into one track, false = one track per device.
    pub merge: bool,
    pub codec: AudioCodec,
    pub bitrate_kbps: u32,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self { devices: vec![AudioDevice::default_output()], merge: true, codec: AudioCodec::Aac, bitrate_kbps: 160 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureMode {
    /// Desktop while no game runs, automatically switches to the game window (and back).
    Auto,
    /// Capture a whole monitor.
    Desktop,
    /// Automatically capture the window of the detected game.
    Game,
    /// Capture one specific window (picked in the overlay).
    Window,
    /// Capture a fixed screen region.
    Region,
}

impl CaptureMode {
    pub const ALL: [CaptureMode; 5] =
        [CaptureMode::Auto, CaptureMode::Game, CaptureMode::Desktop, CaptureMode::Window, CaptureMode::Region];
    pub fn short(self) -> &'static str {
        match self {
            CaptureMode::Auto => "Auto",
            CaptureMode::Game => "Game only",
            CaptureMode::Desktop => "Desktop",
            CaptureMode::Window => "Window",
            CaptureMode::Region => "Region",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            CaptureMode::Auto => "Auto (desktop, switches to game and back)",
            CaptureMode::Desktop => "Desktop (monitor)",
            CaptureMode::Game => "Game mode (auto-detect game window)",
            CaptureMode::Window => "Single window",
            CaptureMode::Region => "Screen region",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplayStorage {
    Ram,
    Disk,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralSettings {
    pub capture_mode: CaptureMode,
    /// Wayland output name ("" = first/primary).
    pub monitor: String,
    pub show_cursor: bool,
    /// In-game overlay notifications (top right, above fullscreen games).
    pub overlay_notifications: bool,
    /// Additional desktop notifications (Plasma notification centre).
    pub notifications: bool,
    pub region_x: i32,
    pub region_y: i32,
    pub region_w: u32,
    pub region_h: u32,
    pub autostart: bool,
    /// Zero-copy GPU capture + encoding (DMA-BUF → VA-API) when the encoder is VA-API.
    pub zero_copy: bool,
    /// Check GitHub for a new release at every start and install it automatically.
    pub auto_update: bool,
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            capture_mode: CaptureMode::Auto,
            monitor: String::new(),
            show_cursor: false,
            overlay_notifications: true,
            notifications: false,
            region_x: 0,
            region_y: 0,
            region_w: 1280,
            region_h: 720,
            autostart: false,
            zero_copy: true,
            auto_update: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReplaySettings {
    /// Start instant replay automatically (desktop mode: on daemon start, game mode: when a game appears).
    pub auto_start: bool,
    pub duration_secs: u32,
    pub storage: ReplayStorage,
    /// Where replay segments are buffered when storage = Disk.
    pub disk_buffer_dir: String,
    pub output_dir: String,
    pub video: VideoSettings,
    pub audio: AudioSettings,
}

impl Default for ReplaySettings {
    fn default() -> Self {
        Self {
            auto_start: true,
            duration_secs: 60,
            storage: ReplayStorage::Ram,
            disk_buffer_dir: dirs::cache_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join("rust-screen-recorder/replay-buffer")
                .to_string_lossy()
                .into(),
            output_dir: default_video_dir().join("Replays").to_string_lossy().into(),
            video: VideoSettings::default(),
            audio: AudioSettings::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingSettings {
    pub output_dir: String,
    pub video: VideoSettings,
    pub audio: AudioSettings,
}

impl Default for RecordingSettings {
    fn default() -> Self {
        Self {
            output_dir: default_video_dir().join("Recordings").to_string_lossy().into(),
            video: VideoSettings::default(),
            audio: AudioSettings {
                devices: vec![AudioDevice::default_output(), AudioDevice::default_input()],
                ..Default::default()
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeySettings {
    pub toggle_overlay: String,
    pub save_replay: String,
    pub toggle_replay: String,
    pub toggle_recording: String,
}

impl Default for HotkeySettings {
    fn default() -> Self {
        Self {
            toggle_overlay: "Alt+Z".into(),
            save_replay: "Alt+F10".into(),
            toggle_replay: "Alt+Shift+F10".into(),
            toggle_recording: "Alt+F9".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GameSettings {
    /// Save clips into a sub folder named after the game.
    pub folder_per_game: bool,
    /// Executables / window titles that are launchers and must never be recorded.
    /// Case-insensitive. Entries match the exe name exactly, or as a substring when they
    /// start with '*'.
    pub launcher_blocklist: Vec<String>,
    /// Additional executables that should be treated as games (exe name, case-insensitive).
    pub extra_games: Vec<String>,
    /// Treat any fullscreen window as a game even if no launcher info was found.
    pub fullscreen_is_game: bool,
    /// Windows smaller than this are never considered games (filters splash screens / launchers).
    pub min_width: u32,
    pub min_height: u32,
    /// Stop an active recording automatically when the game closes.
    pub stop_recording_on_exit: bool,
}

impl Default for GameSettings {
    fn default() -> Self {
        Self {
            folder_per_game: true,
            launcher_blocklist: crate::games::DEFAULT_LAUNCHER_BLOCKLIST.iter().map(|s| s.to_string()).collect(),
            extra_games: vec![],
            fullscreen_is_game: false,
            min_width: 640,
            min_height: 360,
            stop_recording_on_exit: true,
        }
    }
}

/// Upload to a self-hosted FireShare server (https://github.com/ShaneIsrael/fireshare).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FireshareSettings {
    pub enabled: bool,
    /// e.g. https://clips.example.com
    pub url: String,
    /// Upload token from FireShare → Settings → Security → Upload Tokens.
    pub token: String,
    /// Upload every saved instant replay clip automatically.
    pub auto_upload_replays: bool,
    /// Also upload finished recordings.
    pub upload_recordings: bool,
    /// Link the clip to the game in FireShare's library (when FireShare knows the game).
    pub link_game: bool,
    /// Put clips into a FireShare folder named after the game.
    pub folder_per_game: bool,
    /// Fixed FireShare folder (empty = FireShare's default upload folder).
    pub folder: String,
    /// Copy the share link to the clipboard after uploading.
    pub copy_link: bool,
}

impl Default for FireshareSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            token: String::new(),
            auto_upload_replays: true,
            upload_recordings: false,
            link_game: true,
            folder_per_game: false,
            folder: String::new(),
            copy_link: true,
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Video overlays (burned into recordings and clips)
// ------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Corner {
    pub const ALL: [Corner; 4] = [Corner::TopLeft, Corner::TopRight, Corner::BottomLeft, Corner::BottomRight];
    pub fn label(self) -> &'static str {
        match self {
            Corner::TopLeft => "Top left",
            Corner::TopRight => "Top right",
            Corner::BottomLeft => "Bottom left",
            Corner::BottomRight => "Bottom right",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OverlayKind {
    None,
    /// Keyboard + mouse input display.
    Input,
    Webcam,
    /// Any web page (alerts, chat, widgets …) rendered by a headless browser.
    Website,
}

impl OverlayKind {
    pub const ALL: [OverlayKind; 4] = [OverlayKind::None, OverlayKind::Input, OverlayKind::Webcam, OverlayKind::Website];
    pub fn label(self) -> &'static str {
        match self {
            OverlayKind::None => "Empty",
            OverlayKind::Input => "Keyboard & mouse",
            OverlayKind::Webcam => "Webcam",
            OverlayKind::Website => "Website",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyboardLayout {
    /// Whole keyboard (F-row, main block, navigation, numpad).
    Full,
    /// The important left half for gaming (Esc–5/T/G/B, modifiers, space).
    Gaming,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyLabels {
    Auto,
    De,
    Us,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InputOverlay {
    pub layout: KeyboardLayout,
    pub show_mouse: bool,
    pub labels: KeyLabels,
}

impl Default for InputOverlay {
    fn default() -> Self {
        Self { layout: KeyboardLayout::Gaming, show_mouse: true, labels: KeyLabels::Auto }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebcamOverlay {
    /// /dev/videoN ("" = first camera).
    pub device: String,
    pub mirror: bool,
}

impl Default for WebcamOverlay {
    fn default() -> Self {
        Self { device: String::new(), mirror: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebsiteOverlay {
    pub url: String,
    /// Browser viewport the page is rendered at.
    pub page_width: u32,
    pub page_height: u32,
    /// Part of the page that is shown (page pixels). w/h = 0 → whole page.
    pub crop_x: u32,
    pub crop_y: u32,
    pub crop_w: u32,
    pub crop_h: u32,
    /// Transparent page background (for alert / chat widgets).
    pub transparent: bool,
}

impl Default for WebsiteOverlay {
    fn default() -> Self {
        Self { url: String::new(), page_width: 1280, page_height: 720, crop_x: 0, crop_y: 0, crop_w: 0, crop_h: 0, transparent: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlaySlot {
    pub kind: OverlayKind,
    /// Width as percent of the video width.
    pub size_percent: f32,
    pub opacity: f32,
    /// Distance from the corner in percent of the video height.
    pub margin_percent: f32,
    pub input: InputOverlay,
    pub webcam: WebcamOverlay,
    pub website: WebsiteOverlay,
}

impl Default for OverlaySlot {
    fn default() -> Self {
        Self {
            kind: OverlayKind::None,
            size_percent: 22.0,
            opacity: 1.0,
            margin_percent: 2.0,
            input: InputOverlay::default(),
            webcam: WebcamOverlay::default(),
            website: WebsiteOverlay::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlaySettings {
    /// Burn the overlays into recordings and instant replay clips.
    pub enabled: bool,
    pub top_left: OverlaySlot,
    pub top_right: OverlaySlot,
    pub bottom_left: OverlaySlot,
    pub bottom_right: OverlaySlot,
}

impl Default for OverlaySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            top_left: OverlaySlot::default(),
            top_right: OverlaySlot::default(),
            bottom_left: OverlaySlot { kind: OverlayKind::Input, ..Default::default() },
            bottom_right: OverlaySlot::default(),
        }
    }
}

impl OverlaySettings {
    pub fn slot(&self, c: Corner) -> &OverlaySlot {
        match c {
            Corner::TopLeft => &self.top_left,
            Corner::TopRight => &self.top_right,
            Corner::BottomLeft => &self.bottom_left,
            Corner::BottomRight => &self.bottom_right,
        }
    }
    pub fn slot_mut(&mut self, c: Corner) -> &mut OverlaySlot {
        match c {
            Corner::TopLeft => &mut self.top_left,
            Corner::TopRight => &mut self.top_right,
            Corner::BottomLeft => &mut self.bottom_left,
            Corner::BottomRight => &mut self.bottom_right,
        }
    }
    pub fn active(&self) -> bool {
        self.enabled && Corner::ALL.iter().any(|c| self.slot(*c).kind != OverlayKind::None)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub general: GeneralSettings,
    pub replay: ReplaySettings,
    pub recording: RecordingSettings,
    pub hotkeys: HotkeySettings,
    pub games: GameSettings,
    pub fireshare: FireshareSettings,
    pub overlays: OverlaySettings,
}

pub fn default_video_dir() -> PathBuf {
    dirs::video_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Videos")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("Rust Screen Recorder")
}

pub fn config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("RSR_CONFIG") {
        return PathBuf::from(p);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("rust-screen-recorder/config.toml")
}

impl Config {
    pub fn load() -> Config {
        let p = config_path();
        match std::fs::read_to_string(&p) {
            Ok(s) => match toml::from_str(&s) {
                Ok(c) => c,
                Err(e) => {
                    log::error!("Invalid config {}: {e} – using defaults", p.display());
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let p = config_path();
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = p.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?)?;
        // The file can hold the FireShare upload token: owner read/write only.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(tmp, p)?;
        Ok(())
    }
}

/// Expand a leading `~/`.
pub fn expand_path(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest);
        }
    }
    PathBuf::from(p)
}
