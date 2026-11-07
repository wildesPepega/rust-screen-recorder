//! PipeWire consumer for the node KWin created.
//!
//! GPU mode: KWin hands us DMA-BUFs which are imported into VA-API and copied into our own
//! GPU surfaces (zero-copy, no pixel ever touches the CPU). If that is not possible we fall back
//! to shared-memory frames that are copied into system memory.

use crate::gpu::{DmaBufPlane, HwFrame, Importer, DRM_FORMAT_MOD_LINEAR};
use anyhow::{anyhow, Result};
use parking_lot::{Condvar, Mutex};
use pipewire as pw;
use pw::spa;
use spa::pod::{Pod, Property, PropertyFlags, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixFmt {
    Bgrx,
    Bgra,
    Rgbx,
    Rgba,
}

impl PixFmt {
    pub fn ffmpeg(self) -> &'static str {
        match self {
            PixFmt::Bgrx => "bgr0",
            PixFmt::Bgra => "bgra",
            PixFmt::Rgbx => "rgb0",
            PixFmt::Rgba => "rgba",
        }
    }
}

pub enum FrameData {
    /// width * height * 4 bytes, tightly packed.
    Cpu(Vec<u8>),
    /// NV12 VA-API surface.
    Gpu(HwFrame),
}

pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub format: PixFmt,
    pub data: FrameData,
}

impl Frame {
    pub fn is_gpu(&self) -> bool {
        matches!(self.data, FrameData::Gpu(_))
    }
}

#[derive(Default)]
pub struct FrameSlot {
    latest: Mutex<Option<Arc<Frame>>>,
    cond: Condvar,
    pub closed: AtomicBool,
    pub frames: AtomicU64,
    pub gpu: AtomicBool,
}

impl FrameSlot {
    pub fn latest(&self) -> Option<Arc<Frame>> {
        self.latest.lock().clone()
    }
    fn put(&self, f: Frame) {
        *self.latest.lock() = Some(Arc::new(f));
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.cond.notify_all();
    }
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.cond.notify_all();
    }
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
    pub fn wait_first(&self, timeout: Duration) -> Option<Arc<Frame>> {
        let mut g = self.latest.lock();
        if g.is_none() && !self.is_closed() {
            self.cond.wait_for(&mut g, timeout);
        }
        g.clone()
    }
}

/// The frame source an encoder reads from. The daemon can swap the underlying capture at any
/// time (desktop ↔ game window) – the encoder, its buffer and the audio just keep running.
pub struct Source {
    slot: Mutex<Arc<FrameSlot>>,
}

impl Source {
    pub fn new(slot: Arc<FrameSlot>) -> Arc<Source> {
        Arc::new(Source { slot: Mutex::new(slot) })
    }
    pub fn get(&self) -> Arc<FrameSlot> {
        self.slot.lock().clone()
    }
    pub fn set(&self, slot: Arc<FrameSlot>) {
        *self.slot.lock() = slot;
    }
}

pub struct Capture {
    pub slot: Arc<FrameSlot>,
    quit: pw::channel::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    /// `gpu`: try zero-copy DMA-BUF capture (falls back to shared memory automatically).
    pub fn start(node: u32, gpu: bool) -> Result<Capture> {
        let slot = Arc::new(FrameSlot::default());
        let (quit_tx, quit_rx) = pw::channel::channel::<()>();
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<()>>(1);
        let s2 = slot.clone();
        let thread = std::thread::Builder::new().name(format!("pw-capture-{node}")).spawn(move || {
            let r = run(node, gpu, s2.clone(), quit_rx, &ready_tx);
            if let Err(e) = r {
                log::error!("PipeWire capture of node {node} failed: {e:#}");
                let _ = ready_tx.send(Err(e));
            }
            s2.close();
        })?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Capture { slot, quit: quit_tx, thread: Some(thread) }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(anyhow!("PipeWire capture did not start")),
        }
    }

    pub fn stop(mut self) {
        self.stop_inner();
    }
    fn stop_inner(&mut self) {
        let _ = self.quit.send(());
        self.slot.close();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

struct UserData {
    format: spa::param::video::VideoInfoRaw,
    slot: Arc<FrameSlot>,
    importer: Option<Importer>,
    dmabuf: bool,
    import_errors: u32,
}

fn serialize(v: Value) -> Result<Vec<u8>> {
    Ok(spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &v)
        .map_err(|e| anyhow!("pod serialize: {e:?}"))?
        .0
        .into_inner())
}

/// EnumFormat; with `modifier` = DMA-BUF (LINEAR, importable by every VA-API driver).
fn enum_format(modifier: bool) -> Result<Vec<u8>> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    let mut obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
        ),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle { width: 1920, height: 1080 },
            spa::utils::Rectangle { width: 1, height: 1 },
            spa::utils::Rectangle { width: 16384, height: 16384 }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 60, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 1000, denom: 1 }
        ),
    );
    if modifier {
        obj.properties.push(Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY | PropertyFlags::from_bits_retain(spa::sys::SPA_POD_PROP_FLAG_DONT_FIXATE),
            value: Value::Choice(spa::pod::ChoiceValue::Long(spa::utils::Choice(
                spa::utils::ChoiceFlags::empty(),
                spa::utils::ChoiceEnum::Enum {
                    default: DRM_FORMAT_MOD_LINEAR as i64,
                    alternatives: vec![DRM_FORMAT_MOD_LINEAR as i64],
                },
            ))),
        });
    }
    serialize(Value::Object(obj))
}

fn buffers_param(dmabuf: bool) -> Result<Vec<u8>> {
    let types: i32 = if dmabuf {
        1 << spa::sys::SPA_DATA_DmaBuf
    } else {
        (1 << spa::sys::SPA_DATA_MemFd) | (1 << spa::sys::SPA_DATA_MemPtr)
    };
    let obj = spa::pod::Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: spa::sys::SPA_PARAM_Buffers,
        properties: vec![Property {
            key: spa::sys::SPA_PARAM_BUFFERS_dataType,
            flags: PropertyFlags::empty(),
            value: Value::Int(types),
        }],
    };
    serialize(Value::Object(obj))
}

fn run(node: u32, gpu: bool, slot: Arc<FrameSlot>, quit_rx: pw::channel::Receiver<()>, ready: &crossbeam_channel::Sender<Result<()>>) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let ml = mainloop.clone();
    let _quit = quit_rx.attach(mainloop.loop_(), move |_| ml.quit());

    let stream = pw::stream::StreamBox::new(
        &core,
        "rust-screen-recorder",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let importer = if gpu {
        match Importer::new() {
            Ok(i) => Some(i),
            Err(e) => {
                log::warn!("zero-copy capture unavailable ({e:#}), using shared memory");
                None
            }
        }
    } else {
        None
    };
    let use_dmabuf = importer.is_some();

    let ml2 = mainloop.clone();
    let data = UserData { format: Default::default(), slot: slot.clone(), importer, dmabuf: false, import_errors: 0 };
    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(move |_, ud, old, new| {
            log::debug!("pipewire stream: {old:?} -> {new:?}");
            if matches!(new, pw::stream::StreamState::Error(_) | pw::stream::StreamState::Unconnected) {
                ud.slot.close();
                ml2.quit();
            }
        })
        .param_changed(|stream, ud, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            if let Ok((mt, ms)) = spa::param::format_utils::parse_format(param) {
                if mt != spa::param::format::MediaType::Video || ms != spa::param::format::MediaSubtype::Raw {
                    return;
                }
            }
            if ud.format.parse(param).is_err() {
                return;
            }
            let has_mod = ud.format.flags().bits() & spa::sys::SPA_VIDEO_FLAG_MODIFIER != 0;
            ud.dmabuf = has_mod && ud.importer.is_some();
            ud.slot.gpu.store(ud.dmabuf, Ordering::SeqCst);
            log::info!(
                "capture format {:?} {}x{} – {}",
                ud.format.format(),
                ud.format.size().width,
                ud.format.size().height,
                if ud.dmabuf { format!("DMA-BUF (modifier {:#x}), zero-copy GPU path", ud.format.modifier()) } else { "shared memory".into() }
            );
            if let Ok(b) = buffers_param(ud.dmabuf) {
                if let Some(p) = Pod::from_bytes(&b) {
                    let _ = stream.update_params(&mut [p]);
                }
            }
        })
        .process(|stream, ud| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let fmt = match ud.format.format() {
                spa::param::video::VideoFormat::BGRx => PixFmt::Bgrx,
                spa::param::video::VideoFormat::BGRA => PixFmt::Bgra,
                spa::param::video::VideoFormat::RGBx => PixFmt::Rgbx,
                spa::param::video::VideoFormat::RGBA => PixFmt::Rgba,
                _ => return,
            };
            let w = ud.format.size().width;
            let h = ud.format.size().height;
            let datas = buffer.datas_mut();
            if datas.is_empty() || w == 0 || h == 0 {
                return;
            }

            if datas[0].type_() == spa::buffer::DataType::DmaBuf {
                // Cursor-only updates are flagged as corrupted / empty.
                if datas[0].chunk().size() == 0 && datas[0].chunk().stride() == 0 {
                    return;
                }
                let planes: Vec<DmaBufPlane> = datas
                    .iter()
                    .map(|d| DmaBufPlane {
                        fd: d.as_raw().fd as i32,
                        offset: d.chunk().offset(),
                        stride: d.chunk().stride().max(0) as u32,
                        size: d.as_raw().maxsize as usize,
                    })
                    .collect();
                let modifier = ud.format.modifier();
                let Some(imp) = ud.importer.as_mut() else { return };
                match imp.import(w, h, fmt, modifier, &planes) {
                    Ok(hw) => {
                        ud.import_errors = 0;
                        ud.slot.put(Frame { width: w, height: h, format: fmt, data: FrameData::Gpu(hw) })
                    }
                    Err(e) => {
                        ud.import_errors += 1;
                        log::error!("GPU import failed: {e:#}");
                        if ud.import_errors >= 3 {
                            // Renegotiate to shared memory.
                            log::warn!("falling back to shared-memory capture");
                            ud.importer = None;
                            if let Ok(b) = enum_format(false) {
                                if let Some(p) = Pod::from_bytes(&b) {
                                    let _ = stream.update_params(&mut [p]);
                                }
                            }
                        }
                    }
                }
                return;
            }

            let d = &mut datas[0];
            let (size, offset, stride) = {
                let c = d.chunk();
                (c.size() as usize, c.offset() as usize, c.stride())
            };
            if size == 0 {
                return;
            }
            let (w, h) = (w as usize, h as usize);
            let stride = if stride > 0 { stride as usize } else { w * 4 };
            let Some(src) = d.data() else { return };
            let row = w * 4;
            if src.len() < offset + stride * (h - 1) + row {
                return;
            }
            let mut out = vec![0u8; row * h];
            if stride == row {
                out.copy_from_slice(&src[offset..offset + row * h]);
            } else {
                for y in 0..h {
                    let s = offset + y * stride;
                    out[y * row..(y + 1) * row].copy_from_slice(&src[s..s + row]);
                }
            }
            ud.slot.put(Frame { width: w as u32, height: h as u32, format: fmt, data: FrameData::Cpu(out) });
        })
        .register()?;

    let mut bufs = vec![];
    if use_dmabuf {
        bufs.push(enum_format(true)?);
    }
    bufs.push(enum_format(false)?);
    let mut params: Vec<&Pod> = bufs.iter().map(|b| Pod::from_bytes(b).unwrap()).collect();

    stream.connect(
        spa::utils::Direction::Input,
        Some(node),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;
    let _ = ready.send(Ok(()));
    mainloop.run();
    let _ = stream.disconnect();
    slot.close();
    Ok(())
}
