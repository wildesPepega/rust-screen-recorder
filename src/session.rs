//! Encoder sessions: one per consumer (recording / instant replay).
//!
//! * GPU path (VA-API + DMA-BUF capture): frames are scaled and encoded on the GPU inside this
//!   process; only the compressed bitstream goes to the FFmpeg child (audio + muxing).
//! * CPU path: raw frames are piped to FFmpeg, which uploads/encodes them.

use crate::capture::{Frame, FrameData, Source};
use crate::config::{AudioSettings, Backend, Codec, Container, ReplayStorage, VideoParams};
use crate::encoders::{self, RawInput};
use anyhow::{anyhow, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub enum Output {
    File(PathBuf),
    Segments { dir: PathBuf, seg_secs: f32 },
}

pub struct EncoderSession {
    child: Child,
    stop: Arc<AtomicBool>,
    writer: Option<std::thread::JoinHandle<()>>,
    pub started: Instant,
    pub codec: Codec,
    pub container: Container,
    pub gpu: bool,
}

pub fn log_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(|| PathBuf::from("/tmp")).join("rust-screen-recorder")
}

/// Segment container used for the replay ring buffer.
fn segment_format(codec: Codec, audio: &AudioSettings, container: Container) -> (&'static str, &'static str) {
    let flac = encoders::effective_audio_codec(audio.codec, container) == crate::config::AudioCodec::Flac;
    match codec {
        Codec::H264 | Codec::Hevc if !flac => ("mpegts", "ts"),
        _ => ("matroska", "mkv"),
    }
}

impl EncoderSession {
    /// `size_hint`: the size the output is planned for (e.g. the monitor in Auto mode, so a later
    /// switch to the desktop keeps full resolution). Defaults to the first frame's size.
    pub fn spawn(
        name: &str,
        src: Arc<Source>,
        size_hint: Option<(u32, u32)>,
        video: &VideoParams,
        audio: &AudioSettings,
        available: &[(Codec, Backend)],
        out: Output,
    ) -> Result<EncoderSession> {
        let first = src
            .get()
            .wait_first(Duration::from_secs(4))
            .ok_or_else(|| anyhow!("no video frames received from KWin (is the window minimized?)"))?;
        let backend = encoders::resolve_backend(video.codec, video.backend, available)
            .ok_or_else(|| anyhow!("no working encoder for {:?} on this system", video.codec))?;
        let container = encoders::effective_container(video.codec, video.container);
        let gpu = first.is_gpu() && backend == Backend::Vaapi;
        let fps = video.fps.max(1).to_string();
        let (ain, n_audio) = encoders::audio_input_args(audio);

        let mut cmd = encoders::ffmpeg_cmd();
        cmd.args(["-loglevel", "warning", "-y"]);
        let (base_w, base_h) = size_hint.filter(|(w, h)| *w > 0 && *h > 0).unwrap_or((first.width, first.height));
        let input = RawInput {
            pix_fmt: if first.is_gpu() { "nv12" } else { first.format.ffmpeg() },
            width: base_w & !1,
            height: base_h & !1,
        };
        if gpu {
            // Already-encoded video arrives as NUT; FFmpeg only adds audio and muxes.
            cmd.args(["-f", "nut", "-thread_queue_size", "512", "-i", "pipe:0"]);
            cmd.args(ain);
            cmd.args(["-map", "0:v", "-c:v", "copy"]);
        } else {
            cmd.args(encoders::hw_init_args(backend));
            let size = format!("{}x{}", input.width, input.height);
            cmd.args([
                "-f", "rawvideo", "-pix_fmt", input.pix_fmt, "-video_size", &size, "-framerate", &fps,
                "-use_wallclock_as_timestamps", "1", "-thread_queue_size", "64", "-i", "pipe:0",
            ]);
            cmd.args(ain);
            cmd.args(encoders::video_args(video, backend, input)?);
        }
        cmd.args(encoders::audio_output_args(audio, n_audio, container));

        match &out {
            Output::File(path) => {
                if let Some(d) = path.parent() {
                    std::fs::create_dir_all(d).with_context(|| format!("create {}", d.display()))?;
                }
                cmd.args(encoders::muxer_args(container, video.codec, true));
                cmd.arg(path);
            }
            Output::Segments { dir, seg_secs } => {
                std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
                let (fmt, ext) = segment_format(video.codec, audio, container);
                cmd.args([
                    "-f", "segment", "-segment_time", &format!("{seg_secs}"), "-segment_format", fmt,
                    "-reset_timestamps", "0",
                ]);
                cmd.arg(dir.join(format!("seg_%08d.{ext}")));
            }
        }

        let ld = log_dir();
        let _ = std::fs::create_dir_all(&ld);
        let logf = std::fs::File::create(ld.join(format!("ffmpeg-{name}.log")))?;
        log::info!("starting {name} encoder: {:?}", cmd);
        cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(logf);
        let mut child = cmd.spawn().context("failed to start ffmpeg (is it installed?)")?;
        let stdin = child.stdin.take().unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let st2 = stop.clone();
        let fps_n = video.fps.max(1);
        let writer = if gpu {
            let (ow, oh) = encoders::output_size(base_w, base_h, video.width, video.height);
            let enc = crate::gpu::GpuEncoder::new(video, ow, oh, Box::new(stdin));
            std::thread::Builder::new().name(format!("gpu-enc-{name}")).spawn(move || gpu_loop(enc, src, fps_n, st2))?
        } else {
            std::thread::Builder::new()
                .name(format!("writer-{name}"))
                .spawn(move || writer_loop(stdin, src, input, fps_n, st2))?
        };

        Ok(EncoderSession {
            child,
            stop,
            writer: Some(writer),
            started: Instant::now(),
            codec: video.codec,
            container,
            gpu,
        })
    }

    /// Returns false if FFmpeg exited.
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Gracefully finish the file.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGINT);
        }
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
    }
}

/// Fixed-rate pacing: always emit `fps` frames per second, repeating the latest frame.
/// Fixed-rate pacing: always emit `fps` frames per second, repeating the latest frame. When the
/// source is closed or being swapped, the last frame is repeated until new frames arrive.
fn pace(fps: u32, stop: &AtomicBool, src: &Source, mut f: impl FnMut(&Arc<Frame>) -> bool) {
    let interval = Duration::from_secs_f64(1.0 / fps as f64);
    let mut next = Instant::now();
    let mut last: Option<Arc<Frame>> = None;
    while !stop.load(Ordering::SeqCst) {
        if let Some(fr) = src.get().latest() {
            last = Some(fr);
        }
        if let Some(frame) = &last {
            if !f(frame) {
                break;
            }
        }
        next += interval;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else if now - next > interval * 4 {
            next = now; // fell behind – don't burst
        }
    }
}

fn gpu_loop(mut enc: crate::gpu::GpuEncoder, src: Arc<Source>, fps: u32, stop: Arc<AtomicBool>) {
    let mut errors = 0;
    pace(fps, &stop, &src, |frame| {
        let FrameData::Gpu(hw) = &frame.data else { return true };
        let r = enc.encode(hw);
        match r {
            Ok(()) => {
                errors = 0;
                true
            }
            Err(e) => {
                errors += 1;
                log::error!("GPU encode: {e:#}");
                errors < 10
            }
        }
    });
    enc.finish();
}

fn writer_loop(mut stdin: ChildStdin, src: Arc<Source>, input: RawInput, fps: u32, stop: Arc<AtomicBool>) {
    let mut scratch: Vec<u8> = Vec::new();
    let mut last_nv12: Vec<u8> = Vec::new();
    pace(fps, &stop, &src, |frame| {
        let buf: &[u8] = match &frame.data {
            FrameData::Cpu(data) => {
                if frame.width == input.width && frame.height == input.height {
                    data
                } else {
                    // The window was resized: letterbox into the fixed encoder size.
                    fit_into(frame, data, input.width, input.height, &mut scratch);
                    &scratch
                }
            }
            FrameData::Gpu(hw) => {
                // A CPU encoder consuming GPU frames: download (NV12), letterbox if needed.
                if let Err(e) = hw.download_nv12(&mut scratch) {
                    log::error!("{e:#}");
                } else if hw.width() == input.width && hw.height() == input.height {
                    std::mem::swap(&mut scratch, &mut last_nv12);
                } else {
                    fit_nv12(&scratch, hw.width(), hw.height(), input.width, input.height, &mut last_nv12);
                }
                if last_nv12.is_empty() {
                    return true;
                }
                &last_nv12
            }
        };
        stdin.write_all(buf).is_ok()
    });
    drop(stdin);
}

/// Nearest-neighbour letterbox for NV12 frames.
fn fit_nv12(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32, out: &mut Vec<u8>) {
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
    out.clear();
    out.resize(dw * dh, 16);
    out.resize(dw * dh * 3 / 2, 128);
    if sw < 2 || sh < 2 || src.len() < sw * sh * 3 / 2 {
        return;
    }
    let s = f64::min(dw as f64 / sw as f64, dh as f64 / sh as f64);
    let tw = (((sw as f64 * s) as usize).max(2).min(dw)) & !1;
    let th = (((sh as f64 * s) as usize).max(2).min(dh)) & !1;
    let (ox, oy) = (((dw - tw) / 2) & !1, ((dh - th) / 2) & !1);
    for y in 0..th {
        let sy = y * sh / th;
        for x in 0..tw {
            out[(oy + y) * dw + ox + x] = src[sy * sw + x * sw / tw];
        }
    }
    let (suv, duv) = (&src[sw * sh..], dw * dh);
    for y in 0..th / 2 {
        let sy = (y * sh / th).min(sh / 2 - 1);
        for x in 0..tw / 2 {
            let sx = (x * sw / tw).min(sw / 2 - 1);
            let d = duv + (oy / 2 + y) * dw + (ox / 2 + x) * 2;
            out[d] = suv[sy * sw + sx * 2];
            out[d + 1] = suv[sy * sw + sx * 2 + 1];
        }
    }
}

/// Nearest-neighbour scale with black bars (only used when a window changes size mid-session).
fn fit_into(f: &Frame, data: &[u8], dw: u32, dh: u32, out: &mut Vec<u8>) {
    let (dw, dh) = (dw as usize, dh as usize);
    out.clear();
    out.resize(dw * dh * 4, 0);
    let (sw, sh) = (f.width as usize, f.height as usize);
    let s = f64::min(dw as f64 / sw as f64, dh as f64 / sh as f64);
    let (tw, th) = (((sw as f64 * s) as usize).max(1).min(dw), ((sh as f64 * s) as usize).max(1).min(dh));
    let (ox, oy) = ((dw - tw) / 2, (dh - th) / 2);
    for y in 0..th {
        let sy = (y * sh / th).min(sh - 1);
        let srow = &data[sy * sw * 4..(sy + 1) * sw * 4];
        let drow = &mut out[((oy + y) * dw + ox) * 4..((oy + y) * dw + ox + tw) * 4];
        for x in 0..tw {
            let sx = (x * sw / tw).min(sw - 1);
            drow[x * 4..x * 4 + 4].copy_from_slice(&srow[sx * 4..sx * 4 + 4]);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Instant replay
// ---------------------------------------------------------------------------------------------

pub const SEGMENT_SECS: f32 = 2.0;

pub struct ReplaySession {
    pub enc: EncoderSession,
    pub dir: PathBuf,
    pub duration_secs: u32,
    track_titles: Vec<String>,
}

pub fn replay_buffer_dir(storage: ReplayStorage, disk_dir: &str) -> PathBuf {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S%3f").to_string();
    match storage {
        ReplayStorage::Ram => {
            // /dev/shm is a tmpfs: the ring buffer lives entirely in RAM.
            let uid = unsafe { libc::getuid() };
            PathBuf::from(format!("/dev/shm/rust-screen-recorder-{uid}")).join(stamp)
        }
        ReplayStorage::Disk => crate::config::expand_path(disk_dir).join(stamp),
    }
}

impl ReplaySession {
    pub fn start(
        src: Arc<Source>,
        size_hint: Option<(u32, u32)>,
        video: &VideoParams,
        audio: &AudioSettings,
        available: &[(Codec, Backend)],
        dir: PathBuf,
        duration_secs: u32,
    ) -> Result<ReplaySession> {
        let mut v = video.clone();
        // Segments must start on keyframes.
        v.keyframe_interval_secs = v.keyframe_interval_secs.min(SEGMENT_SECS);
        let enc = EncoderSession::spawn("replay", src, size_hint, &v, audio, available, Output::Segments { dir: dir.clone(), seg_secs: SEGMENT_SECS })?;
        let track_titles = if audio.merge && audio.devices.len() > 1 {
            vec!["Mixed audio".into()]
        } else {
            audio.devices.iter().map(|d| d.label.clone()).collect()
        };
        Ok(ReplaySession { enc, dir, duration_secs, track_titles })
    }

    fn segments(&self) -> Vec<PathBuf> {
        list_segments(&self.dir)
    }

    fn keep_count(&self) -> usize {
        (self.duration_secs as f32 / SEGMENT_SECS).ceil() as usize + 1
    }

    /// Delete segments that are older than the replay window.
    pub fn cleanup(&self) {
        let segs = self.segments();
        let keep = self.keep_count() + 1;
        if segs.len() > keep {
            for p in &segs[..segs.len() - keep] {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    /// Snapshot the current ring buffer into a job that can run in the background.
    pub fn snapshot(&self, out: PathBuf) -> Result<SaveJob> {
        let segs = self.segments();
        if segs.is_empty() {
            return Err(anyhow!("the replay buffer is still empty"));
        }
        // Pick just enough segments: the newest one is still being written; its length is the
        // time since the previous segment was finished. All others are SEGMENT_SECS long.
        let want = self.duration_secs as f32;
        let now = std::time::SystemTime::now();
        let partial = if segs.len() >= 2 {
            std::fs::metadata(&segs[segs.len() - 2])
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .map(|d| d.as_secs_f32())
                .unwrap_or(SEGMENT_SECS)
        } else {
            self.enc.started.elapsed().as_secs_f32()
        };
        let mut have = partial;
        let mut n = 1;
        while have + 0.05 < want && n < segs.len() {
            have += SEGMENT_SECS;
            n += 1;
        }
        let chosen = segs[segs.len() - n..].to_vec();
        // Hard-link (or copy) the segments so the ring buffer can keep rotating while we mux.
        let tmp = self.dir.join(format!("save-{}", chrono::Local::now().format("%H%M%S%3f")));
        std::fs::create_dir_all(&tmp)?;
        let mut files = vec![];
        for p in chosen {
            let dst = tmp.join(p.file_name().unwrap());
            if std::fs::hard_link(&p, &dst).is_err() {
                std::fs::copy(&p, &dst)?;
            }
            files.push(dst);
        }
        Ok(SaveJob {
            files,
            tmp,
            out,
            codec: self.enc.codec,
            container: self.enc.container,
            track_titles: self.track_titles.clone(),
        })
    }

    pub fn stop(self) {
        self.enc.stop();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub fn list_segments(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("seg_")))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

pub struct SaveJob {
    files: Vec<PathBuf>,
    tmp: PathBuf,
    pub out: PathBuf,
    codec: Codec,
    container: Container,
    track_titles: Vec<String>,
}

impl SaveJob {
    /// Concatenate the segments without re-encoding (blocking, a fraction of a second).
    pub fn run(self) -> Result<PathBuf> {
        let r = self.run_inner();
        let _ = std::fs::remove_dir_all(&self.tmp);
        r
    }
    fn run_inner(&self) -> Result<PathBuf> {
        if let Some(d) = self.out.parent() {
            std::fs::create_dir_all(d)?;
        }
        let list = self.tmp.join("list.txt");
        let mut s = String::new();
        for f in &self.files {
            s.push_str(&format!("file '{}'\n", f.display().to_string().replace('\'', "'\\''")));
        }
        std::fs::write(&list, s)?;
        let mut cmd = encoders::ffmpeg_cmd();
        cmd.args(["-loglevel", "error", "-y", "-f", "concat", "-safe", "0", "-i"]);
        cmd.arg(&list);
        cmd.args(["-map", "0", "-c", "copy", "-avoid_negative_ts", "make_zero"]);
        cmd.args(encoders::muxer_args(self.container, self.codec, false));
        for (i, t) in self.track_titles.iter().enumerate() {
            cmd.arg(format!("-metadata:s:a:{i}")).arg(format!("title={t}"));
            cmd.arg(format!("-metadata:s:a:{i}")).arg(format!("handler_name={t}"));
        }
        cmd.arg(&self.out);
        let o = cmd.stdin(Stdio::null()).output()?;
        if !o.status.success() {
            return Err(anyhow!("ffmpeg concat failed: {}", String::from_utf8_lossy(&o.stderr).trim()));
        }
        Ok(self.out.clone())
    }
}

/// Duration of a media file in seconds (via ffprobe).
pub fn probe_duration(p: &Path) -> Option<f64> {
    let o = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "default=nw=1:nk=1"])
        .arg(p)
        .output()
        .ok()?;
    String::from_utf8_lossy(&o.stdout).trim().parse().ok()
}
