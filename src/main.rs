//! rust-screen-recorder – ShadowPlay-style instant replay & recording for KDE Plasma (Wayland).

mod audio;
mod capture;
mod config;
mod daemon;
mod encoders;
mod fireshare;
mod ffi;
mod games;
mod gpu;
mod gui;
mod hotkeys;
mod install;
mod ipc;
mod kwin;
mod session;
mod toast;
mod tray;
mod wayland;

use anyhow::{anyhow, Result};
use ipc::{Request, Response};
use std::os::unix::process::CommandExt;
use std::time::Duration;

const HELP: &str = "\
rust-screen-recorder – instant replay & recording for KDE Plasma

USAGE:
    rust-screen-recorder                 start the background recorder (shows the overlay if it already runs)
    rust-screen-recorder daemon          run the daemon in the foreground
    rust-screen-recorder settings [TAB]  open the overlay on a settings tab (general|replay|recording|hotkeys|games|status)
    rust-screen-recorder overlay [TAB]   show the overlay (optionally with a settings tab open)
    rust-screen-recorder ctl <ACTION>    control the running daemon
    rust-screen-recorder install         install the launcher entry (needed once for portal-free capture)
    rust-screen-recorder uninstall       remove launcher + autostart entries
    rust-screen-recorder upload <FILE> [GAME]   upload a video to FireShare, prints the share link

ACTIONS:
    save-replay  toggle-replay  start-replay  stop-replay
    toggle-recording  start-recording  stop-recording
    toggle-overlay  settings  status  reload  quit
    upload-last (upload the last clip to FireShare)  copy-link
";

/// Path of this program on disk. After an update/rebuild the running binary is "(deleted)", so
/// the suffix is stripped to start the *new* binary (overlay etc.) instead of failing.
pub fn self_exe() -> std::path::PathBuf {
    let p = std::env::current_exe().unwrap_or_else(|_| "rust-screen-recorder".into());
    let s = p.to_string_lossy();
    match s.strip_suffix(" (deleted)") {
        Some(clean) => clean.into(),
        None => p,
    }
}

pub fn spawn_daemon() -> Result<()> {
    let exe = self_exe();
    let log_dir = session::log_dir();
    std::fs::create_dir_all(&log_dir)?;
    let log = std::fs::File::create(log_dir.join("daemon.log"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon").stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(())
}

fn ensure_daemon() -> Result<()> {
    if ipc::daemon_running() {
        return Ok(());
    }
    spawn_daemon()?;
    for _ in 0..100 {
        if ipc::daemon_running() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(anyhow!("daemon did not start – see {}", session::log_dir().join("daemon.log").display()))
}

fn ctl(action: &str) -> Result<()> {
    let req = match action {
        "save-replay" => Request::SaveReplay,
        "toggle-replay" => Request::ToggleReplay,
        "start-replay" => Request::StartReplay,
        "stop-replay" => Request::StopReplay,
        "toggle-recording" | "toggle-record" => Request::ToggleRecording,
        "start-recording" => Request::StartRecording,
        "stop-recording" => Request::StopRecording,
        "toggle-overlay" => Request::ToggleOverlay,
        "settings" => Request::OpenSettings(Some("general".into())),
        "upload-last" | "fireshare-upload" => Request::UploadLastClip,
        "copy-link" => Request::CopyLastLink,
        "reload" => Request::ReloadConfig,
        "status" => Request::Status,
        "quit" => Request::Quit,
        other => return Err(anyhow!("unknown action '{other}'\n\n{HELP}")),
    };
    match ipc::send(&req)? {
        Response::Ok(m) => {
            if !m.is_empty() {
                println!("{m}");
            }
        }
        Response::Status(s) => {
            println!("target:      {}", s.target);
            println!("replay:      {}", if s.replay_active { "on" } else { "off" });
            println!("recording:   {}", if s.recording_active { gui::fmt_duration(s.recording_secs) } else { "no".into() });
            println!("game:        {}", s.game.unwrap_or_else(|| "-".into()));
            println!("screencast:  {}", if s.screencast_available { "available" } else { "NOT granted" });
            println!("video path:  {}", if s.video_path.is_empty() { "-" } else { &s.video_path });
            println!("encoders:    {:?}", s.encoders);
            println!("message:     {}", s.last_message);
            println!("windows:");
            let cfg = config::Config::load();
            for w in &s.windows {
                let g = games::detect(w, &cfg.games).map(|g| format!("  => GAME '{}' ({})", g.name, g.source)).unwrap_or_default();
                println!("  [{}] pid {} {}x{} '{}'{}", w.id, w.pid, w.w, w.h, w.caption, g);
            }
        }
        Response::Err(e) => return Err(anyhow!(e)),
    }
    Ok(())
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,zbus=warn,tracing=warn")).format_timestamp_millis().init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(|s| s.as_str()) {
        // Launching the app: start the background recorder; if it already runs, show the overlay.
        None => {
            if ipc::daemon_running() {
                ipc::send(&Request::ToggleOverlay).map(|_| ())
            } else {
                ensure_daemon().map(|_| println!("Rust Screen Recorder runs in the background – press Alt+Z for the overlay."))
            }
        }
        Some("daemon") => daemon::run(),
        Some("settings") => ensure_daemon().and_then(|_| {
            let tab = args.get(1).cloned().unwrap_or_else(|| "general".into());
            ipc::send(&Request::OpenSettings(Some(tab))).map(|_| ())
        }),
        Some("overlay") => {
            let tab = args.get(1).map(|s| s.as_str());
            ensure_daemon().and_then(|_| gui::overlay::run(tab))
        }
        Some("ctl") => match args.get(1) {
            Some(a) => ctl(a),
            None => Err(anyhow!("missing action\n\n{HELP}")),
        },
        Some("install") => install::ensure_desktop_entry().map(|changed| {
            println!(
                "{}",
                if changed { "Launcher entry installed. Restart the daemon to enable portal-free capture." } else { "Already installed." }
            )
        }),
        Some("uninstall") => install::uninstall(),
        Some("upload") => match args.get(1) {
            Some(file) => {
                let cfg = config::Config::load();
                let path = std::path::PathBuf::from(file);
                let title = path.file_stem().map(|s| s.to_string_lossy().replace('_', " ")).unwrap_or_default();
                fireshare::upload(&cfg.fireshare, &path, &title, args.get(2).map(|s| s.as_str())).map(|u| {
                    println!("{}{}", u.url, if u.duplicate { "  (already on FireShare)" } else { "" });
                })
            }
            None => Err(anyhow!("usage: rust-screen-recorder upload <file> [game]")),
        },
        Some("-h" | "--help" | "help") => {
            print!("{HELP}");
            Ok(())
        }
        Some(o) => Err(anyhow!("unknown command '{o}'\n\n{HELP}")),
    };
    if let Err(e) = r {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
