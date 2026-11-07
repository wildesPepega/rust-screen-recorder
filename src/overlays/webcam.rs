//! Webcam overlay: FFmpeg reads the V4L2 camera and hands us small BGRA frames.

use super::{Image, Slot};
use crate::config::WebcamOverlay;
use anyhow::{bail, Result};
use std::io::Read;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const W: u32 = 640;
const H: u32 = 360;

/// (device path, name) of all video capture devices.
pub fn list() -> Vec<(String, String)> {
    let mut v = vec![];
    if let Ok(rd) = std::fs::read_dir("/sys/class/video4linux") {
        for e in rd.flatten() {
            let dev = format!("/dev/{}", e.file_name().to_string_lossy());
            let name = std::fs::read_to_string(e.path().join("name")).unwrap_or_default().trim().to_string();
            // Only the capture node of each camera (index 0); metadata nodes have index > 0.
            let index = std::fs::read_to_string(e.path().join("index")).unwrap_or_default().trim().parse::<u32>().unwrap_or(0);
            if index == 0 {
                v.push((dev, name));
            }
        }
    }
    v.sort();
    v
}

pub fn spawn(cfg: WebcamOverlay, slot: Arc<Slot>, stop: Arc<AtomicBool>) -> Result<()> {
    let device = if cfg.device.is_empty() {
        match list().first() {
            Some((d, _)) => d.clone(),
            None => bail!("no webcam found"),
        }
    } else {
        cfg.device.clone()
    };
    let mut cmd = crate::encoders::ffmpeg_cmd();
    cmd.args(["-loglevel", "error"]);
    if let Some(pattern) = device.strip_prefix("test:") {
        // Development: a synthetic camera (e.g. "test:testsrc2").
        cmd.args(["-re", "-f", "lavfi", "-i", &format!("{pattern}=s={W}x{H}:r=30")]);
    } else {
        cmd.args(["-f", "v4l2", "-framerate", "30", "-i", &device]);
    }
    let mirror = if cfg.mirror { "hflip," } else { "" };
    cmd.args([
        "-vf",
        &format!("{mirror}scale={W}:{H}:force_original_aspect_ratio=increase,crop={W}:{H},format=bgra"),
        "-f",
        "rawvideo",
        "pipe:1",
    ]);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn()?;
    let mut out = child.stdout.take().unwrap();
    std::thread::Builder::new().name("overlay-webcam".into()).spawn(move || {
        let mut buf = vec![0u8; (W * H * 4) as usize];
        while !stop.load(Ordering::SeqCst) {
            if out.read_exact(&mut buf).is_err() {
                log::warn!("webcam {device} stopped delivering frames");
                break;
            }
            slot.put(Image { w: W, h: H, bgra: buf.clone() });
        }
        let _ = child.kill();
        let _ = child.wait();
        slot.clear();
    })?;
    Ok(())
}
