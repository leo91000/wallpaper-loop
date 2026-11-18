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

#[cfg(feature = "vulkan")]
use crate::vulkan;
#[cfg(feature = "vulkan")]
use wayland_client::protocol::wl_buffer;
#[cfg(feature = "vulkan")]
use wayland_protocols::wp::{
    linux_dmabuf::zv1::client::{zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1},
    presentation_time::client::{wp_presentation, wp_presentation_feedback},
};

struct Output {
    proxy: wl_output::WlOutput,
    name: Option<String>,
    scale: i32,
}

struct MpvPlayback {
    player: mpv::Player,
    gpu: gpu::Window,
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

pub struct MpvRenderer {
    pub display: Rc<gpu::Display>,
    pub api: Rc<mpv::Api>,
    pub wake: Sender<()>,
    pub hwdec: String,
    pub frame_step: u32,
}

impl Drop for MpvPlayback {
    fn drop(&mut self) {
        // The libmpv render context must be freed before destroying its EGL context.
        if let Err(error) = self.gpu.activate_for_drop() {
            eprintln!("wallpaper-loop: cannot recover the EGL context for cleanup: {error:#}");
            std::process::exit(1);
        }
    }
}

pub enum Renderer {
    Mpv(MpvRenderer),
    #[cfg(feature = "vulkan")]
    Vulkan(vulkan::Renderer),
}

enum Playback {
    Mpv(MpvPlayback),
    #[cfg(feature = "vulkan")]
    Vulkan(Box<vulkan::Playback>),
}

#[derive(Default)]
struct Stats {
    elapsed_from: Option<Instant>,
    submitted: u64,
    skipped: u64,
    #[cfg(feature = "vulkan")]
    displayed: u64,
    #[cfg(feature = "vulkan")]
    discarded: u64,
    #[cfg(feature = "vulkan")]
    max_gap: u64,
    #[cfg(feature = "vulkan")]
    last_display: Option<u64>,
}

impl Stats {
    fn submitted(&mut self) {
        self.submitted += 1;
        self.elapsed_from.get_or_insert_with(Instant::now);
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
    stats: Stats,
    #[cfg(feature = "vulkan")]
    paused: bool,
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
    #[cfg(feature = "vulkan")]
    pub drm_modifiers: Vec<u64>,
    #[cfg(feature = "vulkan")]
    pub presentation: Option<wp_presentation::WpPresentation>,
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
                stats: Stats::default(),
                #[cfg(feature = "vulkan")]
                paused: false,
                closed: false,
            });
        }
        if shell.version() >= 3 {
            shell.destroy();
        }
        Ok(())
    }

    pub fn step(&mut self, queue: &QueueHandle<Self>, renderer: &Renderer) -> Result<()> {
        match renderer {
            Renderer::Mpv(renderer) => {
                for (index, layer) in self.layers.iter_mut().enumerate() {
                    if let Some(geometry) = layer.geometry(&self.outputs)? {
                        layer
                            .step_mpv(queue, index, renderer, geometry)
                            .with_context(|| format!("update wallpaper on {}", layer.name))?;
                    }
                }
                Ok(())
            }
            #[cfg(feature = "vulkan")]
            Renderer::Vulkan(renderer) => self.step_vulkan(queue, renderer),
        }
    }

    #[cfg(feature = "vulkan")]
    fn step_vulkan(
        &mut self,
        queue: &QueueHandle<Self>,
        renderer: &vulkan::Renderer,
    ) -> Result<()> {
        let now = Instant::now();
        let mut batch = renderer.batch();
        let mut updates = Vec::new();
        for (index, layer) in self.layers.iter_mut().enumerate() {
            let Some(geometry) = layer.geometry(&self.outputs)? else {
                continue;
            };
            let progress = layer
                .draw_vulkan(queue, renderer, geometry, now, &mut batch)
                .with_context(|| format!("draw Vulkan wallpaper on {}", layer.name))?;
            if !matches!(progress, vulkan::Progress::Idle) {
                updates.push((index, geometry, progress));
            }
        }
        batch.finish()?;
        for (index, geometry, progress) in &updates {
            let layer = &mut self.layers[*index];
            match progress {
                vulkan::Progress::Idle => {}
                vulkan::Progress::Skip => layer.stats.skipped += 1,
                vulkan::Progress::Present(buffer) => {
                    layer.surface.set_buffer_scale(geometry.scale);
                    if let Some(presentation) = &self.presentation {
                        presentation.feedback(&layer.surface, queue, *index);
                    }
                    layer.surface.frame(queue, *index);
                    vulkan::attach(&layer.surface, buffer, (geometry.width, geometry.height));
                    layer.surface.commit();
                    layer.stats.submitted();
                }
            }
        }
        for (index, _, _) in updates {
            let layer = &mut self.layers[index];
            if let Some(Playback::Vulkan(playback)) = &mut layer.playback {
                playback
                    .advance(now)
                    .with_context(|| format!("decode next frame for {}", layer.name))?;
            }
        }
        Ok(())
    }

    pub fn stop(&mut self) {
        self.stopping = true;
    }

    pub fn stopping(&self) -> bool {
        self.stopping
    }

    #[cfg(feature = "vulkan")]
    pub fn next_deadline(&self, renderer: &Renderer) -> Option<Instant> {
        let Renderer::Vulkan(renderer) = renderer else {
            return None;
        };
        self.layers
            .iter()
            .filter_map(|layer| match &layer.playback {
                Some(Playback::Vulkan(playback)) if !layer.pause_requested() => {
                    playback.deadline(renderer.frame_step)
                }
                _ => None,
            })
            .min()
    }

    #[cfg(feature = "vulkan")]
    pub fn set_paused(&mut self, output: &str, paused: bool) {
        if let Some(layer) = self.layers.iter_mut().find(|layer| layer.name == output)
            && layer.paused != paused
        {
            layer.paused = paused;
            if !paused && let Some(Playback::Vulkan(playback)) = &mut layer.playback {
                playback.resume();
            }
            eprintln!("{output}: {}", if paused { "paused" } else { "resumed" });
        }
    }

    #[cfg(feature = "vulkan")]
    pub fn all_outputs_submitted(&self) -> bool {
        self.layers.iter().all(|layer| layer.stats.submitted > 0)
    }

    #[cfg(feature = "vulkan")]
    pub fn pending_presentation(&self) -> bool {
        self.presentation.is_some()
            && self
                .layers
                .iter()
                .any(|layer| layer.stats.submitted > layer.stats.displayed + layer.stats.discarded)
    }

    pub fn verify_presentation(&self) -> Result<()> {
        for layer in &self.layers {
            ensure!(
                layer.stats.submitted > 0,
                "no video frame submitted on {} before the deadline",
                layer.name
            );
        }
        Ok(())
    }

    pub fn print_stats(&self) {
        for layer in &self.layers {
            let stats = &layer.stats;
            let seconds = stats
                .elapsed_from
                .map_or(0.0, |start| start.elapsed().as_secs_f64());
            let fps = if seconds > 0.0 {
                stats.submitted as f64 / seconds
            } else {
                0.0
            };
            #[cfg(feature = "vulkan")]
            if let Some(Playback::Vulkan(playback)) = &layer.playback {
                println!(
                    "{}: submitted={} displayed={} discarded={} skipped={} loops={} average={fps:.2} fps max_gap_ms={:.2}",
                    layer.name,
                    stats.submitted,
                    stats.displayed,
                    stats.discarded,
                    stats.skipped,
                    playback.loops(),
                    stats.max_gap as f64 / 1e6
                );
                continue;
            }
            println!(
                "{}: presented={} skipped={} elapsed={seconds:.2}s average={fps:.2} fps",
                layer.name, stats.submitted, stats.skipped
            );
        }
    }
}

impl Layer {
    fn geometry(&self, outputs: &[Output]) -> Result<Option<Geometry>> {
        ensure!(
            !self.closed,
            "Wayland closed the background layer on {}",
            self.name
        );
        let Some((width, height)) = self.size else {
            return Ok(None);
        };
        let scale = self
            .preferred_scale
            .or_else(|| {
                outputs
                    .iter()
                    .find(|output| output.proxy.data::<u32>() == Some(&self.output_id))
                    .map(|output| output.scale)
            })
            .unwrap_or(1);
        Ok(Some(Geometry {
            width: width.checked_mul(scale).context("invalid surface width")?,
            height: height
                .checked_mul(scale)
                .context("invalid surface height")?,
            scale,
        }))
    }

    fn step_mpv(
        &mut self,
        queue: &QueueHandle<Desktop>,
        index: usize,
        renderer: &MpvRenderer,
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
            self.playback = Some(Playback::Mpv(MpvPlayback {
                player,
                gpu,
                next_frame: 0,
                frame_due: false,
                awaiting_wayland: false,
                geometry,
            }));
        }
        let Some(Playback::Mpv(playback)) = &mut self.playback else {
            anyhow::bail!("libmpv playback was not initialized");
        };
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
            self.stats.skipped += 1;
        } else {
            self.surface.frame(queue, index);
            playback.awaiting_wayland = true;
            playback.gpu.swap()?;
            self.stats.submitted();
        }
        playback.player.report_swap();
        Ok(())
    }

    #[cfg(feature = "vulkan")]
    fn pause_requested(&self) -> bool {
        // Keep an initial image available even if the first policy update arrives
        // before the layer maps.
        self.paused && self.stats.submitted > 0
    }

    #[cfg(feature = "vulkan")]
    fn draw_vulkan(
        &mut self,
        queue: &QueueHandle<Desktop>,
        renderer: &vulkan::Renderer,
        geometry: Geometry,
        now: Instant,
        batch: &mut vulkan::Batch<'_>,
    ) -> Result<vulkan::Progress> {
        if self.pause_requested() {
            return Ok(vulkan::Progress::Idle);
        }
        if self.playback.is_none() {
            self.playback = Some(Playback::Vulkan(Box::new(vulkan::Playback::new(
                renderer, &self.file, now,
            )?)));
            eprintln!("{}: decoder=vulkan", self.name);
        }
        let Some(Playback::Vulkan(playback)) = &mut self.playback else {
            anyhow::bail!("Vulkan playback was not initialized");
        };
        playback.resize(renderer, queue, (geometry.width, geometry.height))?;
        playback.draw(renderer.frame_step, now, batch)
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
        match &mut state.layers[*index].playback {
            Some(Playback::Mpv(playback)) => playback.awaiting_wayland = false,
            #[cfg(feature = "vulkan")]
            Some(Playback::Vulkan(playback)) => playback.awaiting_wayland = false,
            None => {}
        }
    }
}

delegate_noop!(Desktop: ignore wl_compositor::WlCompositor);
delegate_noop!(Desktop: ignore wl_region::WlRegion);
delegate_noop!(Desktop: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);

#[cfg(feature = "vulkan")]
impl Dispatch<wl_buffer::WlBuffer, vulkan::Released> for Desktop {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        ready: &vulkan::Released,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            ready.store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

#[cfg(feature = "vulkan")]
impl Dispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for Desktop {
    fn event(
        state: &mut Self,
        _: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        event: zwp_linux_dmabuf_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_linux_dmabuf_v1::Event::Modifier {
            format: vulkan::XR24,
            modifier_hi,
            modifier_lo,
        } = event
        {
            state
                .drm_modifiers
                .push(u64::from(modifier_hi) << 32 | u64::from(modifier_lo));
        }
    }
}

#[cfg(feature = "vulkan")]
impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, usize> for Desktop {
    fn event(
        state: &mut Self,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let stats = &mut state.layers[*index].stats;
        match event {
            wp_presentation_feedback::Event::Presented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                ..
            } => {
                let seconds = u64::from(tv_sec_hi) << 32 | u64::from(tv_sec_lo);
                let ns = seconds
                    .saturating_mul(1_000_000_000)
                    .saturating_add(u64::from(tv_nsec));
                if let Some(previous) = stats.last_display.replace(ns) {
                    stats.max_gap = stats.max_gap.max(ns.saturating_sub(previous));
                }
                stats.displayed += 1;
            }
            wp_presentation_feedback::Event::Discarded => stats.discarded += 1,
            _ => {}
        }
    }
}

#[cfg(feature = "vulkan")]
impl
    Dispatch<
        zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        std::sync::Arc<vulkan::ImportTicket>,
    > for Desktop
{
    fn event(
        _: &mut Self,
        params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        event: zwp_linux_buffer_params_v1::Event,
        import: &std::sync::Arc<vulkan::ImportTicket>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_linux_buffer_params_v1::Event::Created { buffer } => import.created(buffer),
            zwp_linux_buffer_params_v1::Event::Failed => import.failed(),
            _ => return,
        }
        params.destroy();
    }

    wayland_client::event_created_child!(
        Desktop,
        zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        [zwp_linux_buffer_params_v1::EVT_CREATED_OPCODE =>
            (wl_buffer::WlBuffer, std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)))]
    );
}
#[cfg(feature = "vulkan")]
delegate_noop!(Desktop: ignore wp_presentation::WpPresentation);
