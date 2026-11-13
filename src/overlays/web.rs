//! Website overlay: a headless Chromium-based browser renders the page, frames arrive through the
//! DevTools protocol (`Page.startScreencast`) as PNGs with a transparent background.
//!
//! The same source also feeds the live preview in the settings (with the cut-out switched off).

use super::{Image, Slot};
use crate::config::WebsiteOverlay;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tungstenite::{stream::MaybeTlsStream, Message, WebSocket};

const BROWSERS: &[&str] = &[
    "chromium",
    "google-chrome-stable",
    "google-chrome",
    "brave",
    "brave-browser",
    "brave-origin",
    "brave-origin-beta",
    "microsoft-edge-stable",
    "vivaldi-stable",
];

/// Real browser binaries first: distro wrapper scripts (e.g. Brave's) append user flags from
/// *-flags.conf files, which breaks headless mode ("Multiple targets are not supported").
const BINARIES: &[&str] = &[
    "/usr/lib/chromium/chromium",
    "/usr/lib64/chromium/chromium",
    "/usr/lib/chromium-browser/chromium-browser",
    "/opt/google/chrome/chrome",
    "/opt/brave.com/brave/brave",
    "/opt/brave.com/brave-beta/brave",
    "/opt/brave.com/brave-origin/brave",
    "/opt/brave.com/brave-origin-beta/brave",
    "/opt/brave.com/brave-nightly/brave",
    "/opt/microsoft/msedge/msedge",
    "/opt/vivaldi/vivaldi",
];

pub fn find_browser() -> Option<String> {
    if let Some(b) = BINARIES.iter().find(|p| Path::new(p).is_file()) {
        return Some(b.to_string());
    }
    let path = std::env::var("PATH").unwrap_or_default();
    BROWSERS.iter().find_map(|b| {
        path.split(':').map(|d| PathBuf::from(d).join(b)).find(|p| p.is_file()).map(|p| p.to_string_lossy().to_string())
    })
}

/// Trace output for debugging the browser connection (RSR_WEB_DEBUG=1).
fn trace(msg: impl FnOnce() -> String) {
    if std::env::var_os("RSR_WEB_DEBUG").is_some() {
        eprintln!("web overlay: {}", msg());
    }
}

struct Browser {
    child: Child,
    dir: PathBuf,
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.ws.close(None);
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Browser {
    fn launch(cfg: &WebsiteOverlay) -> Result<Browser> {
        let exe = find_browser().ok_or_else(|| anyhow!("no Chromium-based browser found (Chromium, Chrome, Brave, Edge, Vivaldi)"))?;
        remove_stale_profiles();
        let dir = crate::session::log_dir().join(format!("{PROFILE_PREFIX}{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&dir)?;
        let mut cmd = std::process::Command::new(&exe);
        // The browser must never outlive the thread that owns it (daemon crash, overlay closed
        // while the preview runs): the kernel kills it when this thread exits.
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
        let child = cmd
            .args([
                "--headless=new",
                "--remote-debugging-port=0",
                "--no-first-run",
                "--no-default-browser-check",
                "--hide-scrollbars",
                "--mute-audio",
                "--disable-extensions",
                // A throwaway profile: no component downloads, sync or other background traffic,
                // and no disk cache (these grew every profile to 100+ MB).
                "--disable-component-update",
                "--disable-background-networking",
                "--disable-sync",
                "--disable-default-apps",
                "--disk-cache-size=1",
                "--autoplay-policy=no-user-gesture-required",
                &format!("--window-size={},{}", cfg.page_width.max(64), cfg.page_height.max(64)),
                &format!("--user-data-dir={}", dir.display()),
                "about:blank",
            ])
            // Keep game overlays (MangoHud, vkBasalt …) out of the headless browser.
            .env_remove("MANGOHUD")
            .env_remove("ENABLE_VKBASALT")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("cannot start {exe}"))?;
        // The browser writes the chosen port into DevToolsActivePort.
        let port_file = dir.join("DevToolsActivePort");
        let start = Instant::now();
        let port: u16 = loop {
            if let Ok(s) = std::fs::read_to_string(&port_file) {
                if let Some(p) = s.lines().next().and_then(|l| l.trim().parse().ok()) {
                    break p;
                }
            }
            if start.elapsed() > Duration::from_secs(15) {
                bail!("the browser did not start its DevTools endpoint");
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        trace(|| format!("{exe} listens on DevTools port {port}"));
        // Do not touch the tab before it has finished loading: a DevTools connection made too
        // early (Vivaldi is still attaching the tab to its UI then) leaves the tab dead for good –
        // it never answers a single command. A loaded tab has a title; the tab list can be read
        // without connecting to the tab.
        let ws_url = loop {
            let list: Value = reqwest::blocking::get(format!("http://127.0.0.1:{port}/json/list"))?.json()?;
            let tab = list.as_array().and_then(|a| a.iter().find(|t| t["type"] == "page"));
            if let Some(url) = tab.filter(|t| t["title"].as_str().is_some_and(|s| !s.is_empty())).and_then(|t| t["webSocketDebuggerUrl"].as_str()) {
                break url.to_string();
            }
            if start.elapsed() > Duration::from_secs(20) {
                bail!("the browser did not open a tab");
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let (ws, _) = tungstenite::connect(ws_url)?;
        // Reads return regularly so waiting for a message can time out and be stopped.
        if let MaybeTlsStream::Plain(s) = ws.get_ref() {
            s.set_read_timeout(Some(READ_TICK))?;
        }
        let mut b = Browser { child, dir, ws, next_id: 1 };
        b.call_within("Runtime.evaluate", json!({ "expression": "1" }), Duration::from_secs(5)).context("the browser tab does not answer")?;
        trace(|| format!("browser ready after {:.1} s", start.elapsed().as_secs_f32()));
        Ok(b)
    }

    /// Next message from the browser; None when nothing arrived within `READ_TICK`.
    fn read(&mut self) -> Result<Option<Value>> {
        match self.ws.read() {
            Ok(Message::Text(t)) => {
                trace(|| t.chars().take(200).collect());
                Ok(Some(serde_json::from_str(&t)?))
            }
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn send(&mut self, method: &str, params: Value) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        self.ws.send(Message::text(json!({ "id": id, "method": method, "params": params }).to_string()))?;
        Ok(id)
    }

    /// Send a command and wait for its reply (events in between are dropped).
    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.call_within(method, params, Duration::from_secs(15))
    }

    fn call_within(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.send(method, params)?;
        let start = Instant::now();
        while start.elapsed() < timeout {
            let Some(v) = self.read()? else { continue };
            if v["id"].as_u64() == Some(id) {
                if let Some(e) = v.get("error") {
                    bail!("{method}: {e}");
                }
                return Ok(v["result"].clone());
            }
        }
        bail!("the browser did not answer {method}")
    }

    /// Wait for a DevTools event (other messages are dropped).
    fn wait_event(&mut self, method: &str, timeout: Duration) -> Result<()> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if self.read()?.is_some_and(|v| v["method"] == method) {
                return Ok(());
            }
        }
        bail!("timed out waiting for {method}")
    }

    /// Make the page area of the real browser window exactly `w` × `h`. The screencast delivers
    /// what the window really shows, and some browsers (Vivaldi) put their own UI into the
    /// window, so `--window-size` alone gives a smaller page than asked for.
    fn fit_viewport(&mut self, w: u32, h: u32) -> Result<()> {
        for _ in 0..8 {
            let r = self.call("Runtime.evaluate", json!({ "expression": "innerWidth + 'x' + innerHeight" }))?;
            let size = r["result"]["value"].as_str().and_then(|s| s.split_once('x')).and_then(|(a, b)| Some((a.parse::<i64>().ok()?, b.parse::<i64>().ok()?)));
            let Some((iw, ih)) = size else { bail!("cannot read the page size") };
            if (iw, ih) == (w as i64, h as i64) {
                return Ok(());
            }
            let win = self.call("Browser.getWindowForTarget", json!({}))?;
            let (bw, bh) = (win["bounds"]["width"].as_i64().unwrap_or(w as i64), win["bounds"]["height"].as_i64().unwrap_or(h as i64));
            let bounds = json!({ "width": (bw + w as i64 - iw).max(64), "height": (bh + h as i64 - ih).max(64) });
            self.call("Browser.setWindowBounds", json!({ "windowId": win["windowId"], "bounds": bounds }))?;
            std::thread::sleep(Duration::from_millis(150));
        }
        log::warn!("website overlay: the browser window could not be resized to {w}×{h}");
        Ok(())
    }

    fn setup(&mut self, cfg: &WebsiteOverlay) -> Result<()> {
        self.fit_viewport(cfg.page_width.max(64), cfg.page_height.max(64))?;
        if cfg.transparent {
            self.call("Emulation.setDefaultBackgroundColorOverride", json!({ "color": { "r": 0, "g": 0, "b": 0, "a": 0 } }))?;
        }
        self.call("Page.enable", json!({}))?;
        let css = page_css(cfg);
        if !css.is_empty() {
            // Runs in every document before its own scripts, so it also survives page reloads.
            let js = format!(
                "(() => {{ const css = {}; const add = () => {{ const s = document.createElement('style'); s.textContent = css; \
                 (document.head || document.documentElement).appendChild(s); }}; \
                 if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', add); else add(); }})()",
                serde_json::to_string(&css)?
            );
            self.call("Page.addScriptToEvaluateOnNewDocument", json!({ "source": js }))?;
        }
        let nav = self.call("Page.navigate", json!({ "url": cfg.url.trim() }))?;
        if let Some(e) = nav["errorText"].as_str().filter(|e| !e.is_empty()) {
            bail!("cannot open the page ({e})");
        }
        Ok(())
    }
}

const PROFILE_PREFIX: &str = "web-overlay-";
const READ_TICK: Duration = Duration::from_millis(250);
/// Pause before a failed overlay (page not reachable, browser crashed) is started again.
const RETRY_AFTER: Duration = Duration::from_secs(5);

/// Styles added to the page: the zoom and the user's own CSS.
fn page_css(cfg: &WebsiteOverlay) -> String {
    let mut css = String::new();
    let zoom = cfg.zoom_percent.clamp(25, 400);
    if zoom != 100 {
        css.push_str(&format!(":root {{ zoom: {}; }}\n", zoom as f32 / 100.0));
    }
    css.push_str(cfg.custom_css.trim());
    css
}

/// Delete browser profiles left behind by recorder processes that no longer exist (killed or
/// crashed before they could clean up).
fn remove_stale_profiles() {
    let Ok(rd) = std::fs::read_dir(crate::session::log_dir()) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(pid) = name.strip_prefix(PROFILE_PREFIX).and_then(|r| r.split('-').next()).and_then(|p| p.parse::<u32>().ok()) else { continue };
        if !is_recorder_process(pid) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Is `pid` a running rust-screen-recorder (daemon or overlay)? PIDs get reused by other programs.
fn is_recorder_process(pid: u32) -> bool {
    let own = std::fs::read_to_string("/proc/self/comm").unwrap_or_default();
    std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| !own.is_empty() && c == own)
}

fn rand_suffix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos() as u64).unwrap_or(0)
}

/// PNG → straight-alpha BGRA, cropped to the configured part of the page.
fn decode(png_data: &[u8], cfg: &WebsiteOverlay) -> Result<Image> {
    let dec = png::Decoder::new(png_data);
    let mut reader = dec.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    let (pw, ph) = (info.width, info.height);
    let channels = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        c => bail!("unsupported PNG colour type {c:?}"),
    };
    let (cx, cy) = (cfg.crop_x.min(pw.saturating_sub(1)), cfg.crop_y.min(ph.saturating_sub(1)));
    let cw = if cfg.crop_w == 0 { pw - cx } else { cfg.crop_w.min(pw - cx) };
    let ch = if cfg.crop_h == 0 { ph - cy } else { cfg.crop_h.min(ph - cy) };
    let mut out = Vec::with_capacity((cw * ch * 4) as usize);
    for y in cy..cy + ch {
        let row = &buf[(y * pw + cx) as usize * channels..][..cw as usize * channels];
        if channels == 4 {
            // Copy the row in one go, then swap red and blue in place.
            let start = out.len();
            out.extend_from_slice(row);
            for p in out[start..].chunks_exact_mut(4) {
                p.swap(0, 2);
            }
        } else {
            for p in row.chunks_exact(3) {
                out.extend_from_slice(&[p[2], p[1], p[0], 255]);
            }
        }
    }
    Ok(Image { w: cw.max(1), h: ch.max(1), bgra: out })
}

pub fn spawn(cfg: WebsiteOverlay, slot: Arc<Slot>, stop: Arc<AtomicBool>) -> Result<()> {
    if cfg.url.trim().is_empty() {
        bail!("no URL set");
    }
    std::thread::Builder::new().name("overlay-web".into()).spawn(move || {
        // Keep trying: the page may belong to a program that is started later (alert/chat tools).
        let mut last_error = String::new();
        while !stop.load(Ordering::SeqCst) {
            if let Err(e) = run(&cfg, &slot, &stop) {
                let e = format!("{e:#}");
                if e != last_error {
                    log::warn!("website overlay {}: {e} – retrying every {} s", cfg.url, RETRY_AFTER.as_secs());
                    last_error = e;
                }
            }
            slot.clear();
            let wait = Instant::now();
            while wait.elapsed() < RETRY_AFTER && !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    })?;
    Ok(())
}

/// Render the page into `slot` until `stop` is set (blocking). Also used by the settings preview.
pub fn run(cfg: &WebsiteOverlay, slot: &Slot, stop: &AtomicBool) -> Result<()> {
    let mut b = Browser::launch(cfg)?;
    b.setup(cfg)?;
    // Screencasting only works once the navigated page is attached: wait for its load event.
    b.wait_event("Page.loadEventFired", Duration::from_secs(15))?;
    let params = json!({ "format": "png", "maxWidth": cfg.page_width, "maxHeight": cfg.page_height, "everyNthFrame": 1 });
    let mut tries = 0;
    while let Err(e) = b.call("Page.startScreencast", params.clone()) {
        tries += 1;
        if tries > 10 {
            return Err(e);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    let min_gap = Duration::from_secs_f64(1.0 / cfg.fps.clamp(1, 60) as f64) - Duration::from_millis(2);
    let mut last = Instant::now() - Duration::from_secs(1);
    while !stop.load(Ordering::SeqCst) {
        let Some(v) = b.read()? else { continue };
        if v["method"] == "Page.screencastFrame" {
            let p = &v["params"];
            let session = p["sessionId"].clone();
            b.send("Page.screencastFrameAck", json!({ "sessionId": session }))?;
            // The browser sends a frame whenever the page changes; drop what exceeds the limit.
            if last.elapsed() < min_gap {
                continue;
            }
            last = Instant::now();
            if let Some(data) = p["data"].as_str() {
                let png_data = base64::engine::general_purpose::STANDARD.decode(data)?;
                match decode(&png_data, cfg) {
                    Ok(img) => slot.put(img),
                    Err(e) => log::debug!("website overlay frame: {e:#}"),
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    /// RSR_WEB_TEST=file:///path/page.html cargo test web_overlay -- --nocapture
    #[test]
    fn page_css_combines_zoom_and_custom_css() {
        assert_eq!(page_css(&WebsiteOverlay::default()), "");
        let cfg = WebsiteOverlay { zoom_percent: 150, custom_css: " .ad { display: none } ".into(), ..Default::default() };
        assert_eq!(page_css(&cfg), ":root { zoom: 1.5; }\n.ad { display: none }");
    }

    #[test]
    fn crop_keeps_the_chosen_part() {
        // 3 × 2 page: pixel value = index, RGBA.
        let mut data = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut data, 3, 2);
            enc.set_color(png::ColorType::Rgba);
            let px: Vec<u8> = (0..6u8).flat_map(|i| [i, 100 + i, 200 + i, 255]).collect();
            enc.write_header().unwrap().write_image_data(&px).unwrap();
        }
        let cfg = WebsiteOverlay { crop_x: 1, crop_y: 1, crop_w: 2, crop_h: 1, ..Default::default() };
        let img = decode(&data, &cfg).unwrap();
        assert_eq!((img.w, img.h), (2, 1));
        // Pixels 4 and 5, as BGRA.
        assert_eq!(img.bgra, vec![204, 104, 4, 255, 205, 105, 5, 255]);
        // No cut-out = whole page.
        assert_eq!(decode(&data, &WebsiteOverlay::default()).unwrap().bgra.len(), 6 * 4);
    }

    #[test]
    fn web_overlay() {
        let Ok(url) = std::env::var("RSR_WEB_TEST") else { return };
        let zoom_percent = std::env::var("RSR_WEB_ZOOM").ok().and_then(|z| z.parse().ok()).unwrap_or(100);
        let custom_css = std::env::var("RSR_WEB_CSS").unwrap_or_default();
        let cfg = WebsiteOverlay { url, page_width: 900, page_height: 200, zoom_percent, custom_css, ..Default::default() };
        let slot = Arc::new(Slot::default());
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (slot, stop) = (slot.clone(), stop.clone());
            std::thread::spawn(move || {
                if let Err(e) = run(&cfg, &slot, &stop) {
                    eprintln!("website overlay failed: {e:#}");
                }
            });
        }
        let t = Instant::now();
        let mut frames = 0;
        let mut last = 0;
        while t.elapsed() < Duration::from_secs(8) {
            let v = slot.version.load(Ordering::Relaxed);
            if v != last {
                frames += 1;
                last = v;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::SeqCst);
        eprintln!("screencast updates in 8 s: {frames}");
        if let Some((img, _)) = slot.get() {
            let opaque = img.bgra.chunks(4).filter(|p| p[3] > 200).count();
            eprintln!("last frame {}x{}, opaque pixels {opaque} of {}", img.w, img.h, img.w * img.h);
            if let Some(out) = std::env::var_os("RSR_WEB_OUT") {
                let mut rgba = img.bgra.clone();
                rgba.chunks_exact_mut(4).for_each(|p| p.swap(0, 2));
                let mut enc = png::Encoder::new(std::fs::File::create(out).unwrap(), img.w, img.h);
                enc.set_color(png::ColorType::Rgba);
                enc.write_header().unwrap().write_image_data(&rgba).unwrap();
            }
        }
        assert!(frames > 3);
    }
}
