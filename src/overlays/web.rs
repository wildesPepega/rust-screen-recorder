//! Website overlay: a headless Chromium-based browser renders the page, frames arrive through the
//! DevTools protocol (`Page.startScreencast`) as PNGs with a transparent background.

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
        let dir = crate::session::log_dir().join(format!("web-overlay-{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&dir)?;
        let child = std::process::Command::new(&exe)
            .args([
                "--headless=new",
                "--remote-debugging-port=0",
                "--no-first-run",
                "--no-default-browser-check",
                "--hide-scrollbars",
                "--mute-audio",
                "--disable-extensions",
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
        let list: Value = reqwest::blocking::get(format!("http://127.0.0.1:{port}/json/list"))?.json()?;
        let ws_url = list
            .as_array()
            .and_then(|a| a.iter().find(|t| t["type"] == "page"))
            .and_then(|t| t["webSocketDebuggerUrl"].as_str())
            .ok_or_else(|| anyhow!("no page target in the browser"))?
            .to_string();
        let (ws, _) = tungstenite::connect(ws_url)?;
        Ok(Browser { child, dir, ws, next_id: 1 })
    }

    fn send(&mut self, method: &str, params: Value) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        self.ws.send(Message::text(json!({ "id": id, "method": method, "params": params }).to_string()))?;
        Ok(id)
    }

    /// Wait for the reply to `id` (events in between are dropped).
    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.send(method, params)?;
        loop {
            let msg = self.ws.read()?;
            if let Message::Text(t) = msg {
                let v: Value = serde_json::from_str(&t)?;
                if v["id"].as_u64() == Some(id) {
                    if let Some(e) = v.get("error") {
                        bail!("{method}: {e}");
                    }
                    return Ok(v["result"].clone());
                }
            }
        }
    }

    /// Wait for a DevTools event (other messages are dropped).
    fn wait_event(&mut self, method: &str, timeout: Duration) -> Result<()> {
        if let MaybeTlsStream::Plain(s) = self.ws.get_ref() {
            s.set_read_timeout(Some(Duration::from_millis(250)))?;
        }
        let start = Instant::now();
        let r = loop {
            if start.elapsed() > timeout {
                break Err(anyhow!("timed out waiting for {method}"));
            }
            match self.ws.read() {
                Ok(Message::Text(t)) => {
                    if serde_json::from_str::<Value>(&t).map(|v| v["method"] == method).unwrap_or(false) {
                        break Ok(());
                    }
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(e) => break Err(e.into()),
            }
        };
        if let MaybeTlsStream::Plain(s) = self.ws.get_ref() {
            s.set_read_timeout(None)?;
        }
        r
    }

    fn setup(&mut self, cfg: &WebsiteOverlay) -> Result<()> {
        self.call(
            "Emulation.setDeviceMetricsOverride",
            json!({ "width": cfg.page_width.max(64), "height": cfg.page_height.max(64), "deviceScaleFactor": 1, "mobile": false }),
        )?;
        if cfg.transparent {
            self.call("Emulation.setDefaultBackgroundColorOverride", json!({ "color": { "r": 0, "g": 0, "b": 0, "a": 0 } }))?;
        }
        self.call("Page.enable", json!({}))?;
        self.call("Page.navigate", json!({ "url": cfg.url }))?;
        Ok(())
    }
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
        for x in cx..cx + cw {
            let i = ((y * pw + x) as usize) * channels;
            let a = if channels == 4 { buf[i + 3] } else { 255 };
            out.extend_from_slice(&[buf[i + 2], buf[i + 1], buf[i], a]);
        }
    }
    Ok(Image { w: cw.max(1), h: ch.max(1), bgra: out })
}

pub fn spawn(cfg: WebsiteOverlay, slot: Arc<Slot>, stop: Arc<AtomicBool>) -> Result<()> {
    if cfg.url.trim().is_empty() {
        bail!("no URL set");
    }
    std::thread::Builder::new().name("overlay-web".into()).spawn(move || {
        if let Err(e) = run(&cfg, &slot, &stop) {
            log::warn!("website overlay {}: {e:#}", cfg.url);
        }
        slot.clear();
    })?;
    Ok(())
}

fn run(cfg: &WebsiteOverlay, slot: &Slot, stop: &AtomicBool) -> Result<()> {
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
    if let MaybeTlsStream::Plain(s) = b.ws.get_ref() {
        s.set_read_timeout(Some(Duration::from_millis(500)))?;
    }
    let mut last = Instant::now() - Duration::from_secs(1);
    while !stop.load(Ordering::SeqCst) {
        let msg = match b.ws.read() {
            Ok(m) => m,
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) => return Err(e.into()),
        };
        let Message::Text(t) = msg else { continue };
        let v: Value = serde_json::from_str(&t)?;
        if std::env::var_os("RSR_WEB_DEBUG").is_some() {
            eprintln!("cdp: {}", &t.chars().take(160).collect::<String>());
        }
        if v["method"] == "Page.screencastFrame" {
            let p = &v["params"];
            let session = p["sessionId"].clone();
            b.send("Page.screencastFrameAck", json!({ "sessionId": session }))?;
            // At most ~30 fps.
            if last.elapsed() < Duration::from_millis(33) {
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

/// One screenshot of the whole page (for the crop editor in the settings).
pub fn screenshot(cfg: &WebsiteOverlay) -> Result<Image> {
    let mut b = Browser::launch(cfg)?;
    b.setup(cfg)?;
    let _ = b.wait_event("Page.loadEventFired", Duration::from_secs(15));
    std::thread::sleep(Duration::from_millis(1200)); // let the page animate in
    let r = b.call("Page.captureScreenshot", json!({ "format": "png" }))?;
    let data = r["data"].as_str().ok_or_else(|| anyhow!("no screenshot data"))?;
    let png_data = base64::engine::general_purpose::STANDARD.decode(data)?;
    let full = WebsiteOverlay { crop_x: 0, crop_y: 0, crop_w: 0, crop_h: 0, ..cfg.clone() };
    decode(&png_data, &full)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// RSR_WEB_TEST=file:///path/page.html cargo test web_overlay -- --nocapture
    #[test]
    fn web_overlay() {
        let Ok(url) = std::env::var("RSR_WEB_TEST") else { return };
        let cfg = WebsiteOverlay { url, page_width: 900, page_height: 200, ..Default::default() };
        let shot = screenshot(&cfg).expect("screenshot");
        eprintln!("screenshot {}x{}", shot.w, shot.h);
        let slot = Arc::new(Slot::default());
        let stop = Arc::new(AtomicBool::new(false));
        spawn(cfg, slot.clone(), stop.clone()).unwrap();
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
        }
        assert!(frames > 3);
    }
}
