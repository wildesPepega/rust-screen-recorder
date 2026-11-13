//! Video overlays burned into recordings / replay clips: keyboard & mouse input display, controller
//! input display, webcam and websites (rendered by a headless browser). One source per screen corner.
//!
//! Every source renders into an [`Image`] (straight-alpha BGRA). Encoders ask the [`OverlayHub`]
//! for the current [`Layer`]s each frame and composite them – on the GPU with `overlay_vaapi`
//! (zero-copy path) or in software on the CPU path.

pub mod controller;
pub mod input;
pub mod webcam;
pub mod web;

use crate::config::{Corner, OverlayKind, OverlaySettings, OverlaySlot};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// Straight (non-premultiplied) alpha, BGRA byte order.
pub struct Image {
    pub w: u32,
    pub h: u32,
    pub bgra: Vec<u8>,
}

/// Latest image of one source.
#[derive(Default)]
pub struct Slot {
    latest: Mutex<Option<Arc<Image>>>,
    version: AtomicU64,
}

impl Slot {
    pub fn put(&self, img: Image) {
        *self.latest.lock() = Some(Arc::new(img));
        self.version.fetch_add(1, Ordering::Relaxed);
    }
    pub fn clear(&self) {
        *self.latest.lock() = None;
        self.version.fetch_add(1, Ordering::Relaxed);
    }
    pub fn get(&self) -> Option<(Arc<Image>, u64)> {
        self.latest.lock().clone().map(|i| (i, self.version.load(Ordering::Relaxed)))
    }
}

#[derive(Clone)]
pub struct Layer {
    pub corner: Corner,
    pub image: Arc<Image>,
    /// Changes whenever the image content changes (GPU upload cache key).
    pub version: u64,
    pub opacity: f32,
    pub size_percent: f32,
    pub margin_percent: f32,
}

impl Layer {
    /// Destination rectangle (x, y, w, h) inside an `out_w` × `out_h` frame (even values).
    pub fn rect(&self, out_w: u32, out_h: u32) -> (u32, u32, u32, u32) {
        place(self.corner, self.size_percent, self.margin_percent, self.image.w, self.image.h, out_w, out_h)
    }
}

pub fn place(corner: Corner, size_percent: f32, margin_percent: f32, img_w: u32, img_h: u32, out_w: u32, out_h: u32) -> (u32, u32, u32, u32) {
    let even = |v: f32| ((v.round() as u32).max(2)) & !1;
    let w = even(out_w as f32 * (size_percent / 100.0).clamp(0.02, 1.0)).min(out_w & !1);
    let h = even(w as f32 * img_h.max(1) as f32 / img_w.max(1) as f32).min(out_h & !1);
    let m = even(out_h as f32 * (margin_percent / 100.0).clamp(0.0, 0.3)) & !1;
    let x = match corner {
        Corner::TopLeft | Corner::BottomLeft => m,
        _ => out_w.saturating_sub(w + m),
    };
    let y = match corner {
        Corner::TopLeft | Corner::TopRight => m,
        _ => out_h.saturating_sub(h + m),
    };
    (x & !1, y & !1, w, h)
}

/// A running source; dropping it stops the source.
struct Running {
    corner: Corner,
    cfg: OverlaySlot,
    slot: Arc<Slot>,
    stop: Arc<AtomicBool>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Shared by the daemon and all encoders. The daemon starts/stops the sources; encoders only
/// read layers, so overlays can be changed while recording.
#[derive(Default)]
pub struct OverlayHub {
    running: Mutex<Vec<Running>>,
    cfg: Mutex<Option<OverlaySettings>>,
}

impl OverlayHub {
    pub fn new() -> Arc<OverlayHub> {
        Arc::new(OverlayHub::default())
    }

    /// Start the sources for `cfg` (no-op when the same configuration is already running).
    pub fn start(&self, cfg: &OverlaySettings) {
        if self.cfg.lock().as_ref() == Some(cfg) && !self.running.lock().is_empty() {
            return;
        }
        self.stop();
        if !cfg.active() {
            return;
        }
        let mut v = vec![];
        for corner in Corner::ALL {
            let sc = cfg.slot(corner).clone();
            let slot = Arc::new(Slot::default());
            let stop = Arc::new(AtomicBool::new(false));
            let started = match sc.kind {
                OverlayKind::None => continue,
                OverlayKind::Input => input::spawn(sc.input.clone(), slot.clone(), stop.clone()),
                OverlayKind::Webcam => webcam::spawn(sc.webcam.clone(), slot.clone(), stop.clone()),
                OverlayKind::Website => web::spawn(sc.website.clone(), slot.clone(), stop.clone()),
                OverlayKind::Controller => controller::spawn(sc.controller.clone(), slot.clone(), stop.clone()),
            };
            match started {
                Ok(()) => v.push(Running { corner, cfg: sc, slot, stop }),
                Err(e) => log::warn!("{} overlay ({}): {e:#}", sc.kind.label(), corner.label()),
            }
        }
        log::info!("{} overlay source(s) running", v.len());
        *self.running.lock() = v;
        *self.cfg.lock() = Some(cfg.clone());
    }

    pub fn stop(&self) {
        self.running.lock().clear();
        *self.cfg.lock() = None;
    }

    pub fn layers(&self) -> Vec<Layer> {
        self.running
            .lock()
            .iter()
            .filter_map(|r| {
                let (image, version) = r.slot.get()?;
                Some(Layer {
                    corner: r.corner,
                    image,
                    version,
                    opacity: r.cfg.opacity.clamp(0.05, 1.0),
                    size_percent: r.cfg.size_percent,
                    margin_percent: r.cfg.margin_percent,
                })
            })
            .collect()
    }
}

/// Software compositing for the CPU path: alpha-blend `layers` onto a BGRx/BGRA frame.
pub fn blend_cpu(frame: &mut [u8], fw: u32, fh: u32, layers: &[Layer]) {
    for l in layers {
        let (x0, y0, w, h) = l.rect(fw, fh);
        let img = &l.image;
        for y in 0..h {
            let fy = y0 + y;
            if fy >= fh {
                break;
            }
            let sy = (y as u64 * img.h as u64 / h as u64) as u32;
            for x in 0..w {
                let fx = x0 + x;
                if fx >= fw {
                    break;
                }
                let sx = (x as u64 * img.w as u64 / w as u64) as u32;
                let si = ((sy * img.w + sx) * 4) as usize;
                let a = img.bgra[si + 3] as f32 / 255.0 * l.opacity;
                if a <= 0.0 {
                    continue;
                }
                let di = ((fy * fw + fx) * 4) as usize;
                for c in 0..3 {
                    frame[di + c] = (img.bgra[si + c] as f32 * a + frame[di + c] as f32 * (1.0 - a)) as u8;
                }
            }
        }
    }
}

/// Convert a tiny-skia (premultiplied RGBA) pixmap into a straight-alpha BGRA image.
pub fn image_from_pixmap(pm: tiny_skia::Pixmap) -> Image {
    let (w, h) = (pm.width(), pm.height());
    let mut px = pm.take();
    for p in px.chunks_exact_mut(4) {
        let a = p[3];
        if a > 0 && a < 255 {
            for c in 0..3 {
                p[c] = ((p[c] as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
            }
        }
        p.swap(0, 2);
    }
    Image { w, h, bgra: px }
}
