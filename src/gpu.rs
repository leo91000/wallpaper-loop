use std::rc::Rc;

use anyhow::{Context, Result};
use khronos_egl::{self as egl, API};
use wayland_client::{Connection, Proxy, protocol::wl_surface::WlSurface};
use wayland_egl::WlEglSurface;

pub struct Display {
    raw: egl::Display,
    config: egl::Config,
    context: egl::Context,
    pub connection: Connection,
}

impl Display {
    pub fn new(connection: Connection) -> Result<Rc<Self>> {
        const PLATFORM_WAYLAND: egl::Enum = 0x31d8;
        // The system backend owns the native wl_display for the life of the connection.
        let raw = unsafe {
            API.get_platform_display(
                PLATFORM_WAYLAND,
                connection.display().id().as_ptr().cast(),
                &[egl::ATTRIB_NONE],
            )
        }
        .context("open Wayland EGL display")?;
        API.initialize(raw).context("initialize EGL")?;
        let (config, context) = Self::create_context(raw).inspect_err(|_| {
            let _ = API.terminate(raw);
        })?;
        Ok(Rc::new(Self {
            raw,
            config,
            context,
            connection,
        }))
    }

    fn create_context(display: egl::Display) -> Result<(egl::Config, egl::Context)> {
        API.bind_api(egl::OPENGL_API)
            .context("bind desktop OpenGL")?;
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
            .context("EGL has no OpenGL window config")?;
        let context = API
            .create_context(display, config, None, &[egl::NONE])
            .context("create OpenGL context")?;
        Ok((config, context))
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        let _ = API.make_current(self.raw, None, None, None);
        let _ = API.destroy_context(self.raw, self.context);
        let _ = API.terminate(self.raw);
    }
}

pub struct Window {
    display: Rc<Display>,
    surface: egl::Surface,
    window: WlEglSurface,
}

impl Window {
    pub fn new(display: Rc<Display>, surface: &WlSurface, width: i32, height: i32) -> Result<Self> {
        let window = WlEglSurface::new(surface.id(), width, height)?;
        // The native window wraps a wl_surface on the same Wayland display.
        let surface = unsafe {
            API.create_window_surface(display.raw, display.config, window.ptr().cast_mut(), None)
        }
        .context("create EGL window surface")?;
        let gpu = Self {
            display,
            surface,
            window,
        };
        gpu.activate()?;
        API.swap_interval(gpu.display.raw, 0)
            .context("set EGL swap interval")?;
        Ok(gpu)
    }

    pub fn activate(&self) -> Result<()> {
        if API.get_current_surface(egl::DRAW) != Some(self.surface) {
            API.make_current(
                self.display.raw,
                Some(self.surface),
                Some(self.surface),
                Some(self.display.context),
            )
            .context("activate EGL window")?;
        }
        Ok(())
    }

    pub fn resize(&self, width: i32, height: i32) {
        self.window.resize(width, height, 0, 0);
    }

    pub fn swap(&self) -> Result<()> {
        API.swap_buffers(self.display.raw, self.surface)
            .context("present EGL buffer")
    }

    pub fn activate_for_drop(&self) -> Result<()> {
        if API.get_current_context() == Some(self.display.context) {
            return Ok(());
        }
        API.make_current(self.display.raw, None, None, Some(self.display.context))
            .context("activate OpenGL context without the window")
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        if API.get_current_surface(egl::DRAW) == Some(self.surface) {
            let _ = API.make_current(self.display.raw, None, None, None);
        }
        let _ = API.destroy_surface(self.display.raw, self.surface);
    }
}
