//! Tiny JSON-lines protocol over a Unix socket between the daemon and the GUI / CLI.

use crate::config::{Backend, CaptureMode, Codec};
use crate::kwin::WindowInfo;
use crate::wayland::OutputInfo;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Status,
    SaveReplay,
    StartReplay,
    StopReplay,
    ToggleReplay,
    StartRecording,
    StopRecording,
    ToggleRecording,
    ToggleOverlay,
    ReloadConfig,
    SetCaptureMode(CaptureMode),
    SelectWindow(String),
    /// Open the overlay with a settings tab (general|replay|recording|hotkeys|games|status).
    OpenSettings(Option<String>),
    /// Upload the last saved clip to FireShare.
    UploadLastClip,
    /// Copy the last FireShare link to the clipboard again.
    CopyLastLink,
    Quit,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Status {
    pub replay_active: bool,
    pub replay_secs: u32,
    pub recording_active: bool,
    pub recording_secs: u64,
    pub capture_mode: Option<CaptureMode>,
    pub target: String,
    pub game: Option<String>,
    pub selected_window: String,
    pub windows: Vec<WindowInfo>,
    pub outputs: Vec<OutputInfo>,
    pub encoders: Vec<(Codec, Backend)>,
    pub screencast_available: bool,
    pub last_message: String,
    pub last_saved: String,
    #[serde(default)]
    pub video_path: String,
    #[serde(default)]
    pub fireshare_enabled: bool,
    #[serde(default)]
    pub fireshare_auto: bool,
    #[serde(default)]
    pub uploading: bool,
    #[serde(default)]
    pub has_last_clip: bool,
    #[serde(default)]
    pub last_link: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Ok(String),
    Status(Box<Status>),
    Err(String),
}

pub fn socket_path() -> PathBuf {
    dirs::runtime_dir().unwrap_or_else(std::env::temp_dir).join("rust-screen-recorder.sock")
}

pub fn send(req: &Request) -> Result<Response> {
    let mut s = UnixStream::connect(socket_path()).map_err(|_| anyhow!("daemon is not running"))?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    s.write_all(line.as_bytes())?;
    let mut r = BufReader::new(s);
    let mut resp = String::new();
    r.read_line(&mut resp)?;
    Ok(serde_json::from_str(&resp)?)
}

pub fn status() -> Option<Status> {
    match send(&Request::Status) {
        Ok(Response::Status(s)) => Some(*s),
        _ => None,
    }
}

pub fn daemon_running() -> bool {
    UnixStream::connect(socket_path()).is_ok()
}
