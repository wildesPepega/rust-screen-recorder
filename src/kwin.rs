//! KWin integration: a small KWin script (loaded at runtime over D-Bus, never installed into
//! KWin's configuration) reports all windows, their PIDs and internal UUIDs back to the daemon.
//! The UUID is what `zkde_screencast.stream_window` needs.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

pub const BUS_NAME: &str = "org.rustscreenrecorder.Daemon";
pub const SCRIPT_NAME: &str = "rust-screen-recorder-windows";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct WindowInfo {
    pub id: String,
    pub pid: i32,
    pub caption: String,
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub fullscreen: bool,
    #[serde(default)]
    pub normal: bool,
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    #[serde(default)]
    pub w: i32,
    #[serde(default)]
    pub h: i32,
    #[serde(default)]
    pub output: String,
}

#[derive(Debug)]
pub struct WindowUpdate {
    pub windows: Vec<WindowInfo>,
    pub active: String,
}

const SCRIPT: &str = r#"
const OVERLAY_TITLE = "Rust Screen Recorder Overlay";
function info(w) {
    var g = w.frameGeometry;
    return {
        id: w.internalId.toString(),
        pid: w.pid,
        caption: w.caption,
        class: w.resourceClass ? w.resourceClass.toString() : "",
        fullscreen: !!w.fullScreen,
        normal: !!w.normalWindow,
        x: Math.round(g.x), y: Math.round(g.y), w: Math.round(g.width), h: Math.round(g.height),
        output: w.output ? w.output.name : ""
    };
}
var timer = null;
function push() {
    var list = [];
    var all = workspace.windowList();
    for (var i = 0; i < all.length; i++) {
        var w = all[i];
        if (w.deleted || w.specialWindow || w.desktopWindow || w.dock) continue;
        list.push(info(w));
    }
    var a = workspace.activeWindow;
    callDBus("org.rustscreenrecorder.Daemon", "/KWin", "org.rustscreenrecorder.KWin", "Update",
             JSON.stringify(list), a ? a.internalId.toString() : "");
}
function setupOverlay(w) {
    // Dialogs opened from the overlay (folder picker) must stay above it.
    if (w.caption.indexOf("Rust Screen Recorder - ") === 0) {
        w.keepAbove = true;
        workspace.activeWindow = w;
    }
    if (w.caption === OVERLAY_TITLE) {
        w.keepAbove = true;
        w.skipTaskbar = true;
        w.skipSwitcher = true;
        w.skipPager = true;
        w.onAllDesktops = true;
        workspace.activeWindow = w;
    }
}
function watch(w) {
    setupOverlay(w);
    w.captionChanged.connect(function () { setupOverlay(w); push(); });
    w.fullScreenChanged.connect(push);
    w.frameGeometryChanged.connect(function () {});
}
var existing = workspace.windowList();
for (var j = 0; j < existing.length; j++) watch(existing[j]);
workspace.windowAdded.connect(function (w) { watch(w); push(); });
workspace.windowRemoved.connect(push);
workspace.windowActivated.connect(push);
push();
"#;

pub struct KwinSink {
    pub tx: UnboundedSender<WindowUpdate>,
}

#[zbus::interface(name = "org.rustscreenrecorder.KWin")]
impl KwinSink {
    async fn update(&self, windows: String, active: String) {
        match serde_json::from_str::<Vec<WindowInfo>>(&windows) {
            Ok(windows) => {
                let _ = self.tx.send(WindowUpdate { windows, active });
            }
            Err(e) => log::warn!("bad window list from KWin script: {e}"),
        }
    }
}

/// Load (or reload) the runtime KWin script. The script file lives in $XDG_RUNTIME_DIR.
pub async fn load_script(conn: &zbus::Connection) -> Result<()> {
    let dir = dirs::runtime_dir().unwrap_or_else(std::env::temp_dir).join("rust-screen-recorder");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("kwin-windows.js");
    std::fs::write(&path, SCRIPT)?;
    let _ = unload_script(conn).await;
    let reply = conn
        .call_method(
            Some("org.kde.KWin"),
            "/Scripting",
            Some("org.kde.kwin.Scripting"),
            "loadScript",
            &(path.to_string_lossy().to_string(), SCRIPT_NAME),
        )
        .await?;
    let id: i32 = reply.body().deserialize()?;
    if id < 0 {
        anyhow::bail!("KWin refused to load the window script");
    }
    // KWin 6 exposes loaded scripts as /Scripting/Script<id>; older builds as /<id>.
    for path in [format!("/Scripting/Script{id}"), format!("/{id}")] {
        if conn
            .call_method(Some("org.kde.KWin"), path.as_str(), Some("org.kde.kwin.Script"), "run", &())
            .await
            .is_ok()
        {
            log::info!("KWin window script running ({path})");
            return Ok(());
        }
    }
    // Fallback: start all loaded scripts.
    conn.call_method(Some("org.kde.KWin"), "/Scripting", Some("org.kde.kwin.Scripting"), "start", &()).await?;
    Ok(())
}

pub async fn unload_script(conn: &zbus::Connection) -> Result<()> {
    conn.call_method(Some("org.kde.KWin"), "/Scripting", Some("org.kde.kwin.Scripting"), "unloadScript", &(SCRIPT_NAME,))
        .await?;
    Ok(())
}

pub const OVERLAY_TITLE: &str = "Rust Screen Recorder Overlay";
