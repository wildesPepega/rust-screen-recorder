pub mod overlay;
pub mod settings;
pub mod theme;

use crate::ipc::{self, Request, Status};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;

/// Polls the daemon status in the background.
#[derive(Clone)]
pub struct StatusPoller {
    pub status: Arc<Mutex<Option<Status>>>,
}

impl StatusPoller {
    pub fn start(ctx: egui::Context, interval: Duration) -> StatusPoller {
        let status = Arc::new(Mutex::new(None));
        let s2 = status.clone();
        std::thread::spawn(move || loop {
            let st = ipc::status();
            *s2.lock() = st;
            ctx.request_repaint();
            std::thread::sleep(interval);
        });
        StatusPoller { status }
    }
    pub fn get(&self) -> Option<Status> {
        self.status.lock().clone()
    }
}

/// Fire-and-forget request (does not block the UI thread).
pub fn send_async(req: Request) {
    std::thread::spawn(move || {
        if let Err(e) = ipc::send(&req) {
            log::error!("{e:#}");
        }
    });
}

pub fn fmt_duration(secs: u64) -> String {
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
}

pub fn open_folder(path: &std::path::Path) {
    let _ = std::fs::create_dir_all(path);
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}
