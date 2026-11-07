//! Desktop integration.
//!
//! KWin hands its privileged `zkde_screencast_unstable_v1` interface only to programs whose
//! application .desktop entry (matched by the absolute executable path in `Exec=`) declares it
//! in `X-KDE-Wayland-Interfaces`. This is how Spectacle records without the portal.
//! We only install our *own* launcher entry – no KDE/KWin/portal configuration is touched.

use anyhow::Result;
use std::path::PathBuf;

pub const DESKTOP_ID: &str = "rust-screen-recorder.desktop";

fn exe() -> Result<PathBuf> {
    Ok(crate::self_exe().canonicalize()?)
}

fn applications_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from("~/.local/share")).join("applications")
}

fn desktop_entry(exe: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Rust Screen Recorder\n\
         GenericName=Screen Recorder\n\
         Comment=Instant replay, game and screen recording for KDE Plasma\n\
         Exec={exe}\n\
         Icon=media-record\n\
         Terminal=false\n\
         Categories=AudioVideo;Video;Recorder;\n\
         Keywords=record;replay;clip;shadowplay;screencast;\n\
         X-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1\n\
         StartupNotify=false\n"
    )
}

/// Write (or refresh) the launcher entry. Returns true when the file changed.
pub fn ensure_desktop_entry() -> Result<bool> {
    let exe = exe()?;
    let dir = applications_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(DESKTOP_ID);
    let content = desktop_entry(&exe.to_string_lossy());
    if std::fs::read_to_string(&path).ok().as_deref() == Some(content.as_str()) {
        return Ok(false);
    }
    std::fs::write(&path, content)?;
    // Refresh the KDE service cache so KWin sees the entry immediately.
    let _ = std::process::Command::new("kbuildsycoca6")
        .arg("--noincremental")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    log::info!("installed {}", path.display());
    Ok(true)
}

/// Optional login autostart (user opt-in from the settings).
pub fn set_autostart(enabled: bool) -> Result<()> {
    let dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from("~/.config")).join("autostart");
    let path = dir.join("rust-screen-recorder-daemon.desktop");
    if enabled {
        std::fs::create_dir_all(&dir)?;
        let exe = exe()?;
        std::fs::write(
            &path,
            format!(
                "[Desktop Entry]\nType=Application\nName=Rust Screen Recorder (background)\nExec={} daemon\nIcon=media-record\nX-KDE-autostart-phase=2\nNoDisplay=true\n",
                exe.display()
            ),
        )?;
    } else if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let p = applications_dir().join(DESKTOP_ID);
    if p.exists() {
        std::fs::remove_file(&p)?;
    }
    set_autostart(false)?;
    Ok(())
}
