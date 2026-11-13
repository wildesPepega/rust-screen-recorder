//! Controller input display (Xbox or PlayStation look).
//!
//! Gamepads are read through evdev like the keyboard, but their device nodes are normally
//! readable by the logged-in user (udev `uaccess`), so no `input` group is needed. Controllers are
//! often switched on after the recorder started, so /dev/input is rescanned every few seconds.
//! Only the *current* state is kept for drawing.

use super::{image_from_pixmap, Slot};
use crate::config::{ControllerModel, ControllerOverlay};
use crate::toast::Fonts;
use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use anyhow::Result;
use evdev::{AbsoluteAxisCode as Abs, EventSummary, KeyCode};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tiny_skia::{Color, FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap, Stroke, Transform};

const W: f32 = 560.0;
const H: f32 = 384.0;
const RESCAN: Duration = Duration::from_secs(3);
/// Stick movement below this is noise (drift) and not worth a redraw.
const DEADZONE: f32 = 0.06;

/// Buttons by their *position* on the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Btn {
    South,
    East,
    West,
    North,
    L1,
    R1,
    Select,
    Start,
    Guide,
    L3,
    R3,
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct PadState {
    pub pressed: HashSet<Btn>,
    /// Sticks, -1 … 1 (x right, y down).
    pub lx: f32,
    pub ly: f32,
    pub rx: f32,
    pub ry: f32,
    /// Triggers, 0 … 1.
    pub lt: f32,
    pub rt: f32,
}

/// How one device reports its buttons and axes.
#[derive(Debug, Default, Clone)]
pub struct PadMap {
    /// xpad-style drivers report the left face button (X) as BTN_NORTH and the top one (Y) as
    /// BTN_WEST. Sony and Nintendo drivers use the codes by position.
    pub swap_north_west: bool,
    /// Simple HID pads without ABS_RX/RY use ABS_Z/RZ for the right stick instead of the triggers.
    pub z_is_right_stick: bool,
    /// (minimum, maximum) per absolute axis code.
    pub ranges: HashMap<u16, (i32, i32)>,
}

impl PadMap {
    fn from_device(dev: &evdev::Device) -> PadMap {
        let vendor = dev.input_id().vendor();
        let axes = dev.supported_absolute_axes();
        let mut ranges = HashMap::new();
        if let Ok(infos) = dev.get_absinfo() {
            for (axis, info) in infos {
                ranges.insert(axis.0, (info.minimum(), info.maximum()));
            }
        }
        PadMap {
            swap_north_west: !matches!(vendor, 0x054c | 0x057e),
            z_is_right_stick: axes.is_some_and(|a| a.contains(Abs::ABS_Z) && !a.contains(Abs::ABS_RX)),
            ranges,
        }
    }

    /// -1 … 1 for sticks.
    fn centered(&self, axis: Abs, v: i32) -> f32 {
        let (min, max) = self.ranges.get(&axis.0).copied().unwrap_or((-32768, 32767));
        if max <= min {
            return 0.0;
        }
        (((v - min) as f32 / (max - min) as f32) * 2.0 - 1.0).clamp(-1.0, 1.0)
    }

    /// 0 … 1 for triggers.
    fn unit(&self, axis: Abs, v: i32) -> f32 {
        let (min, max) = self.ranges.get(&axis.0).copied().unwrap_or((0, 255));
        if max <= min {
            return 0.0;
        }
        ((v - min) as f32 / (max - min) as f32).clamp(0.0, 1.0)
    }
}

impl PadState {
    pub fn key(&mut self, map: &PadMap, code: KeyCode, down: bool) {
        let btn = match code {
            KeyCode::BTN_SOUTH => Btn::South,
            KeyCode::BTN_EAST => Btn::East,
            KeyCode::BTN_NORTH if map.swap_north_west => Btn::West,
            KeyCode::BTN_NORTH => Btn::North,
            KeyCode::BTN_WEST if map.swap_north_west => Btn::North,
            KeyCode::BTN_WEST => Btn::West,
            KeyCode::BTN_TL => Btn::L1,
            KeyCode::BTN_TR => Btn::R1,
            KeyCode::BTN_SELECT => Btn::Select,
            KeyCode::BTN_START => Btn::Start,
            KeyCode::BTN_MODE => Btn::Guide,
            KeyCode::BTN_THUMBL => Btn::L3,
            KeyCode::BTN_THUMBR => Btn::R3,
            KeyCode::BTN_DPAD_UP => Btn::Up,
            KeyCode::BTN_DPAD_DOWN => Btn::Down,
            KeyCode::BTN_DPAD_LEFT => Btn::Left,
            KeyCode::BTN_DPAD_RIGHT => Btn::Right,
            // Digital triggers (pads without analog ones; analog pads send the axis as well).
            KeyCode::BTN_TL2 => {
                if !map.ranges.contains_key(&Abs::ABS_Z.0) || map.z_is_right_stick {
                    self.lt = if down { 1.0 } else { 0.0 };
                }
                return;
            }
            KeyCode::BTN_TR2 => {
                if !map.ranges.contains_key(&Abs::ABS_RZ.0) || map.z_is_right_stick {
                    self.rt = if down { 1.0 } else { 0.0 };
                }
                return;
            }
            _ => return,
        };
        if down {
            self.pressed.insert(btn);
        } else {
            self.pressed.remove(&btn);
        }
    }

    pub fn axis(&mut self, map: &PadMap, axis: Abs, v: i32) {
        let mut hat = |neg: Btn, pos: Btn| {
            self.pressed.remove(&neg);
            self.pressed.remove(&pos);
            if v < 0 {
                self.pressed.insert(neg);
            } else if v > 0 {
                self.pressed.insert(pos);
            }
        };
        match axis {
            Abs::ABS_HAT0X => hat(Btn::Left, Btn::Right),
            Abs::ABS_HAT0Y => hat(Btn::Up, Btn::Down),
            Abs::ABS_X => self.lx = map.centered(axis, v),
            Abs::ABS_Y => self.ly = map.centered(axis, v),
            Abs::ABS_RX => self.rx = map.centered(axis, v),
            Abs::ABS_RY => self.ry = map.centered(axis, v),
            Abs::ABS_Z if map.z_is_right_stick => self.rx = map.centered(axis, v),
            Abs::ABS_RZ if map.z_is_right_stick => self.ry = map.centered(axis, v),
            Abs::ABS_Z => self.lt = map.unit(axis, v),
            Abs::ABS_RZ => self.rt = map.unit(axis, v),
            _ => {}
        }
    }

    /// Is the difference to `other` visible in the drawing?
    fn differs(&self, other: &PadState) -> bool {
        let far = |a: f32, b: f32| (a - b).abs() > 0.02;
        self.pressed != other.pressed
            || far(self.lx, other.lx)
            || far(self.ly, other.ly)
            || far(self.rx, other.rx)
            || far(self.ry, other.ry)
            || far(self.lt, other.lt)
            || far(self.rt, other.rt)
    }
}

fn is_gamepad(dev: &evdev::Device) -> bool {
    dev.supported_keys().is_some_and(|k| k.contains(KeyCode::BTN_SOUTH)) && dev.supported_absolute_axes().is_some_and(|a| a.contains(Abs::ABS_X))
}

pub fn spawn(cfg: ControllerOverlay, slot: Arc<Slot>, stop: Arc<AtomicBool>) -> Result<()> {
    let fonts = Fonts::load()?;
    let state = Arc::new(Mutex::new(PadState::default()));
    {
        let (state, stop) = (state.clone(), stop.clone());
        std::thread::Builder::new().name("overlay-pad-scan".into()).spawn(move || scan_loop(state, stop))?;
    }
    std::thread::Builder::new().name("overlay-pad".into()).spawn(move || {
        let mut drawn: Option<PadState> = None;
        while !stop.load(Ordering::SeqCst) {
            let s = state.lock().clone();
            if drawn.as_ref().is_none_or(|d| d.differs(&s)) {
                slot.put(image_from_pixmap(render(&cfg, &s, &fonts)));
                drawn = Some(s);
            }
            std::thread::sleep(Duration::from_millis(16));
        }
        slot.clear();
    })?;
    Ok(())
}

/// Open every gamepad in /dev/input, also the ones that are connected later.
fn scan_loop(state: Arc<Mutex<PadState>>, stop: Arc<AtomicBool>) {
    // Device nodes that were already looked at (gamepad or not).
    let mut known: HashSet<PathBuf> = HashSet::new();
    let mut next = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        if Instant::now() < next {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        next = Instant::now() + RESCAN;
        let nodes: HashSet<PathBuf> = std::fs::read_dir("/dev/input")
            .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("event"))).collect())
            .unwrap_or_default();
        // An unplugged controller's node disappears; the same name may come back later.
        known.retain(|p| nodes.contains(p));
        for path in nodes {
            if !known.insert(path.clone()) {
                continue;
            }
            let Ok(dev) = evdev::Device::open(&path) else { continue };
            if !is_gamepad(&dev) {
                continue;
            }
            log::info!("controller overlay: reading {} ({})", dev.name().unwrap_or("gamepad"), path.display());
            let (state, stop) = (state.clone(), stop.clone());
            let _ = std::thread::Builder::new().name("overlay-pad-dev".into()).spawn(move || read_device(dev, state, stop));
        }
    }
}

fn read_device(mut dev: evdev::Device, state: Arc<Mutex<PadState>>, stop: Arc<AtomicBool>) {
    let map = PadMap::from_device(&dev);
    let fd = dev.as_raw_fd();
    while !stop.load(Ordering::SeqCst) {
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, 150) } <= 0 {
            continue;
        }
        let Ok(events) = dev.fetch_events() else { break };
        let mut s = state.lock();
        for ev in events {
            match ev.destructure() {
                EventSummary::Key(_, code, v) if v == 0 || v == 1 => s.key(&map, code, v == 1),
                EventSummary::AbsoluteAxis(_, axis, v) => s.axis(&map, axis, v),
                _ => {}
            }
        }
    }
    // Unplugged: nothing stays "pressed".
    *state.lock() = PadState::default();
}

// ------------------------------------------------------------------------------------------------
// Rendering
// ------------------------------------------------------------------------------------------------

const ID: Transform = Transform { sx: 1.0, kx: 0.0, ky: 0.0, sy: 1.0, tx: 0.0, ty: 0.0 };
const INK: (u8, u8, u8) = (0x10, 0x12, 0x16);
const TEXT: (u8, u8, u8) = (0xf0, 0xf2, 0xf5);
const ACCENT: (u8, u8, u8) = (0x3d, 0xdc, 0x97);

fn paint((r, g, b): (u8, u8, u8), a: u8) -> Paint<'static> {
    let mut p = Paint::default();
    p.anti_alias = true;
    p.set_color(Color::from_rgba8(r, g, b, a));
    p
}

fn line(width: f32) -> Stroke {
    Stroke { width, line_cap: LineCap::Round, line_join: LineJoin::Round, ..Default::default() }
}

fn circle(cx: f32, cy: f32, r: f32) -> Path {
    PathBuilder::from_circle(cx, cy, r).unwrap()
}

fn rounded(x: f32, y: f32, w: f32, h: f32, r: f32) -> Path {
    let r = r.min(w / 2.0).min(h / 2.0);
    let k = r * 0.4477; // distance of the control points from the corner for a circular arc
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - k, y, x + w, y + k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - k, x + w - k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + k, y + h, x, y + h - k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + k, x + k, y, x + r, y);
    pb.close();
    pb.finish().unwrap()
}

/// Controller outline; the right half is the mirrored left half.
fn body(model: ControllerModel) -> Path {
    // (control 1, control 2, end) of each curve on the right half, from the top centre clockwise
    // to the bottom centre.
    let right: &[[(f32, f32); 3]] = match model {
        ControllerModel::Xbox => &[
            [(340.0, 66.0), (384.0, 66.0), (414.0, 76.0)],
            [(466.0, 86.0), (496.0, 112.0), (508.0, 172.0)],
            [(526.0, 256.0), (540.0, 336.0), (502.0, 358.0)],
            [(468.0, 376.0), (440.0, 338.0), (410.0, 294.0)],
            [(394.0, 270.0), (378.0, 262.0), (346.0, 262.0)],
            [(320.0, 262.0), (300.0, 262.0), (280.0, 262.0)],
        ],
        ControllerModel::PlayStation => &[
            [(330.0, 78.0), (380.0, 78.0), (408.0, 72.0)],
            [(452.0, 64.0), (486.0, 80.0), (500.0, 134.0)],
            [(520.0, 214.0), (546.0, 330.0), (512.0, 364.0)],
            [(482.0, 392.0), (452.0, 350.0), (432.0, 300.0)],
            [(420.0, 272.0), (404.0, 266.0), (372.0, 266.0)],
            [(340.0, 266.0), (310.0, 266.0), (280.0, 266.0)],
        ],
    };
    let top = match model {
        ControllerModel::Xbox => 66.0,
        ControllerModel::PlayStation => 78.0,
    };
    let mut pb = PathBuilder::new();
    pb.move_to(280.0, top);
    for [a, b, c] in right {
        pb.cubic_to(a.0, a.1, b.0, b.1, c.0, c.1);
    }
    // Back up the left side: the same curves mirrored and reversed.
    let m = |p: (f32, f32)| (W - p.0, p.1);
    for i in (0..right.len()).rev() {
        let [a, b, _] = right[i];
        let end = if i == 0 { (280.0, top) } else { right[i - 1][2] };
        let (a, b, end) = (m(a), m(b), m(end));
        pb.cubic_to(b.0, b.1, a.0, a.1, end.0, end.1);
    }
    pb.close();
    pb.finish().unwrap()
}

fn text_width(font: &FontVec, size: f32, text: &str) -> f32 {
    let f = font.as_scaled(PxScale::from(size));
    text.chars().map(|c| f.h_advance(f.glyph_id(c))).sum()
}

fn centered_text(pm: &mut Pixmap, font: &FontVec, size: f32, cx: f32, cy: f32, text: &str, rgb: (u8, u8, u8)) {
    let w = text_width(font, size, text);
    crate::toast::draw_text(pm, font, size, cx - w / 2.0, cy + size * 0.36, w + 8.0, text, rgb, 1.0);
}

struct Look {
    idle: Paint<'static>,
    hot: Paint<'static>,
    border: Paint<'static>,
    well: Paint<'static>,
}

/// One face button: its colour, and the symbol drawn on it.
struct Face {
    btn: Btn,
    dx: f32,
    dy: f32,
    rgb: (u8, u8, u8),
    glyph: Glyph,
}

enum Glyph {
    Letter(&'static str),
    Cross,
    Circle,
    Square,
    Triangle,
}

fn faces(model: ControllerModel) -> [Face; 4] {
    match model {
        ControllerModel::Xbox => [
            Face { btn: Btn::South, dx: 0.0, dy: 1.0, rgb: (0x7b, 0xd3, 0x4f), glyph: Glyph::Letter("A") },
            Face { btn: Btn::East, dx: 1.0, dy: 0.0, rgb: (0xf0, 0x5a, 0x4f), glyph: Glyph::Letter("B") },
            Face { btn: Btn::West, dx: -1.0, dy: 0.0, rgb: (0x4f, 0x9d, 0xf5), glyph: Glyph::Letter("X") },
            Face { btn: Btn::North, dx: 0.0, dy: -1.0, rgb: (0xf5, 0xc9, 0x3a), glyph: Glyph::Letter("Y") },
        ],
        ControllerModel::PlayStation => [
            Face { btn: Btn::South, dx: 0.0, dy: 1.0, rgb: (0x7c, 0xa8, 0xff), glyph: Glyph::Cross },
            Face { btn: Btn::East, dx: 1.0, dy: 0.0, rgb: (0xf2, 0x6d, 0x6d), glyph: Glyph::Circle },
            Face { btn: Btn::West, dx: -1.0, dy: 0.0, rgb: (0xe9, 0x8f, 0xd0), glyph: Glyph::Square },
            Face { btn: Btn::North, dx: 0.0, dy: -1.0, rgb: (0x4f, 0xd8, 0xb0), glyph: Glyph::Triangle },
        ],
    }
}

fn draw_glyph(pm: &mut Pixmap, fonts: &Fonts, g: &Glyph, cx: f32, cy: f32, rgb: (u8, u8, u8)) {
    let p = paint(rgb, 255);
    let r = 7.0;
    let mut pb = PathBuilder::new();
    match g {
        Glyph::Letter(l) => {
            centered_text(pm, &fonts.bold, 19.0, cx, cy, l, rgb);
            return;
        }
        Glyph::Cross => {
            pb.move_to(cx - r, cy - r);
            pb.line_to(cx + r, cy + r);
            pb.move_to(cx + r, cy - r);
            pb.line_to(cx - r, cy + r);
        }
        Glyph::Circle => pb.push_circle(cx, cy, r + 0.5),
        Glyph::Square => {
            pb.move_to(cx - r, cy - r);
            pb.line_to(cx + r, cy - r);
            pb.line_to(cx + r, cy + r);
            pb.line_to(cx - r, cy + r);
            pb.close();
        }
        Glyph::Triangle => {
            pb.move_to(cx, cy - r - 1.5);
            pb.line_to(cx + r + 1.0, cy + r - 1.0);
            pb.line_to(cx - r - 1.0, cy + r - 1.0);
            pb.close();
        }
    }
    pm.stroke_path(&pb.finish().unwrap(), &p, &line(2.4), ID, None);
}

fn draw_stick(pm: &mut Pixmap, look: &Look, cx: f32, cy: f32, x: f32, y: f32, pressed: bool) {
    let well = circle(cx, cy, 33.0);
    pm.fill_path(&well, &look.well, FillRule::Winding, ID, None);
    pm.stroke_path(&well, &look.border, &line(1.4), ID, None);
    // Keep the cap inside the well, also on a diagonal.
    let len = (x * x + y * y).sqrt();
    let (x, y) = if len > 1.0 { (x / len, y / len) } else { (x, y) };
    let moved = len > DEADZONE * 2.0;
    let (px, py) = (cx + x * 13.0, cy + y * 13.0);
    let cap = circle(px, py, 21.0);
    let cap_idle = paint((0x3c, 0x41, 0x4c), 255);
    pm.fill_path(&cap, if pressed { &look.hot } else { &cap_idle }, FillRule::Winding, ID, None);
    let rim = if moved && !pressed { paint(ACCENT, 255) } else { paint((255, 255, 255), 90) };
    pm.stroke_path(&cap, &rim, &line(if moved { 2.4 } else { 1.4 }), ID, None);
    let dimple = if pressed { paint(INK, 70) } else { paint((255, 255, 255), 30) };
    pm.stroke_path(&circle(px, py, 12.0), &dimple, &line(1.6), ID, None);
}

fn draw_dpad(pm: &mut Pixmap, look: &Look, cx: f32, cy: f32, s: &PadState) {
    let (arm, len) = (11.0, 31.0);
    let cross = {
        let mut pb = PathBuilder::new();
        pb.push_path(&rounded(cx - arm, cy - len, arm * 2.0, len * 2.0, 5.0));
        pb.push_path(&rounded(cx - len, cy - arm, len * 2.0, arm * 2.0, 5.0));
        pb.finish().unwrap()
    };
    pm.fill_path(&cross, &look.well, FillRule::Winding, ID, None);
    for (btn, dx, dy) in [(Btn::Up, 0.0, -1.0), (Btn::Down, 0.0, 1.0), (Btn::Left, -1.0, 0.0), (Btn::Right, 1.0, 0.0)] {
        let on = s.pressed.contains(&btn);
        if on {
            let (x0, y0) = (cx + dx * arm - arm * (1.0 - f32::abs(dx)), cy + dy * arm - arm * (1.0 - f32::abs(dy)));
            let (w, h) = (if dx == 0.0 { arm * 2.0 } else { len - arm }, if dy == 0.0 { arm * 2.0 } else { len - arm });
            let (x0, y0) = (if dx < 0.0 { cx - len } else { x0 }, if dy < 0.0 { cy - len } else { y0 });
            pm.fill_path(&rounded(x0, y0, w, h, 5.0), &look.hot, FillRule::Winding, ID, None);
        }
        // Small arrow on each arm.
        let (tx, ty) = (cx + dx * (len - 9.0), cy + dy * (len - 9.0));
        let mut pb = PathBuilder::new();
        pb.move_to(tx + dx * 4.0, ty + dy * 4.0);
        pb.line_to(tx - dx * 2.5 + dy * 4.5, ty - dy * 2.5 + dx * 4.5);
        pb.line_to(tx - dx * 2.5 - dy * 4.5, ty - dy * 2.5 - dx * 4.5);
        pb.close();
        let col = if on { paint(INK, 255) } else { paint((255, 255, 255), 110) };
        pm.fill_path(&pb.finish().unwrap(), &col, FillRule::Winding, ID, None);
    }
    // Outline of the whole cross (drawn as the union of both bars).
    let mut outline = PathBuilder::new();
    let r = 5.0;
    let pts = [
        (cx - arm, cy - len), (cx + arm, cy - len), (cx + arm, cy - arm), (cx + len, cy - arm), (cx + len, cy + arm), (cx + arm, cy + arm),
        (cx + arm, cy + len), (cx - arm, cy + len), (cx - arm, cy + arm), (cx - len, cy + arm), (cx - len, cy - arm), (cx - arm, cy - arm),
    ];
    for (i, p) in pts.iter().enumerate() {
        let prev = pts[(i + pts.len() - 1) % pts.len()];
        let next = pts[(i + 1) % pts.len()];
        let toward = |q: (f32, f32)| {
            let (dx, dy) = (q.0 - p.0, q.1 - p.1);
            let l = (dx * dx + dy * dy).sqrt();
            (p.0 + dx / l * r, p.1 + dy / l * r)
        };
        let (a, b) = (toward(prev), toward(next));
        if i == 0 {
            outline.move_to(a.0, a.1);
        } else {
            outline.line_to(a.0, a.1);
        }
        outline.quad_to(p.0, p.1, b.0, b.1);
    }
    outline.close();
    pm.stroke_path(&outline.finish().unwrap(), &look.border, &line(1.4), ID, None);
}

/// Shoulder button or trigger. `amount` 0 … 1 fills it from the bottom. The body later covers
/// the lower part, so the label sits at `label_y` in the part that stays visible.
fn draw_shoulder(pm: &mut Pixmap, look: &Look, fonts: &Fonts, (x, y, w, h): (f32, f32, f32, f32), label_y: f32, amount: f32, label: &str) {
    let shape = rounded(x, y, w, h, 9.0);
    pm.fill_path(&shape, &look.idle, FillRule::Winding, ID, None);
    if amount > 0.01 {
        let mut mask = tiny_skia::Mask::new(pm.width(), pm.height()).unwrap();
        mask.fill_path(&shape, FillRule::Winding, true, ID);
        let fh = h * amount.clamp(0.0, 1.0);
        let fill = tiny_skia::Rect::from_xywh(x, y + h - fh, w, fh).unwrap();
        pm.fill_rect(fill, &look.hot, ID, Some(&mask));
    }
    pm.stroke_path(&shape, &look.border, &line(1.4), ID, None);
    let covered = y + h - h * amount.clamp(0.0, 1.0) < label_y;
    centered_text(pm, &fonts.bold, 14.0, x + w / 2.0, label_y, label, if covered { INK } else { TEXT });
}

/// Draw the controller for `s` (also used for the preview in the settings).
pub fn render(cfg: &ControllerOverlay, s: &PadState, fonts: &Fonts) -> Pixmap {
    let mut pm = Pixmap::new(W as u32, H as u32).unwrap();
    let look = Look { idle: paint((18, 20, 26), 205), hot: paint(ACCENT, 250), border: paint((255, 255, 255), 60), well: paint((8, 9, 12), 235) };
    let ps = cfg.model == ControllerModel::PlayStation;
    let down = |b: Btn| s.pressed.contains(&b);

    // Triggers and shoulder buttons peek out behind the body.
    let (lt, rt, lb, rb) = if ps { ("L2", "R2", "L1", "R1") } else { ("LT", "RT", "LB", "RB") };
    let on = |b: Btn| if down(b) { 1.0 } else { 0.0 };
    draw_shoulder(&mut pm, &look, fonts, (104.0, 2.0, 70.0, 38.0), 21.0, s.lt, lt);
    draw_shoulder(&mut pm, &look, fonts, (W - 174.0, 2.0, 70.0, 38.0), 21.0, s.rt, rt);
    draw_shoulder(&mut pm, &look, fonts, (84.0, 44.0, 124.0, 40.0), 55.5, on(Btn::L1), lb);
    draw_shoulder(&mut pm, &look, fonts, (W - 208.0, 44.0, 124.0, 40.0), 55.5, on(Btn::R1), rb);

    // Body (translucent: clear what it covers first so the shoulders do not shine through).
    let body = body(cfg.model);
    let mut clear = Paint::default();
    clear.anti_alias = true;
    clear.blend_mode = tiny_skia::BlendMode::Clear;
    pm.fill_path(&body, &clear, FillRule::Winding, ID, None);
    pm.fill_path(&body, &look.idle, FillRule::Winding, ID, None);
    pm.stroke_path(&body, &look.border, &line(1.6), ID, None);

    // Where things sit: Xbox has the left stick on top, PlayStation the d-pad.
    let (upper_left, lower_left, lower_right, face) = ((150.0, 152.0), (216.0, 218.0), (344.0, 218.0), (410.0, 152.0));
    let (lstick, dpad) = if ps { (lower_left, upper_left) } else { (upper_left, lower_left) };
    draw_stick(&mut pm, &look, lstick.0, lstick.1, s.lx, s.ly, down(Btn::L3));
    draw_stick(&mut pm, &look, lower_right.0, lower_right.1, s.rx, s.ry, down(Btn::R3));
    draw_dpad(&mut pm, &look, dpad.0, dpad.1, s);

    for f in faces(cfg.model) {
        let (cx, cy) = (face.0 + f.dx * 31.0, face.1 + f.dy * 31.0);
        let on = down(f.btn);
        let c = circle(cx, cy, 16.5);
        let lit = paint(f.rgb, 255);
        pm.fill_path(&c, if on { &lit } else { &look.well }, FillRule::Winding, ID, None);
        pm.stroke_path(&c, &if on { paint((255, 255, 255), 120) } else { paint(f.rgb, 150) }, &line(1.5), ID, None);
        draw_glyph(&mut pm, fonts, &f.glyph, cx, cy, if on { INK } else { f.rgb });
    }

    // Centre: menu buttons and the logo button.
    let small = |pm: &mut Pixmap, shape: &Path, on: bool| {
        pm.fill_path(shape, if on { &look.hot } else { &look.well }, FillRule::Winding, ID, None);
        pm.stroke_path(shape, &look.border, &line(1.3), ID, None);
    };
    if ps {
        // Touchpad, with Create / Options beside it and the PS button between the sticks.
        let pad = rounded(222.0, 92.0, 116.0, 70.0, 12.0);
        pm.fill_path(&pad, &paint((255, 255, 255), 16), FillRule::Winding, ID, None);
        pm.stroke_path(&pad, &look.border, &line(1.3), ID, None);
        small(&mut pm, &rounded(196.0, 96.0, 13.0, 26.0, 6.5), down(Btn::Select));
        small(&mut pm, &rounded(351.0, 96.0, 13.0, 26.0, 6.5), down(Btn::Start));
        small(&mut pm, &circle(280.0, 218.0, 13.0), down(Btn::Guide));
    } else {
        small(&mut pm, &circle(246.0, 152.0, 10.5), down(Btn::Select));
        small(&mut pm, &circle(314.0, 152.0, 10.5), down(Btn::Start));
        small(&mut pm, &circle(280.0, 110.0, 18.0), down(Btn::Guide));
        // The guide button's "X".
        let col = if down(Btn::Guide) { paint(INK, 255) } else { paint((255, 255, 255), 120) };
        let mut pb = PathBuilder::new();
        pb.move_to(273.0, 103.0);
        pb.line_to(287.0, 117.0);
        pb.move_to(287.0, 103.0);
        pb.line_to(273.0, 117.0);
        pm.stroke_path(&pb.finish().unwrap(), &col, &line(2.6), ID, None);
        // View (two windows) and menu (three lines) symbols.
        let ink = |on: bool| if on { paint(INK, 255) } else { paint((255, 255, 255), 130) };
        let mut pb = PathBuilder::new();
        for dy in [-3.5, 0.0, 3.5] {
            pb.move_to(309.5, 152.0 + dy);
            pb.line_to(318.5, 152.0 + dy);
        }
        pm.stroke_path(&pb.finish().unwrap(), &ink(down(Btn::Start)), &line(1.5), ID, None);
        let mut pb = PathBuilder::new();
        pb.push_path(&rounded(241.0, 147.5, 7.0, 6.0, 1.0));
        pb.push_path(&rounded(244.5, 150.5, 7.0, 6.0, 1.0));
        pm.stroke_path(&pb.finish().unwrap(), &ink(down(Btn::Select)), &line(1.2), ID, None);
    }
    pm
}

/// Static preview with a few inputs active, for the settings.
pub fn preview(cfg: &ControllerOverlay) -> Option<super::Image> {
    let fonts = Fonts::load().ok()?;
    Some(image_from_pixmap(render(cfg, &sample_state(), &fonts)))
}

fn sample_state() -> PadState {
    let mut s = PadState { lx: -0.7, ly: -0.5, rt: 0.65, ..Default::default() };
    s.pressed.extend([Btn::South, Btn::L1, Btn::Right]);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xbox_map() -> PadMap {
        let mut ranges = HashMap::new();
        for a in [Abs::ABS_X, Abs::ABS_Y, Abs::ABS_RX, Abs::ABS_RY] {
            ranges.insert(a.0, (-32768, 32767));
        }
        for a in [Abs::ABS_Z, Abs::ABS_RZ] {
            ranges.insert(a.0, (0, 1023));
        }
        PadMap { swap_north_west: true, z_is_right_stick: false, ranges }
    }

    #[test]
    fn xpad_reports_x_and_y_under_swapped_codes() {
        let mut s = PadState::default();
        s.key(&xbox_map(), KeyCode::BTN_NORTH, true); // the X button, left
        assert!(s.pressed.contains(&Btn::West));
        s.key(&xbox_map(), KeyCode::BTN_NORTH, false);
        s.key(&xbox_map(), KeyCode::BTN_WEST, true); // the Y button, top
        assert_eq!(s.pressed, HashSet::from([Btn::North]));
    }

    #[test]
    fn sony_codes_are_positions() {
        let map = PadMap { swap_north_west: false, ..xbox_map() };
        let mut s = PadState::default();
        s.key(&map, KeyCode::BTN_NORTH, true); // triangle
        s.key(&map, KeyCode::BTN_WEST, true); // square
        assert_eq!(s.pressed, HashSet::from([Btn::North, Btn::West]));
    }

    #[test]
    fn axes_are_normalised_with_the_device_range() {
        let map = xbox_map();
        let mut s = PadState::default();
        s.axis(&map, Abs::ABS_X, 32767);
        s.axis(&map, Abs::ABS_Y, -32768);
        s.axis(&map, Abs::ABS_RZ, 1023);
        s.axis(&map, Abs::ABS_Z, 0);
        assert_eq!((s.lx, s.ly, s.rt, s.lt), (1.0, -1.0, 1.0, 0.0));
        // DualSense sticks are 0 … 255 with the centre at 127/128.
        let mut ps = xbox_map();
        ps.ranges.insert(Abs::ABS_X.0, (0, 255));
        s.axis(&ps, Abs::ABS_X, 128);
        assert!(s.lx.abs() < 0.01);
    }

    #[test]
    fn hat_is_the_dpad_and_releases_the_other_side() {
        let map = xbox_map();
        let mut s = PadState::default();
        s.axis(&map, Abs::ABS_HAT0X, -1);
        assert!(s.pressed.contains(&Btn::Left));
        s.axis(&map, Abs::ABS_HAT0X, 1);
        assert_eq!(s.pressed, HashSet::from([Btn::Right]));
        s.axis(&map, Abs::ABS_HAT0X, 0);
        assert!(s.pressed.is_empty());
    }

    #[test]
    fn pads_without_rx_use_z_for_the_right_stick() {
        let mut map = xbox_map();
        map.z_is_right_stick = true;
        map.ranges.insert(Abs::ABS_Z.0, (0, 255));
        let mut s = PadState::default();
        s.axis(&map, Abs::ABS_Z, 255);
        assert_eq!((s.rx, s.lt), (1.0, 0.0));
        s.key(&map, KeyCode::BTN_TL2, true);
        assert_eq!(s.lt, 1.0);
    }

    #[test]
    fn small_stick_noise_is_not_a_redraw() {
        let a = PadState::default();
        let b = PadState { lx: 0.01, ..Default::default() };
        assert!(!a.differs(&b));
        assert!(a.differs(&PadState { lx: 0.2, ..Default::default() }));
    }

    /// RSR_INPUT_PREVIEW=/dir cargo test controller_preview – renders both models as PNG.
    #[test]
    fn controller_preview() {
        let Some(dir) = std::env::var_os("RSR_INPUT_PREVIEW") else { return };
        let fonts = Fonts::load().unwrap();
        let mut busy = sample_state();
        busy.pressed.extend([Btn::North, Btn::East, Btn::Up, Btn::R3, Btn::Start, Btn::Guide]);
        busy.rx = 1.0;
        busy.ry = 1.0;
        busy.lt = 1.0;
        for (name, model) in [("xbox", ControllerModel::Xbox), ("playstation", ControllerModel::PlayStation)] {
            let cfg = ControllerOverlay { model };
            for (state, s) in [("idle", PadState::default()), ("sample", sample_state()), ("busy", busy.clone())] {
                render(&cfg, &s, &fonts).save_png(std::path::Path::new(&dir).join(format!("pad_{name}_{state}.png"))).unwrap();
            }
        }
    }
}
