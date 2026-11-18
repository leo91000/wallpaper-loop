use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    os::{
        fd::{BorrowedFd, RawFd},
        unix::ffi::OsStrExt,
    },
    path::Path,
    ptr::{self, NonNull},
    rc::Rc,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use ffmpeg_sys_next as ff;
use libloading::Library;

use super::color::Color;

const NO_PTS: i64 = i64::MIN;
const DRM_MODIFIER_TILING: ff::VkImageTiling = 1_000_158_000;
const R8: u32 = u32::from_le_bytes(*b"R8  ");
const RG88: u32 = u32::from_le_bytes(*b"RG88");
const GR88: u32 = u32::from_le_bytes(*b"GR88");

fn check(code: c_int) -> Result<()> {
    if code >= 0 {
        return Ok(());
    }
    let mut message = [0 as c_char; 128];
    // av_strerror always terminates this nonempty buffer.
    unsafe { ff::av_strerror(code, message.as_mut_ptr(), message.len()) };
    let message = unsafe { CStr::from_ptr(message.as_ptr()) };
    bail!("{}", message.to_string_lossy());
}

#[repr(C)]
struct SemaphoreWait {
    kind: i32,
    next: *const c_void,
    flags: u32,
    count: u32,
    semaphores: *const ff::VkSemaphore,
    values: *const u64,
}

type WaitSemaphores = unsafe extern "system" fn(ff::VkDevice, *const SemaphoreWait, u64) -> i32;

pub struct Device {
    reference: NonNull<ff::AVBufferRef>,
    wait_semaphores: WaitSemaphores,
    _loader: Library,
}

impl Device {
    pub fn open(node: &Path) -> Result<Rc<Self>> {
        let node = CString::new(node.as_os_str().as_bytes())?;
        let loader = unsafe { Library::new("libvulkan.so.1") }.context("load Vulkan")?;
        let wait_semaphores = unsafe { *loader.get(b"vkWaitSemaphores")? };
        let mut drm = ptr::null_mut();
        check(unsafe {
            ff::av_hwdevice_ctx_create(
                &mut drm,
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
                node.as_ptr(),
                ptr::null_mut(),
                0,
            )
        })
        .context("open FFmpeg DRM device")?;
        let mut native = ptr::null_mut();
        // Derive from the GBM render node so the decoder and EGL use the same GPU.
        let result = unsafe {
            let result = ff::av_hwdevice_ctx_create_derived(
                &mut native,
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN,
                drm,
                0,
            );
            ff::av_buffer_unref(&mut drm);
            result
        };
        check(result).context("derive Vulkan device from the DRM render node")?;
        Ok(Rc::new(Self {
            reference: NonNull::new(native).context("FFmpeg returned no Vulkan device")?,
            wait_semaphores,
            _loader: loader,
        }))
    }

    fn wait(&self, frame: &ff::AVVkFrame) -> Result<()> {
        let count = frame.img.iter().take_while(|&&image| image != 0).count();
        ensure!(count > 0, "Vulkan frame contains no images");
        // The decoder is driven serially. No other call on this stream can mutate
        // these timeline points while the exported image is being sampled.
        let wait = SemaphoreWait {
            kind: 1_000_207_004,
            next: ptr::null(),
            flags: 0,
            count: count as u32,
            semaphores: frame.sem.as_ptr(),
            values: frame.sem_value.as_ptr(),
        };
        let native = unsafe {
            let device = &*(*self.reference.as_ptr())
                .data
                .cast::<ff::AVHWDeviceContext>();
            &*device.hwctx.cast::<ff::AVVulkanDeviceContext>()
        };
        // Wait even if libavutil attached a fence to a freshly exported fd.
        // Cached EGL imports can refer to an earlier fd for the same image.
        let status = unsafe { (self.wait_semaphores)(native.act_dev, &wait, 500_000_000) };
        ensure!(status == 0, "Vulkan decode fence wait returned {status}");
        Ok(())
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe { ff::av_buffer_unref(&mut self.reference.as_ptr()) };
    }
}

struct Frame(NonNull<ff::AVFrame>);

impl Frame {
    fn new() -> Result<Self> {
        Ok(Self(
            NonNull::new(unsafe { ff::av_frame_alloc() }).context("allocate frame")?,
        ))
    }

    fn raw(&self) -> &ff::AVFrame {
        unsafe { self.0.as_ref() }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { ff::av_frame_free(&mut self.0.as_ptr()) };
    }
}

#[repr(C)]
struct ModifierList {
    kind: i32,
    next: *const c_void,
    count: u32,
    modifiers: *const u64,
}

struct PoolOptions {
    list: ModifierList,
    _modifiers: Vec<u64>,
    previous_free: Option<unsafe extern "C" fn(*mut ff::AVHWFramesContext)>,
    previous_opaque: *mut c_void,
}

unsafe extern "C" fn free_pool(context: *mut ff::AVHWFramesContext) {
    // The pool owns this record for as long as Vulkan can consult its pNext chain.
    unsafe {
        let options = Box::from_raw((*context).user_opaque.cast::<PoolOptions>());
        (*context).user_opaque = options.previous_opaque;
        (*context).free = options.previous_free;
        if let Some(free) = options.previous_free {
            free(context);
        }
    }
}

struct DecodeOptions {
    modifiers: Vec<u64>,
    generation: u64,
}

unsafe extern "C" fn format(
    codec: *mut ff::AVCodecContext,
    mut offered: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    // FFmpeg provides a terminated list and a valid codec context.
    unsafe {
        while *offered != ff::AVPixelFormat::AV_PIX_FMT_NONE
            && *offered != ff::AVPixelFormat::AV_PIX_FMT_VULKAN
        {
            offered = offered.add(1);
        }
        if *offered == ff::AVPixelFormat::AV_PIX_FMT_NONE {
            return ff::AVPixelFormat::AV_PIX_FMT_NONE;
        }
        let mut pool = ptr::null_mut();
        if ff::avcodec_get_hw_frames_parameters(
            codec,
            (*codec).hw_device_ctx,
            ff::AVPixelFormat::AV_PIX_FMT_VULKAN,
            &mut pool,
        ) < 0
        {
            return ff::AVPixelFormat::AV_PIX_FMT_NONE;
        }
        let context = &mut *(*pool).data.cast::<ff::AVHWFramesContext>();
        if context.sw_format != ff::AVPixelFormat::AV_PIX_FMT_NV12 {
            ff::av_buffer_unref(&mut pool);
            return ff::AVPixelFormat::AV_PIX_FMT_NONE;
        }
        let options = &mut *(*codec).opaque.cast::<DecodeOptions>();
        let native = &mut *context.hwctx.cast::<ff::AVVulkanFramesContext>();
        let modifiers = options.modifiers.clone();
        let mut layout = Box::new(PoolOptions {
            list: ModifierList {
                kind: 1_000_158_003,
                next: native.create_pnext,
                count: modifiers.len() as u32,
                modifiers: modifiers.as_ptr(),
            },
            _modifiers: modifiers,
            previous_free: context.free,
            previous_opaque: context.user_opaque,
        });
        native.tiling = DRM_MODIFIER_TILING;
        native.create_pnext = ptr::from_mut(&mut layout.list).cast();
        context.user_opaque = Box::into_raw(layout).cast();
        context.free = Some(free_pool);
        if ff::av_hwframe_ctx_init(pool) < 0 {
            ff::av_buffer_unref(&mut pool);
            return ff::AVPixelFormat::AV_PIX_FMT_NONE;
        }
        ff::av_buffer_unref(&mut (*codec).hw_frames_ctx);
        (*codec).hw_frames_ctx = pool;
        options.generation = options.generation.wrapping_add(1);
    }
    ff::AVPixelFormat::AV_PIX_FMT_VULKAN
}

pub struct Decoder {
    input: *mut ff::AVFormatContext,
    codec: *mut ff::AVCodecContext,
    packet: *mut ff::AVPacket,
    frame: Frame,
    device: Rc<Device>,
    options: Box<DecodeOptions>,
    video: c_int,
    time_base: ff::AVRational,
    seek: i64,
    fallback_duration: Duration,
    draining: bool,
    saw_frame: bool,
    pub loops: u64,
}

#[derive(Clone, Copy)]
pub struct FrameInfo {
    pub width: i32,
    pub height: i32,
    pub aspect: f32,
    pub color: Color,
    pub timestamp: Option<i64>,
    pub duration: Duration,
    pub loop_count: u64,
}

impl Decoder {
    pub fn open(device: Rc<Device>, path: &Path, modifiers: &[u64]) -> Result<Self> {
        let path = CString::new(path.as_os_str().as_bytes())?;
        let mut decoder = Self {
            input: ptr::null_mut(),
            codec: ptr::null_mut(),
            packet: ptr::null_mut(),
            frame: Frame::new()?,
            device,
            options: Box::new(DecodeOptions {
                modifiers: modifiers.to_vec(),
                generation: 0,
            }),
            video: -1,
            time_base: ff::AVRational { num: 0, den: 1 },
            seek: 0,
            fallback_duration: Duration::from_secs_f64(1.0 / 30.0),
            draining: false,
            saw_frame: false,
            loops: 0,
        };
        // Each allocation is immediately owned by Decoder, including on error.
        unsafe {
            check(ff::avformat_open_input(
                &mut decoder.input,
                path.as_ptr(),
                ptr::null(),
                ptr::null_mut(),
            ))
            .context("open video")?;
            check(ff::avformat_find_stream_info(
                decoder.input,
                ptr::null_mut(),
            ))
            .context("read video stream information")?;
            let mut implementation = ptr::null();
            decoder.video = ff::av_find_best_stream(
                decoder.input,
                ff::AVMediaType::AVMEDIA_TYPE_VIDEO,
                -1,
                -1,
                &mut implementation,
                0,
            );
            check(decoder.video).context("select video stream")?;
            let stream = *(*decoder.input).streams.add(decoder.video as usize);
            ensure!(
                (*(*stream).codecpar).codec_id == ff::AVCodecID::AV_CODEC_ID_H264,
                "Vulkan currently supports 8-bit H.264 video; use --backend mpv for other codecs"
            );
            decoder.time_base = (*stream).time_base;
            decoder.seek = if (*stream).start_time == NO_PTS {
                0
            } else {
                (*stream).start_time
            };
            let rate = ff::av_guess_frame_rate(decoder.input, stream, ptr::null_mut());
            if let Some(duration) = frame_duration(
                1,
                ff::AVRational {
                    num: rate.den,
                    den: rate.num,
                },
            ) {
                decoder.fallback_duration = duration;
            }
            decoder.codec = ff::avcodec_alloc_context3(implementation);
            decoder.packet = ff::av_packet_alloc();
            ensure!(
                !decoder.codec.is_null() && !decoder.packet.is_null(),
                "allocate video decoder"
            );
            let codec = &mut *decoder.codec;
            check(ff::avcodec_parameters_to_context(codec, (*stream).codecpar))
                .context("configure H.264 decoder")?;
            codec.hw_device_ctx = ff::av_buffer_ref(decoder.device.reference.as_ptr());
            ensure!(!codec.hw_device_ctx.is_null(), "retain Vulkan device");
            codec.opaque = ptr::from_mut(decoder.options.as_mut()).cast();
            codec.get_format = Some(format);
            codec.thread_count = 1;
            codec.pkt_timebase = decoder.time_base;
            check(ff::avcodec_open2(codec, implementation, ptr::null_mut()))
                .context("open Vulkan Video decoder")?;
        }
        Ok(decoder)
    }

    pub fn next(&mut self) -> Result<FrameInfo> {
        unsafe { ff::av_frame_unref(self.frame.0.as_ptr()) };
        loop {
            let code = unsafe { ff::avcodec_receive_frame(self.codec, self.frame.0.as_ptr()) };
            if code == 0 {
                self.saw_frame = true;
                break;
            }
            if code == ff::AVERROR_EOF && self.draining {
                ensure!(self.saw_frame, "video contains no decodable frames");
                unsafe {
                    check(ff::av_seek_frame(
                        self.input,
                        self.video,
                        self.seek,
                        ff::AVSEEK_FLAG_BACKWARD,
                    ))
                    .context("seek to start of video")?;
                    ff::avcodec_flush_buffers(self.codec);
                }
                self.draining = false;
                self.saw_frame = false;
                self.loops += 1;
                continue;
            }
            if code != -libc::EAGAIN {
                check(code).context("decode with Vulkan Video (8-bit NV12 export required)")?;
            }
            ensure!(
                !self.draining,
                "decoder requested a packet after end of stream"
            );
            self.feed()?;
        }

        let frame = self.frame.raw();
        ensure!(
            frame.format == ff::AVPixelFormat::AV_PIX_FMT_VULKAN as i32
                && frame.width > 0
                && frame.height > 0,
            "unexpected Vulkan video frame"
        );
        ensure!(
            frame.width % 2 == 0 && frame.height % 2 == 0,
            "NV12 requires even frame dimensions"
        );
        let ratio = frame.sample_aspect_ratio;
        let pixel_aspect = if ratio.num > 0 && ratio.den > 0 {
            ratio.num as f32 / ratio.den as f32
        } else {
            1.0
        };
        Ok(FrameInfo {
            width: frame.width,
            height: frame.height,
            aspect: frame.width as f32 * pixel_aspect / frame.height as f32,
            color: Color::from_frame(frame)?,
            timestamp: units_to_ns(frame.best_effort_timestamp, self.time_base),
            duration: frame_duration(frame.duration, self.time_base)
                .unwrap_or(self.fallback_duration),
            loop_count: self.loops,
        })
    }

    fn feed(&mut self) -> Result<()> {
        loop {
            unsafe {
                let code = ff::av_read_frame(self.input, self.packet);
                if code == ff::AVERROR_EOF {
                    self.draining = true;
                    return check(ff::avcodec_send_packet(self.codec, ptr::null()))
                        .context("drain video");
                }
                check(code).context("read video packet")?;
                if (*self.packet).stream_index == self.video {
                    let result = ff::avcodec_send_packet(self.codec, self.packet);
                    ff::av_packet_unref(self.packet);
                    return check(result).context("submit video packet");
                }
                ff::av_packet_unref(self.packet);
            }
        }
    }

    pub fn mapped(&self) -> Result<Mapped> {
        let frame = Frame::new()?;
        unsafe {
            (*frame.0.as_ptr()).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            check(ff::av_hwframe_map(
                frame.0.as_ptr(),
                self.frame.0.as_ptr(),
                ff::AV_HWFRAME_MAP_READ as i32 | ff::AV_HWFRAME_MAP_DIRECT as i32,
            ))
            .context("export Vulkan frame as DRM DMA-BUF")?;
        }
        let native = unsafe { &*self.frame.raw().data[0].cast::<ff::AVVkFrame>() };
        self.device.wait(native)?;
        Ok(Mapped {
            frame,
            generation: self.options.generation,
            image: native.img[0],
            // The mapped AVFrame also owns a reference to the decoded AVFrame.
            _device: Rc::clone(&self.device),
        })
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            ff::av_frame_unref(self.frame.0.as_ptr());
            ff::av_packet_free(&mut self.packet);
            ff::avcodec_free_context(&mut self.codec);
            ff::avformat_close_input(&mut self.input);
        }
    }
}

fn units_to_ns(value: i64, base: ff::AVRational) -> Option<i64> {
    if value == NO_PTS || base.num <= 0 || base.den <= 0 {
        return None;
    }
    i64::try_from(i128::from(value) * i128::from(base.num) * 1_000_000_000 / i128::from(base.den))
        .ok()
}

fn frame_duration(value: i64, base: ff::AVRational) -> Option<Duration> {
    units_to_ns(value, base)
        .filter(|&value| value > 0)
        .map(|value| Duration::from_nanos(value as u64))
}

pub struct Mapped {
    frame: Frame,
    pub generation: u64,
    pub image: u64,
    _device: Rc<Device>,
}

#[derive(Clone, Copy)]
pub struct Plane<'a> {
    pub fd: BorrowedFd<'a>,
    pub offset: isize,
    pub pitch: isize,
    pub modifier: u64,
    pub format: u32,
}

impl Mapped {
    pub fn planes(&self) -> Result<[Plane<'_>; 2]> {
        let raw = self.frame.raw().data[0].cast::<ff::AVDRMFrameDescriptor>();
        ensure!(!raw.is_null(), "missing DMA-BUF descriptor");
        let descriptor = unsafe { &*raw };
        ensure!(
            descriptor.nb_layers == 2 && descriptor.nb_objects == 1,
            "expected one DMA-BUF with two NV12 layers"
        );
        let build = |index: usize, format: u32| {
            let layer = &descriptor.layers[index];
            ensure!(
                layer.nb_planes == 1 && layer.planes[0].object_index == 0,
                "unexpected NV12 plane layout"
            );
            let object = &descriptor.objects[0];
            let plane = &layer.planes[0];
            ensure!(
                object.fd >= 0 && plane.offset >= 0 && plane.pitch > 0,
                "invalid DMA-BUF descriptor"
            );
            // The mapped frame owns the exported fd until it is unmapped.
            let fd = unsafe { BorrowedFd::borrow_raw(object.fd as RawFd) };
            Ok(Plane {
                fd,
                offset: plane.offset,
                pitch: plane.pitch,
                modifier: object.format_modifier,
                format,
            })
        };
        ensure!(
            descriptor.layers[0].format == R8 && matches!(descriptor.layers[1].format, RG88 | GR88),
            "unsupported NV12 plane formats"
        );
        // NVIDIA exposes the two byte lanes of this RG88 EGL view as R=U, G=V.
        // The asymmetric color-chart test verifies this import on the target.
        Ok([build(0, R8)?, build(1, RG88)?])
    }
}
