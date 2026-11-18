use std::{path::PathBuf, rc::Rc, time::Instant};

use anyhow::{Context, Result, ensure};
use calloop::channel::Sender;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop,
    globals::{GlobalList, GlobalListContents},
    protocol::{wl_callback, wl_compositor, wl_output, wl_region, wl_registry, wl_surface},
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use crate::{gpu, mpv};

struct Output {
    proxy: wl_output::WlOutput,
    name: Option<String>,
    scale: i32,
}

struct Playback {
    player: mpv::Player,
    gpu: gpu::Window,
    elapsed_from: Option<Instant>,
    presented: u64,
    skipped: u64,
    next_frame: u32,
    frame_due: bool,
    awaiting_wayland: bool,
    geometry: Geometry,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Geometry {
    width: i32,
    height: i32,
    scale: i32,
}

pub struct Renderer {
    pub display: Rc<gpu::Display>,
    pub api: Rc<mpv::Api>,
    pub wake: Sender<()>,
    pub hwdec: String,
    pub frame_step: u32,
}

impl Drop for Playback {
    fn drop(&mut self) {
        // The libmpv render context must be freed before destroying its EGL context.
        if let Err(error) = self.gpu.activate_for_drop() {
            eprintln!("wallpaper-loop: cannot recover the EGL context for cleanup: {error:#}");
            std::process::exit(1);
        }
    }
}

struct Layer {
    name: String,
    file: PathBuf,
    surface: wl_surface::WlSurface,
    role: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    size: Option<(i32, i32)>,
    output_id: u32,
    preferred_scale: Option<i32>,
    playback: Option<Playback>,
    closed: bool,
}

impl Drop for Layer {
    fn drop(&mut self) {
        drop(self.playback.take());
        self.role.destroy();
        self.surface.destroy();
    }
}

#[derive(Default)]
pub struct Desktop {
    outputs: Vec<Output>,
    layers: Vec<Layer>,
    stopping: bool,
}

impl Desktop {
    pub fn connect_outputs(&mut self, globals: &GlobalList, queue: &QueueHandle<Self>) {
        for global in globals.contents().clone_list() {
            if global.interface == "wl_output" && global.version >= 4 {
                self.add_output(globals.registry(), global.name, queue);
            }
        }
    }

    fn add_output(
        &mut self,
        registry: &wl_registry::WlRegistry,
        id: u32,
        queue: &QueueHandle<Self>,
    ) {
        if self
            .outputs
            .iter()
            .any(|output| output.proxy.data::<u32>() == Some(&id))
        {
            return;
        }
        let proxy = registry.bind::<wl_output::WlOutput, _, _>(id, 4, queue, id);
        self.outputs.push(Output {
            proxy,
            name: None,
            scale: 1,
        });
    }

    pub fn output_names(&self) -> impl Iterator<Item = &str> {
        self.outputs
            .iter()
            .filter_map(|output| output.name.as_deref())
    }

    pub fn map(
        &mut self,
        globals: &GlobalList,
        queue: &QueueHandle<Self>,
        streams: Vec<(String, PathBuf)>,
    ) -> Result<()> {
        let compositor: wl_compositor::WlCompositor = globals.bind(queue, 4..=6, ())?;
        let shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = globals.bind(queue, 1..=4, ())?;
        for (name, file) in streams {
            let output = self
                .outputs
                .iter()
                .find(|output| output.name.as_deref() == Some(&name))
                .with_context(|| {
                    format!("Wayland output {name} is unavailable; use --list-outputs")
                })?;
            let output_id = *output
                .proxy
                .data::<u32>()
                .context("missing Wayland output ID")?;
            let index = self.layers.len();
            let surface = compositor.create_surface(queue, index);
            let input = compositor.create_region(queue, ());
            surface.set_input_region(Some(&input));
            input.destroy();
            let role = shell.get_layer_surface(
                &surface,
                Some(&output.proxy),
                zwlr_layer_shell_v1::Layer::Background,
                "wallpaper-loop".to_owned(),
                queue,
                index,
            );
            use zwlr_layer_surface_v1::Anchor;
            role.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
            role.set_size(0, 0);
            role.set_exclusive_zone(-1);
            surface.commit();
            self.layers.push(Layer {
                name,
                file,
                surface,
                role,
                size: None,
                output_id,
                preferred_scale: None,
                playback: None,
                closed: false,
            });
        }
        if shell.version() >= 3 {
            shell.destroy();
        }
        Ok(())
    }

    pub fn step(&mut self, queue: &QueueHandle<Self>, renderer: &Renderer) -> Result<()> {
        for (index, layer) in self.layers.iter_mut().enumerate() {
            ensure!(
                !layer.closed,
                "Wayland closed the background layer on {}",
                layer.name
            );
            let Some((width, height)) = layer.size else {
                continue;
            };
            let scale = layer
                .preferred_scale
                .or_else(|| {
                    self.outputs
                        .iter()
                        .find(|output| output.proxy.data::<u32>() == Some(&layer.output_id))
                        .map(|output| output.scale)
                })
                .unwrap_or(1);
            let geometry = Geometry {
                width: width.checked_mul(scale).context("invalid surface width")?,
                height: height
                    .checked_mul(scale)
                    .context("invalid surface height")?,
                scale,
            };
            let result = layer.step(queue, index, renderer, geometry);
            result.with_context(|| format!("update wallpaper on {}", layer.name))?;
        }
        Ok(())
    }

    pub fn stop(&mut self) {
        self.stopping = true;
    }

    pub fn stopping(&self) -> bool {
        self.stopping
    }

    pub fn verify_presentation(&self) -> Result<()> {
        for layer in &self.layers {
            ensure!(
                layer
                    .playback
                    .as_ref()
                    .is_some_and(|playing| playing.presented > 0),
                "no video frame submitted on {} before the deadline",
                layer.name
            );
        }
        Ok(())
    }

    pub fn print_stats(&self) {
        for layer in &self.layers {
            let Some(playback) = &layer.playback else {
                continue;
            };
            let seconds = playback
                .elapsed_from
                .map_or(0.0, |start| start.elapsed().as_secs_f64());
            println!(
                "{}: presented={} skipped={} elapsed={seconds:.2}s average={:.2} fps",
                layer.name,
                playback.presented,
                playback.skipped,
                if seconds > 0.0 {
                    playback.presented as f64 / seconds
                } else {
                    0.0
                }
            );
        }
    }
}

impl Layer {
    fn step(
        &mut self,
        queue: &QueueHandle<Desktop>,
        index: usize,
        renderer: &Renderer,
        geometry: Geometry,
    ) -> Result<()> {
        let Geometry {
            width,
            height,
            scale,
        } = geometry;
        if self.playback.is_none() {
            self.surface.set_buffer_scale(scale);
            let gpu = gpu::Window::new(Rc::clone(&renderer.display), &self.surface, width, height)?;
            let player = mpv::Player::new(
                Rc::clone(&renderer.api),
                renderer.wake.clone(),
                &renderer.hwdec,
                &self.file,
                &renderer.display.connection,
            )?;
            self.playback = Some(Playback {
                player,
                gpu,
                elapsed_from: None,
                presented: 0,
                skipped: 0,
                next_frame: 0,
                frame_due: false,
                awaiting_wayland: false,
                geometry,
            });
        }
        let playback = self
            .playback
            .as_mut()
            .context("playback was not initialized")?;
        let pending = playback.player.take_updates();
        if pending & mpv::EVENTS != 0 {
            playback.player.process_events(&self.name)?;
        }
        let resized = playback.geometry != geometry;
        if pending & mpv::UPDATE == 0
            && !resized
            && (!playback.frame_due || playback.awaiting_wayland)
        {
            return Ok(());
        }
        playback.gpu.activate()?;
        if resized {
            self.surface.set_buffer_scale(scale);
            playback.gpu.resize(width, height);
            playback.geometry = geometry;
            playback.frame_due = true;
        }
        if pending & mpv::UPDATE != 0 && playback.player.has_frame() {
            playback.frame_due = true;
        }
        if !playback.frame_due || playback.awaiting_wayland {
            return Ok(());
        }
        playback.frame_due = false;
        let skip = if renderer.frame_step > 1 && !resized && playback.player.can_skip_frame()? {
            let skip = playback.next_frame != 0;
            playback.next_frame = (playback.next_frame + 1) % renderer.frame_step;
            skip
        } else {
            false
        };
        playback.player.render(width, height, skip)?;
        if skip {
            playback.skipped += 1;
        } else {
            self.surface.frame(queue, index);
            playback.awaiting_wayland = true;
            playback.gpu.swap()?;
            playback.presented += 1;
            playback.elapsed_from.get_or_insert_with(Instant::now);
        }
        playback.player.report_swap();
        Ok(())
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Desktop {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "wl_output" && version >= 4 => {
                state.add_output(registry, name, queue);
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(index) = state
                    .outputs
                    .iter()
                    .position(|output| output.proxy.data::<u32>() == Some(&name))
                {
                    state.outputs.swap_remove(index).proxy.release();
                }
                for layer in &mut state.layers {
                    if layer.output_id == name {
                        layer.closed = true;
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for Desktop {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state
            .outputs
            .iter_mut()
            .find(|output| &output.proxy == proxy)
        else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => output.name = Some(name),
            wl_output::Event::Scale { factor } => output.scale = factor.max(1),
            _ => {}
        }
    }
}

impl Dispatch<wl_surface::WlSurface, usize> for Desktop {
    fn event(
        state: &mut Self,
        _: &wl_surface::WlSurface,
        event: wl_surface::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_surface::Event::PreferredBufferScale { factor } = event {
            state.layers[*index].preferred_scale = Some(factor.max(1));
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, usize> for Desktop {
    fn event(
        state: &mut Self,
        role: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let layer = &mut state.layers[*index];
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                role.ack_configure(serial);
                match (i32::try_from(width), i32::try_from(height)) {
                    (Ok(width), Ok(height)) if width > 0 && height > 0 => {
                        layer.size = Some((width, height))
                    }
                    _ => layer.closed = true,
                }
            }
            zwlr_layer_surface_v1::Event::Closed => layer.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, usize> for Desktop {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(playback) = &mut state.layers[*index].playback {
            playback.awaiting_wayland = false;
        }
    }
}

delegate_noop!(Desktop: ignore wl_compositor::WlCompositor);
delegate_noop!(Desktop: ignore wl_region::WlRegion);
delegate_noop!(Desktop: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
