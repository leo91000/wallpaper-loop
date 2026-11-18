---cargo
[package]
edition = "2024"

[dependencies]
anyhow = "=1.0.104"
libc = "=0.2.191"
khronos-egl = { version = "=6.0.0", features = ["static"] }
wayland-client = { version = "=0.31.15", features = ["system"] }
wayland-protocols = { version = "=0.32.13", features = ["client", "staging"] }
wayland-protocols-wlr = { version = "=0.3.12", features = ["client"] }
---

//! A fixed-size experiment for comparing predecoded Wayland buffer submission.

use std::{
    ffi::c_void,
    fs::{File, OpenOptions},
    io::{self, Read},
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    path::Path,
    process::{Command, Stdio},
    ptr::NonNull,
    rc::Rc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use khronos_egl::{self as egl, API};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop,
    globals::registry_queue_init,
    protocol::{wl_buffer, wl_compositor, wl_output, wl_region, wl_registry, wl_surface},
};
use wayland_protocols::wp::{
    commit_timing::v1::client::{wp_commit_timer_v1, wp_commit_timing_manager_v1},
    fifo::v1::client::{wp_fifo_manager_v1, wp_fifo_v1},
    linux_dmabuf::zv1::client::{zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1},
    presentation_time::client::{wp_presentation, wp_presentation_feedback},
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

const WIDTH: i32 = 2560;
const HEIGHT: i32 = 1440;
const FPS: u64 = 30;
const XR24: u32 = u32::from_le_bytes(*b"XR24");
const ROW_BYTES: usize = WIDTH as usize * 4;

#[link(name = "gbm")]
unsafe extern "C" {
    fn gbm_create_device(fd: i32) -> *mut c_void;
    fn gbm_device_destroy(device: *mut c_void);
    fn gbm_bo_create(
        device: *mut c_void,
        width: u32,
        height: u32,
        format: u32,
        flags: u32,
    ) -> *mut c_void;
    fn gbm_bo_destroy(bo: *mut c_void);
    fn gbm_bo_get_plane_count(bo: *mut c_void) -> i32;
    fn gbm_bo_get_stride(bo: *mut c_void) -> u32;
    fn gbm_bo_get_offset(bo: *mut c_void, plane: i32) -> u32;
    fn gbm_bo_get_modifier(bo: *mut c_void) -> u64;
    fn gbm_bo_get_fd(bo: *mut c_void) -> i32;

}

struct Gbm {
    raw: NonNull<c_void>,
    _file: File,
}

impl Gbm {
    fn open(path: &Path) -> Result<Rc<Self>> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let raw = NonNull::new(unsafe { gbm_create_device(file.as_raw_fd()) })
            .context("create GBM render device")?;
        Ok(Rc::new(Self { raw, _file: file }))
    }
}

impl Drop for Gbm {
    fn drop(&mut self) {
        unsafe { gbm_device_destroy(self.raw.as_ptr()) };
    }
}

struct Backing {
    raw: NonNull<c_void>,
    _device: Rc<Gbm>,
}

impl Backing {
    fn new(
        device: &Rc<Gbm>,
        uploader: &Uploader,
        pixels: &[u8],
        supported: &[u64],
    ) -> Result<Self> {
        const USE_RENDERING: u32 = 1 << 2;
        let raw = NonNull::new(unsafe {
            gbm_bo_create(
                device.raw.as_ptr(),
                WIDTH as u32,
                HEIGHT as u32,
                XR24,
                USE_RENDERING,
            )
        })
        .context("allocate XR24 GBM buffer")?;
        let backing = Self {
            raw,
            _device: Rc::clone(device),
        };
        ensure!(
            supported.contains(&backing.modifier()),
            "compositor did not advertise the allocated XR24 modifier {:#x}",
            backing.modifier()
        );
        uploader.upload(&backing, pixels)?;
        Ok(backing)
    }

    fn modifier(&self) -> u64 {
        unsafe { gbm_bo_get_modifier(self.raw.as_ptr()) }
    }

    fn fd(&self) -> Result<OwnedFd> {
        let fd = unsafe { gbm_bo_get_fd(self.raw.as_ptr()) };
        ensure!(
            fd >= 0,
            "export GBM DMA-BUF: {}",
            io::Error::last_os_error()
        );
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn export(&self, params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1) -> Result<()> {
        let raw = self.raw.as_ptr();
        ensure!(
            unsafe { gbm_bo_get_plane_count(raw) } == 1,
            "expected one XR24 plane"
        );
        let modifier = self.modifier();
        let fd = self.fd()?;
        params.add(
            fd.as_fd(),
            0,
            unsafe { gbm_bo_get_offset(raw, 0) },
            unsafe { gbm_bo_get_stride(raw) },
            (modifier >> 32) as u32,
            modifier as u32,
        );
        Ok(())
    }
}

impl Drop for Backing {
    fn drop(&mut self) {
        unsafe { gbm_bo_destroy(self.raw.as_ptr()) };
    }
}

#[link(name = "EGL")]
unsafe extern "system" {
    fn eglCreateImage(
        display: *mut c_void,
        context: *mut c_void,
        target: egl::Enum,
        buffer: *mut c_void,
        attribs: *const egl::Attrib,
    ) -> *mut c_void;
}

#[link(name = "GL")]
unsafe extern "system" {
    fn glGenTextures(count: i32, textures: *mut u32);
    fn glBindTexture(target: u32, texture: u32);
    fn glTexSubImage2D(
        target: u32,
        level: i32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        format: u32,
        kind: u32,
        data: *const c_void,
    );
    fn glDeleteTextures(count: i32, textures: *const u32);
    fn glGetError() -> u32;
    fn glFinish();
}

struct Uploader {
    display: egl::Display,
    context: egl::Context,
    bind_image: unsafe extern "system" fn(u32, *mut c_void),
    _device: Rc<Gbm>,
}

impl Uploader {
    fn new(device: &Rc<Gbm>) -> Result<Self> {
        const PLATFORM_GBM: egl::Enum = 0x31d7;
        let display = unsafe {
            API.get_platform_display(PLATFORM_GBM, device.raw.as_ptr(), &[egl::ATTRIB_NONE])?
        };
        API.initialize(display)?;
        let setup = (|| {
            API.bind_api(egl::OPENGL_API)?;
            let config = API
                .choose_first_config(
                    display,
                    &[
                        egl::SURFACE_TYPE,
                        egl::WINDOW_BIT,
                        egl::RENDERABLE_TYPE,
                        egl::OPENGL_BIT,
                        egl::RED_SIZE,
                        8,
                        egl::GREEN_SIZE,
                        8,
                        egl::BLUE_SIZE,
                        8,
                        egl::NONE,
                    ],
                )?
                .context("no EGL/GBM OpenGL config")?;
            let proc = API
                .get_proc_address("glEGLImageTargetTexture2DOES")
                .context("OpenGL cannot bind EGL images")?;
            // EGL publishes this entry point with the signature from GL_OES_EGL_image.
            let bind_image = unsafe {
                std::mem::transmute::<
                    extern "system" fn(),
                    unsafe extern "system" fn(u32, *mut c_void),
                >(proc)
            };
            let context = API.create_context(display, config, None, &[egl::NONE])?;
            if let Err(error) = API.make_current(display, None, None, Some(context)) {
                let _ = API.destroy_context(display, context);
                return Err(error).context("bind surfaceless OpenGL context");
            }
            Ok(Self {
                display,
                context,
                bind_image,
                _device: Rc::clone(device),
            })
        })();
        if setup.is_err() {
            let _ = API.terminate(display);
        }
        setup
    }

    fn upload(&self, backing: &Backing, pixels: &[u8]) -> Result<()> {
        const LINUX_DMA_BUF: egl::Enum = 0x3270;
        const DMA_BUF_FOURCC: egl::Attrib = 0x3271;
        const DMA_BUF_FD: egl::Attrib = 0x3272;
        const DMA_BUF_OFFSET: egl::Attrib = 0x3273;
        const DMA_BUF_PITCH: egl::Attrib = 0x3274;
        const DMA_BUF_MODIFIER_LO: egl::Attrib = 0x3443;
        const DMA_BUF_MODIFIER_HI: egl::Attrib = 0x3444;
        const TEXTURE_2D: u32 = 0x0de1;
        const BGRA: u32 = 0x80e1;
        const UNSIGNED_BYTE: u32 = 0x1401;
        let fd = backing.fd()?;
        let modifier = backing.modifier();
        let attrs = [
            egl::WIDTH as egl::Attrib,
            WIDTH as egl::Attrib,
            egl::HEIGHT as egl::Attrib,
            HEIGHT as egl::Attrib,
            DMA_BUF_FOURCC,
            XR24 as egl::Attrib,
            DMA_BUF_FD,
            fd.as_raw_fd() as egl::Attrib,
            DMA_BUF_OFFSET,
            unsafe { gbm_bo_get_offset(backing.raw.as_ptr(), 0) } as egl::Attrib,
            DMA_BUF_PITCH,
            unsafe { gbm_bo_get_stride(backing.raw.as_ptr()) } as egl::Attrib,
            DMA_BUF_MODIFIER_LO,
            (modifier as u32) as egl::Attrib,
            DMA_BUF_MODIFIER_HI,
            ((modifier >> 32) as u32) as egl::Attrib,
            egl::ATTRIB_NONE,
        ];
        let image = unsafe {
            eglCreateImage(
                self.display.as_ptr(),
                std::ptr::null_mut(),
                LINUX_DMA_BUF,
                std::ptr::null_mut(),
                attrs.as_ptr(),
            )
        };
        if image.is_null() {
            bail!("import GBM storage into EGL: {:?}", API.get_error());
        }
        let image = unsafe { egl::Image::from_ptr(image) };
        let mut texture = 0;
        let error = unsafe {
            glGenTextures(1, &mut texture);
            glBindTexture(TEXTURE_2D, texture);
            (self.bind_image)(TEXTURE_2D, image.as_ptr());
            glTexSubImage2D(
                TEXTURE_2D,
                0,
                0,
                0,
                WIDTH,
                HEIGHT,
                BGRA,
                UNSIGNED_BYTE,
                pixels.as_ptr().cast(),
            );
            // The compositor sees immutable, fully uploaded storage only.
            glFinish();
            glDeleteTextures(1, &texture);
            glGetError()
        };
        API.destroy_image(self.display, image)?;
        ensure!(
            error == 0,
            "upload to EGL image failed with GL error {error:#x}"
        );
        Ok(())
    }
}

impl Drop for Uploader {
    fn drop(&mut self) {
        let _ = API.make_current(self.display, None, None, None);
        let _ = API.destroy_context(self.display, self.context);
        let _ = API.terminate(self.display);
    }
}

#[derive(Clone, Copy)]
struct Key {
    layer: usize,
    frame: usize,
}

struct Frame {
    buffer: wl_buffer::WlBuffer,
    _backing: Backing,
    busy: bool,
}

impl Drop for Frame {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}

struct Layer {
    name: String,
    surface: wl_surface::WlSurface,
    role: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    timer: Option<wp_commit_timer_v1::WpCommitTimerV1>,
    fifo: Option<wp_fifo_v1::WpFifoV1>,
    configured: Option<(u32, u32)>,
    closed: bool,
    frames: Vec<Frame>,
    presented: u64,
    discarded: u64,
    zero_copy: u64,
    busy: u64,
    first_ns: u64,
    last_ns: u64,
    max_gap_ns: u64,
}

impl Drop for Layer {
    fn drop(&mut self) {
        if let Some(timer) = &self.timer {
            timer.destroy();
        }
        if let Some(fifo) = &self.fifo {
            fifo.destroy();
        }
        self.role.destroy();
        self.surface.destroy();
    }
}

#[derive(Default)]
struct State {
    outputs: Vec<(wl_output::WlOutput, String)>,
    layers: Vec<Layer>,
    clock: Option<i32>,
    xr24_modifiers: Vec<u64>,
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        id: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.outputs[*id].1 = name;
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, usize> for State {
    fn event(
        state: &mut Self,
        role: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        id: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let layer = &mut state.layers[*id];
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                role.ack_configure(serial);
                layer.configured = Some((width, height));
            }
            zwlr_layer_surface_v1::Event::Closed => layer.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        event: zwp_linux_dmabuf_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_linux_dmabuf_v1::Event::Modifier {
            format: XR24,
            modifier_hi,
            modifier_lo,
        } = event
        {
            state
                .xr24_modifiers
                .push(u64::from(modifier_hi) << 32 | u64::from(modifier_lo));
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, Key> for State {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        key: &Key,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            state.layers[key.layer].frames[key.frame].busy = false;
        }
    }
}

impl Dispatch<wp_presentation::WpPresentation, ()> for State {
    fn event(
        state: &mut Self,
        _: &wp_presentation::WpPresentation,
        event: wp_presentation::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            state.clock = i32::try_from(clk_id).ok();
        }
    }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, usize> for State {
    fn event(
        state: &mut Self,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        id: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let layer = &mut state.layers[*id];
        match event {
            wp_presentation_feedback::Event::Presented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                flags,
                ..
            } => {
                let seconds = u64::from(tv_sec_hi) << 32 | u64::from(tv_sec_lo);
                let time = seconds * 1_000_000_000 + u64::from(tv_nsec);
                if layer.presented == 0 {
                    layer.first_ns = time;
                } else {
                    layer.max_gap_ns = layer.max_gap_ns.max(time.saturating_sub(layer.last_ns));
                }
                layer.last_ns = time;
                layer.presented += 1;
                if matches!(flags, WEnum::Value(flags) if flags.contains(wp_presentation_feedback::Kind::ZeroCopy))
                {
                    layer.zero_copy += 1;
                }
            }
            wp_presentation_feedback::Event::Discarded => layer.discarded += 1,
            _ => {}
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, wayland_client::globals::GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &wayland_client::globals::GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_region::WlRegion);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
delegate_noop!(State: ignore zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1);
delegate_noop!(State: ignore wp_fifo_manager_v1::WpFifoManagerV1);
delegate_noop!(State: ignore wp_fifo_v1::WpFifoV1);
delegate_noop!(State: ignore wp_commit_timing_manager_v1::WpCommitTimingManagerV1);
delegate_noop!(State: ignore wp_commit_timer_v1::WpCommitTimerV1);

fn cache(
    path: &str,
    count: usize,
    mut upload: impl FnMut(usize, &[u8]) -> Result<()>,
) -> Result<()> {
    let mut decoder = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-nostdin",
            "-threads",
            "2",
            "-i",
            path,
            "-an",
            "-filter_threads",
            "2",
            "-vf",
            "scale=2560:1440,fps=30",
            "-frames:v",
            &count.to_string(),
            "-pix_fmt",
            "bgr0",
            "-threads",
            "1",
            "-f",
            "rawvideo",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .context("start ffmpeg")?;
    let mut stdout = decoder.stdout.take().context("open ffmpeg pipe")?;
    let mut pixels = vec![0; ROW_BYTES * HEIGHT as usize];
    let result: Result<()> = (|| {
        for index in 0..count {
            stdout
                .read_exact(&mut pixels)
                .context("read decoded frame")?;
            upload(index, &pixels)?;
        }
        Ok(())
    })();
    drop(stdout);
    if result.is_err() {
        let _ = decoder.kill();
    }
    let status = decoder.wait()?;
    result?;
    ensure!(status.success(), "ffmpeg failed for {path}");
    Ok(())
}

fn pump(queue: &mut EventQueue<State>, state: &mut State) -> Result<()> {
    queue.dispatch_pending(state)?;
    queue.flush()?;
    if let Some(reader) = queue.prepare_read() {
        let mut fd = libc::pollfd {
            fd: reader.connection_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let available = unsafe { libc::poll(&mut fd, 1, 0) };
        if available < 0 {
            return Err(io::Error::last_os_error().into());
        }
        if available > 0 {
            reader.read()?;
        }
    }
    queue.dispatch_pending(state)?;
    Ok(())
}

fn clock_ns(id: i32) -> Result<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(id, &mut time) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(u64::try_from(time.tv_sec)? * 1_000_000_000 + u64::try_from(time.tv_nsec)?)
}

fn offset(frame: u64) -> u64 {
    frame * 1_000_000_000 / FPS
}

fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        args.len() >= 6 && args.len() % 2 == 0,
        "usage: dmabuf-replay RENDER_NODE SECONDS CACHED_FRAMES BATCH OUTPUT VIDEO [OUTPUT VIDEO ...]"
    );
    let seconds: u64 = args[1].parse()?;
    let frames: usize = args[2].parse()?;
    let batch: u64 = args[3].parse()?;
    let (streams, _) = args[4..].as_chunks::<2>();
    ensure!(
        (2..=60).contains(&seconds) && (8..=45).contains(&frames) && streams.len() <= 3,
        "use 2–60 seconds, 8–45 cached frames and at most three outputs"
    );
    ensure!(
        (1..=frames as u64 / 2).contains(&batch),
        "batch must fit within half the cache"
    );

    let device = Gbm::open(Path::new(&args[0]))?;
    let connection = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<State>(&connection)?;
    let qh = queue.handle();
    let mut state = State::default();
    for global in globals.contents().clone_list() {
        if global.interface == "wl_output" && global.version >= 4 {
            let output = globals
                .registry()
                .bind(global.name, 4, &qh, state.outputs.len());
            state.outputs.push((output, String::new()));
        }
    }
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ())?;
    let shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = globals.bind(&qh, 1..=4, ())?;
    let dmabuf: zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1 = globals.bind(&qh, 3..=3, ())?;
    let presentation: wp_presentation::WpPresentation = globals.bind(&qh, 1..=2, ())?;
    let timing: Option<wp_commit_timing_manager_v1::WpCommitTimingManagerV1> = if batch > 1 {
        Some(globals.bind(&qh, 1..=1, ())?)
    } else {
        None
    };
    let fifo_manager: Option<wp_fifo_manager_v1::WpFifoManagerV1> = if batch > 1 {
        Some(globals.bind(&qh, 1..=1, ())?)
    } else {
        None
    };
    queue.roundtrip(&mut state)?;
    let clock = state
        .clock
        .context("Wayland presentation did not report a clock")?;
    ensure!(
        !state.xr24_modifiers.is_empty(),
        "compositor did not advertise XR24 DMA-BUF"
    );
    for stream in streams {
        let name = &stream[0];
        ensure!(
            !state.layers.iter().any(|layer| &layer.name == name),
            "duplicate output {name}"
        );
        let output = state
            .outputs
            .iter()
            .find(|(_, candidate)| candidate == name)
            .with_context(|| format!("output {name} was not found"))?;
        let index = state.layers.len();
        let surface = compositor.create_surface(&qh, ());
        let input = compositor.create_region(&qh, ());
        surface.set_input_region(Some(&input));
        input.destroy();
        let opaque = compositor.create_region(&qh, ());
        opaque.add(0, 0, WIDTH, HEIGHT);
        surface.set_opaque_region(Some(&opaque));
        opaque.destroy();
        let role = shell.get_layer_surface(
            &surface,
            Some(&output.0),
            zwlr_layer_shell_v1::Layer::Background,
            "dmabuf-replay".to_owned(),
            &qh,
            index,
        );
        use zwlr_layer_surface_v1::Anchor;
        role.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        role.set_size(0, 0);
        role.set_exclusive_zone(-1);
        let timer = timing
            .as_ref()
            .map(|timing| timing.get_timer(&surface, &qh, ()));
        let fifo = fifo_manager
            .as_ref()
            .map(|manager| manager.get_fifo(&surface, &qh, ()));
        surface.commit();
        state.layers.push(Layer {
            name: name.clone(),
            surface,
            role,
            timer,
            fifo,
            configured: None,
            closed: false,
            frames: Vec::new(),
            presented: 0,
            discarded: 0,
            zero_copy: 0,
            busy: 0,
            first_ns: 0,
            last_ns: 0,
            max_gap_ns: 0,
        });
    }
    queue.roundtrip(&mut state)?;
    let uploader = Uploader::new(&device)?;
    for (index, stream) in streams.iter().enumerate() {
        let layer = &mut state.layers[index];
        ensure!(
            layer.configured == Some((WIDTH as u32, HEIGHT as u32)),
            "{} must be configured at 2560×1440 logical pixels",
            layer.name
        );
        cache(&stream[1], frames, |frame, pixels| {
            let backing = Backing::new(&device, &uploader, pixels, &state.xr24_modifiers)?;
            let params = dmabuf.create_params(&qh, ());
            backing.export(&params)?;
            let buffer = params.create_immed(
                WIDTH,
                HEIGHT,
                XR24,
                zwp_linux_buffer_params_v1::Flags::empty(),
                &qh,
                Key {
                    layer: index,
                    frame,
                },
            );
            params.destroy();
            layer.frames.push(Frame {
                buffer,
                _backing: backing,
                busy: false,
            });
            if frame % 8 == 7 {
                queue.flush()?;
            }
            Ok(())
        })?;
        queue.roundtrip(&mut state)?;
    }
    drop(uploader);
    println!(
        "ready: {} layers, {frames} frames per layer, batch={batch}",
        state.layers.len()
    );
    let first = Instant::now();
    let start_cpu = clock_ns(libc::CLOCK_PROCESS_CPUTIME_ID)?;
    let epoch = clock_ns(clock)? + 80_000_000;
    let requested = seconds * FPS;
    for base in (0..requested).step_by(batch as usize) {
        thread::sleep(
            (first + Duration::from_nanos(offset(base))).saturating_duration_since(Instant::now()),
        );
        pump(&mut queue, &mut state)?;
        for frame_number in base..(base + batch).min(requested) {
            let position = frame_number as usize % frames;
            for (index, layer) in state.layers.iter_mut().enumerate() {
                if layer.closed {
                    bail!("Wayland closed {}", layer.name);
                }
                let frame = &mut layer.frames[position];
                if frame.busy {
                    layer.busy += 1;
                    continue;
                }
                if let (Some(timer), Some(fifo)) = (&layer.timer, &layer.fifo) {
                    let target = epoch + offset(frame_number);
                    let seconds = target / 1_000_000_000;
                    timer.set_timestamp(
                        (seconds >> 32) as u32,
                        seconds as u32,
                        (target % 1_000_000_000) as u32,
                    );
                    fifo.wait_barrier();
                    fifo.set_barrier();
                }
                presentation.feedback(&layer.surface, &qh, index);
                layer.surface.attach(Some(&frame.buffer), 0, 0);
                layer.surface.damage_buffer(0, 0, WIDTH, HEIGHT);
                layer.surface.commit();
                frame.busy = true;
            }
        }
        queue.flush()?;
    }
    let end = first + Duration::from_secs(seconds) + Duration::from_millis(300);
    while Instant::now() < end {
        thread::sleep(Duration::from_millis(25));
        pump(&mut queue, &mut state)?;
    }
    let cpu_ns = clock_ns(libc::CLOCK_PROCESS_CPUTIME_ID)? - start_cpu;
    let elapsed = first.elapsed().as_secs_f64();
    println!(
        "replay: seconds={elapsed:.3} cpu_ms={:.3} cpu_percent={:.4}",
        cpu_ns as f64 / 1e6,
        cpu_ns as f64 / 1e7 / elapsed,
    );
    for layer in &state.layers {
        let span_ns = layer.last_ns.saturating_sub(layer.first_ns);
        let fps = if span_ns > 0 {
            (layer.presented - 1) as f64 * 1e9 / span_ns as f64
        } else {
            0.0
        };
        println!(
            "{}: requested={requested} presented={} discarded={} busy={} zero_copy={} fps={fps:.2} max_gap_ms={:.2}",
            layer.name,
            layer.presented,
            layer.discarded,
            layer.busy,
            layer.zero_copy,
            layer.max_gap_ns as f64 / 1e6
        );
    }
    ensure!(
        state.layers.iter().all(|layer| layer.presented > 0),
        "a layer presented zero frames"
    );
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("dmabuf-replay: {error:#}");
        std::process::exit(1);
    }
}
