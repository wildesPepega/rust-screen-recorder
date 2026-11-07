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
use wayland_client::protocol::{wl_buffer, wl_compositor, wl_region, wl_registry, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

const W: u32 = 380;
const H: u32 = 92;
const SHOW_FOR: Duration = Duration::from_millis(3200);
const FADE: Duration = Duration::from_millis(180);

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

struct Fonts {
    bold: FontVec,
    regular: FontVec,
}

impl Fonts {
    fn load() -> Result<Fonts> {
        let find = |pattern: &str| -> Result<FontVec> {
            let out = std::process::Command::new("fc-match").args(["-f", "%{file}", pattern]).output()?;
            let path = String::from_utf8_lossy(&out.stdout).to_string();
            let data = std::fs::read(path.trim()).map_err(|e| anyhow!("font {pattern}: {e}"))?;
            FontVec::try_from_vec(data).map_err(|_| anyhow!("invalid font for {pattern}"))
        };
        Ok(Fonts { bold: find("sans-serif:bold")?, regular: find("sans-serif")? })
    }
}

fn draw_text(pm: &mut Pixmap, font: &FontVec, size: f32, x: f32, baseline: f32, max_w: f32, text: &str, rgb: (u8, u8, u8), alpha: f32) {
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

fn render(t: &Toast, fonts: &Fonts, alpha: f32) -> Vec<u8> {
    let mut pm = Pixmap::new(W, H).unwrap();
    let accent = match t.kind {
        ToastKind::Success => (0x3d, 0xdc, 0x97),
        ToastKind::Record => (0xff, 0x4d, 0x4f),
        ToastKind::Info => (0x9a, 0xa0, 0xab),
        ToastKind::Error => (0xff, 0xa5, 0x2e),
        ToastKind::Upload => (0x4d, 0xa3, 0xff),
    };
    // card
    let r = 14.0;
    let (w, h) = (W as f32, H as f32);
    let mut pb = PathBuilder::new();
    pb.move_to(r, 0.0);
    pb.line_to(w - r, 0.0);
    pb.quad_to(w, 0.0, w, r);
    pb.line_to(w, h - r);
    pb.quad_to(w, h, w - r, h);
    pb.line_to(r, h);
    pb.quad_to(0.0, h, 0.0, h - r);
    pb.line_to(0.0, r);
    pb.quad_to(0.0, 0.0, r, 0.0);
    pb.close();
    let card = pb.finish().unwrap();
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.set_color(Color::from_rgba8(0x16, 0x18, 0x1d, (235.0 * alpha) as u8));
    pm.fill_path(&card, &paint, FillRule::Winding, Transform::identity(), None);
    // accent bar + dot
    paint.set_color(Color::from_rgba8(accent.0, accent.1, accent.2, (255.0 * alpha) as u8));
    pm.fill_rect(Rect::from_xywh(0.0, 18.0, 4.0, h - 36.0).unwrap(), &paint, Transform::identity(), None);
    let mut dot = PathBuilder::new();
    dot.push_circle(36.0, h / 2.0, 14.0);
    pm.fill_path(&dot.finish().unwrap(), &paint, FillRule::Winding, Transform::identity(), None);
    // icon inside the dot (vector shapes – independent of the font)
    let dark = Color::from_rgba8(0x10, 0x12, 0x16, (255.0 * alpha) as u8);
    let mut ip = Paint::default();
    ip.anti_alias = true;
    ip.set_color(dark);
    let cy = h / 2.0;
    match t.kind {
        ToastKind::Success => {
            let mut pb = PathBuilder::new();
            pb.move_to(29.5, cy + 0.5);
            pb.line_to(34.0, cy + 5.0);
            pb.line_to(42.5, cy - 4.5);
            let stroke = tiny_skia::Stroke { width: 3.0, line_cap: tiny_skia::LineCap::Round, line_join: tiny_skia::LineJoin::Round, ..Default::default() };
            pm.stroke_path(&pb.finish().unwrap(), &ip, &stroke, Transform::identity(), None);
        }
        ToastKind::Record => {
            let mut pb = PathBuilder::new();
            pb.push_circle(36.0, cy, 6.0);
            pm.fill_path(&pb.finish().unwrap(), &ip, FillRule::Winding, Transform::identity(), None);
        }
        ToastKind::Upload => {
            // upward arrow
            let mut pb = PathBuilder::new();
            pb.move_to(36.0, cy - 7.0);
            pb.line_to(36.0, cy + 7.0);
            pb.move_to(30.5, cy - 1.5);
            pb.line_to(36.0, cy - 7.0);
            pb.line_to(41.5, cy - 1.5);
            let stroke = tiny_skia::Stroke { width: 2.6, line_cap: tiny_skia::LineCap::Round, line_join: tiny_skia::LineJoin::Round, ..Default::default() };
            pm.stroke_path(&pb.finish().unwrap(), &ip, &stroke, Transform::identity(), None);
        }
        ToastKind::Info | ToastKind::Error => {
            let txt = if t.kind == ToastKind::Info { "i" } else { "!" };
            draw_text(&mut pm, &fonts.bold, 18.0, 33.5, cy + 6.5, 20.0, txt, (0x10, 0x12, 0x16), alpha);
        }
    }
    draw_text(&mut pm, &fonts.bold, 18.0, 64.0, 38.0, w - 80.0, &t.title, (0xf2, 0xf3, 0xf5), alpha);
    draw_text(&mut pm, &fonts.regular, 14.0, 64.0, 62.0, w - 80.0, &t.body, (0xa8, 0xae, 0xb8), alpha);
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
    loop {
        // New toast?
        let timeout = if shown.is_some() { Duration::from_millis(16) } else { Duration::from_millis(250) };
        match rx.recv_timeout(timeout) {
            Ok(mut t) => {
                while let Ok(n) = rx.try_recv() {
                    t = n; // only the newest matters
                }
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
                    let layer = ls.get_layer_surface(&surface, None, zwlr_layer_shell_v1::Layer::Overlay, "rust-screen-recorder".into(), &qh, ());
                    layer.set_size(W, H);
                    layer.set_anchor(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Right);
                    layer.set_margin(28, 28, 0, 0);
                    layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
                    layer.set_exclusive_zone(-1);
                    st.configured = false;
                    st.closed = false;
                    surface.commit();
                    log::debug!("toast surface created");
                    shown = Some(Shown { surface, layer, buffers: vec![], toast: t, since: Instant::now(), last_alpha: -1.0 });
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(_) => return Ok(()),
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
                let alpha = if el < FADE {
                    el.as_secs_f32() / FADE.as_secs_f32()
                } else if el < SHOW_FOR {
                    1.0
                } else if el < SHOW_FOR + FADE {
                    1.0 - (el - SHOW_FOR).as_secs_f32() / FADE.as_secs_f32()
                } else {
                    done = true;
                    0.0
                };
                let alpha = (alpha * 10.0).round() / 10.0; // ~10 fade steps
                if !done && (alpha - s.last_alpha).abs() > 0.01 {
                    let px = render(&s.toast, &fonts, alpha);
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
                _ => {}
            }
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
        ];
        for (name, t) in samples {
            let mut px = render(&t, &fonts, 1.0);
            for c in px.chunks_exact_mut(4) {
                c.swap(0, 2);
            }
            let pm = tiny_skia::Pixmap::from_vec(px, tiny_skia::IntSize::from_wh(W, H).unwrap()).unwrap();
            pm.save_png(std::path::Path::new(&dir).join(format!("toast_{name}.png"))).unwrap();
        }
    }
}
