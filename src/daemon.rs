//! The background daemon: owns the capture, the encoders, hotkeys, tray and IPC.

use crate::capture::{Capture, FrameSlot, Source};
use crate::config::{CaptureMode, Config, ReplayStorage};
use crate::games::{self, GameInfo};
use crate::hotkeys::{self, HotkeyAction};
use crate::ipc::{Request, Response, Status};
use crate::kwin::{self, KwinSink, WindowInfo, WindowUpdate};
use crate::session::{self, EncoderSession, Output, ReplaySession};
use crate::toast::ToastKind;
use crate::wayland::{StreamSource, Wayland};
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Child;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

pub enum Event {
    Req(Request, Option<oneshot::Sender<Response>>),
    Hotkey(HotkeyAction),
    Windows(WindowUpdate),
    SaveDone(Result<(PathBuf, f64, String), String>),
    /// (result, file name)
    UploadDone(Result<crate::fireshare::Uploaded, String>, String),
}

struct ActiveCapture {
    source: StreamSource,
    stream_id: u64,
    cap: Capture,
}

struct Recording {
    enc: EncoderSession,
    path: PathBuf,
    game: Option<String>,
    source: StreamSource,
    handle: Arc<Source>,
}

struct Replay {
    session: ReplaySession,
    source: StreamSource,
    handle: Arc<Source>,
    /// Game name for clip folders (None = desktop/window).
    game: Option<String>,
}

struct Daemon {
    cfg: Config,
    wl: Wayland,
    dbus: zbus::Connection,
    tx: mpsc::UnboundedSender<Event>,
    encoders: Vec<(crate::config::Codec, crate::config::Backend)>,
    captures: Vec<ActiveCapture>,
    replay: Option<Replay>,
    recording: Option<Recording>,
    windows: Vec<WindowInfo>,
    active_window: String,
    selected_window: String,
    game: Option<(String, GameInfo)>,
    /// (window id, caption) → detection result cache
    detect_cache: HashMap<(String, String), Option<GameInfo>>,
    overlay: Option<Child>,
    last_message: String,
    last_saved: String,
    saving: bool,
    tray: Option<ksni::Handle<crate::tray::Tray>>,
    toaster: Option<crate::toast::Toaster>,
    /// Last saved clip (path, game) – for FireShare uploads.
    last_clip: Option<(PathBuf, Option<String>)>,
    uploading: bool,
    last_link: String,
    upload_queue: std::collections::VecDeque<(PathBuf, Option<String>)>,
    /// Mirrors upload_queue.len() for the progress toast ticker.
    queue_len: Arc<std::sync::atomic::AtomicUsize>,
    upload: Option<UploadJob>,
}

struct UploadJob {
    name: String,
    progress: Arc<crate::fireshare::Progress>,
    /// Stops the toast ticker.
    done: Arc<std::sync::atomic::AtomicBool>,
}

pub fn run() -> Result<()> {
    let update_note = startup_update();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async_main(update_note))
}

/// Auto-update before anything is captured. Returns a message for the user (toast).
fn startup_update() -> Option<(ToastKind, String, String)> {
    use crate::updater::{self, Outcome};
    if let Ok(from) = std::env::var(updater::ENV_UPDATED_FROM) {
        // We are the freshly installed version (re-executed after the update).
        unsafe { std::env::remove_var(updater::ENV_UPDATED_FROM) };
        return Some((ToastKind::Success, format!("Updated to v{}", updater::CURRENT), format!("Rust Screen Recorder was updated from v{from}")));
    }
    if !Config::load().general.auto_update || std::env::var_os("RSR_NO_UPDATE").is_some() {
        return None;
    }
    let exe = crate::self_exe();
    match updater::check_and_install(&exe) {
        Ok(Outcome::UpToDate) => {
            log::info!("up to date (v{})", updater::CURRENT);
            None
        }
        Ok(Outcome::Available(tag, why)) => {
            log::info!("update {tag} available but not installed: {why}");
            Some((ToastKind::Info, format!("Update {tag} available"), format!("Not installed automatically: {why}")))
        }
        Ok(Outcome::Installed(tag)) => {
            log::info!("installed {tag} – restarting");
            use std::os::unix::process::CommandExt;
            let err = std::process::Command::new(&exe)
                .args(std::env::args_os().skip(1))
                .env(updater::ENV_UPDATED_FROM, updater::CURRENT)
                .exec();
            log::error!("could not restart into the new version: {err}");
            Some((ToastKind::Info, format!("Updated to {tag}"), "Restart Rust Screen Recorder to use the new version".into()))
        }
        Err(e) => {
            log::warn!("update check failed: {e:#}");
            None
        }
    }
}

async fn async_main(update_note: Option<(ToastKind, String, String)>) -> Result<()> {
    if !crate::encoders::ffmpeg_available() {
        return Err(anyhow!("ffmpeg was not found in PATH – please install ffmpeg"));
    }
    let changed = crate::install::ensure_desktop_entry().unwrap_or(false);
    if changed {
        // Give KWin's service cache a moment to see the new entry.
        tokio::time::sleep(Duration::from_millis(800)).await;
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();

    // D-Bus: single instance + KWin script sink.
    let (win_tx, mut win_rx) = mpsc::unbounded_channel::<WindowUpdate>();
    let dbus = zbus::connection::Builder::session()?
        .name(kwin::BUS_NAME)?
        .serve_at("/KWin", KwinSink { tx: win_tx })?
        .build()
        .await
        .map_err(|e| anyhow!("cannot own D-Bus name (is the daemon already running?): {e}"))?;
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(u) = win_rx.recv().await {
                let _ = tx.send(Event::Windows(u));
            }
        });
    }

    let wl = tokio::task::spawn_blocking(Wayland::start).await??;
    let encoders = tokio::task::spawn_blocking(crate::encoders::detect_available).await?;

    let cfg = Config::load();

    if let Err(e) = kwin::load_script(&dbus).await {
        log::error!("could not load KWin window script: {e:#}");
    }

    // Hotkeys
    let (hk_tx, mut hk_rx) = mpsc::unbounded_channel::<HotkeyAction>();
    if let Err(e) = hotkeys::register(&dbus, &cfg.hotkeys).await {
        log::error!("could not register global shortcuts: {e:#}");
    }
    {
        let conn = dbus.clone();
        tokio::spawn(async move {
            if let Err(e) = hotkeys::listen(conn, hk_tx).await {
                log::error!("hotkey listener: {e:#}");
            }
        });
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(a) = hk_rx.recv().await {
                let _ = tx.send(Event::Hotkey(a));
            }
        });
    }

    // IPC server
    {
        let path = crate::ipc::socket_path();
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path)?;
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let (r, mut w) = sock.into_split();
                    let mut lines = BufReader::new(r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let resp = match serde_json::from_str::<Request>(&line) {
                            Ok(req) => {
                                let (otx, orx) = oneshot::channel();
                                let _ = tx.send(Event::Req(req, Some(otx)));
                                orx.await.unwrap_or(Response::Err("daemon busy".into()))
                            }
                            Err(e) => Response::Err(format!("bad request: {e}")),
                        };
                        let mut s = serde_json::to_string(&resp).unwrap_or_default();
                        s.push('\n');
                        if w.write_all(s.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
    }

    // Tray
    let tray = crate::tray::spawn(tx.clone()).await;

    // Signals
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = term.recv() => {},
            }
            let _ = tx.send(Event::Req(Request::Quit, None));
        });
    }

    let mut d = Daemon {
        cfg,
        wl,
        dbus: dbus.clone(),
        tx: tx.clone(),
        encoders,
        captures: vec![],
        replay: None,
        recording: None,
        windows: vec![],
        active_window: String::new(),
        selected_window: String::new(),
        game: None,
        detect_cache: HashMap::new(),
        overlay: None,
        last_message: String::new(),
        last_saved: String::new(),
        saving: false,
        tray,
        last_clip: None,
        uploading: false,
        last_link: String::new(),
        upload_queue: Default::default(),
        queue_len: Default::default(),
        upload: None,
        toaster: match crate::toast::Toaster::start() {
            Ok(t) => Some(t),
            Err(e) => {
                log::warn!("in-game notifications unavailable: {e:#}");
                None
            }
        },
    };

    if !d.wl.available {
        d.notify(
            ToastKind::Error,
            "Screen capture not permitted yet",
            "KWin has not granted the screencast interface. The launcher entry was just installed – please restart Rust Screen Recorder.",
        )
        .await;
    }

    if let Some((kind, title, body)) = &update_note {
        d.notify(*kind, title, body).await;
    } else {
    d.notify(
        ToastKind::Info,
        "Rust Screen Recorder is running",
        &format!("{} opens the overlay with all settings", d.cfg.hotkeys.toggle_overlay),
    )
    .await;
    }

    // Desktop/Region mode: start replay right away.
    if d.cfg.replay.auto_start && matches!(d.cfg.general.capture_mode, CaptureMode::Desktop | CaptureMode::Region | CaptureMode::Auto) {
        if d.blocking(|d| d.start_replay()).await.is_some() {
            let s = format!("Press {} to save the last {} seconds", d.cfg.hotkeys.save_replay, d.cfg.replay.duration_secs);
            d.notify(ToastKind::Success, "Instant Replay on", &s).await;
        }
    }

    let mut tick = tokio::time::interval(Duration::from_millis(1000));
    loop {
        tokio::select! {
            ev = rx.recv() => {
                let Some(ev) = ev else { break };
                if !d.handle(ev).await {
                    break;
                }
            }
            _ = tick.tick() => d.tick().await,
        }
    }

    d.shutdown();
    let _ = kwin::unload_script(&dbus).await;
    let _ = std::fs::remove_file(crate::ipc::socket_path());
    Ok(())
}

impl Daemon {
    /// Run a blocking operation (spawning encoders etc.) without stalling the tokio runtime.
    async fn blocking<R>(&mut self, f: impl FnOnce(&mut Daemon) -> Result<R>) -> Option<R> {
        let r = tokio::task::block_in_place(|| f(self));
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                let msg = format!("{e:#}");
                log::error!("{msg}");
                self.last_message = msg.clone();
                self.notify(ToastKind::Error, "Rust Screen Recorder", &msg).await;
                None
            }
        }
    }

    async fn handle(&mut self, ev: Event) -> bool {
        match ev {
            Event::Req(req, reply) => {
                let quit = matches!(req, Request::Quit);
                let resp = self.request(req).await;
                if let Some(r) = reply {
                    let _ = r.send(resp);
                }
                self.update_tray().await;
                return !quit;
            }
            Event::Hotkey(a) => {
                let req = match a {
                    HotkeyAction::ToggleOverlay => Request::ToggleOverlay,
                    HotkeyAction::SaveReplay => Request::SaveReplay,
                    HotkeyAction::ToggleReplay => Request::ToggleReplay,
                    HotkeyAction::ToggleRecording => Request::ToggleRecording,
                };
                let _ = self.request(req).await;
                self.update_tray().await;
            }
            Event::Windows(u) => {
                self.windows = u.windows;
                self.active_window = u.active;
                self.on_windows_changed().await;
            }
            Event::UploadDone(r, name) => {
                self.uploading = false;
                if let Some(job) = self.upload.take() {
                    job.done.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                match r {
                    Ok(up) => {
                        self.last_link = up.url.clone();
                        let copied = self.cfg.fireshare.copy_link && crate::fireshare::copy_to_clipboard(&self.dbus, &up.url).await;
                        let title = if up.duplicate { "Already on FireShare" } else { "Uploaded to FireShare" };
                        let body = if copied { format!("Link copied · {}", up.url) } else { up.url.clone() };
                        self.last_message = format!("{title}: {} ({name})", up.url);
                        self.notify(ToastKind::Success, title, &body).await;
                    }
                    Err(e) => {
                        self.last_message = format!("FireShare upload failed: {e}");
                        self.notify(ToastKind::Error, "FireShare upload failed", &format!("{name}: {e}")).await;
                    }
                }
                self.next_upload().await;
            }
            Event::SaveDone(r) => {
                self.saving = false;
                match r {
                    Ok((p, secs, what)) => {
                        self.last_saved = p.display().to_string();
                        self.last_message = format!("Saved {} ({secs:.0} s)", p.display());
                        let game = if what == "Desktop" { None } else { Some(what.clone()) };
                        self.last_clip = Some((p.clone(), game.clone()));
                        self.notify(ToastKind::Success, "Clip saved", &format!("Last {secs:.0} seconds · {what}")).await;
                        if self.cfg.fireshare.enabled && self.cfg.fireshare.auto_upload_replays {
                            self.start_upload(p, game).await;
                        }
                    }
                    Err(e) => {
                        self.last_message = format!("Saving failed: {e}");
                        self.notify(ToastKind::Error, "Saving the clip failed", &e).await;
                    }
                }
            }
        }
        true
    }

    async fn request(&mut self, req: Request) -> Response {
        match req {
            Request::Status => return Response::Status(Box::new(self.status())),
            Request::SaveReplay => self.save_replay().await,
            Request::StartReplay => {
                if self.replay.is_none() {
                    if self.blocking(|d| d.start_replay()).await.is_some() {
                        let s = format!("Press {} to save the last {} seconds", self.cfg.hotkeys.save_replay, self.cfg.replay.duration_secs);
                        self.notify(ToastKind::Success, "Instant Replay on", &s).await;
                    }
                }
            }
            Request::StopReplay => {
                if self.replay.is_some() {
                    self.stop_replay();
                    self.notify(ToastKind::Info, "Instant Replay off", "Clips can no longer be saved").await;
                }
            }
            Request::ToggleReplay => {
                let r = if self.replay.is_some() { Request::StopReplay } else { Request::StartReplay };
                return Box::pin(self.request(r)).await;
            }
            Request::StartRecording => {
                if self.recording.is_none() {
                    if let Some(what) = self.blocking(|d| d.start_recording()).await {
                        let body = format!("{what} · stop with {}", self.cfg.hotkeys.toggle_recording);
                        self.notify(ToastKind::Record, "Recording started", &body).await;
                    }
                }
            }
            Request::StopRecording => {
                self.finish_recording("").await;
            }
            Request::ToggleRecording => {
                let r = if self.recording.is_some() { Request::StopRecording } else { Request::StartRecording };
                return Box::pin(self.request(r)).await;
            }
            Request::ToggleOverlay => self.toggle_overlay(),
            Request::OpenSettings(tab) => self.open_overlay(tab),
            Request::UploadLastClip => match self.last_clip.clone() {
                Some((p, game)) if self.cfg.fireshare.enabled => self.start_upload(p, game).await,
                Some(_) => self.notify(ToastKind::Info, "FireShare is off", "Enable it in the Instant Replay settings").await,
                None => self.notify(ToastKind::Info, "No clip yet", &format!("Save a clip with {} first", self.cfg.hotkeys.save_replay)).await,
            },
            Request::CopyLastLink => {
                if !self.last_link.is_empty() && crate::fireshare::copy_to_clipboard(&self.dbus, &self.last_link).await {
                    self.notify(ToastKind::Success, "Link copied", &self.last_link.clone()).await;
                }
            }
            Request::ReloadConfig => {
                let old = std::mem::replace(&mut self.cfg, Config::load());
                if old.hotkeys != self.cfg.hotkeys {
                    if let Err(e) = hotkeys::register(&self.dbus, &self.cfg.hotkeys).await {
                        log::error!("hotkeys: {e:#}");
                    }
                }
                if old.general.autostart != self.cfg.general.autostart {
                    let _ = crate::install::set_autostart(self.cfg.general.autostart);
                }
                self.detect_cache.clear();
                if old.games != self.cfg.games {
                    self.game = None;
                }
                // Restart replay with the new settings.
                let replay_cfg_changed = old.replay != self.cfg.replay
                    || old.general.capture_mode != self.cfg.general.capture_mode
                    || old.general.monitor != self.cfg.general.monitor
                    || old.general.show_cursor != self.cfg.general.show_cursor
                    || old.general.zero_copy != self.cfg.general.zero_copy;
                if old.general.capture_mode != self.cfg.general.capture_mode {
                    self.on_windows_changed().await;
                }
                if replay_cfg_changed && self.replay.is_some() {
                    self.stop_replay();
                    self.blocking(|d| d.start_replay()).await;
                }
                return Response::Ok("reloaded".into());
            }
            Request::SetCaptureMode(m) => {
                if self.cfg.general.capture_mode != m {
                    self.cfg.general.capture_mode = m;
                    let _ = self.cfg.save();
                    let had_replay = self.replay.is_some();
                    self.stop_replay();
                    self.on_windows_changed().await;
                    let auto = self.cfg.replay.auto_start && !matches!(m, CaptureMode::Window);
                    if (had_replay || auto) && self.replay.is_none() && self.desired_source().is_ok() {
                        self.blocking(|d| d.start_replay()).await;
                    }
                }
            }
            Request::SelectWindow(id) => {
                self.selected_window = id;
                if self.cfg.general.capture_mode != CaptureMode::Window {
                    self.cfg.general.capture_mode = CaptureMode::Window;
                    let _ = self.cfg.save();
                }
                let had_replay = self.replay.is_some();
                self.stop_replay();
                if had_replay || self.cfg.replay.auto_start {
                    self.blocking(|d| d.start_replay()).await;
                }
            }
            Request::Quit => return Response::Ok("bye".into()),
        }
        Response::Ok(self.last_message.clone())
    }

    fn status(&mut self) -> Status {
        let target = self.target_label();
        let gpu: Vec<bool> = self.replay.iter().map(|r| r.session.enc.gpu).chain(self.recording.iter().map(|r| r.enc.gpu)).collect();
        Status {
            replay_active: self.replay.is_some(),
            replay_secs: self.cfg.replay.duration_secs,
            recording_active: self.recording.is_some(),
            recording_secs: self.recording.as_ref().map(|r| r.enc.started.elapsed().as_secs()).unwrap_or(0),
            capture_mode: Some(self.cfg.general.capture_mode),
            target,
            game: self.game.as_ref().map(|(_, g)| g.name.clone()),
            selected_window: self.selected_window.clone(),
            windows: self
                .windows
                .iter()
                .filter(|w| w.normal && w.caption != kwin::OVERLAY_TITLE && w.pid != std::process::id() as i32)
                .cloned()
                .collect(),
            outputs: self.wl.outputs(),
            encoders: self.encoders.clone(),
            screencast_available: self.wl.available,
            last_message: self.last_message.clone(),
            last_saved: self.last_saved.clone(),
            fireshare_enabled: self.cfg.fireshare.enabled,
            fireshare_auto: self.cfg.fireshare.auto_upload_replays,
            uploading: self.uploading,
            upload_name: self.upload.as_ref().map(|u| u.name.clone()).unwrap_or_default(),
            upload_progress: self.upload.as_ref().map(|u| u.progress.fraction()).unwrap_or(0.0),
            upload_sent: self.upload.as_ref().map(|u| u.progress.sent.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(0),
            upload_total: self.upload.as_ref().map(|u| u.progress.total.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(0),
            upload_phase: self.upload.as_ref().map(|u| u.progress.phase.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(0),
            upload_queue: self.upload_queue.len(),
            has_last_clip: self.last_clip.is_some(),
            last_link: self.last_link.clone(),
            video_path: if gpu.is_empty() {
                String::new()
            } else if gpu.iter().all(|g| *g) {
                "GPU zero-copy (DMA-BUF to VA-API)".into()
            } else if gpu.iter().any(|g| *g) {
                "mixed (GPU zero-copy + CPU)".into()
            } else {
                "CPU copy".into()
            },
        }
    }

    fn desktop_label(&self) -> String {
        if self.cfg.general.monitor.is_empty() {
            "Desktop (primary monitor)".into()
        } else {
            format!("Desktop ({})", self.cfg.general.monitor)
        }
    }

    fn target_label(&self) -> String {
        match self.cfg.general.capture_mode {
            CaptureMode::Desktop => self.desktop_label(),
            CaptureMode::Auto => match &self.game {
                Some((_, g)) => format!("Auto · Game: {} ({})", g.name, g.source),
                None => format!("Auto · {}", self.desktop_label()),
            },
            CaptureMode::Game => match &self.game {
                Some((_, g)) => format!("Game: {} ({})", g.name, g.source),
                None => "Waiting for a game…".into(),
            },
            CaptureMode::Window => self
                .windows
                .iter()
                .find(|w| w.id == self.selected_window)
                .map(|w| format!("Window: {}", w.caption))
                .unwrap_or_else(|| "No window selected".into()),
            CaptureMode::Region => {
                let g = &self.cfg.general;
                format!("Region {}×{} at {},{}", g.region_w, g.region_h, g.region_x, g.region_y)
            }
        }
    }

    // ------------------------------------------------------------------------------------------
    // Capture handling (a small pool: replay and recording may capture different sources)
    // ------------------------------------------------------------------------------------------

    /// What should be captured right now, plus the game name for file naming.
    fn desired_source(&self) -> Result<(StreamSource, Option<String>)> {
        let g = &self.cfg.general;
        let desktop = StreamSource::Output(g.monitor.clone());
        Ok(match g.capture_mode {
            CaptureMode::Desktop => (desktop, None),
            CaptureMode::Region => {
                (StreamSource::Region { x: g.region_x, y: g.region_y, w: g.region_w.max(16), h: g.region_h.max(16) }, None)
            }
            CaptureMode::Window => {
                let Some(w) = self.windows.iter().find(|w| w.id == self.selected_window) else {
                    return Err(anyhow!("No window selected – pick one in the overlay (Alt+Z)"));
                };
                (StreamSource::Window(w.id.clone()), games::detect(w, &self.cfg.games).map(|g| g.name))
            }
            CaptureMode::Game => match &self.game {
                Some((id, info)) => (StreamSource::Window(id.clone()), Some(info.name.clone())),
                None => return Err(anyhow!("No game detected – start a game first")),
            },
            CaptureMode::Auto => match &self.game {
                Some((id, info)) => (StreamSource::Window(id.clone()), Some(info.name.clone())),
                None => (desktop, None),
            },
        })
    }

    fn ensure_capture(&mut self, src: &StreamSource) -> Result<Arc<FrameSlot>> {
        if let Some(c) = self.captures.iter().find(|c| &c.source == src && !c.cap.slot.is_closed()) {
            return Ok(c.cap.slot.clone());
        }
        let (stream_id, node) = self.wl.start_stream(src.clone(), self.cfg.general.show_cursor)?;
        let cap = match Capture::start(node, self.want_gpu()) {
            Ok(c) => c,
            Err(e) => {
                self.wl.close_stream(stream_id);
                return Err(e);
            }
        };
        let slot = cap.slot.clone();
        self.captures.push(ActiveCapture { source: src.clone(), stream_id, cap });
        Ok(slot)
    }

    /// Use zero-copy DMA-BUF capture when an encoder will run on VA-API.
    fn want_gpu(&self) -> bool {
        if !self.cfg.general.zero_copy {
            return false;
        }
        [self.cfg.replay.video.effective(), self.cfg.recording.video.effective()].iter().any(|v| {
            crate::encoders::resolve_backend(v.codec, v.backend, &self.encoders) == Some(crate::config::Backend::Vaapi)
        })
    }

    /// Stop every capture that neither replay nor recording uses any more.
    fn release_unused(&mut self) {
        let used: Vec<StreamSource> =
            self.replay.iter().map(|r| r.source.clone()).chain(self.recording.iter().map(|r| r.source.clone())).collect();
        let mut keep = vec![];
        for c in self.captures.drain(..) {
            if used.contains(&c.source) && !c.cap.slot.is_closed() {
                keep.push(c);
            } else {
                c.cap.stop();
                self.wl.close_stream(c.stream_id);
            }
        }
        self.captures = keep;
    }

    /// Planned encoder size: the monitor in Auto/Desktop mode (so switching between the desktop
    /// and a smaller game window keeps full resolution), otherwise the captured source.
    fn size_hint(&self) -> Option<(u32, u32)> {
        if !matches!(self.cfg.general.capture_mode, CaptureMode::Auto | CaptureMode::Desktop) {
            return None;
        }
        let outs = self.wl.outputs();
        let m = &self.cfg.general.monitor;
        outs.iter()
            .find(|o| !m.is_empty() && &o.name == m)
            .or_else(|| outs.first())
            .filter(|o| o.width > 0 && o.height > 0)
            .map(|o| (o.width as u32, o.height as u32))
    }

    fn output_file(&self, base: &str, game: Option<&str>, kind: &str, ext: &str) -> PathBuf {
        let base = crate::config::expand_path(base);
        let label = game.unwrap_or("Desktop").to_string();
        let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
        let file = format!("{}_{kind}_{stamp}.{ext}", games::sanitize(&label));
        if self.cfg.games.folder_per_game {
            base.join(games::sanitize(&label)).join(file)
        } else {
            base.join(file)
        }
    }

    // ------------------------------------------------------------------------------------------
    // Replay / recording
    // ------------------------------------------------------------------------------------------

    fn start_replay(&mut self) -> Result<()> {
        if self.replay.is_some() {
            return Ok(());
        }
        let (src, game) = self.desired_source()?;
        let slot = self.ensure_capture(&src)?;
        let handle = Source::new(slot);
        let hint = self.size_hint();
        let r = &self.cfg.replay;
        let dir = session::replay_buffer_dir(r.storage, &r.disk_buffer_dir);
        if r.storage == ReplayStorage::Ram && !PathBuf::from("/dev/shm").is_dir() {
            return Err(anyhow!("/dev/shm is not available for RAM replay"));
        }
        match ReplaySession::start(handle.clone(), hint, &r.video.effective(), &r.audio, &self.encoders, dir, r.duration_secs.max(5)) {
            Ok(s) => {
                self.replay = Some(Replay { session: s, source: src, handle, game });
                self.last_message = "Instant Replay is on".into();
                Ok(())
            }
            Err(e) => {
                self.release_unused();
                Err(e)
            }
        }
    }

    fn stop_replay(&mut self) {
        if let Some(r) = self.replay.take() {
            tokio::task::block_in_place(|| r.session.stop());
            self.last_message = "Instant Replay is off".into();
        }
        self.release_unused();
    }

    /// Point replay (and in Auto mode also the recording) at whatever `desired_source` says now.
    ///
    /// Auto mode switches *seamlessly*: the running encoders keep going and only get a new frame
    /// source, so the replay buffer and the recording continue across desktop ↔ game.
    async fn retarget(&mut self) -> bool {
        let Ok((want, game)) = self.desired_source() else {
            if self.cfg.general.capture_mode != CaptureMode::Auto {
                self.stop_replay();
            }
            return false;
        };
        let auto = self.cfg.general.capture_mode == CaptureMode::Auto;
        if auto {
            let replay_differs = self.replay.as_ref().is_some_and(|r| r.source != want);
            let rec_differs = self.recording.as_ref().is_some_and(|r| r.source != want);
            if replay_differs || rec_differs {
                let w2 = want.clone();
                let Some(slot) = self.blocking(move |d| d.ensure_capture(&w2)).await else { return false };
                if let Some(r) = self.replay.as_mut() {
                    r.handle.set(slot.clone());
                    r.source = want.clone();
                    r.game = game.clone();
                }
                if let Some(r) = self.recording.as_mut() {
                    r.handle.set(slot.clone());
                    r.source = want.clone();
                }
                self.release_unused();
                log::info!("switched capture seamlessly to {want:?}");
                return true;
            }
            if self.replay.is_none() && self.cfg.replay.auto_start {
                return self.blocking(|d| d.start_replay()).await.is_some();
            }
            return false;
        }
        // Other modes: restart the replay on the new target.
        if self.replay.as_ref().is_some_and(|r| r.source == want) {
            return false;
        }
        let was_on = self.replay.is_some();
        self.stop_replay();
        if was_on || self.cfg.replay.auto_start {
            return self.blocking(|d| d.start_replay()).await.is_some();
        }
        false
    }

    async fn save_replay(&mut self) {
        let Some(r) = &self.replay else {
            self.last_message = "Instant Replay is not running".into();
            self.notify(ToastKind::Info, "Instant Replay is off", &format!("Turn it on with {}", self.cfg.hotkeys.toggle_replay)).await;
            return;
        };
        if self.saving {
            return;
        }
        let ext = r.session.enc.container.ext();
        let game = r.game.clone();
        let out = self.output_file(&self.cfg.replay.output_dir, game.as_deref(), "Replay", ext);
        match self.replay.as_ref().unwrap().session.snapshot(out) {
            Ok(job) => {
                self.saving = true;
                self.last_message = format!("Saving replay to {}", job.out.display());
                let tx = self.tx.clone();
                let what = game.unwrap_or_else(|| "Desktop".into());
                tokio::task::spawn_blocking(move || {
                    let r = job
                        .run()
                        .map(|p| {
                            let secs = session::probe_duration(&p).unwrap_or(0.0);
                            (p, secs, what)
                        })
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Event::SaveDone(r));
                });
            }
            Err(e) => {
                self.last_message = format!("{e:#}");
                self.notify(ToastKind::Error, "Instant Replay", &format!("{e:#}")).await;
            }
        }
    }

    fn start_recording(&mut self) -> Result<String> {
        let (src, game) = self.desired_source()?;
        let slot = self.ensure_capture(&src)?;
        let handle = Source::new(slot);
        let hint = self.size_hint();
        let rc = &self.cfg.recording;
        let v = rc.video.effective();
        let container = crate::encoders::effective_container(v.codec, v.container);
        let path = self.output_file(&rc.output_dir, game.as_deref(), "Recording", container.ext());
        match EncoderSession::spawn("recording", handle.clone(), hint, &v, &rc.audio, &self.encoders, Output::File(path.clone())) {
            Ok(enc) => {
                self.recording = Some(Recording { enc, path: path.clone(), game: game.clone(), source: src, handle });
                self.last_message = format!("Recording to {}", path.display());
                Ok(game.unwrap_or_else(|| "Desktop".into()))
            }
            Err(e) => {
                self.release_unused();
                Err(e)
            }
        }
    }

    fn stop_recording(&mut self) -> Option<(PathBuf, u64, Option<String>)> {
        let r = self.recording.take()?;
        let secs = r.enc.started.elapsed().as_secs();
        tokio::task::block_in_place(|| r.enc.stop());
        self.last_message = format!("Saved {}", r.path.display());
        self.release_unused();
        Some((r.path, secs, r.game))
    }

    async fn finish_recording(&mut self, why: &str) {
        if let Some((p, secs, game)) = self.stop_recording() {
            if self.cfg.fireshare.enabled && self.cfg.fireshare.upload_recordings {
                self.start_upload(p.clone(), game).await;
            }
            self.last_saved = p.display().to_string();
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let body = if why.is_empty() { name } else { format!("{why} – {name}") };
            self.notify(ToastKind::Success, &format!("Recording saved ({})", crate::gui::fmt_duration(secs)), &body).await;
        }
    }

    // ------------------------------------------------------------------------------------------
    // Game mode
    // ------------------------------------------------------------------------------------------

    fn detect_cached(&mut self, w: &WindowInfo) -> Option<GameInfo> {
        let key = (w.id.clone(), w.caption.clone());
        if let Some(r) = self.detect_cache.get(&key) {
            return r.clone();
        }
        let r = games::detect(w, &self.cfg.games);
        if let Some(g) = &r {
            log::info!("detected game '{}' via {} (exe {}, window '{}')", g.name, g.source, g.exe, w.caption);
        }
        self.detect_cache.insert(key, r.clone());
        r
    }

    async fn on_windows_changed(&mut self) {
        // Drop cache entries for windows that no longer exist.
        let ids: std::collections::HashSet<String> = self.windows.iter().map(|w| w.id.clone()).collect();
        self.detect_cache.retain(|(id, _), _| ids.contains(id));
        let mode = self.cfg.general.capture_mode;

        // Window-mode: selected window vanished.
        if mode == CaptureMode::Window && !self.selected_window.is_empty() && !ids.contains(&self.selected_window) {
            self.selected_window.clear();
            self.finish_recording("The captured window was closed").await;
            self.stop_replay();
        }

        if !matches!(mode, CaptureMode::Game | CaptureMode::Auto) {
            self.game = None;
            return;
        }

        // Current game still alive?
        if let Some((id, g)) = &self.game {
            if !ids.contains(id) {
                let name = g.name.clone();
                self.game = None;
                self.on_game_closed(&name).await;
            }
        }

        // Prefer the focused window; otherwise any game window.
        let mut candidate: Option<(String, GameInfo)> = None;
        let active = self.windows.iter().find(|w| w.id == self.active_window).cloned();
        if let Some(w) = active {
            if let Some(g) = self.detect_cached(&w) {
                candidate = Some((w.id.clone(), g));
            }
        }
        if candidate.is_none() && self.game.is_none() {
            let ws = self.windows.clone();
            for w in ws.iter().rev() {
                if let Some(g) = self.detect_cached(w) {
                    candidate = Some((w.id.clone(), g));
                    break;
                }
            }
        }
        let Some((id, info)) = candidate else { return };
        if self.game.as_ref().is_some_and(|(cur, _)| *cur == id) {
            return;
        }
        // Do not switch away from a game while it is being recorded.
        if mode == CaptureMode::Game && self.game.is_some() && self.recording.as_ref().is_some_and(|r| matches!(r.source, StreamSource::Window(_))) {
            return;
        }
        self.game = Some((id, info.clone()));
        self.last_message = format!("Game detected: {}", info.name);
        let replay_was_on = self.replay.is_some();
        if self.retarget().await {
            let body = if replay_was_on && mode == CaptureMode::Auto {
                let rec = if self.recording.is_some() { " · recording continues" } else { "" };
                format!("Now capturing the game – replay keeps the desktop part{rec}")
            } else {
                format!("Instant Replay on · {} saves the last {} s", self.cfg.hotkeys.save_replay, self.cfg.replay.duration_secs)
            };
            self.notify(ToastKind::Success, &info.name, &body).await;
        } else if self.replay.is_none() {
            self.notify(ToastKind::Info, &info.name, &format!("Game detected · record with {}", self.cfg.hotkeys.toggle_recording)).await;
        }
        self.update_tray().await;
    }

    /// The game window is gone: save its recording, and in Auto mode go back to the desktop.
    async fn on_game_closed(&mut self, name: &str) {
        let rec_on_game = self.recording.as_ref().is_some_and(|r| matches!(r.source, StreamSource::Window(_)));
        if rec_on_game && self.cfg.general.capture_mode != CaptureMode::Auto {
            if self.cfg.games.stop_recording_on_exit || self.cfg.general.capture_mode == CaptureMode::Game {
                self.finish_recording(&format!("{name} was closed")).await;
            }
        }
        let replay_was_on = self.replay.is_some();
        self.last_message = format!("{name} was closed");
        if self.cfg.general.capture_mode == CaptureMode::Auto {
            if self.retarget().await && replay_was_on {
                let rec = if self.recording.is_some() { " · recording continues" } else { "" };
                self.notify(ToastKind::Info, "Back to desktop", &format!("{name} closed · replay keeps the game part{rec}")).await;
            }
        } else {
            self.stop_replay();
        }
        self.update_tray().await;
    }

    // ------------------------------------------------------------------------------------------

    async fn tick(&mut self) {
        // Captures that KWin closed (window gone, output unplugged …).
        let closed: Vec<StreamSource> = self.captures.iter().filter(|c| c.cap.slot.is_closed()).map(|c| c.source.clone()).collect();
        for src in closed {
            let auto = self.cfg.general.capture_mode == CaptureMode::Auto;
            // The game window may close before KWin's window list update arrives.
            if let (StreamSource::Window(id), Some((gid, g))) = (&src, &self.game) {
                if id == gid {
                    let name = g.name.clone();
                    self.game = None;
                    self.on_game_closed(&name).await;
                    self.release_unused();
                    continue;
                }
            }
            if auto {
                self.retarget().await;
            } else {
                if self.recording.as_ref().is_some_and(|r| r.source == src) {
                    self.finish_recording("Capture ended").await;
                }
                if self.replay.as_ref().is_some_and(|r| r.source == src) {
                    self.stop_replay();
                }
            }
            self.release_unused();
        }
        if let Some(r) = &mut self.replay {
            if !r.session.enc.alive() {
                log::error!("replay encoder exited – see {}", session::log_dir().join("ffmpeg-replay.log").display());
                if let Some(r) = self.replay.take() {
                    r.session.stop();
                }
                self.last_message = "Instant Replay encoder stopped unexpectedly (see ffmpeg-replay.log)".into();
                self.notify(ToastKind::Error, "Instant Replay stopped", "The encoder exited unexpectedly – check the encoder settings.").await;
                self.release_unused();
            } else {
                r.session.cleanup();
            }
        }
        if let Some(rec) = &mut self.recording {
            if !rec.enc.alive() {
                let p = rec.path.clone();
                self.recording = None;
                self.last_message = "Recording encoder stopped unexpectedly (see ffmpeg-recording.log)".into();
                self.notify(ToastKind::Error, "Recording stopped", &format!("The encoder exited unexpectedly. Partial file: {}", p.display())).await;
                self.release_unused();
            }
        }
        if let Some(o) = &mut self.overlay {
            if !matches!(o.try_wait(), Ok(None)) {
                self.overlay = None;
            }
        }
        self.update_tray().await;
    }

    fn toggle_overlay(&mut self) {
        if let Some(mut o) = self.overlay.take() {
            if matches!(o.try_wait(), Ok(None)) {
                let _ = o.kill();
                let _ = o.wait();
                return;
            }
        }
        self.open_overlay(None);
    }

    /// Show the overlay, optionally directly on a settings tab (replaces an open overlay).
    fn open_overlay(&mut self, tab: Option<String>) {
        if let Some(mut o) = self.overlay.take() {
            if matches!(o.try_wait(), Ok(None)) {
                if tab.is_none() {
                    self.overlay = Some(o);
                    return;
                }
                let _ = o.kill();
                let _ = o.wait();
            }
        }
        let mut cmd = std::process::Command::new(crate::self_exe());
        cmd.arg("overlay");
        if let Some(t) = tab {
            cmd.arg(t);
        }
        match cmd.spawn() {
            Ok(c) => self.overlay = Some(c),
            Err(e) => log::error!("overlay: {e}"),
        }
    }

    /// Queue a clip for upload to FireShare (uploads run one after another).
    async fn start_upload(&mut self, path: PathBuf, game: Option<String>) {
        if self.upload_queue.iter().any(|(p, _)| *p == path) || self.upload.as_ref().is_some_and(|u| path.ends_with(&u.name)) {
            return;
        }
        self.upload_queue.push_back((path, game));
        self.queue_len.store(self.upload_queue.len().saturating_sub(usize::from(!self.uploading)), std::sync::atomic::Ordering::Relaxed);
        if self.uploading {
            let n = self.upload_queue.len();
            self.notify(ToastKind::Upload, "Queued for FireShare", &format!("{n} clip{} waiting", if n == 1 { "" } else { "s" })).await;
            return;
        }
        self.next_upload().await;
    }

    async fn next_upload(&mut self) {
        let Some((path, game)) = self.upload_queue.pop_front() else { return };
        self.queue_len.store(self.upload_queue.len(), std::sync::atomic::Ordering::Relaxed);
        self.uploading = true;
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let progress = Arc::new(crate::fireshare::Progress::default());
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.upload = Some(UploadJob { name: name.clone(), progress: progress.clone(), done: done.clone() });

        // Progress toast, refreshed 4× per second while the upload runs.
        if self.cfg.general.overlay_notifications {
            if let Some(toaster) = self.toaster.clone() {
                let (p, d, n, ql) = (progress.clone(), done.clone(), name.clone(), self.queue_len.clone());
                tokio::spawn(async move {
                    let start = std::time::Instant::now();
                    while !d.load(std::sync::atomic::Ordering::Relaxed) {
                        let queued = ql.load(std::sync::atomic::Ordering::Relaxed);
                        let (title, body) = progress_text(&p, &n, start.elapsed().as_secs_f64(), queued);
                        toaster.show_progress(ToastKind::Upload, title, body, p.fraction());
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                });
            }
        }

        let cfg = self.cfg.fireshare.clone();
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let title = path.file_stem().map(|s| s.to_string_lossy().replace('_', " ")).unwrap_or_default();
            let r = crate::fireshare::upload(&cfg, &path, &title, game.as_deref(), progress).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::UploadDone(r, name));
        });
    }

    /// In-game toast and/or desktop notification (fire and forget – never blocks).
    async fn notify(&self, kind: ToastKind, summary: &str, body: &str) {
        if self.cfg.general.overlay_notifications {
            if let Some(t) = &self.toaster {
                t.show(kind, summary, body);
            }
        }
        if !self.cfg.general.notifications {
            return;
        }
        let conn = self.dbus.clone();
        let (summary, body) = (summary.to_string(), body.to_string());
        tokio::spawn(async move {
            let hints: HashMap<&str, zbus::zvariant::Value> = HashMap::new();
            let args = ("Rust Screen Recorder", 0u32, "media-record", summary.as_str(), body.as_str(), Vec::<&str>::new(), hints, 4000i32);
            let call = conn.call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "Notify",
                &args,
            );
            match tokio::time::timeout(Duration::from_secs(5), call).await {
                Ok(Err(e)) => log::debug!("notification failed: {e}"),
                Err(_) => log::debug!("notification server did not answer"),
                _ => {}
            }
        });
    }

    async fn update_tray(&self) {
        if let Some(t) = &self.tray {
            let (rep, rec) = (self.replay.is_some(), self.recording.is_some());
            let target = self.target_label();
            t.update(move |t| {
                t.replay = rep;
                t.recording = rec;
                t.target = target;
            })
            .await;
        }
    }

    fn shutdown(&mut self) {
        if let Some(r) = self.recording.take() {
            r.enc.stop();
        }
        if let Some(r) = self.replay.take() {
            r.session.stop();
        }
        self.release_unused();
        if let Some(mut o) = self.overlay.take() {
            let _ = o.kill();
        }
    }
}

/// "Uploading to FireShare · 42 %" / "clip.mp4 · 12.3 / 29.0 MB · 3.1 MB/s · 6 s left · +2 queued"
pub fn progress_text(p: &crate::fireshare::Progress, name: &str, elapsed: f64, queued: usize) -> (String, String) {
    use std::sync::atomic::Ordering;
    let (sent, total, phase) = (p.sent.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed), p.phase.load(Ordering::Relaxed));
    let q = if queued > 0 { format!(" · +{queued} queued") } else { String::new() };
    match phase {
        0 => ("Preparing upload…".into(), format!("{name}{q}")),
        2 => ("Finishing upload…".into(), format!("FireShare is processing {name}{q}")),
        _ => {
            let pct = p.fraction() * 100.0;
            let speed = if elapsed > 0.5 { sent as f64 / elapsed } else { 0.0 };
            let eta = if speed > 0.0 && total > sent { format!(" · {:.0} s left", (total - sent) as f64 / speed) } else { String::new() };
            (
                format!("Uploading to FireShare · {pct:.0} %"),
                format!("{:.1} / {:.1} MB · {:.1} MB/s{eta}{q}", sent as f64 / 1e6, total as f64 / 1e6, speed / 1e6),
            )
        }
    }
}
