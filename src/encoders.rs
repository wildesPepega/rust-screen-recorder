//! FFmpeg encoder selection, argument building and hardware capability detection.

use crate::config::{AudioCodec, AudioSettings, Backend, Codec, Container, RateControl, Speed, VideoParams};
use std::process::{Command, Stdio};

pub fn ffmpeg_cmd() -> Command {
    let mut c = Command::new("ffmpeg");
    // Never let game overlays / layers hook into our encoder process.
    for v in ["MANGOHUD", "MANGOHUD_DLSYM", "ENABLE_VKBASALT", "OBS_VKCAPTURE", "LD_PRELOAD"] {
        c.env_remove(v);
    }
    c.arg("-hide_banner").arg("-nostdin");
    c
}

pub fn encoder_name(codec: Codec, backend: Backend) -> Option<&'static str> {
    use Backend::*;
    use Codec::*;
    Some(match (backend, codec) {
        (Vaapi, H264) => "h264_vaapi",
        (Vaapi, Hevc) => "hevc_vaapi",
        (Vaapi, Av1) => "av1_vaapi",
        (Vaapi, Vp9) => "vp9_vaapi",
        (Nvenc, H264) => "h264_nvenc",
        (Nvenc, Hevc) => "hevc_nvenc",
        (Nvenc, Av1) => "av1_nvenc",
        (Amf, H264) => "h264_amf",
        (Amf, Hevc) => "hevc_amf",
        (Amf, Av1) => "av1_amf",
        (Qsv, H264) => "h264_qsv",
        (Qsv, Hevc) => "hevc_qsv",
        (Qsv, Av1) => "av1_qsv",
        (Qsv, Vp9) => "vp9_qsv",
        (Vulkan, H264) => "h264_vulkan",
        (Vulkan, Hevc) => "hevc_vulkan",
        (Vulkan, Av1) => "av1_vulkan",
        (Cpu, H264) => "libx264",
        (Cpu, Hevc) => "libx265",
        (Cpu, Av1) => "libsvtav1",
        (Cpu, Vp9) => "libvpx-vp9",
        _ => return None,
    })
}

/// Resolve `Auto` (or an unavailable backend) to a working backend for `codec`.
pub fn resolve_backend(codec: Codec, wanted: Backend, available: &[(Codec, Backend)]) -> Option<Backend> {
    if wanted != Backend::Auto {
        if available.contains(&(codec, wanted)) || available.is_empty() {
            return Some(wanted);
        }
        log::warn!("{wanted:?} cannot encode {codec:?} on this system, falling back to auto");
    }
    Backend::AUTO_ORDER.into_iter().find(|b| available.contains(&(codec, *b)))
}

/// Compute the output size: fit the source into the requested box, keep aspect, never upscale,
/// always even dimensions.
pub fn output_size(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let (mut w, mut h) = (src_w, src_h);
    if max_w > 0 && max_h > 0 && (src_w > max_w || src_h > max_h) {
        let s = f64::min(max_w as f64 / src_w as f64, max_h as f64 / src_h as f64);
        w = (src_w as f64 * s).round() as u32;
        h = (src_h as f64 * s).round() as u32;
    }
    (w.max(2) & !1, h.max(2) & !1)
}

/// Description of the raw video arriving on stdin.
#[derive(Clone, Copy, Debug)]
pub struct RawInput {
    pub pix_fmt: &'static str,
    pub width: u32,
    pub height: u32,
}

/// Arguments that must appear before the inputs (hardware device init).
pub fn hw_init_args(backend: Backend) -> Vec<String> {
    let s: &[&str] = match backend {
        Backend::Vaapi => &["-init_hw_device", "vaapi=va:/dev/dri/renderD128", "-filter_hw_device", "va"],
        Backend::Vulkan => &["-init_hw_device", "vulkan=vk", "-filter_hw_device", "vk"],
        _ => &[],
    };
    s.iter().map(|s| s.to_string()).collect()
}

/// Video filter chain + encoder options (everything after the inputs, for stream 0:v).
pub fn video_args(p: &VideoParams, backend: Backend, input: RawInput) -> anyhow::Result<Vec<String>> {
    let enc = encoder_name(p.codec, backend)
        .ok_or_else(|| anyhow::anyhow!("{:?} is not supported by {:?}", p.codec, backend))?;
    let (ow, oh) = output_size(input.width, input.height, p.width, p.height);
    let needs_scale = ow != input.width || oh != input.height;
    let ten = p.ten_bit && p.codec != Codec::H264;
    let mut a: Vec<String> = Vec::new();
    let push = |a: &mut Vec<String>, xs: &[&str]| a.extend(xs.iter().map(|s| s.to_string()));

    let sw_fmt = if ten { "p010le" } else { "nv12" };
    let scale = if needs_scale { format!("scale={ow}:{oh}:flags=bicubic,") } else { String::new() };
    let vf = match backend {
        Backend::Vaapi => format!("hwupload,scale_vaapi=w={ow}:h={oh}:format={}", if ten { "p010" } else { "nv12" }),
        Backend::Vulkan => format!("{scale}format={sw_fmt},hwupload"),
        Backend::Nvenc => {
            if ten {
                format!("{scale}format=p010le")
            } else if needs_scale {
                format!("scale={ow}:{oh}:flags=bicubic")
            } else {
                "null".into()
            }
        }
        Backend::Amf | Backend::Qsv => format!("{scale}format={sw_fmt}"),
        Backend::Cpu => format!("{scale}format={}", if ten { "yuv420p10le" } else { "yuv420p" }),
        Backend::Auto => unreachable!(),
    };
    push(&mut a, &["-map", "0:v", "-vf", &vf, "-c:v", enc]);

    let fps = p.fps.max(1);
    let gop = ((fps as f32 * p.keyframe_interval_secs.max(0.25)).round() as u32).max(1);
    push(&mut a, &["-fps_mode", "cfr", "-r", &fps.to_string(), "-g", &gop.to_string()]);

    let br = p.bitrate_kbps.max(100);
    let b = format!("{br}k");
    let peak = format!("{}k", br * 3 / 2);
    let buf = format!("{}k", br * 2);
    let q = p.quality.min(51);
    // AV1 hardware encoders use a 0..255 qp scale.
    let hwq = if p.codec == Codec::Av1 { (q * 5).min(255) } else { q };

    match backend {
        Backend::Vaapi => {
            match p.rate_control {
                RateControl::Cbr => push(&mut a, &["-rc_mode", "CBR", "-b:v", &b, "-maxrate", &b, "-bufsize", &buf]),
                RateControl::Vbr => push(&mut a, &["-rc_mode", "VBR", "-b:v", &b, "-maxrate", &peak, "-bufsize", &buf]),
                RateControl::Quality => push(&mut a, &["-rc_mode", "CQP", "-qp", &hwq.to_string()]),
            }
            if ten && p.codec == Codec::Hevc {
                push(&mut a, &["-profile:v", "main10"]);
            }
            // Fewer B-frames = lower latency & better segment cutting.
            push(&mut a, &["-bf", "0"]);
        }
        Backend::Nvenc => {
            let preset = match p.speed {
                Speed::Fastest => "p1",
                Speed::Fast => "p3",
                Speed::Balanced => "p5",
                Speed::Quality => "p7",
            };
            push(&mut a, &["-preset", preset, "-tune", "hq"]);
            match p.rate_control {
                RateControl::Cbr => push(&mut a, &["-rc", "cbr", "-b:v", &b, "-maxrate", &b, "-bufsize", &buf]),
                RateControl::Vbr => push(&mut a, &["-rc", "vbr", "-b:v", &b, "-maxrate", &peak, "-bufsize", &buf]),
                RateControl::Quality => push(&mut a, &["-rc", "constqp", "-qp", &hwq.to_string()]),
            }
            if ten && p.codec == Codec::Hevc {
                push(&mut a, &["-profile:v", "main10"]);
            }
        }
        Backend::Amf => {
            let quality = match p.speed {
                Speed::Fastest | Speed::Fast => "speed",
                Speed::Balanced => "balanced",
                Speed::Quality => "quality",
            };
            push(&mut a, &["-usage", "transcoding", "-quality", quality]);
            match p.rate_control {
                RateControl::Cbr => push(&mut a, &["-rc", "cbr", "-b:v", &b, "-maxrate", &b, "-bufsize", &buf]),
                RateControl::Vbr => push(&mut a, &["-rc", "vbr_peak", "-b:v", &b, "-maxrate", &peak, "-bufsize", &buf]),
                RateControl::Quality => {
                    let s = hwq.to_string();
                    push(&mut a, &["-rc", "cqp", "-qp_i", &s, "-qp_p", &s]);
                }
            }
        }
        Backend::Qsv => {
            let preset = match p.speed {
                Speed::Fastest => "veryfast",
                Speed::Fast => "faster",
                Speed::Balanced => "medium",
                Speed::Quality => "veryslow",
            };
            push(&mut a, &["-preset", preset]);
            match p.rate_control {
                RateControl::Cbr => push(&mut a, &["-b:v", &b, "-maxrate", &b, "-bufsize", &buf]),
                RateControl::Vbr => push(&mut a, &["-b:v", &b, "-maxrate", &peak, "-bufsize", &buf]),
                RateControl::Quality => push(&mut a, &["-global_quality", &q.max(1).to_string()]),
            }
        }
        Backend::Vulkan => match p.rate_control {
            RateControl::Cbr => push(&mut a, &["-rc_mode", "cbr", "-b:v", &b, "-maxrate", &b, "-bufsize", &buf]),
            RateControl::Vbr => push(&mut a, &["-rc_mode", "vbr", "-b:v", &b, "-maxrate", &peak, "-bufsize", &buf]),
            RateControl::Quality => push(&mut a, &["-rc_mode", "cqp", "-qp", &hwq.to_string()]),
        },
        Backend::Cpu => {
            match p.codec {
                Codec::H264 | Codec::Hevc => {
                    let preset = match p.speed {
                        Speed::Fastest => "ultrafast",
                        Speed::Fast => "veryfast",
                        Speed::Balanced => "faster",
                        Speed::Quality => "medium",
                    };
                    push(&mut a, &["-preset", preset]);
                    match p.rate_control {
                        RateControl::Cbr => push(&mut a, &["-b:v", &b, "-minrate", &b, "-maxrate", &b, "-bufsize", &buf]),
                        RateControl::Vbr => push(&mut a, &["-b:v", &b, "-maxrate", &peak, "-bufsize", &buf]),
                        RateControl::Quality => push(&mut a, &["-crf", &q.to_string()]),
                    }
                }
                Codec::Av1 => {
                    let preset = match p.speed {
                        Speed::Fastest => "12",
                        Speed::Fast => "11",
                        Speed::Balanced => "10",
                        Speed::Quality => "8",
                    };
                    push(&mut a, &["-preset", preset]);
                    match p.rate_control {
                        RateControl::Cbr | RateControl::Vbr => push(&mut a, &["-b:v", &b]),
                        RateControl::Quality => push(&mut a, &["-crf", &(q * 63 / 51).to_string()]),
                    }
                }
                Codec::Vp9 => {
                    let cpu = match p.speed {
                        Speed::Fastest => "8",
                        Speed::Fast => "7",
                        Speed::Balanced => "6",
                        Speed::Quality => "4",
                    };
                    push(&mut a, &["-deadline", "realtime", "-cpu-used", cpu, "-row-mt", "1"]);
                    match p.rate_control {
                        RateControl::Cbr => push(&mut a, &["-b:v", &b, "-minrate", &b, "-maxrate", &b]),
                        RateControl::Vbr => push(&mut a, &["-b:v", &b, "-maxrate", &peak]),
                        RateControl::Quality => push(&mut a, &["-crf", &(q * 63 / 51).to_string(), "-b:v", "0"]),
                    }
                }
            }
            // Force keyframes so replay segments can be cut exactly.
            let kf = format!("expr:gte(t,n_forced*{})", p.keyframe_interval_secs.max(0.25));
            push(&mut a, &["-force_key_frames", &kf]);
        }
        Backend::Auto => unreachable!(),
    }
    Ok(a)
}

/// Audio input arguments (one `-f pulse -i` per device). Returns (args, number_of_inputs).
pub fn audio_input_args(a: &AudioSettings) -> (Vec<String>, usize) {
    let mut v: Vec<String> = vec![];
    for d in &a.devices {
        let name = if d.name.trim().is_empty() { "@DEFAULT_MONITOR@" } else { d.name.trim() };
        v.extend(
            ["-f", "pulse", "-thread_queue_size", "4096", "-sample_rate", "48000", "-channels", "2", "-fragment_size", "3840", "-i", name]
                .iter()
                .map(|s| s.to_string()),
        );
    }
    (v, a.devices.len())
}

/// Audio filtering/mapping/encoding (inputs 1..=n are audio, in `a.devices` order).
pub fn audio_output_args(a: &AudioSettings, n_inputs: usize, container: Container) -> Vec<String> {
    let mut v: Vec<String> = vec![];
    if n_inputs == 0 {
        return v;
    }
    let codec = effective_audio_codec(a.codec, container);
    let mut fc = String::new();
    for i in 1..=n_inputs {
        let vol = a.devices.get(i - 1).map(|d| d.volume).unwrap_or(100) as f32 / 100.0;
        fc.push_str(&format!("[{i}:a]aresample=async=1000,volume={vol:.2}[a{i}];"));
    }
    if a.merge && n_inputs > 1 {
        for i in 1..=n_inputs {
            fc.push_str(&format!("[a{i}]"));
        }
        fc.push_str(&format!("amix=inputs={n_inputs}:duration=longest:normalize=0[aout]"));
        v.extend(["-filter_complex".to_string(), fc, "-map".into(), "[aout]".into()]);
    } else {
        fc.pop();
        v.push("-filter_complex".into());
        v.push(fc);
        for i in 1..=n_inputs {
            v.push("-map".into());
            v.push(format!("[a{i}]"));
        }
        // Name the tracks after the devices (shown by most players/editors).
        for (i, d) in a.devices.iter().enumerate() {
            v.push(format!("-metadata:s:a:{i}"));
            v.push(format!("title={}", d.label));
            v.push(format!("-metadata:s:a:{i}"));
            v.push(format!("handler_name={}", d.label));
        }
    }
    match codec {
        AudioCodec::Aac => v.extend(["-c:a", "aac", "-b:a"].iter().map(|s| s.to_string())),
        AudioCodec::Opus => v.extend(["-c:a", "libopus", "-b:a"].iter().map(|s| s.to_string())),
        AudioCodec::Flac => v.extend(["-c:a", "flac"].iter().map(|s| s.to_string())),
    }
    if codec != AudioCodec::Flac {
        v.push(format!("{}k", a.bitrate_kbps.clamp(32, 512)));
    }
    v.extend(["-ar", "48000", "-ac", "2"].iter().map(|s| s.to_string()));
    v
}

/// WebM only supports Opus; FLAC in MOV is not sensible.
pub fn effective_audio_codec(c: AudioCodec, container: Container) -> AudioCodec {
    match (container, c) {
        (Container::Webm, _) => AudioCodec::Opus,
        (Container::Mov, AudioCodec::Flac) => AudioCodec::Aac,
        _ => c,
    }
}

/// WebM only takes VP9/AV1. Returns a container that works for the codec.
pub fn effective_container(codec: Codec, c: Container) -> Container {
    match (c, codec) {
        (Container::Webm, Codec::H264 | Codec::Hevc) => Container::Mkv,
        _ => c,
    }
}

/// Muxer options. `live` = written while recording (crash safe fragmented MP4),
/// otherwise a normal MP4 with the index at the front (replay clips).
pub fn muxer_args(c: Container, codec: Codec, live: bool) -> Vec<String> {
    let mut v: Vec<String> = vec![];
    if matches!(c, Container::Mp4 | Container::Mov) {
        v.push("-movflags".into());
        v.push(if live { "+frag_keyframe+empty_moov+default_base_moof".into() } else { "+faststart".into() });
        if codec == Codec::Hevc {
            v.push("-tag:v".into());
            v.push("hvc1".into());
        }
    }
    v
}

/// Probe which (codec, backend) pairs actually work by encoding a few frames.
pub fn detect_available() -> Vec<(Codec, Backend)> {
    let mut handles = vec![];
    for codec in Codec::ALL {
        for backend in Backend::AUTO_ORDER {
            if encoder_name(codec, backend).is_none() {
                continue;
            }
            handles.push(std::thread::spawn(move || (codec, backend, test_encoder(codec, backend))));
        }
    }
    let mut out: Vec<(Codec, Backend)> = handles
        .into_iter()
        .filter_map(|h| h.join().ok())
        .filter(|(_, _, ok)| *ok)
        .map(|(c, b, _)| (c, b))
        .collect();
    out.sort_by_key(|(c, b)| (*c as u8, *b as u8));
    log::info!("Available encoders: {out:?}");
    out
}

fn test_encoder(codec: Codec, backend: Backend) -> bool {
    let p = VideoParams {
        codec,
        backend,
        width: 0,
        height: 0,
        fps: 30,
        ..Default::default()
    };
    let input = RawInput { pix_fmt: "bgr0", width: 320, height: 240 };
    let Ok(vargs) = video_args(&p, backend, input) else { return false };
    let mut cmd = ffmpeg_cmd();
    cmd.args(["-loglevel", "error"]).args(hw_init_args(backend));
    cmd.args(["-f", "lavfi", "-i", "color=c=black:s=320x240:r=30,format=bgr0", "-frames:v", "5"]);
    cmd.args(vargs).args(["-f", "null", "-"]);
    cmd.stdout(Stdio::null()).stderr(Stdio::null()).stdin(Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => {
            let start = std::time::Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(st)) => return st.success(),
                    Ok(None) if start.elapsed().as_secs() < 8 => std::thread::sleep(std::time::Duration::from_millis(30)),
                    _ => {
                        let _ = child.kill();
                        return false;
                    }
                }
            }
        }
        Err(_) => false,
    }
}

pub fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}
