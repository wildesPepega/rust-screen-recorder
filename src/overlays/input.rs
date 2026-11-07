//! Keyboard & mouse input display.
//!
//! Wayland gives applications no global key events, so the evdev devices in /dev/input are read
//! directly. That needs membership in the `input` group (see [`has_access`]). Only the *current*
//! state is kept for drawing – nothing is logged or stored.

use super::{image_from_pixmap, Slot};
use crate::config::{InputOverlay, KeyLabels, KeyboardLayout};
use crate::toast::{draw_text, Fonts};
use anyhow::{bail, Result};
use evdev::{EventSummary, KeyCode, RelativeAxisCode};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};

const U: f32 = 40.0; // pixels per key unit
const PAD: f32 = 8.0;
const GAP: f32 = 3.0;
const SCROLL_FLASH: Duration = Duration::from_millis(220);

#[derive(Default, Clone)]
pub struct InputState {
    pub pressed: HashSet<u16>,
    pub wheel_up: Option<Instant>,
    pub wheel_down: Option<Instant>,
}

/// Can we read a keyboard or mouse in /dev/input? (requires the `input` group)
pub fn has_access() -> bool {
    evdev::enumerate().any(|(_, d)| {
        d.supported_keys().is_some_and(|k| (k.contains(KeyCode::KEY_A) && k.contains(KeyCode::KEY_SPACE)) || k.contains(KeyCode::BTN_LEFT))
    })
}

pub fn spawn(cfg: InputOverlay, slot: Arc<Slot>, stop: Arc<AtomicBool>) -> Result<()> {
    if !has_access() {
        bail!("no permission to read /dev/input – add your user to the 'input' group");
    }
    let state = Arc::new(Mutex::new(InputState::default()));
    let changed = Arc::new(AtomicBool::new(true));
    let mut n = 0;
    for (_path, dev) in evdev::enumerate() {
        let keys = dev.supported_keys();
        let is_kbd = keys.is_some_and(|k| k.contains(KeyCode::KEY_A) && k.contains(KeyCode::KEY_SPACE));
        let is_mouse = keys.is_some_and(|k| k.contains(KeyCode::BTN_LEFT));
        if !is_kbd && !is_mouse {
            continue;
        }
        n += 1;
        let (st, ch, sp) = (state.clone(), changed.clone(), stop.clone());
        std::thread::Builder::new().name("overlay-evdev".into()).spawn(move || read_device(dev, st, ch, sp))?;
    }
    if n == 0 {
        bail!("no keyboard or mouse found in /dev/input");
    }
    let labels = resolve_labels(cfg.labels);
    std::thread::Builder::new().name("overlay-input".into()).spawn(move || {
        let fonts = match Fonts::load() {
            Ok(f) => f,
            Err(e) => {
                log::error!("input overlay: {e:#}");
                return;
            }
        };
        while !stop.load(Ordering::SeqCst) {
            let s = state.lock().clone();
            let animating = [s.wheel_up, s.wheel_down].iter().flatten().any(|t| t.elapsed() < SCROLL_FLASH + Duration::from_millis(50));
            if changed.swap(false, Ordering::Relaxed) || animating {
                slot.put(image_from_pixmap(render(&cfg, &s, labels, &fonts)));
            }
            std::thread::sleep(Duration::from_millis(33));
        }
        slot.clear();
    })?;
    Ok(())
}

fn read_device(mut dev: evdev::Device, state: Arc<Mutex<InputState>>, changed: Arc<AtomicBool>, stop: Arc<AtomicBool>) {
    let fd = dev.as_raw_fd();
    while !stop.load(Ordering::SeqCst) {
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, 150) } <= 0 {
            continue;
        }
        let Ok(events) = dev.fetch_events() else { return };
        let mut s = state.lock();
        for ev in events {
            match ev.destructure() {
                EventSummary::Key(_, code, 1) => {
                    s.pressed.insert(code.code());
                    changed.store(true, Ordering::Relaxed);
                }
                EventSummary::Key(_, code, 0) => {
                    s.pressed.remove(&code.code());
                    changed.store(true, Ordering::Relaxed);
                }
                EventSummary::RelativeAxis(_, RelativeAxisCode::REL_WHEEL, v) if v != 0 => {
                    if v > 0 {
                        s.wheel_up = Some(Instant::now());
                    } else {
                        s.wheel_down = Some(Instant::now());
                    }
                    changed.store(true, Ordering::Relaxed);
                }
                _ => {}
            }
        }
    }
}

fn resolve_labels(l: KeyLabels) -> bool {
    match l {
        KeyLabels::De => true,
        KeyLabels::Us => false,
        KeyLabels::Auto => {
            // KDE keyboard layout, then the system keymap.
            let kde = dirs::config_dir()
                .and_then(|d| std::fs::read_to_string(d.join("kxkbrc")).ok())
                .and_then(|s| s.lines().find(|l| l.starts_with("LayoutList=")).map(|l| l[11..].to_string()));
            let sys = std::process::Command::new("localectl")
                .arg("status")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default();
            let first = kde.as_deref().map(|l| l.split(',').next().unwrap_or("").to_string());
            match first {
                Some(l) if !l.is_empty() => l.starts_with("de") || l.starts_with("at") || l.starts_with("ch"),
                _ => sys.contains("Keymap: de") || sys.contains("Layout: de"),
            }
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Layouts (key units)
// ------------------------------------------------------------------------------------------------

struct K {
    code: KeyCode,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

fn k(code: KeyCode, x: f32, y: f32) -> K {
    K { code, x, y, w: 1.0, h: 1.0 }
}
fn kw(code: KeyCode, x: f32, y: f32, w: f32) -> K {
    K { code, x, y, w, h: 1.0 }
}
fn kh(code: KeyCode, x: f32, y: f32, h: f32) -> K {
    K { code, x, y, w: 1.0, h }
}

fn row(codes: &[KeyCode], x0: f32, y: f32) -> Vec<K> {
    codes.iter().enumerate().map(|(i, c)| k(*c, x0 + i as f32, y)).collect()
}

fn gaming_layout() -> Vec<K> {
    use KeyCode as C;
    let mut v = vec![k(C::KEY_ESC, 0.0, 0.0)];
    v.extend(row(&[C::KEY_GRAVE, C::KEY_1, C::KEY_2, C::KEY_3, C::KEY_4, C::KEY_5], 0.0, 1.1));
    v.push(kw(C::KEY_TAB, 0.0, 2.1, 1.5));
    v.extend(row(&[C::KEY_Q, C::KEY_W, C::KEY_E, C::KEY_R, C::KEY_T], 1.5, 2.1));
    v.push(kw(C::KEY_CAPSLOCK, 0.0, 3.1, 1.75));
    v.extend(row(&[C::KEY_A, C::KEY_S, C::KEY_D, C::KEY_F, C::KEY_G], 1.75, 3.1));
    v.push(kw(C::KEY_LEFTSHIFT, 0.0, 4.1, 1.25));
    v.extend(row(&[C::KEY_102ND, C::KEY_Z, C::KEY_X, C::KEY_C, C::KEY_V, C::KEY_B], 1.25, 4.1));
    v.push(kw(C::KEY_LEFTCTRL, 0.0, 5.1, 1.25));
    v.push(kw(C::KEY_LEFTMETA, 1.25, 5.1, 1.25));
    v.push(kw(C::KEY_LEFTALT, 2.5, 5.1, 1.25));
    v.push(kw(C::KEY_SPACE, 3.75, 5.1, 3.5));
    v
}

fn full_layout() -> Vec<K> {
    use KeyCode as C;
    let mut v = vec![k(C::KEY_ESC, 0.0, 0.0)];
    v.extend(row(&[C::KEY_F1, C::KEY_F2, C::KEY_F3, C::KEY_F4], 2.0, 0.0));
    v.extend(row(&[C::KEY_F5, C::KEY_F6, C::KEY_F7, C::KEY_F8], 6.5, 0.0));
    v.extend(row(&[C::KEY_F9, C::KEY_F10, C::KEY_F11, C::KEY_F12], 11.0, 0.0));
    v.extend(row(&[C::KEY_SYSRQ, C::KEY_SCROLLLOCK, C::KEY_PAUSE], 15.25, 0.0));
    let y1 = 1.25;
    v.extend(row(
        &[C::KEY_GRAVE, C::KEY_1, C::KEY_2, C::KEY_3, C::KEY_4, C::KEY_5, C::KEY_6, C::KEY_7, C::KEY_8, C::KEY_9, C::KEY_0, C::KEY_MINUS, C::KEY_EQUAL],
        0.0,
        y1,
    ));
    v.push(kw(C::KEY_BACKSPACE, 13.0, y1, 2.0));
    v.extend(row(&[C::KEY_INSERT, C::KEY_HOME, C::KEY_PAGEUP], 15.25, y1));
    v.extend(row(&[C::KEY_NUMLOCK, C::KEY_KPSLASH, C::KEY_KPASTERISK, C::KEY_KPMINUS], 18.5, y1));
    let y2 = 2.25;
    v.push(kw(C::KEY_TAB, 0.0, y2, 1.5));
    v.extend(row(
        &[C::KEY_Q, C::KEY_W, C::KEY_E, C::KEY_R, C::KEY_T, C::KEY_Y, C::KEY_U, C::KEY_I, C::KEY_O, C::KEY_P, C::KEY_LEFTBRACE, C::KEY_RIGHTBRACE],
        1.5,
        y2,
    ));
    v.push(kw(C::KEY_ENTER, 13.5, y2, 1.5));
    v.extend(row(&[C::KEY_DELETE, C::KEY_END, C::KEY_PAGEDOWN], 15.25, y2));
    v.extend(row(&[C::KEY_KP7, C::KEY_KP8, C::KEY_KP9], 18.5, y2));
    v.push(kh(C::KEY_KPPLUS, 21.5, y2, 2.0));
    let y3 = 3.25;
    v.push(kw(C::KEY_CAPSLOCK, 0.0, y3, 1.75));
    v.extend(row(
        &[C::KEY_A, C::KEY_S, C::KEY_D, C::KEY_F, C::KEY_G, C::KEY_H, C::KEY_J, C::KEY_K, C::KEY_L, C::KEY_SEMICOLON, C::KEY_APOSTROPHE, C::KEY_BACKSLASH],
        1.75,
        y3,
    ));
    v.push(kw(C::KEY_ENTER, 13.75, y3, 1.25));
    v.extend(row(&[C::KEY_KP4, C::KEY_KP5, C::KEY_KP6], 18.5, y3));
    let y4 = 4.25;
    v.push(kw(C::KEY_LEFTSHIFT, 0.0, y4, 1.25));
    v.extend(row(
        &[C::KEY_102ND, C::KEY_Z, C::KEY_X, C::KEY_C, C::KEY_V, C::KEY_B, C::KEY_N, C::KEY_M, C::KEY_COMMA, C::KEY_DOT, C::KEY_SLASH],
        1.25,
        y4,
    ));
    v.push(kw(C::KEY_RIGHTSHIFT, 12.25, y4, 2.75));
    v.push(k(C::KEY_UP, 16.25, y4));
    v.extend(row(&[C::KEY_KP1, C::KEY_KP2, C::KEY_KP3], 18.5, y4));
    v.push(kh(C::KEY_KPENTER, 21.5, y4, 2.0));
    let y5 = 5.25;
    v.push(kw(C::KEY_LEFTCTRL, 0.0, y5, 1.25));
    v.push(kw(C::KEY_LEFTMETA, 1.25, y5, 1.25));
    v.push(kw(C::KEY_LEFTALT, 2.5, y5, 1.25));
    v.push(kw(C::KEY_SPACE, 3.75, y5, 6.25));
    v.push(kw(C::KEY_RIGHTALT, 10.0, y5, 1.25));
    v.push(kw(C::KEY_RIGHTMETA, 11.25, y5, 1.25));
    v.push(kw(C::KEY_COMPOSE, 12.5, y5, 1.25));
    v.push(kw(C::KEY_RIGHTCTRL, 13.75, y5, 1.25));
    v.extend(row(&[C::KEY_LEFT, C::KEY_DOWN, C::KEY_RIGHT], 15.25, y5));
    v.push(kw(C::KEY_KP0, 18.5, y5, 2.0));
    v.push(k(C::KEY_KPDOT, 20.5, y5));
    v
}

fn label(c: KeyCode, de: bool) -> String {
    use KeyCode as C;
    let s = match c {
        C::KEY_ESC => "Esc",
        C::KEY_TAB => "Tab",
        C::KEY_CAPSLOCK => "Caps",
        C::KEY_LEFTSHIFT | C::KEY_RIGHTSHIFT => "Shift",
        C::KEY_LEFTCTRL | C::KEY_RIGHTCTRL => {
            if de {
                "Strg"
            } else {
                "Ctrl"
            }
        }
        C::KEY_LEFTALT => "Alt",
        C::KEY_RIGHTALT => {
            if de {
                "AltGr"
            } else {
                "Alt"
            }
        }
        C::KEY_LEFTMETA | C::KEY_RIGHTMETA => "Win",
        C::KEY_COMPOSE => "Menu",
        C::KEY_SPACE => "",
        C::KEY_ENTER | C::KEY_KPENTER => "Enter",
        C::KEY_BACKSPACE => "Back",
        C::KEY_SYSRQ => "Prt",
        C::KEY_SCROLLLOCK => "Scr",
        C::KEY_PAUSE => "Pse",
        C::KEY_INSERT => "Ins",
        C::KEY_DELETE => "Del",
        C::KEY_HOME => "Home",
        C::KEY_END => "End",
        C::KEY_PAGEUP => "PgUp",
        C::KEY_PAGEDOWN => "PgDn",
        C::KEY_UP => "↑",
        C::KEY_DOWN => "↓",
        C::KEY_LEFT => "←",
        C::KEY_RIGHT => "→",
        C::KEY_NUMLOCK => "Num",
        C::KEY_KPSLASH => "/",
        C::KEY_KPASTERISK => "*",
        C::KEY_KPMINUS => "-",
        C::KEY_KPPLUS => "+",
        C::KEY_KPDOT => {
            if de {
                ","
            } else {
                "."
            }
        }
        C::KEY_GRAVE => {
            if de {
                "^"
            } else {
                "`"
            }
        }
        C::KEY_MINUS => {
            if de {
                "ß"
            } else {
                "-"
            }
        }
        C::KEY_EQUAL => {
            if de {
                "´"
            } else {
                "="
            }
        }
        C::KEY_LEFTBRACE => {
            if de {
                "Ü"
            } else {
                "["
            }
        }
        C::KEY_RIGHTBRACE => {
            if de {
                "+"
            } else {
                "]"
            }
        }
        C::KEY_SEMICOLON => {
            if de {
                "Ö"
            } else {
                ";"
            }
        }
        C::KEY_APOSTROPHE => {
            if de {
                "Ä"
            } else {
                "'"
            }
        }
        C::KEY_BACKSLASH => {
            if de {
                "#"
            } else {
                "\\"
            }
        }
        C::KEY_102ND => {
            if de {
                "<"
            } else {
                "\\"
            }
        }
        C::KEY_Y => {
            if de {
                "Z"
            } else {
                "Y"
            }
        }
        C::KEY_Z => {
            if de {
                "Y"
            } else {
                "Z"
            }
        }
        C::KEY_COMMA => ",",
        C::KEY_DOT => ".",
        C::KEY_SLASH => {
            if de {
                "-"
            } else {
                "/"
            }
        }
        _ => {
            // KEY_A → "A", KEY_F5 → "F5", KEY_KP7 → "7"
            let name = format!("{c:?}");
            let n = name.trim_start_matches("KEY_").trim_start_matches("KP").to_string();
            return n;
        }
    };
    s.to_string()
}

// ------------------------------------------------------------------------------------------------
// Rendering
// ------------------------------------------------------------------------------------------------

fn rounded(x: f32, y: f32, w: f32, h: f32, r: f32) -> tiny_skia::Path {
    let r = r.min(w / 2.0).min(h / 2.0);
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish().unwrap()
}

struct Style {
    idle: Paint<'static>,
    hot: Paint<'static>,
    border: Paint<'static>,
}

fn paint(r: u8, g: u8, b: u8, a: u8) -> Paint<'static> {
    let mut p = Paint::default();
    p.anti_alias = true;
    p.set_color(Color::from_rgba8(r, g, b, a));
    p
}

fn draw_key(pm: &mut Pixmap, st: &Style, x: f32, y: f32, w: f32, h: f32, down: bool) {
    let path = rounded(x, y, w, h, 6.0);
    pm.fill_path(&path, if down { &st.hot } else { &st.idle }, FillRule::Winding, Transform::identity(), None);
    pm.stroke_path(&path, &st.border, &Stroke { width: 1.2, ..Default::default() }, Transform::identity(), None);
}

/// Mouse is 2.6 × 4.2 units, drawn at (ox, oy) in pixels.
fn draw_mouse(pm: &mut Pixmap, st: &Style, ox: f32, oy: f32, s: &InputState) {
    let (w, h) = (2.6 * U, 4.2 * U);
    let down = |c: KeyCode| s.pressed.contains(&c.code());
    // body
    let dark = paint(40, 44, 52, 240);
    let body = rounded(ox, oy, w, h, w * 0.45);
    pm.fill_path(&body, &st.idle, FillRule::Winding, Transform::identity(), None);
    pm.stroke_path(&body, &st.border, &Stroke { width: 1.2, ..Default::default() }, Transform::identity(), None);
    // buttons (top 40 %)
    let bh = h * 0.4;
    let half = w / 2.0;
    let lb = rounded(ox + 3.0, oy + 3.0, half - 4.5, bh, 14.0);
    let rb = rounded(ox + half + 1.5, oy + 3.0, half - 4.5, bh, 14.0);
    pm.fill_path(&lb, if down(KeyCode::BTN_LEFT) { &st.hot } else { &st.idle }, FillRule::Winding, Transform::identity(), None);
    pm.fill_path(&rb, if down(KeyCode::BTN_RIGHT) { &st.hot } else { &st.idle }, FillRule::Winding, Transform::identity(), None);
    pm.stroke_path(&lb, &st.border, &Stroke { width: 1.0, ..Default::default() }, Transform::identity(), None);
    pm.stroke_path(&rb, &st.border, &Stroke { width: 1.0, ..Default::default() }, Transform::identity(), None);
    // wheel
    let (ww, wh) = (w * 0.16, bh * 0.55);
    let (wx, wy) = (ox + half - ww / 2.0, oy + bh * 0.28);
    let wheel = rounded(wx, wy, ww, wh, ww / 2.0);
    pm.fill_path(&wheel, if down(KeyCode::BTN_MIDDLE) { &st.hot } else { &dark }, FillRule::Winding, Transform::identity(), None);
    pm.stroke_path(&wheel, &st.border, &Stroke { width: 1.0, ..Default::default() }, Transform::identity(), None);
    // scroll arrows
    let flash = |t: Option<Instant>| t.is_some_and(|t| t.elapsed() < SCROLL_FLASH);
    let tri = |pm: &mut Pixmap, up: bool, on: bool| {
        let cx = wx + ww / 2.0;
        let (ty, dir) = if up { (wy - 5.0, -1.0) } else { (wy + wh + 5.0, 1.0) };
        let mut pb = PathBuilder::new();
        pb.move_to(cx - 7.0, ty);
        pb.line_to(cx + 7.0, ty);
        pb.line_to(cx, ty + dir * 8.0);
        pb.close();
        let p = if on { st.hot.clone() } else { paint(255, 255, 255, 60) };
        pm.fill_path(&pb.finish().unwrap(), &p, FillRule::Winding, Transform::identity(), None);
    };
    tri(pm, true, flash(s.wheel_up));
    tri(pm, false, flash(s.wheel_down));
    // side buttons (left edge): forward (extra) above back (side)
    for (code, y) in [(KeyCode::BTN_EXTRA, 0.47), (KeyCode::BTN_SIDE, 0.62)] {
        let sb = rounded(ox - 7.0, oy + h * y, 12.0, h * 0.12, 4.0);
        pm.fill_path(&sb, if down(code) { &st.hot } else { &dark }, FillRule::Winding, Transform::identity(), None);
        pm.stroke_path(&sb, &st.border, &Stroke { width: 1.0, ..Default::default() }, Transform::identity(), None);
    }
}

/// Draw the overlay for `s` (also used for the preview in the settings).
pub fn render(cfg: &InputOverlay, s: &InputState, de: bool, fonts: &Fonts) -> Pixmap {
    let keys = match cfg.layout {
        KeyboardLayout::Full => full_layout(),
        KeyboardLayout::Gaming => gaming_layout(),
    };
    let kb_w = keys.iter().map(|k| k.x + k.w).fold(0.0, f32::max) * U;
    let kb_h = keys.iter().map(|k| k.y + k.h).fold(0.0, f32::max) * U;
    let mouse_w = if cfg.show_mouse { 2.6 * U + 0.6 * U } else { 0.0 };
    let width = (PAD * 2.0 + mouse_w + kb_w).ceil() as u32;
    let height = (PAD * 2.0 + kb_h.max(if cfg.show_mouse { 4.2 * U } else { 0.0 })).ceil() as u32;
    let mut pm = Pixmap::new(width, height).unwrap();
    let st = Style { idle: paint(18, 20, 26, 190), hot: paint(0x3d, 0xdc, 0x97, 245), border: paint(255, 255, 255, 46) };
    if cfg.show_mouse {
        let my = PAD + (kb_h - 4.2 * U).max(0.0) / 2.0;
        draw_mouse(&mut pm, &st, PAD + 4.0, my, s);
    }
    let x0 = PAD + mouse_w;
    for key in &keys {
        let (x, y) = (x0 + key.x * U + GAP / 2.0, PAD + key.y * U + GAP / 2.0);
        let (w, h) = (key.w * U - GAP, key.h * U - GAP);
        let down = s.pressed.contains(&key.code.code());
        draw_key(&mut pm, &st, x, y, w, h, down);
        let text = label(key.code, de);
        let arrow = match key.code {
            KeyCode::KEY_UP => Some((0.0, -1.0)),
            KeyCode::KEY_DOWN => Some((0.0, 1.0)),
            KeyCode::KEY_LEFT => Some((-1.0, 0.0)),
            KeyCode::KEY_RIGHT => Some((1.0, 0.0)),
            _ => None,
        };
        if let Some((dx, dy)) = arrow {
            // The UI font has no arrow glyphs: draw a triangle.
            let (cx, cy, r) = (x + w / 2.0, y + h / 2.0, w.min(h) * 0.22);
            let mut pb = PathBuilder::new();
            pb.move_to(cx + dx * r, cy + dy * r);
            pb.line_to(cx - dx * r * 0.6 + dy * r * 0.9, cy - dy * r * 0.6 + dx * r * 0.9);
            pb.line_to(cx - dx * r * 0.6 - dy * r * 0.9, cy - dy * r * 0.6 - dx * r * 0.9);
            pb.close();
            let col = if down { paint(0x10, 0x12, 0x16, 255) } else { paint(0xf0, 0xf2, 0xf5, 235) };
            pm.fill_path(&pb.finish().unwrap(), &col, FillRule::Winding, Transform::identity(), None);
        } else if !text.is_empty() {
            let size = if text.chars().count() > 2 { 12.0 } else { 17.0 };
            let col = if down { (0x10, 0x12, 0x16) } else { (0xf0, 0xf2, 0xf5) };
            // approximate centring
            let tw = text.chars().count() as f32 * size * 0.55;
            draw_text(&mut pm, &fonts.bold, size, x + (w - tw).max(4.0) / 2.0, y + h / 2.0 + size * 0.36, w - 4.0, &text, col, 1.0);
        }
    }
    pm
}

/// Static preview (nothing pressed) for the settings.
pub fn preview(cfg: &InputOverlay) -> Option<super::Image> {
    let fonts = Fonts::load().ok()?;
    let mut s = InputState::default();
    // Show a few keys "pressed" so the look is clear.
    for c in [KeyCode::KEY_W, KeyCode::KEY_LEFTSHIFT, KeyCode::BTN_LEFT] {
        s.pressed.insert(c.code());
    }
    Some(image_from_pixmap(render(cfg, &s, resolve_labels(cfg.labels), &fonts)))
}

#[cfg(test)]
mod tests {
    use super::*;
    /// RSR_INPUT_PREVIEW=/dir cargo test input_preview – renders both layouts as PNG.
    #[test]
    fn input_preview() {
        let Some(dir) = std::env::var_os("RSR_INPUT_PREVIEW") else { return };
        let fonts = Fonts::load().unwrap();
        let mut s = InputState::default();
        for c in [KeyCode::KEY_W, KeyCode::KEY_A, KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_SPACE, KeyCode::BTN_LEFT, KeyCode::BTN_SIDE] {
            s.pressed.insert(c.code());
        }
        s.wheel_up = Some(Instant::now());
        for (name, layout) in [("gaming", KeyboardLayout::Gaming), ("full", KeyboardLayout::Full)] {
            let cfg = InputOverlay { layout, show_mouse: true, labels: KeyLabels::De };
            render(&cfg, &s, true, &fonts).save_png(std::path::Path::new(&dir).join(format!("input_{name}.png"))).unwrap();
        }
    }
}
