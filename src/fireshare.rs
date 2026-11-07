//! FireShare integration (https://github.com/ShaneIsrael/fireshare).
//!
//! Uses FireShare's *upload token* API (Settings → Security → Upload Tokens): no password is
//! stored, the token can be revoked at any time on the server.
//!
//! * `GET  /api/upload/token`          – validate the token (returns the user name)
//! * `POST /api/upload/token/chunked`  – upload in chunks (works behind proxies with body limits)
//!
//! FireShare identifies a video by the xxHash3-128 of its first 16 MiB, and serves it at
//! `/watch/<id>` – so the share link is known as soon as the upload finished.

use crate::config::FireshareSettings;
use anyhow::{anyhow, bail, Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

const CHUNK: usize = 8 * 1024 * 1024;
const SUPPORTED: [&str; 4] = ["mp4", "m4v", "mov", "webm"];

#[derive(Debug, Clone)]
pub struct Uploaded {
    pub url: String,
    /// The server already had this exact video.
    pub duplicate: bool,
}

fn base(cfg: &FireshareSettings) -> Result<String> {
    let b = cfg.url.trim().trim_end_matches('/').to_string();
    if !(b.starts_with("http://") || b.starts_with("https://")) {
        bail!("FireShare URL must start with http:// or https://");
    }
    if cfg.token.trim().is_empty() {
        bail!("no FireShare upload token configured");
    }
    Ok(b)
}

fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent(concat!("rust-screen-recorder/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .build()?)
}

fn auth(cfg: &FireshareSettings) -> String {
    format!("Bearer {}", cfg.token.trim())
}

/// Check URL + token. Returns the FireShare user name the token belongs to.
pub fn check(cfg: &FireshareSettings) -> Result<String> {
    let b = base(cfg)?;
    let r = client()?
        .get(format!("{b}/api/upload/token"))
        .header("Authorization", auth(cfg))
        .send()
        .with_context(|| format!("cannot reach {b}"))?;
    let status = r.status();
    let body = r.text().unwrap_or_default();
    match status.as_u16() {
        200 => {
            let v: serde_json::Value = serde_json::from_str(&body).context("unexpected answer – is this a FireShare server?")?;
            Ok(v["username"].as_str().unwrap_or("?").to_string())
        }
        401 => bail!("the upload token was rejected (wrong, revoked or deleted)"),
        404 => bail!("this FireShare version has no upload tokens – please update FireShare"),
        429 => bail!("too many invalid tokens – FireShare asks to wait"),
        s => bail!("FireShare answered {s}: {}", body.trim()),
    }
}

/// FireShare's video id: xxh3-128 (hex) of the first 16 MiB.
pub fn video_id(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut buf = Vec::with_capacity(16 * 1024 * 1024);
    f.by_ref().take(16 * 1024 * 1024).read_to_end(&mut buf)?;
    Ok(format!("{:032x}", xxhash_rust::xxh3::xxh3_128(&buf)))
}

/// MKV etc. are not accepted by FireShare: remux losslessly to MP4 first.
fn ensure_supported(path: &Path) -> Result<(PathBuf, Option<PathBuf>)> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if SUPPORTED.contains(&ext.as_str()) {
        return Ok((path.to_path_buf(), None));
    }
    let tmp_dir = crate::session::log_dir().join("fireshare");
    std::fs::create_dir_all(&tmp_dir)?;
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("clip");
    let out = tmp_dir.join(format!("{stem}.mp4"));
    let o = crate::encoders::ffmpeg_cmd()
        .args(["-loglevel", "error", "-y", "-i"])
        .arg(path)
        .args(["-map", "0", "-c", "copy", "-movflags", "+faststart", "-tag:v", "hvc1"])
        .arg(&out)
        .output()?;
    if !o.status.success() {
        // -tag:v hvc1 only applies to HEVC; retry without it for other codecs.
        let o2 = crate::encoders::ffmpeg_cmd()
            .args(["-loglevel", "error", "-y", "-i"])
            .arg(path)
            .args(["-map", "0", "-c", "copy", "-movflags", "+faststart"])
            .arg(&out)
            .output()?;
        if !o2.status.success() {
            bail!("could not convert {} to MP4 for FireShare: {}", path.display(), String::from_utf8_lossy(&o2.stderr).trim());
        }
    }
    Ok((out.clone(), Some(out)))
}

/// Upload a clip. `game` links it to a game in FireShare's library (if that game exists there).
pub fn upload(cfg: &FireshareSettings, path: &Path, title: &str, game: Option<&str>) -> Result<Uploaded> {
    let b = base(cfg)?;
    // Validate the token first: a server rejecting a large multipart body early only shows up
    // as a broken connection, this gives a clear error instead.
    check(cfg)?;
    let (file, tmp) = ensure_supported(path)?;
    let r = upload_inner(cfg, &b, &file, title, game);
    if let Some(t) = tmp {
        let _ = std::fs::remove_file(t);
    }
    r
}

fn upload_inner(cfg: &FireshareSettings, b: &str, file: &Path, title: &str, game: Option<&str>) -> Result<Uploaded> {
    let id = video_id(file)?;
    let url = format!("{b}/watch/{id}");
    let game = if cfg.link_game { game.filter(|g| !g.trim().is_empty()) } else { None };
    let folder = if cfg.folder_per_game { game.map(|g| g.to_string()) } else { None }
        .or_else(|| Some(cfg.folder.trim().to_string()).filter(|f| !f.is_empty()));

    match send_chunks(cfg, b, file, title, game, folder.as_deref()) {
        Ok(dup) => Ok(Uploaded { url, duplicate: dup }),
        // FireShare rejects game names it does not know (400) – upload without the game link.
        Err(UploadError::UnknownGame) if game.is_some() => {
            log::info!("FireShare does not know the game {game:?} – uploading without game link");
            let dup = send_chunks(cfg, b, file, title, None, folder.as_deref()).map_err(|e| anyhow!("{e}"))?;
            Ok(Uploaded { url, duplicate: dup })
        }
        Err(e) => Err(anyhow!("{e}")),
    }
}

#[derive(Debug)]
enum UploadError {
    UnknownGame,
    Other(String),
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UploadError::UnknownGame => write!(f, "FireShare does not know this game"),
            UploadError::Other(s) => write!(f, "{s}"),
        }
    }
}

/// Returns Ok(duplicate).
fn send_chunks(cfg: &FireshareSettings, b: &str, file: &Path, title: &str, game: Option<&str>, folder: Option<&str>) -> Result<bool, UploadError> {
    let e = |m: String| UploadError::Other(m);
    let http = client().map_err(|x| e(x.to_string()))?;
    let size = std::fs::metadata(file).map_err(|x| e(x.to_string()))?.len() as usize;
    let total = size.div_ceil(CHUNK).max(1);
    let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("clip.mp4").to_string();
    // Unique per upload: [A-Za-z0-9_-] only.
    let check_sum = format!("rsr{}{}", std::process::id(), chrono::Local::now().format("%Y%m%d%H%M%S%3f"));
    let mut f = std::fs::File::open(file).map_err(|x| e(x.to_string()))?;
    let mut buf = vec![0u8; CHUNK];

    for part in 1..=total {
        let n = read_full(&mut f, &mut buf).map_err(|x| e(x.to_string()))?;
        let blob = reqwest::blocking::multipart::Part::bytes(buf[..n].to_vec())
            .file_name(name.clone())
            .mime_str("application/octet-stream")
            .map_err(|x| e(x.to_string()))?;
        let mut form = reqwest::blocking::multipart::Form::new()
            .part("blob", blob)
            .text("chunkPart", part.to_string())
            .text("totalChunks", total.to_string())
            .text("checkSum", check_sum.clone())
            .text("fileName", name.clone())
            .text("fileSize", size.to_string())
            .text("title", title.to_string());
        // folder/game/title must be identical on every chunk (see FireShare docs).
        if let Some(fo) = folder {
            form = form.text("folder", fo.to_string());
        }
        if let Some(g) = game {
            form = form.text("game", g.to_string());
        }
        let r = http
            .post(format!("{b}/api/upload/token/chunked"))
            .header("Authorization", auth(cfg))
            .multipart(form)
            .send()
            .map_err(|x| e(format!("upload failed: {x}")))?;
        let status = r.status().as_u16();
        let body = r.text().unwrap_or_default();
        match status {
            202 if part < total => continue,
            201 => return Ok(false),
            409 => return Ok(true),
            400 if game.is_some() && body.to_lowercase().contains("game") => return Err(UploadError::UnknownGame),
            401 => return Err(e("FireShare rejected the upload token".into())),
            413 => return Err(e("the clip is too large for this FireShare server".into())),
            s => return Err(e(format!("FireShare answered {s} on part {part}/{total}: {}", body.trim()))),
        }
    }
    Err(e("FireShare did not confirm the finished upload".into()))
}

fn read_full(f: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let r = f.read(&mut buf[n..])?;
        if r == 0 {
            break;
        }
        n += r;
    }
    Ok(n)
}

/// Put text on the clipboard (Klipper via D-Bus, fallback wl-copy).
pub async fn copy_to_clipboard(conn: &zbus::Connection, text: &str) -> bool {
    let r = conn
        .call_method(Some("org.kde.klipper"), "/klipper", Some("org.kde.klipper.klipper"), "setClipboardContents", &(text,))
        .await;
    if r.is_ok() {
        return true;
    }
    std::process::Command::new("wl-copy")
        .arg(text)
        .stdin(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    #[test]
    fn xxh3_matches_python_xxhash() {
        // python3 -c "import xxhash; print(xxhash.xxh3_128_hexdigest(b'fireshare'))"
        // Known vector for the empty input from the xxHash spec:
        assert_eq!(format!("{:032x}", xxhash_rust::xxh3::xxh3_128(b"")), "99aa06d3014798d86001c324468d497f");
    }
}
