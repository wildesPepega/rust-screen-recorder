//! Global hotkeys through KDE's KGlobalAccel D-Bus service. Works while games have focus and does
//! not need the GlobalShortcuts portal (no dialog). Each action can also be triggered with
//! `rust-screen-recorder ctl <action>`.

use crate::config::HotkeySettings;
use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use tokio::sync::mpsc::UnboundedSender;

pub const COMPONENT: &str = "rust-screen-recorder";
const COMPONENT_FRIENDLY: &str = "Rust Screen Recorder";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    ToggleOverlay,
    SaveReplay,
    ToggleReplay,
    ToggleRecording,
}

impl HotkeyAction {
    const ALL: [HotkeyAction; 4] = [Self::ToggleOverlay, Self::SaveReplay, Self::ToggleReplay, Self::ToggleRecording];
    fn id(self) -> &'static str {
        match self {
            Self::ToggleOverlay => "toggle-overlay",
            Self::SaveReplay => "save-replay",
            Self::ToggleReplay => "toggle-replay",
            Self::ToggleRecording => "toggle-recording",
        }
    }
    fn friendly(self) -> &'static str {
        match self {
            Self::ToggleOverlay => "Show/Hide Overlay",
            Self::SaveReplay => "Save Instant Replay",
            Self::ToggleReplay => "Start/Stop Instant Replay",
            Self::ToggleRecording => "Start/Stop Recording",
        }
    }
    fn key(self, h: &HotkeySettings) -> &str {
        match self {
            Self::ToggleOverlay => &h.toggle_overlay,
            Self::SaveReplay => &h.save_replay,
            Self::ToggleReplay => &h.toggle_replay,
            Self::ToggleRecording => &h.toggle_recording,
        }
    }
    fn from_id(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.id() == s)
    }
}

/// Convert "Ctrl+Alt+F10" into a Qt key code (Qt::Key | Qt::KeyboardModifier).
pub fn parse_key(s: &str) -> Result<i32> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(0);
    }
    let mut code: i32 = 0;
    let mut key: Option<i32> = None;
    for part in s.split('+').map(|p| p.trim()) {
        let up = part.to_uppercase();
        match up.as_str() {
            "SHIFT" => code |= 0x0200_0000,
            "CTRL" | "CONTROL" => code |= 0x0400_0000,
            "ALT" => code |= 0x0800_0000,
            "META" | "SUPER" | "WIN" => code |= 0x1000_0000,
            _ => {
                if key.is_some() {
                    return Err(anyhow!("more than one key in '{s}'"));
                }
                key = Some(qt_key(&up).ok_or_else(|| anyhow!("unknown key '{part}'"))?);
            }
        }
    }
    let key = key.ok_or_else(|| anyhow!("no key in '{s}'"))?;
    Ok(code | key)
}

fn qt_key(k: &str) -> Option<i32> {
    if let Some(n) = k.strip_prefix('F').and_then(|n| n.parse::<i32>().ok()) {
        if (1..=35).contains(&n) {
            return Some(0x0100_0030 + n - 1);
        }
    }
    if k.len() == 1 {
        let c = k.chars().next()?;
        if c.is_ascii_alphanumeric() {
            return Some(c as i32);
        }
    }
    Some(match k {
        "ESC" | "ESCAPE" => 0x0100_0000,
        "TAB" => 0x0100_0001,
        "BACKSPACE" => 0x0100_0003,
        "RETURN" | "ENTER" => 0x0100_0004,
        "INS" | "INSERT" => 0x0100_0006,
        "DEL" | "DELETE" => 0x0100_0007,
        "PAUSE" => 0x0100_0008,
        "PRINT" | "PRINTSCREEN" => 0x0100_0009,
        "HOME" => 0x0100_0010,
        "END" => 0x0100_0011,
        "LEFT" => 0x0100_0012,
        "UP" => 0x0100_0013,
        "RIGHT" => 0x0100_0014,
        "DOWN" => 0x0100_0015,
        "PGUP" | "PAGEUP" => 0x0100_0016,
        "PGDOWN" | "PAGEDOWN" => 0x0100_0017,
        "SCROLLLOCK" => 0x0100_0026,
        "SPACE" => 0x20,
        "`" | "GRAVE" => 0x60,
        "-" | "MINUS" => 0x2d,
        "=" | "EQUAL" => 0x3d,
        _ => return None,
    })
}

fn action_id(a: HotkeyAction) -> Vec<String> {
    vec![COMPONENT.into(), a.id().into(), COMPONENT_FRIENDLY.into(), a.friendly().into()]
}

/// Register / update all shortcuts.
pub async fn register(conn: &zbus::Connection, h: &HotkeySettings) -> Result<()> {
    for a in HotkeyAction::ALL {
        let id = action_id(a);
        conn.call_method(Some("org.kde.kglobalaccel"), "/kglobalaccel", Some("org.kde.KGlobalAccel"), "doRegister", &(id.clone(),))
            .await?;
        let key = match parse_key(a.key(h)) {
            Ok(k) => k,
            Err(e) => {
                log::warn!("hotkey for {}: {e}", a.id());
                0
            }
        };
        let keys: Vec<i32> = if key == 0 { vec![] } else { vec![key] };
        // flags: SetPresent (2) | NoAutoloading (4) → take exactly our configured key.
        let reply = conn
            .call_method(
                Some("org.kde.kglobalaccel"),
                "/kglobalaccel",
                Some("org.kde.KGlobalAccel"),
                "setShortcut",
                &(id, keys.clone(), 6u32),
            )
            .await?;
        let got: Vec<i32> = reply.body().deserialize().unwrap_or_default();
        if !keys.is_empty() && got != keys {
            log::warn!("hotkey '{}' for {} is already used by another application", a.key(h), a.id());
        }
    }
    Ok(())
}

/// Listen for presses and forward them.
pub async fn listen(conn: zbus::Connection, tx: UnboundedSender<HotkeyAction>) -> Result<()> {
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.kde.kglobalaccel.Component")?
        .member("globalShortcutPressed")?
        .build();
    let mut stream = zbus::MessageStream::for_match_rule(rule, &conn, Some(64)).await?;
    while let Some(Ok(msg)) = stream.next().await {
        if let Ok((component, action, _ts)) = msg.body().deserialize::<(String, String, i64)>() {
            if component == COMPONENT {
                if let Some(a) = HotkeyAction::from_id(&action) {
                    let _ = tx.send(a);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keys() {
        assert_eq!(parse_key("Alt+F10").unwrap(), 0x0800_0000 | 0x0100_0039);
        assert_eq!(parse_key("Alt+Z").unwrap(), 0x0800_0000 | 'Z' as i32);
        assert_eq!(parse_key("ctrl+shift+r").unwrap(), 0x0600_0000 | 'R' as i32);
        assert!(parse_key("Alt+Foo").is_err());
    }
}
