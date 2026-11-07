//! System tray icon (StatusNotifierItem) for Plasma.

use crate::daemon::Event;
use crate::ipc::Request;
use tokio::sync::mpsc::UnboundedSender;

pub struct Tray {
    pub tx: UnboundedSender<Event>,
    pub replay: bool,
    pub recording: bool,
    pub target: String,
}

impl Tray {
    fn send(&self, r: Request) {
        let _ = self.tx.send(Event::Req(r, None));
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "rust-screen-recorder".into()
    }
    fn title(&self) -> String {
        "Rust Screen Recorder".into()
    }
    fn icon_name(&self) -> String {
        if self.recording {
            "media-record".into()
        } else if self.replay {
            "media-playlist-repeat".into()
        } else {
            "camera-video".into()
        }
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        let mut s = vec![];
        if self.recording {
            s.push("● Recording");
        }
        if self.replay {
            s.push("Instant Replay on");
        }
        if s.is_empty() {
            s.push("Idle");
        }
        ksni::ToolTip {
            title: "Rust Screen Recorder".into(),
            description: format!("{}\n{}", s.join(" · "), self.target),
            ..Default::default()
        }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Request::ToggleOverlay);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        vec![
            StandardItem {
                label: self.target.clone(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Show overlay".into(),
                icon_name: "view-fullscreen".into(),
                activate: Box::new(|t: &mut Self| t.send(Request::ToggleOverlay)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Save instant replay".into(),
                icon_name: "document-save".into(),
                enabled: self.replay,
                activate: Box::new(|t: &mut Self| t.send(Request::SaveReplay)),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Instant replay".into(),
                checked: self.replay,
                activate: Box::new(|t: &mut Self| t.send(Request::ToggleReplay)),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Recording".into(),
                checked: self.recording,
                activate: Box::new(|t: &mut Self| t.send(Request::ToggleRecording)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Global settings…".into(),
                icon_name: "configure".into(),
                activate: Box::new(|t: &mut Self| t.send(Request::OpenSettings(Some("general".into())))),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| t.send(Request::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

pub async fn spawn(tx: UnboundedSender<Event>) -> Option<ksni::Handle<Tray>> {
    use ksni::TrayMethods;
    match (Tray { tx, replay: false, recording: false, target: String::new() }).spawn().await {
        Ok(h) => Some(h),
        Err(e) => {
            log::warn!("tray icon unavailable: {e}");
            None
        }
    }
}
