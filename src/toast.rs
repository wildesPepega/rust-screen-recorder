//! In-game overlay notifications ("toasts").
//!
//! Drawn on a wlr-layer-shell surface in the *overlay* layer: shown above fullscreen games, never
//! takes keyboard focus and lets every click pass through, so the game is never interrupted.

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use anyhow::{anyhow, Result};
use crossbeam_channel::{unbounded, Receiver, Sender};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Rect, Transform};
use wayland_client::protocol::{wl_buffer, wl_compositor, wl_output, wl_region, wl_registry, wl_shm, wl_shm_pool, wl_surface};
use wayland_protocols_plasma::output_order::v1::client::kde_output_order_v1;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

const W: u32 = 360;
const H: u32 = 64;
/// Slide-in distance in pixels.
const SLIDE: f32 = 28.0;
const SHOW_FOR: Duration = Duration::from_millis(3200);
const FADE: Duration = Duration::from_millis(200);
/// A queued toast replaces the visible one only after it was shown this long.
const MIN_SHOW: Duration = Duration::from_millis(1600);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ToastKind {
    /// Green – saved / on.
    Success,
    /// Red – recording.
    Record,
    /// Grey – off / info.
    Info,
    /// Orange – something went wrong.
    Error,
    /// Blue – upload in progress.
    Upload,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub kind: ToastKind,
    pub title: String,
    pub body: String,
}

#[derive(Clone)]
pub struct Toaster {
    tx: Sender<Toast>,
}

impl Toaster {
    pub fn start() -> Result<Toaster> {
        let (tx, rx) = unbounded();
        let fonts = Fonts::load()?;
        let conn = Connection::connect_to_env()?;
        let mut queue: EventQueue<State> = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut st = State::default();
        queue.roundtrip(&mut st)?;
        queue.roundtrip(&mut st)?; // output names + output order
        if st.layer_shell.is_none() {
            return Err(anyhow!("compositor has no wlr-layer-shell"));
        }
        std::thread::Builder::new().name("toasts".into()).spawn(move || {
            if let Err(e) = run(conn, queue, qh, st, rx, fonts) {
                log::error!("toast thread: {e:#}");
            }
        })?;
        Ok(Toaster { tx })
    }

    pub fn show(&self, kind: ToastKind, title: impl Into<String>, body: impl Into<String>) {
        let _ = self.tx.send(Toast { kind, title: title.into(), body: body.into() });
    }

}

// ------------------------------------------------------------------------------------------------
// Rendering
// ------------------------------------------------------------------------------------------------

pub(crate) struct Fonts {
    pub bold: FontVec,
    pub regular: FontVec,
}

impl Fonts {
    pub fn load() -> Result<Fonts> {
        let find = |pattern: &str| -> Result<FontVec> {
            let out = std::process::Command::new("fc-match").args(["-f", "%{file}", pattern]).output()?;
            let path = String::from_utf8_lossy(&out.stdout).to_string();
            let data = std::fs::read(path.trim()).map_err(|e| anyhow!("font {pattern}: {e}"))?;
            FontVec::try_from_vec(data).map_err(|_| anyhow!("invalid font for {pattern}"))
        };
        Ok(Fonts { bold: find("sans-serif:bold")?, regular: find("sans-serif")? })
    }
}

pub(crate) fn draw_text(pm: &mut Pixmap, font: &FontVec, size: f32, x: f32, baseline: f32, max_w: f32, text: &str, rgb: (u8, u8, u8), alpha: f32) {
    let scaled = font.as_scaled(PxScale::from(size));
    let mut caret = x;
    // Ellipsize when too long.
    let ell_w = scaled.h_advance(scaled.glyph_id('…'));
    let total: f32 = text.chars().map(|c| scaled.h_advance(scaled.glyph_id(c))).sum();
    let mut chars: Vec<char> = text.chars().collect();
    let mut ellipsis = false;
    if total > max_w {
        let mut w = 0.0;
        let mut keep = 0;
        for c in &chars {
            let a = scaled.h_advance(scaled.glyph_id(*c));
            if w + a + ell_w > max_w {
                break;
            }
            w += a;
            keep += 1;
        }
        chars.truncate(keep);
        ellipsis = true;
    }
    if ellipsis {
        chars.push('…');
    }
    let (pw, ph) = (pm.width() as i32, pm.height() as i32);
    let data = pm.data_mut();
    let mut prev = None;
    for c in chars {
        let id = scaled.glyph_id(c);
        if let Some(p) = prev {
            caret += scaled.kern(p, id);
        }
        let g = id.with_scale_and_position(PxScale::from(size), ab_glyph::point(caret, baseline));
        caret += scaled.h_advance(id);
        prev = Some(id);
        if let Some(og) = font.outline_glyph(g) {
            let b = og.px_bounds();
            og.draw(|gx, gy, cov| {
                let px = b.min.x as i32 + gx as i32;
                let py = b.min.y as i32 + gy as i32;
                if px < 0 || py < 0 || px >= pw || py >= ph {
                    return;
                }
                let a = (cov * alpha).clamp(0.0, 1.0);
                let i = ((py * pw + px) * 4) as usize;
                // premultiplied "over"
                for (k, col) in [rgb.0, rgb.1, rgb.2].iter().enumerate() {
                    data[i + k] = ((*col as f32) * a + data[i + k] as f32 * (1.0 - a)) as u8;
                }
                data[i + 3] = (255.0 * a + data[i + 3] as f32 * (1.0 - a)) as u8;
            });
        }
    }
}

fn accent(kind: ToastKind) -> (u8, u8, u8) {
    match kind {
        ToastKind::Success => (0x5c, 0xd6, 0x7a),
        ToastKind::Record => (0xf0, 0x3e, 0x3e),
        ToastKind::Info => (0xc4, 0xc8, 0xd0),
        ToastKind::Error => (0xf5, 0xa6, 0x23),
        ToastKind::Upload => (0x3d, 0x9b, 0xff),
    }
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> tiny_skia::Path {
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

/// Thin line icon in the accent colour, centred on (cx, cy), roughly 18 px.
fn draw_icon(pm: &mut Pixmap, kind: ToastKind, cx: f32, cy: f32, rgb: (u8, u8, u8), alpha: f32) {
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.set_color(Color::from_rgba8(rgb.0, rgb.1, rgb.2, (255.0 * alpha) as u8));
    let stroke = tiny_skia::Stroke { width: 2.0, line_cap: tiny_skia::LineCap::Round, line_join: tiny_skia::LineJoin::Round, ..Default::default() };
    let id = Transform::identity();
    let dot = |pm: &mut Pixmap, x: f32, y: f32, r: f32| {
        let mut pb = PathBuilder::new();
        pb.push_circle(x, y, r);
        pm.fill_path(&pb.finish().unwrap(), &paint, FillRule::Winding, id, None);
    };
    match kind {
        ToastKind::Success => {
            let mut pb = PathBuilder::new();
            pb.move_to(cx - 7.0, cy + 0.5);
            pb.line_to(cx - 2.0, cy + 5.5);
            pb.line_to(cx + 8.0, cy - 5.5);
            pm.stroke_path(&pb.finish().unwrap(), &paint, &tiny_skia::Stroke { width: 2.4, ..stroke.clone() }, id, None);
        }
        ToastKind::Record => {
            let mut pb = PathBuilder::new();
            pb.push_circle(cx, cy, 8.5);
            pm.stroke_path(&pb.finish().unwrap(), &paint, &tiny_skia::Stroke { width: 1.6, ..stroke.clone() }, id, None);
            dot(pm, cx, cy, 4.5);
        }
        ToastKind::Upload => {
            let mut pb = PathBuilder::new();
            pb.move_to(cx, cy + 3.0);
            pb.line_to(cx, cy - 8.0);
            pb.move_to(cx - 4.5, cy - 3.5);
            pb.line_to(cx, cy - 8.0);
            pb.line_to(cx + 4.5, cy - 3.5);
            pb.move_to(cx - 8.0, cy + 3.0);
            pb.line_to(cx - 8.0, cy + 8.0);
            pb.line_to(cx + 8.0, cy + 8.0);
            pb.line_to(cx + 8.0, cy + 3.0);
            pm.stroke_path(&pb.finish().unwrap(), &paint, &stroke, id, None);
        }
        ToastKind::Info => {
            let mut pb = PathBuilder::new();
            pb.push_circle(cx, cy, 8.5);
            pm.stroke_path(&pb.finish().unwrap(), &paint, &tiny_skia::Stroke { width: 1.6, ..stroke.clone() }, id, None);
            let mut pb = PathBuilder::new();
            pb.move_to(cx, cy - 0.5);
            pb.line_to(cx, cy + 4.5);
            pm.stroke_path(&pb.finish().unwrap(), &paint, &stroke, id, None);
            dot(pm, cx, cy - 4.0, 1.2);
        }
        ToastKind::Error => {
            let mut pb = PathBuilder::new();
            pb.move_to(cx, cy - 8.5);
            pb.line_to(cx + 9.0, cy + 7.5);
            pb.line_to(cx - 9.0, cy + 7.5);
            pb.close();
            pm.stroke_path(&pb.finish().unwrap(), &paint, &tiny_skia::Stroke { width: 1.6, ..stroke.clone() }, id, None);
            let mut pb = PathBuilder::new();
            pb.move_to(cx, cy - 3.0);
            pb.line_to(cx, cy + 1.5);
            pm.stroke_path(&pb.finish().unwrap(), &paint, &stroke, id, None);
            dot(pm, cx, cy + 4.6, 1.2);
        }
    }
}

/// Flat, compact game-overlay style: dark panel with square-ish corners, a full-height accent
/// edge on the left and a thin line icon. `alpha` fades, `dx` slides the panel in from the right.
fn render(t: &Toast, fonts: &Fonts, alpha: f32, dx: f32) -> Vec<u8> {
    let mut pm = Pixmap::new(W, H).unwrap();
    let a = accent(t.kind);
    let (w, h) = (W as f32, H as f32);
    let card = rounded_rect(dx + 0.5, 0.5, w - 1.0, h - 1.0, 3.0);
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.set_color(Color::from_rgba8(0x0b, 0x0c, 0x0f, (240.0 * alpha) as u8));
    pm.fill_path(&card, &paint, FillRule::Winding, Transform::identity(), None);
    paint.set_color(Color::from_rgba8(0xff, 0xff, 0xff, (18.0 * alpha) as u8));
    pm.stroke_path(&card, &paint, &tiny_skia::Stroke { width: 1.0, ..Default::default() }, Transform::identity(), None);
    // accent edge, clipped to the panel shape
    let mut mask = tiny_skia::Mask::new(W, H).unwrap();
    mask.fill_path(&card, FillRule::Winding, true, Transform::identity());
    paint.set_color(Color::from_rgba8(a.0, a.1, a.2, (255.0 * alpha) as u8));
    if let Some(edge) = Rect::from_xywh(dx, 0.0, 3.0, h) {
        pm.fill_rect(edge, &paint, Transform::identity(), Some(&mask));
    }
    draw_icon(&mut pm, t.kind, dx + 30.0, h / 2.0, a, alpha);
    let tx = dx + 54.0;
    let max_w = w - 54.0 - 16.0;
    draw_text(&mut pm, &fonts.bold, 15.0, tx, 28.0, max_w, &t.title, (0xf4, 0xf5, 0xf7), alpha);
    draw_text(&mut pm, &fonts.regular, 12.5, tx, 47.0, max_w, &t.body, (0x8f, 0x96, 0xa1), alpha);
    // tiny-skia: premultiplied RGBA → wl_shm ARGB8888 (little endian BGRA)
    let mut out = pm.take();
    for px in out.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    out
}

// ------------------------------------------------------------------------------------------------
// Wayland
// ------------------------------------------------------------------------------------------------

#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    configured: bool,
    closed: bool,
    /// wl_output global name → (proxy, connector name like "DP-1").
    outputs: std::collections::HashMap<u32, (wl_output::WlOutput, String)>,
    /// Output names in KDE's priority order; the first one is the primary monitor.
    order: Vec<String>,
    order_building: Vec<String>,
    order_done: bool,
}

impl State {
    fn primary_output(&self) -> Option<wl_output::WlOutput> {
        let name = self.order.first()?;
        let out = self.outputs.values().find(|(_, n)| n == name).map(|(o, _)| o.clone());
        if out.is_none() {
            log::debug!("primary output {name} not found among wl_outputs");
        }
        out
    }
}

struct Shown {
    surface: wl_surface::WlSurface,
    layer: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    buffers: Vec<wl_buffer::WlBuffer>,
    toast: Toast,
    since: Instant,
    last_alpha: f32,
}

impl Shown {
    fn destroy(self) {
        self.layer.destroy();
        self.surface.destroy();
        for b in self.buffers {
            b.destroy();
        }
    }
}

fn shm_buffer(st: &State, qh: &QueueHandle<State>, pixels: &[u8]) -> Result<wl_buffer::WlBuffer> {
    let shm = st.shm.as_ref().ok_or_else(|| anyhow!("no wl_shm"))?;
    let size = pixels.len();
    let fd = unsafe {
        let fd = libc::memfd_create(c"rsr-toast".as_ptr(), libc::MFD_CLOEXEC);
        if fd < 0 {
            return Err(anyhow!("memfd_create failed"));
        }
        OwnedFd::from_raw_fd(fd)
    };
    let mut f = std::fs::File::from(fd);
    use std::io::Write;
    f.write_all(pixels)?;
    let pool = shm.create_pool(f.as_fd(), size as i32, qh, ());
    let buf = pool.create_buffer(0, W as i32, H as i32, (W * 4) as i32, wl_shm::Format::Argb8888, qh, ());
    pool.destroy();
    Ok(buf)
}

fn run(conn: Connection, mut queue: EventQueue<State>, qh: QueueHandle<State>, mut st: State, rx: Receiver<Toast>, fonts: Fonts) -> Result<()> {
    let mut shown: Option<Shown> = None;
    let mut pending: std::collections::VecDeque<Toast> = Default::default();
    loop {
        // Collect new toasts; they are shown one after another (each at least MIN_SHOW).
        let timeout = if shown.is_some() || !pending.is_empty() { Duration::from_millis(16) } else { Duration::from_millis(250) };
        match rx.recv_timeout(timeout) {
            Ok(t) => {
                pending.push_back(t);
                while let Ok(n) = rx.try_recv() {
                    pending.push_back(n);
                }
                while pending.len() > 6 {
                    pending.pop_front();
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(_) => return Ok(()),
        }
        let ready = shown.as_ref().map(|s| s.since.elapsed() >= MIN_SHOW + FADE).unwrap_or(true);
        if ready {
            if let Some(t) = pending.pop_front() {
                if let Some(s) = shown.as_mut() {
                    // Replace the content of the visible toast.
                    s.toast = t;
                    s.since = Instant::now() - FADE;
                    s.last_alpha = -1.0;
                } else {
                    let (Some(comp), Some(ls)) = (st.compositor.as_ref(), st.layer_shell.as_ref()) else { continue };
                    let surface = comp.create_surface(&qh, ());
                    // Empty input region → clicks go through to the game.
                    let region = comp.create_region(&qh, ());
                    surface.set_input_region(Some(&region));
                    region.destroy();
                    // Always on the primary monitor (KDE output order); compositor's choice otherwise.
                    let output = st.primary_output();
                    let layer = ls.get_layer_surface(&surface, output.as_ref(), zwlr_layer_shell_v1::Layer::Overlay, "rust-screen-recorder".into(), &qh, ());
                    layer.set_size(W, H);
                    layer.set_anchor(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Right);
                    layer.set_margin(28, 8, 0, 0);
                    layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
                    layer.set_exclusive_zone(-1);
                    st.configured = false;
                    st.closed = false;
                    surface.commit();
                    log::debug!("toast surface created");
                    shown = Some(Shown { surface, layer, buffers: vec![], toast: t, since: Instant::now(), last_alpha: -1.0 });
                }
            }
        }

        conn.flush()?;
        queue.dispatch_pending(&mut st)?;
        if let Some(guard) = queue.prepare_read() {
            let fd = guard.connection_fd().as_raw_fd();
            let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            if unsafe { libc::poll(&mut pfd, 1, 0) } > 0 {
                let _ = guard.read();
            }
        }
        queue.dispatch_pending(&mut st)?;

        let mut done = false;
        if let Some(s) = shown.as_mut() {
            if st.closed {
                done = true;
            } else if st.configured {
                let el = s.since.elapsed();
                // 0 = hidden, 1 = fully in; quantised to keep the number of shm buffers small.
                let (phase, leaving) = if el < FADE {
                    (el.as_secs_f32() / FADE.as_secs_f32(), false)
                } else if el < SHOW_FOR {
                    (1.0, false)
                } else if el < SHOW_FOR + FADE {
                    (1.0 - (el - SHOW_FOR).as_secs_f32() / FADE.as_secs_f32(), true)
                } else {
                    done = true;
                    (0.0, true)
                };
                let phase = (phase * 12.0).round() / 12.0;
                // In: ease-out slide from the right. Out: fade with a short drift to the right.
                let dx = if leaving { (1.0 - phase) * SLIDE * 0.5 } else { (1.0 - phase).powi(3) * SLIDE };
                let alpha = phase;
                if !done && (alpha - s.last_alpha).abs() > 0.01 {
                    let px = render(&s.toast, &fonts, alpha, dx.round());
                    let buf = shm_buffer(&st, &qh, &px)?;
                    s.surface.attach(Some(&buf), 0, 0);
                    s.surface.damage_buffer(0, 0, W as i32, H as i32);
                    s.surface.commit();
                    s.buffers.push(buf);
                    if s.buffers.len() > 3 {
                        s.buffers.remove(0).destroy();
                    }
                    log::trace!("toast frame alpha {alpha}");
                    s.last_alpha = alpha;
                }
            }
        }
        if done {
            if let Some(s) = shown.take() {
                s.destroy();
            }
            conn.flush()?;
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(st: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "wl_compositor" => st.compositor = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_shm" => st.shm = Some(reg.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => st.layer_shell = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_output" if version >= 4 => {
                    let o: wl_output::WlOutput = reg.bind(name, 4, qh, name);
                    st.outputs.insert(name, (o, String::new()));
                }
                "kde_output_order_v1" => {
                    let _: kde_output_order_v1::KdeOutputOrderV1 = reg.bind(name, 1, qh, ());
                }
                _ => {}
            }
        } else if let wl_registry::Event::GlobalRemove { name } = ev {
            if let Some((o, _)) = st.outputs.remove(&name) {
                o.release();
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(st: &mut Self, _: &wl_output::WlOutput, ev: wl_output::Event, gname: &u32, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name } = ev {
            if let Some(o) = st.outputs.get_mut(gname) {
                o.1 = name;
            }
        }
    }
}

impl Dispatch<kde_output_order_v1::KdeOutputOrderV1, ()> for State {
    fn event(st: &mut Self, _: &kde_output_order_v1::KdeOutputOrderV1, ev: kde_output_order_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            kde_output_order_v1::Event::Output { output_name } => {
                if st.order_done {
                    st.order_building.clear();
                    st.order_done = false;
                }
                st.order_building.push(output_name);
            }
            kde_output_order_v1::Event::Done => {
                st.order = std::mem::take(&mut st.order_building);
                st.order_done = true;
                log::debug!("output order: {:?}", st.order);
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for State {
    fn event(st: &mut Self, l: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ev: zwlr_layer_surface_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => {
                l.ack_configure(serial);
                log::debug!("toast configured");
                st.configured = true;
            }
            zwlr_layer_surface_v1::Event::Closed => {
                log::debug!("toast closed by compositor");
                st.closed = true
            }
            _ => {}
        }
    }
}

macro_rules! ignore {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut Self, _: &$t, _: <$t as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore!(
    wl_compositor::WlCompositor,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    wl_surface::WlSurface,
    wl_region::WlRegion,
    zwlr_layer_shell_v1::ZwlrLayerShellV1
);

#[cfg(test)]
mod tests {
    use super::*;
    /// RSR_TOAST_PREVIEW=/path/dir cargo test toast_preview -- renders sample toasts as PNG.
    #[test]
    fn toast_preview() {
        let Some(dir) = std::env::var_os("RSR_TOAST_PREVIEW") else { return };
        let fonts = Fonts::load().unwrap();
        let samples = [
            ("upload", Toast { kind: ToastKind::Upload, title: "Uploading to FireShare…".into(), body: "Call of Duty_Replay_2026-11-07_07-11-19.mp4".into() }),
            ("done", Toast { kind: ToastKind::Success, title: "Uploaded to FireShare".into(), body: "Link copied · https://clips.example.com/watch/ef03ff6f…".into() }),
            ("saved", Toast { kind: ToastKind::Success, title: "Clip saved".into(), body: "Last 60 seconds · Counter-Strike 2".into() }),
            ("record", Toast { kind: ToastKind::Record, title: "Recording started".into(), body: "Desktop (primary monitor) · 1080p60".into() }),
            ("info", Toast { kind: ToastKind::Info, title: "Instant Replay off".into(), body: "Clips can no longer be saved".into() }),
            ("error", Toast { kind: ToastKind::Error, title: "Saving the clip failed".into(), body: "The encoder exited unexpectedly".into() }),
        ];
        for (name, t) in samples {
            let mut px = render(&t, &fonts, 1.0, 0.0);
            for c in px.chunks_exact_mut(4) {
                c.swap(0, 2);
            }
            let pm = tiny_skia::Pixmap::from_vec(px, tiny_skia::IntSize::from_wh(W, H).unwrap()).unwrap();
            pm.save_png(std::path::Path::new(&dir).join(format!("toast_{name}.png"))).unwrap();
        }
    }
}
