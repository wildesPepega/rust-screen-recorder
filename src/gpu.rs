#![allow(unsafe_op_in_unsafe_fn)]
//! Zero-copy GPU path (VA-API, AMD/Intel).
//!
//! KWin DMA-BUF → DRM_PRIME AVFrame → `hwmap` to a VA surface → `scale_vaapi` (colour
//! conversion + scaling on the GPU) → `*_vaapi` encoder. The pixels never touch the CPU.
//! Only the compressed bitstream leaves the process (as NUT through a pipe to the FFmpeg
//! child that mixes the audio and writes the file / replay segments).

use crate::config::{Codec, RateControl, VideoParams};
use crate::ffi::*;
use anyhow::{anyhow, bail, Result};
use std::ffi::{CStr, CString};
use std::io::Write;
use std::os::raw::{c_int, c_void};
use std::ptr;

const AVERROR_EAGAIN: c_int = -(libc::EAGAIN);
const AVERROR_EOF: c_int = -0x2046_4F45;
const BUFFERSRC_FLAG_KEEP_REF: c_int = 8;
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

pub fn err_str(e: c_int) -> String {
    let mut buf = [0i8; 256];
    unsafe {
        av_strerror(e, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

fn check(r: c_int, what: &str) -> Result<c_int> {
    if r < 0 {
        Err(anyhow!("{what}: {}", err_str(r)))
    } else {
        Ok(r)
    }
}

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

/// Reference-counted hardware frame that can be shared between threads.
pub struct HwFrame(*mut AVFrame);
unsafe impl Send for HwFrame {}
unsafe impl Sync for HwFrame {}

impl HwFrame {
    pub fn as_ptr(&self) -> *mut AVFrame {
        self.0
    }
    /// New AVFrame referencing the same GPU surface.
    fn clone_raw(&self) -> *mut AVFrame {
        unsafe { av_frame_clone(self.0) }
    }
    pub fn width(&self) -> u32 {
        unsafe { (*self.0).width as u32 }
    }
    pub fn height(&self) -> u32 {
        unsafe { (*self.0).height as u32 }
    }
    /// Copy the frame to system memory (NV12) – used when a CPU encoder consumes GPU frames.
    pub fn download_nv12(&self, out: &mut Vec<u8>) -> Result<()> {
        unsafe {
            let mut sw = av_frame_alloc();
            (*sw).format = AVPixelFormat::AV_PIX_FMT_NV12 as c_int;
            let r = av_hwframe_transfer_data(sw, self.0, 0);
            if r < 0 {
                av_frame_free(&mut sw);
                bail!("GPU download failed: {}", err_str(r));
            }
            let (w, h) = ((*sw).width as usize, (*sw).height as usize);
            out.clear();
            out.reserve(w * h * 3 / 2);
            for plane in 0..2 {
                let rows = if plane == 0 { h } else { h / 2 };
                let ls = (*sw).linesize[plane] as usize;
                let base = (*sw).data[plane];
                for y in 0..rows {
                    out.extend_from_slice(std::slice::from_raw_parts(base.add(y * ls), w));
                }
            }
            av_frame_free(&mut sw);
        }
        Ok(())
    }
}

impl Drop for HwFrame {
    fn drop(&mut self) {
        unsafe { av_frame_free(&mut self.0) }
    }
}

// ------------------------------------------------------------------------------------------------
// Filter graph helper
// ------------------------------------------------------------------------------------------------

struct Graph {
    graph: *mut AVFilterGraph,
    srcs: Vec<*mut AVFilterContext>,
    sink: *mut AVFilterContext,
}

/// One input of a filter graph.
struct GraphInput {
    label: &'static str,
    w: c_int,
    h: c_int,
    fmt: AVPixelFormat,
    hw_frames: *mut AVBufferRef,
}

impl Graph {
    unsafe fn new(desc: &str, w: c_int, h: c_int, fmt: AVPixelFormat, tb: AVRational, hw_frames: *mut AVBufferRef) -> Result<Graph> {
        Graph::new_multi(desc, tb, &[GraphInput { label: "in", w, h, fmt, hw_frames }])
    }

    unsafe fn new_multi(desc: &str, tb: AVRational, ins: &[GraphInput]) -> Result<Graph> {
        let graph = avfilter_graph_alloc();
        if graph.is_null() {
            bail!("avfilter_graph_alloc failed");
        }
        let mut g = Graph { graph, srcs: vec![], sink: ptr::null_mut() };
        for i in ins {
            let src = avfilter_graph_alloc_filter(graph, avfilter_get_by_name(cstr("buffer").as_ptr()), cstr(i.label).as_ptr());
            if src.is_null() {
                bail!("cannot create buffer source");
            }
            let par = av_buffersrc_parameters_alloc();
            (*par).format = i.fmt as c_int;
            (*par).width = i.w;
            (*par).height = i.h;
            (*par).time_base = tb;
            (*par).sample_aspect_ratio = AVRational { num: 1, den: 1 };
            (*par).hw_frames_ctx = av_buffer_ref(i.hw_frames);
            let r = av_buffersrc_parameters_set(src, par);
            av_buffer_unref(&mut (*par).hw_frames_ctx);
            av_free(par as *mut c_void);
            check(r, "buffersrc parameters")?;
            check(avfilter_init_str(src, ptr::null()), "buffersrc init")?;
            g.srcs.push(src);
        }

        check(
            avfilter_graph_create_filter(&mut g.sink, avfilter_get_by_name(cstr("buffersink").as_ptr()), cstr("out").as_ptr(), ptr::null(), ptr::null_mut(), graph),
            "buffersink",
        )?;

        // Chain the named sources as graph outputs.
        let mut outputs: *mut AVFilterInOut = ptr::null_mut();
        for (idx, i) in ins.iter().enumerate().rev() {
            let o = avfilter_inout_alloc();
            (*o).name = av_strdup(cstr(i.label).as_ptr());
            (*o).filter_ctx = g.srcs[idx];
            (*o).pad_idx = 0;
            (*o).next = outputs;
            outputs = o;
        }
        let mut inputs = avfilter_inout_alloc();
        (*inputs).name = av_strdup(cstr("out").as_ptr());
        (*inputs).filter_ctx = g.sink;
        (*inputs).pad_idx = 0;
        (*inputs).next = ptr::null_mut();
        let d = cstr(desc);
        let r = avfilter_graph_parse_ptr(graph, d.as_ptr(), &mut inputs, &mut outputs, ptr::null_mut());
        avfilter_inout_free(&mut inputs);
        avfilter_inout_free(&mut outputs);
        check(r, &format!("parse filter '{desc}'"))?;
        check(avfilter_graph_config(graph, ptr::null_mut()), &format!("configure filter '{desc}'"))?;
        Ok(g)
    }

    unsafe fn push_to(&mut self, idx: usize, f: *mut AVFrame) -> Result<()> {
        check(av_buffersrc_add_frame_flags(self.srcs[idx], f, BUFFERSRC_FLAG_KEEP_REF), "filter push").map(|_| ())
    }

    unsafe fn push(&mut self, f: *mut AVFrame) -> Result<()> {
        self.push_to(0, f)
    }

    unsafe fn pull(&mut self) -> Result<Option<*mut AVFrame>> {
        let f = av_frame_alloc();
        let r = av_buffersink_get_frame(self.sink, f);
        if r == AVERROR_EAGAIN || r == AVERROR_EOF {
            let mut f = f;
            av_frame_free(&mut f);
            return Ok(None);
        }
        if r < 0 {
            let mut f = f;
            av_frame_free(&mut f);
            bail!("filter pull: {}", err_str(r));
        }
        Ok(Some(f))
    }

    unsafe fn out_frames_ctx(&self) -> *mut AVBufferRef {
        av_buffersink_get_hw_frames_ctx(self.sink)
    }
}

impl Drop for Graph {
    fn drop(&mut self) {
        unsafe { avfilter_graph_free(&mut self.graph) }
    }
}

// ------------------------------------------------------------------------------------------------
// DMA-BUF importer (runs on the PipeWire thread)
// ------------------------------------------------------------------------------------------------

pub struct DmaBufPlane {
    pub fd: i32,
    pub offset: u32,
    pub stride: u32,
    pub size: usize,
}

pub struct Importer {
    drm_dev: *mut AVBufferRef,
    va_dev: *mut AVBufferRef,
    frames: *mut AVBufferRef,
    va_frames: *mut AVBufferRef,
    graph: Option<Graph>,
    key: (u32, u32, u32, u64),
}

unsafe impl Send for Importer {}

/// Fourcc + matching FFmpeg software format for a SPA video format name.
pub fn drm_format(spa: crate::capture::PixFmt) -> (u32, AVPixelFormat) {
    use crate::capture::PixFmt::*;
    let fourcc = |s: &[u8; 4]| u32::from_le_bytes(*s);
    match spa {
        Bgrx => (fourcc(b"XR24"), AVPixelFormat::AV_PIX_FMT_BGR0),
        Bgra => (fourcc(b"AR24"), AVPixelFormat::AV_PIX_FMT_BGRA),
        Rgbx => (fourcc(b"XB24"), AVPixelFormat::AV_PIX_FMT_RGB0),
        Rgba => (fourcc(b"AB24"), AVPixelFormat::AV_PIX_FMT_RGBA),
    }
}

/// One VA-API device for the whole process: every capture maps its DMA-BUFs onto it, so all GPU
/// surfaces live on one VADisplay and encoders can switch between captures (desktop ↔ game).
fn shared_devices() -> Result<(*mut AVBufferRef, *mut AVBufferRef)> {
    static DEVS: std::sync::OnceLock<(usize, usize)> = std::sync::OnceLock::new();
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = LOCK.lock().unwrap();
    if let Some((va, drm)) = DEVS.get() {
        return Ok((*va as *mut AVBufferRef, *drm as *mut AVBufferRef));
    }
    unsafe {
        let node = cstr(&render_node());
        let mut va = ptr::null_mut();
        check(av_hwdevice_ctx_create(&mut va, AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, node.as_ptr(), ptr::null_mut(), 0), "open VA-API device")?;
        let mut drm = ptr::null_mut();
        check(av_hwdevice_ctx_create(&mut drm, AVHWDeviceType::AV_HWDEVICE_TYPE_DRM, node.as_ptr(), ptr::null_mut(), 0), "open DRM device")?;
        let _ = DEVS.set((va as usize, drm as usize));
        Ok((va, drm))
    }
}

impl Importer {
    pub fn new() -> Result<Importer> {
        let (va, drm) = shared_devices()?;
        unsafe {
            Ok(Importer {
                drm_dev: av_buffer_ref(drm),
                va_dev: av_buffer_ref(va),
                frames: ptr::null_mut(),
                va_frames: ptr::null_mut(),
                graph: None,
                key: (0, 0, 0, 0),
            })
        }
    }

    unsafe fn setup(&mut self, w: u32, h: u32, fourcc: u32, sw: AVPixelFormat, modifier: u64) -> Result<()> {
        self.graph = None;
        av_buffer_unref(&mut self.frames);
        let fr = av_hwframe_ctx_alloc(self.drm_dev);
        let fc = (*fr).data as *mut AVHWFramesContext;
        (*fc).format = AVPixelFormat::AV_PIX_FMT_DRM_PRIME;
        (*fc).sw_format = sw;
        (*fc).width = w as c_int;
        (*fc).height = h as c_int;
        let mut fr2 = fr;
        if let Err(e) = check(av_hwframe_ctx_init(fr), "DRM frames ctx") {
            av_buffer_unref(&mut fr2);
            return Err(e);
        }
        self.frames = fr;
        // VA-API frames context on the shared device that the DMA-BUFs get mapped into.
        av_buffer_unref(&mut self.va_frames);
        let vf = av_hwframe_ctx_alloc(self.va_dev);
        let vfc = (*vf).data as *mut AVHWFramesContext;
        (*vfc).format = AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*vfc).sw_format = sw;
        (*vfc).width = w as c_int;
        (*vfc).height = h as c_int;
        (*vfc).initial_pool_size = 0;
        let mut vf2 = vf;
        if let Err(e) = check(av_hwframe_ctx_init(vf), "VA-API frames ctx") {
            av_buffer_unref(&mut vf2);
            return Err(e);
        }
        self.va_frames = vf;
        // Copy into our own NV12 surfaces so KWin can reuse its buffer immediately.
        let g = Graph::new(
            "scale_vaapi=format=nv12:extra_hw_frames=24",
            w as c_int,
            h as c_int,
            AVPixelFormat::AV_PIX_FMT_VAAPI,
            AVRational { num: 1, den: 1_000_000 },
            self.va_frames,
        )?;
        self.graph = Some(g);
        self.key = (w, h, fourcc, modifier);
        log::info!("GPU import ready: {w}x{h} fourcc {fourcc:#x} modifier {modifier:#x}");
        Ok(())
    }

    /// Import one DMA-BUF frame. Returns a GPU frame that no longer references the DMA-BUF.
    pub fn import(&mut self, w: u32, h: u32, fmt: crate::capture::PixFmt, modifier: u64, planes: &[DmaBufPlane]) -> Result<HwFrame> {
        let (fourcc, sw) = drm_format(fmt);
        unsafe {
            if self.key != (w, h, fourcc, modifier) || self.graph.is_none() {
                self.setup(w, h, fourcc, sw, modifier)?;
            }
            let desc: *mut AVDRMFrameDescriptor = Box::into_raw(Box::new(std::mem::zeroed::<AVDRMFrameDescriptor>()));
            let p0 = &planes[0];
            (*desc).nb_objects = 1;
            (*desc).objects[0].fd = p0.fd;
            (*desc).objects[0].size = p0.size;
            (*desc).objects[0].format_modifier = modifier;
            (*desc).nb_layers = 1;
            (*desc).layers[0].format = fourcc;
            (*desc).layers[0].nb_planes = planes.len().min(4) as c_int;
            for (i, p) in planes.iter().take(4).enumerate() {
                (*desc).layers[0].planes[i].object_index = 0;
                (*desc).layers[0].planes[i].offset = p.offset as isize;
                (*desc).layers[0].planes[i].pitch = p.stride as isize;
            }
            unsafe extern "C" fn free_desc(_: *mut c_void, data: *mut u8) {
                drop(Box::from_raw(data as *mut AVDRMFrameDescriptor));
            }
            let mut f = av_frame_alloc();
            (*f).format = AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int;
            (*f).width = w as c_int;
            (*f).height = h as c_int;
            (*f).data[0] = desc as *mut u8;
            (*f).buf[0] = av_buffer_create(desc as *mut u8, std::mem::size_of::<AVDRMFrameDescriptor>(), Some(free_desc), ptr::null_mut(), 0);
            (*f).hw_frames_ctx = av_buffer_ref(self.frames);
            (*f).pts = 0;

            // Map the DMA-BUF onto a VA surface of the shared device.
            let mut mapped = av_frame_alloc();
            (*mapped).format = AVPixelFormat::AV_PIX_FMT_VAAPI as c_int;
            (*mapped).hw_frames_ctx = av_buffer_ref(self.va_frames);
            let r = av_hwframe_map(mapped, f, AV_HWFRAME_MAP_READ as c_int);
            av_frame_free(&mut f);
            if r < 0 {
                av_frame_free(&mut mapped);
                bail!("map DMA-BUF to VA-API: {}", err_str(r));
            }
            (*mapped).pts = 0;
            let g = self.graph.as_mut().unwrap();
            let r = g.push(mapped);
            av_frame_free(&mut mapped);
            r?;
            let out = g.pull()?.ok_or_else(|| anyhow!("GPU copy produced no frame"))?;
            // Wait until the GPU finished reading the DMA-BUF before PipeWire recycles it.
            sync_surface(out);
            Ok(HwFrame(out))
        }
    }
}

impl Drop for Importer {
    fn drop(&mut self) {
        self.graph = None;
        unsafe {
            av_buffer_unref(&mut self.va_frames);
            av_buffer_unref(&mut self.frames);
            av_buffer_unref(&mut self.drm_dev);
            av_buffer_unref(&mut self.va_dev);
        }
    }
}

unsafe fn sync_surface(f: *mut AVFrame) {
    if (*f).hw_frames_ctx.is_null() {
        return;
    }
    let fc = (*(*f).hw_frames_ctx).data as *mut AVHWFramesContext;
    let dev = (*fc).device_ctx;
    let va = (*dev).hwctx as *mut AVVAAPIDeviceContext;
    let surface = (*f).data[3] as usize as VASurfaceID;
    vaSyncSurface((*va).display, surface);
}

/// A black VA-API surface (letterbox background) on the same device as `like`.
unsafe fn black_surface(like: *mut AVBufferRef, w: u32, h: u32, ten_bit: bool) -> Result<*mut AVFrame> {
    let src_fc = (*like).data as *mut AVHWFramesContext;
    let fr = av_hwframe_ctx_alloc((*src_fc).device_ref);
    let fc = (*fr).data as *mut AVHWFramesContext;
    let sw_fmt = if ten_bit { AVPixelFormat::AV_PIX_FMT_P010LE } else { AVPixelFormat::AV_PIX_FMT_NV12 };
    (*fc).format = AVPixelFormat::AV_PIX_FMT_VAAPI;
    (*fc).sw_format = sw_fmt;
    (*fc).width = w as c_int;
    (*fc).height = h as c_int;
    let mut fr = fr;
    if let Err(e) = check(av_hwframe_ctx_init(fr), "background frames ctx") {
        av_buffer_unref(&mut fr);
        return Err(e);
    }
    let mut hw = av_frame_alloc();
    let r = av_hwframe_get_buffer(fr, hw, 0);
    av_buffer_unref(&mut fr);
    if r < 0 {
        av_frame_free(&mut hw);
        bail!("background surface: {}", err_str(r));
    }
    let mut sw = av_frame_alloc();
    (*sw).format = sw_fmt as c_int;
    (*sw).width = w as c_int;
    (*sw).height = h as c_int;
    check(av_frame_get_buffer(sw, 0), "background buffer")?;
    // Limited-range black: Y = 16, U = V = 128 (scaled to 16 bit for P010).
    for plane in 0..2 {
        let rows = if plane == 0 { h as usize } else { h as usize / 2 };
        let ls = (*sw).linesize[plane] as usize;
        let base = (*sw).data[plane];
        for y in 0..rows {
            let row = std::slice::from_raw_parts_mut(base.add(y * ls), ls);
            if ten_bit {
                let v: u16 = if plane == 0 { 64 << 6 } else { 512 << 6 };
                for px in row.chunks_exact_mut(2) {
                    px.copy_from_slice(&v.to_le_bytes());
                }
            } else {
                row.fill(if plane == 0 { 16 } else { 128 });
            }
        }
    }
    let r = av_hwframe_transfer_data(hw, sw, 0);
    av_frame_free(&mut sw);
    if r < 0 {
        av_frame_free(&mut hw);
        bail!("background upload: {}", err_str(r));
    }
    Ok(hw)
}

pub fn render_node() -> String {
    std::env::var("RSR_RENDER_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into())
}

// ------------------------------------------------------------------------------------------------
// GPU encoder (runs on the encoder thread)
// ------------------------------------------------------------------------------------------------

pub struct GpuEncoder {
    params: VideoParams,
    out_w: u32,
    out_h: u32,
    graph: Option<Graph>,
    graph_key: (u32, u32, usize),
    /// Black background for letterboxing (None when the aspect ratio matches).
    bg: Option<HwFrame>,
    enc: *mut AVCodecContext,
    oc: *mut AVFormatContext,
    stream: *mut AVStream,
    pkt: *mut AVPacket,
    pts: i64,
    sink: Box<Box<dyn Write + Send>>,
    header_written: bool,
}

unsafe impl Send for GpuEncoder {}

unsafe extern "C" fn write_cb(opaque: *mut c_void, buf: *const u8, size: c_int) -> c_int {
    let w = &mut *(opaque as *mut Box<dyn Write + Send>);
    match w.write_all(std::slice::from_raw_parts(buf, size as usize)) {
        Ok(()) => size,
        Err(_) => -(libc::EPIPE),
    }
}

pub fn vaapi_encoder_name(c: Codec) -> &'static str {
    match c {
        Codec::H264 => "h264_vaapi",
        Codec::Hevc => "hevc_vaapi",
        Codec::Av1 => "av1_vaapi",
        Codec::Vp9 => "vp9_vaapi",
    }
}

impl GpuEncoder {
    pub fn new(params: &VideoParams, out_w: u32, out_h: u32, sink: Box<dyn Write + Send>) -> GpuEncoder {
        GpuEncoder {
            params: params.clone(),
            out_w,
            out_h,
            graph: None,
            graph_key: (0, 0, 0),
            bg: None,
            enc: ptr::null_mut(),
            oc: ptr::null_mut(),
            stream: ptr::null_mut(),
            pkt: unsafe { av_packet_alloc() },
            pts: 0,
            sink: Box::new(sink),
            header_written: false,
        }
    }

    unsafe fn open_encoder(&mut self, frames: *mut AVBufferRef) -> Result<()> {
        let p = &self.params;
        let name = vaapi_encoder_name(p.codec);
        let codec = avcodec_find_encoder_by_name(cstr(name).as_ptr());
        if codec.is_null() {
            bail!("{name} is not available in this FFmpeg build");
        }
        let enc = avcodec_alloc_context3(codec);
        self.enc = enc;
        let fps = p.fps.max(1) as c_int;
        (*enc).width = self.out_w as c_int;
        (*enc).height = self.out_h as c_int;
        (*enc).time_base = AVRational { num: 1, den: fps };
        (*enc).framerate = AVRational { num: fps, den: 1 };
        (*enc).pix_fmt = AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*enc).hw_frames_ctx = av_buffer_ref(frames);
        (*enc).gop_size = ((fps as f32) * p.keyframe_interval_secs.max(0.25)).round().max(1.0) as c_int;
        (*enc).max_b_frames = 0;
        let br = p.bitrate_kbps.max(100) as i64 * 1000;
        let set = |k: &str, v: &str| {
            av_opt_set((*enc).priv_data, cstr(k).as_ptr(), cstr(v).as_ptr(), 0);
        };
        match p.rate_control {
            RateControl::Cbr => {
                set("rc_mode", "CBR");
                (*enc).bit_rate = br;
                (*enc).rc_max_rate = br;
                (*enc).rc_buffer_size = (br * 2) as c_int;
            }
            RateControl::Vbr => {
                set("rc_mode", "VBR");
                (*enc).bit_rate = br;
                (*enc).rc_max_rate = br * 3 / 2;
                (*enc).rc_buffer_size = (br * 2) as c_int;
            }
            RateControl::Quality => {
                set("rc_mode", "CQP");
                let q = p.quality.min(51);
                let q = if p.codec == Codec::Av1 { (q * 5).min(255) } else { q };
                set("qp", &q.to_string());
            }
        }
        if p.ten_bit && p.codec == Codec::Hevc {
            av_opt_set(enc as *mut c_void, cstr("profile").as_ptr(), cstr("main10").as_ptr(), 1);
        }

        // NUT muxer writing into the pipe to the FFmpeg child.
        let mut oc = ptr::null_mut();
        check(avformat_alloc_output_context2(&mut oc, ptr::null(), cstr("nut").as_ptr(), ptr::null()), "nut muxer")?;
        self.oc = oc;
        if (*(*oc).oformat).flags & AVFMT_GLOBALHEADER as c_int != 0 {
            (*enc).flags |= AV_CODEC_FLAG_GLOBAL_HEADER as c_int;
        }
        check(avcodec_open2(enc, codec, ptr::null_mut()), &format!("open {name}"))?;

        let bufsize = 1 << 16;
        let buf = av_malloc(bufsize) as *mut u8;
        let opaque = &mut *self.sink as *mut Box<dyn Write + Send> as *mut c_void;
        let pb = avio_alloc_context(buf, bufsize as c_int, 1, opaque, None, Some(write_cb), None);
        (*oc).pb = pb;
        (*oc).flags |= AVFMT_FLAG_CUSTOM_IO as c_int;
        let st = avformat_new_stream(oc, ptr::null());
        check(avcodec_parameters_from_context((*st).codecpar, enc), "codec parameters")?;
        (*st).time_base = (*enc).time_base;
        self.stream = st;
        check(avformat_write_header(oc, ptr::null_mut()), "write header")?;
        self.header_written = true;
        log::info!("GPU encoder {name} {}x{} @ {fps} fps (zero-copy)", self.out_w, self.out_h);
        Ok(())
    }

    unsafe fn drain(&mut self) -> Result<()> {
        loop {
            let r = avcodec_receive_packet(self.enc, self.pkt);
            if r == AVERROR_EAGAIN || r == AVERROR_EOF {
                return Ok(());
            }
            check(r, "encode")?;
            av_packet_rescale_ts(self.pkt, (*self.enc).time_base, (*self.stream).time_base);
            (*self.pkt).stream_index = 0;
            check(av_interleaved_write_frame(self.oc, self.pkt), "write packet")?;
        }
    }

    /// Encode one (possibly repeated) captured frame.
    pub fn encode(&mut self, frame: &HwFrame) -> Result<()> {
        unsafe {
            let (w, h) = (frame.width(), frame.height());
            let fctx = (*(*frame.as_ptr()).hw_frames_ctx).data as usize;
            if self.graph.is_none() || self.graph_key != (w, h, fctx) {
                let fmt = if self.params.ten_bit && self.params.codec != Codec::H264 { "p010" } else { "nv12" };
                let (ow, oh) = (self.out_w, self.out_h);
                // Keep the aspect ratio of whatever is captured right now and letterbox it into the
                // fixed output size (the source can change size, e.g. desktop ↔ game window).
                // The bars come from a real black surface (overlay_vaapi) – VPP fill colours are
                // ignored by some drivers (Mesa/radeonsi gives green bars with pad_vaapi).
                let fps = self.params.fps.max(1) as c_int;
                let tb = AVRational { num: 1, den: fps };
                let ten = fmt == "p010";
                let in_frames = (*frame.as_ptr()).hw_frames_ctx;
                self.graph = None;
                self.bg = None;
                if (w as u64) * (oh as u64) == (h as u64) * (ow as u64) {
                    let desc = format!("scale_vaapi=w={ow}:h={oh}:format={fmt}:extra_hw_frames=8");
                    let sw_fmt = AVPixelFormat::AV_PIX_FMT_VAAPI;
                    self.graph = Some(Graph::new(&desc, w as c_int, h as c_int, sw_fmt, tb, in_frames)?);
                } else {
                    let bg = HwFrame(black_surface(in_frames, ow, oh, ten)?);
                    let desc = format!(
                        "[in]scale_vaapi=w={ow}:h={oh}:force_original_aspect_ratio=decrease:force_divisible_by=2:format={fmt}[fg];\
                         [bg][fg]overlay_vaapi=x=(W-w)/2:y=(H-h)/2:extra_hw_frames=8"
                    );
                    let ins = [
                        GraphInput { label: "in", w: w as c_int, h: h as c_int, fmt: AVPixelFormat::AV_PIX_FMT_VAAPI, hw_frames: in_frames },
                        GraphInput {
                            label: "bg",
                            w: ow as c_int,
                            h: oh as c_int,
                            fmt: AVPixelFormat::AV_PIX_FMT_VAAPI,
                            hw_frames: (*bg.as_ptr()).hw_frames_ctx,
                        },
                    ];
                    self.graph = Some(Graph::new_multi(&desc, tb, &ins)?);
                    self.bg = Some(bg);
                }
                self.graph_key = (w, h, fctx);
                log::debug!("GPU encoder input {w}x{h} → {ow}x{oh}");
            }
            let mut f = frame.clone_raw();
            (*f).pts = self.pts;
            self.pts += 1;
            if let Some(bg) = &self.bg {
                let mut b = bg.clone_raw();
                (*b).pts = (*f).pts;
                let r = self.graph.as_mut().unwrap().push_to(1, b);
                av_frame_free(&mut b);
                if let Err(e) = r {
                    av_frame_free(&mut f);
                    return Err(e);
                }
            }
            let g = self.graph.as_mut().unwrap();
            let r = g.push(f);
            av_frame_free(&mut f);
            r?;
            while let Some(mut out) = self.graph.as_mut().unwrap().pull()? {
                if self.enc.is_null() {
                    let fc = self.graph.as_ref().unwrap().out_frames_ctx();
                    if let Err(e) = self.open_encoder(fc) {
                        av_frame_free(&mut out);
                        return Err(e);
                    }
                }
                let r = avcodec_send_frame(self.enc, out);
                av_frame_free(&mut out);
                check(r, "send frame")?;
                self.drain()?;
            }
        }
        Ok(())
    }

    pub fn finish(&mut self) {
        unsafe {
            if !self.enc.is_null() {
                avcodec_send_frame(self.enc, ptr::null());
                let _ = self.drain();
            }
            if self.header_written {
                av_write_trailer(self.oc);
                avio_flush((*self.oc).pb);
            }
        }
    }
}

impl Drop for GpuEncoder {
    fn drop(&mut self) {
        self.graph = None;
        self.bg = None;
        unsafe {
            if !self.oc.is_null() {
                if !(*self.oc).pb.is_null() {
                    av_freep(&mut (*(*self.oc).pb).buffer as *mut *mut u8 as *mut c_void);
                    avio_context_free(&mut (*self.oc).pb);
                }
                avformat_free_context(self.oc);
            }
            avcodec_free_context(&mut self.enc);
            av_packet_free(&mut self.pkt);
        }
    }
}
