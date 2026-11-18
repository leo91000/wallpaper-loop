use std::{
    ffi::{CStr, CString, c_char, c_double, c_int, c_void},
    os::unix::ffi::OsStrExt,
    path::Path,
    ptr,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
};

use anyhow::{Context, Result, bail, ensure};
use calloop::channel::Sender;
use libloading::Library;

pub const EVENTS: u8 = 1;
pub const UPDATE: u8 = 2;

type Handle = *mut c_void;
type Callback = Option<extern "C" fn(*mut c_void)>;

// Values from libmpv's public client.h and render.h.
const EVENT_NONE: c_int = 0;
const EVENT_SHUTDOWN: c_int = 1;
const EVENT_COMMAND_REPLY: c_int = 5;
const EVENT_END_FILE: c_int = 7;
const EVENT_PROPERTY_CHANGE: c_int = 22;
const END_FILE_ERROR: c_int = 4;
const FORMAT_STRING: c_int = 1;
const DECODER_OBSERVER: u64 = 1;
const LOAD_REQUEST: u64 = 2;
const UPDATE_FRAME: u64 = 1;

#[repr(i32)]
enum ParamKind {
    End = 0,
    ApiType = 1,
    GlInit = 2,
    Fbo = 3,
    FlipY = 4,
    WaylandDisplay = 9,
    NextFrameInfo = 11,
    BlockForTargetTime = 12,
    SkipRendering = 13,
}

#[repr(C)]
struct Param {
    kind: ParamKind,
    data: *mut c_void,
}

impl Param {
    const END: Self = Self {
        kind: ParamKind::End,
        data: ptr::null_mut(),
    };

    fn value<T>(kind: ParamKind, value: &mut T) -> Self {
        Self {
            kind,
            data: ptr::from_mut(value).cast(),
        }
    }
}

#[repr(C)]
struct GlInit {
    get_proc_address: extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    context: *mut c_void,
}

#[repr(C)]
struct Fbo {
    id: c_int,
    width: c_int,
    height: c_int,
    internal_format: c_int,
}

#[repr(C)]
#[derive(Default)]
struct FrameInfo {
    flags: u64,
    target_time: i64,
}

#[repr(C)]
struct Event {
    id: c_int,
    error: c_int,
    userdata: u64,
    data: *mut c_void,
}

#[repr(C)]
struct Property {
    name: *const c_char,
    format: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct EndFile {
    reason: c_int,
    error: c_int,
}

macro_rules! bindings {
    ($($name:ident: $signature:ty),+ $(,)?) => {
        pub struct Api {
            $($name: $signature,)+
            _library: Library,
        }

        impl Api {
            pub fn load() -> Result<Self> {
                // Hold the shared object alive until every handle and callback is gone.
                let library = unsafe { Library::new("libmpv.so.2") }
                    .context("load libmpv.so.2 (install libmpv or set LD_LIBRARY_PATH)")?;
                Ok(Self {
                    $($name: unsafe { *library.get(stringify!($name).as_bytes())? },)+
                    _library: library,
                })
            }
        }
    };
}

bindings! {
    mpv_create: unsafe extern "C" fn() -> Handle,
    mpv_initialize: unsafe extern "C" fn(Handle) -> c_int,
    mpv_terminate_destroy: unsafe extern "C" fn(Handle),
    mpv_error_string: unsafe extern "C" fn(c_int) -> *const c_char,
    mpv_set_option_string: unsafe extern "C" fn(Handle, *const c_char, *const c_char) -> c_int,
    mpv_set_wakeup_callback: unsafe extern "C" fn(Handle, Callback, *mut c_void),
    mpv_observe_property: unsafe extern "C" fn(Handle, u64, *const c_char, c_int) -> c_int,
    mpv_command_async: unsafe extern "C" fn(Handle, u64, *const *const c_char) -> c_int,
    mpv_wait_event: unsafe extern "C" fn(Handle, c_double) -> *const Event,
    mpv_render_context_create: unsafe extern "C" fn(*mut Handle, Handle, *mut Param) -> c_int,
    mpv_render_context_free: unsafe extern "C" fn(Handle),
    mpv_render_context_set_update_callback: unsafe extern "C" fn(Handle, Callback, *mut c_void),
    mpv_render_context_update: unsafe extern "C" fn(Handle) -> u64,
    mpv_render_context_get_info: unsafe extern "C" fn(Handle, Param) -> c_int,
    mpv_render_context_render: unsafe extern "C" fn(Handle, *mut Param) -> c_int,
    mpv_render_context_report_swap: unsafe extern "C" fn(Handle),
}

impl Api {
    fn check(&self, code: c_int) -> Result<()> {
        if code < 0 {
            // mpv_error_string returns a static null-terminated string for every code.
            let error = unsafe { CStr::from_ptr((self.mpv_error_string)(code)) };
            bail!("libmpv: {}", error.to_string_lossy());
        }
        Ok(())
    }
}

struct Wake {
    pending: AtomicU8,
    sender: Sender<()>,
}

impl Wake {
    fn notify(&self, flag: u8) {
        if self.pending.fetch_or(flag, Ordering::AcqRel) == 0 {
            let _ = self.sender.send(());
        }
    }
}

extern "C" fn wake_events(data: *mut c_void) {
    // The player keeps this allocation alive until both callbacks are detached and mpv is destroyed.
    unsafe { &*data.cast::<Wake>() }.notify(EVENTS);
}

extern "C" fn wake_render(data: *mut c_void) {
    unsafe { &*data.cast::<Wake>() }.notify(UPDATE);
}

extern "C" fn get_gl_proc(_ctx: *mut c_void, symbol: *const c_char) -> *mut c_void {
    let Ok(name) = (unsafe { CStr::from_ptr(symbol) }).to_str() else {
        return ptr::null_mut();
    };
    khronos_egl::API
        .get_proc_address(name)
        .map_or(ptr::null_mut(), |proc| proc as *mut c_void)
}

pub struct Player {
    api: Rc<Api>,
    handle: Handle,
    render: Handle,
    wake: Arc<Wake>,
    decoder: Option<String>,
}

impl Player {
    pub fn new(
        api: Rc<Api>,
        sender: Sender<()>,
        hwdec: &str,
        file: &Path,
        wayland_display: &wayland_client::Connection,
    ) -> Result<Self> {
        let hwdec = CString::new(hwdec)?;
        let path = CString::new(file.as_os_str().as_bytes())?;
        let handle = unsafe { (api.mpv_create)() };
        ensure!(!handle.is_null(), "mpv_create returned null");
        // The caller keeps this stream's OpenGL context current during initialization.
        let mut player = Self {
            api,
            handle,
            render: ptr::null_mut(),
            wake: Arc::new(Wake {
                pending: AtomicU8::new(0),
                sender,
            }),
            decoder: None,
        };
        let api = &player.api;
        for (name, value) in [
            (c"config", c"no"),
            (c"vo", c"libmpv"),
            (c"audio", c"no"),
            (c"terminal", c"no"),
            (c"osd-level", c"0"),
            (c"load-scripts", c"no"),
            (c"osc", c"no"),
            (c"loop-file", c"inf"),
            (c"hwdec", hwdec.as_c_str()),
            (c"panscan", c"1"),
            (c"video-sync", c"audio"),
            (c"video-timing-offset", c"0"),
        ] {
            api.check(unsafe {
                (api.mpv_set_option_string)(handle, name.as_ptr(), value.as_ptr())
            })
            .with_context(|| format!("set option {}", name.to_string_lossy()))?;
        }
        api.check(unsafe { (api.mpv_initialize)(handle) })?;

        let mut gl = GlInit {
            get_proc_address: get_gl_proc,
            context: ptr::null_mut(),
        };
        let mut params = [
            Param {
                kind: ParamKind::ApiType,
                data: c"opengl".as_ptr().cast_mut().cast(),
            },
            Param::value(ParamKind::GlInit, &mut gl),
            Param {
                kind: ParamKind::WaylandDisplay,
                data: {
                    use wayland_client::Proxy;
                    wayland_display.display().id().as_ptr().cast()
                },
            },
            Param::END,
        ];
        api.check(unsafe {
            (api.mpv_render_context_create)(&mut player.render, handle, params.as_mut_ptr())
        })?;

        let wake = Arc::as_ptr(&player.wake).cast_mut().cast();
        unsafe {
            (api.mpv_set_wakeup_callback)(handle, Some(wake_events), wake);
            (api.mpv_render_context_set_update_callback)(player.render, Some(wake_render), wake);
        }
        api.check(unsafe {
            (api.mpv_observe_property)(
                handle,
                DECODER_OBSERVER,
                c"hwdec-current".as_ptr(),
                FORMAT_STRING,
            )
        })?;
        let args = [c"loadfile".as_ptr(), path.as_ptr(), ptr::null()];
        api.check(unsafe { (api.mpv_command_async)(handle, LOAD_REQUEST, args.as_ptr()) })?;
        Ok(player)
    }

    pub fn take_updates(&self) -> u8 {
        self.wake.pending.swap(0, Ordering::AcqRel)
    }

    pub fn process_events(&mut self, name: &str) -> Result<()> {
        loop {
            // Event and payload remain valid until the next call to mpv_wait_event.
            let event = unsafe { &*(self.api.mpv_wait_event)(self.handle, 0.0) };
            match event.id {
                EVENT_NONE => return Ok(()),
                EVENT_SHUTDOWN => bail!("{name}: libmpv shut down"),
                EVENT_COMMAND_REPLY if event.userdata == LOAD_REQUEST => {
                    self.api.check(event.error)?
                }
                EVENT_END_FILE => {
                    let end = unsafe { &*event.data.cast::<EndFile>() };
                    if end.reason == END_FILE_ERROR {
                        self.api.check(end.error).context("video playback ended")?;
                        bail!("{name}: video playback ended with an unspecified error");
                    }
                }
                EVENT_PROPERTY_CHANGE if event.userdata == DECODER_OBSERVER => {
                    let property = unsafe { &*event.data.cast::<Property>() };
                    if property.format == FORMAT_STRING && !property.data.is_null() {
                        let value =
                            unsafe { CStr::from_ptr(*property.data.cast::<*const c_char>()) };
                        let value = value.to_string_lossy();
                        if self.decoder.as_deref() != Some(&value) {
                            eprintln!("{name}: decoder={value}");
                            self.decoder = Some(value.into_owned());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    pub fn has_frame(&self) -> bool {
        unsafe { (self.api.mpv_render_context_update)(self.render) & UPDATE_FRAME != 0 }
    }

    pub fn can_skip_frame(&self) -> Result<bool> {
        let mut info = FrameInfo::default();
        self.api.check(unsafe {
            (self.api.mpv_render_context_get_info)(
                self.render,
                Param::value(ParamKind::NextFrameInfo, &mut info),
            )
        })?;
        const PRESENT: u64 = 1;
        const REDRAW_OR_REPEAT: u64 = (1 << 1) | (1 << 2);
        Ok(info.flags & PRESENT != 0 && info.flags & REDRAW_OR_REPEAT == 0)
    }

    pub fn render(&self, width: i32, height: i32, skip: bool) -> Result<()> {
        let mut framebuffer = Fbo {
            id: 0,
            width,
            height,
            internal_format: 0,
        };
        let mut flip = 1i32;
        let mut block = 0i32;
        let mut skip = i32::from(skip);
        let mut params = [
            Param::value(ParamKind::Fbo, &mut framebuffer),
            Param::value(ParamKind::FlipY, &mut flip),
            Param::value(ParamKind::BlockForTargetTime, &mut block),
            Param::value(ParamKind::SkipRendering, &mut skip),
            Param::END,
        ];
        self.api.check(unsafe {
            (self.api.mpv_render_context_render)(self.render, params.as_mut_ptr())
        })
    }

    pub fn report_swap(&self) {
        unsafe { (self.api.mpv_render_context_report_swap)(self.render) }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        unsafe {
            if !self.render.is_null() {
                (self.api.mpv_render_context_set_update_callback)(
                    self.render,
                    None,
                    ptr::null_mut(),
                );
                (self.api.mpv_render_context_free)(self.render);
            }
            (self.api.mpv_set_wakeup_callback)(self.handle, None, ptr::null_mut());
            (self.api.mpv_terminate_destroy)(self.handle);
        }
    }
}
