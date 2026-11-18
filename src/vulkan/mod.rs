mod color;
mod decode;
mod gpu;

use std::{
    fs,
    os::fd::AsFd,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use wayland_client::{
    Dispatch, Proxy, QueueHandle,
    protocol::{wl_buffer, wl_surface},
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
};

pub use gpu::XR24;
pub type Released = Arc<AtomicBool>;

#[derive(Default)]
enum Import {
    #[default]
    Pending,
    Ready(wl_buffer::WlBuffer),
    Failed,
    Cancelled,
}

#[derive(Default)]
pub struct ImportTicket(Mutex<Import>);

impl ImportTicket {
    pub fn created(&self, buffer: wl_buffer::WlBuffer) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if matches!(*state, Import::Pending) {
            *state = Import::Ready(buffer);
        } else {
            buffer.destroy();
        }
    }

    pub fn failed(&self) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if matches!(*state, Import::Pending) {
            *state = Import::Failed;
        }
    }

    fn verify(&self) -> Result<()> {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        ensure!(
            !matches!(*state, Import::Failed),
            "the compositor rejected the XR24 DMA-BUF"
        );
        Ok(())
    }

    fn with_ready<T>(
        &self,
        access: impl FnOnce(&wl_buffer::WlBuffer, &AtomicBool) -> T,
    ) -> Option<T> {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let Import::Ready(buffer) = &*state else {
            return None;
        };
        let released = buffer.data::<Released>()?;
        Some(access(buffer, released))
    }

    fn available(&self) -> bool {
        self.with_ready(|_, released| released.load(Ordering::Acquire))
            .unwrap_or(false)
    }

    fn reserve(&self) -> Option<wl_buffer::WlBuffer> {
        self.with_ready(|buffer, released| {
            released
                .swap(false, Ordering::AcqRel)
                .then(|| buffer.clone())
        })
        .flatten()
    }

    fn cancel(&self) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if let Import::Ready(buffer) = std::mem::replace(&mut *state, Import::Cancelled) {
            buffer.destroy();
        }
    }
}

const OUTPUT_SLOTS: usize = 4;

pub struct Renderer {
    gpu: gpu::Gpu,
    device: Rc<decode::Device>,
    dmabuf: zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    pub frame_step: u32,
}

impl Renderer {
    pub fn new(
        render_node: Option<&Path>,
        advertised: &[u64],
        dmabuf: zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        frame_step: u32,
    ) -> Result<Self> {
        let node = match render_node {
            Some(node) => node.to_owned(),
            None => default_render_node()?,
        };
        let gpu = gpu::Gpu::open(&node, advertised).context("initialize Vulkan display interop")?;
        let device = decode::Device::open(&node)?;
        Ok(Self {
            gpu,
            device,
            dmabuf,
            frame_step,
        })
    }

    pub fn batch(&self) -> Batch<'_> {
        Batch {
            gpu: &self.gpu,
            inputs: Vec::new(),
        }
    }

    fn pool<State>(&self, queue: &QueueHandle<State>, size: (i32, i32)) -> Result<Vec<Slot>>
    where
        State: Dispatch<wl_buffer::WlBuffer, Released>
            + Dispatch<zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, Arc<ImportTicket>>
            + 'static,
    {
        (0..OUTPUT_SLOTS)
            .map(|_| {
                let (target, exported) = self.gpu.allocate(size.0, size.1)?;
                let import = Arc::new(ImportTicket::default());
                let params = self.dmabuf.create_params(queue, Arc::clone(&import));
                params.add(
                    exported.fd.as_fd(),
                    0,
                    exported.offset,
                    exported.stride,
                    (exported.modifier >> 32) as u32,
                    exported.modifier as u32,
                );
                params.create(
                    size.0,
                    size.1,
                    XR24,
                    zwp_linux_buffer_params_v1::Flags::empty(),
                );
                Ok(Slot { import, target })
            })
            .collect()
    }
}

/// Retain every decoded image until all the draws in this batch have finished.
pub struct Batch<'a> {
    gpu: &'a gpu::Gpu,
    inputs: Vec<decode::Mapped>,
}

impl Batch<'_> {
    fn complete(&mut self) -> Result<()> {
        if self.inputs.is_empty() {
            return Ok(());
        }
        let result = self.gpu.finish();
        self.inputs.clear();
        result
    }

    pub fn finish(mut self) -> Result<()> {
        self.complete()
    }
}

impl Drop for Batch<'_> {
    fn drop(&mut self) {
        // A failed draw on one output must still fence the draws already queued.
        let _ = self.complete();
    }
}

fn default_render_node() -> Result<PathBuf> {
    let mut nodes = fs::read_dir("/dev/dri")
        .context("find a DRM render node")?
        .filter_map(std::result::Result::ok)
        .filter(|node| node.file_name().as_encoded_bytes().starts_with(b"renderD"))
        .map(|node| node.path());
    let first = nodes
        .next()
        .context("no DRM render node found; set --render-node")?;
    ensure!(
        nodes.next().is_none(),
        "multiple DRM render nodes; set --render-node for the display GPU"
    );
    Ok(first)
}

struct Slot {
    import: Arc<ImportTicket>,
    target: gpu::Target,
}

impl Drop for Slot {
    fn drop(&mut self) {
        // The compositor owns a DMA-BUF reference for any commit still in flight.
        // The old storage is never handed back to the client for reuse after resize.
        self.import.cancel();
    }
}

pub struct Playback {
    decoder: decode::Decoder,
    current: decode::FrameInfo,
    schedule: Schedule,
    imports: gpu::Imports,
    slots: Vec<Slot>,
    size: (i32, i32),
    frame: u64,
    pub awaiting_wayland: bool,
}

impl Playback {
    pub fn new(renderer: &Renderer, path: &Path, now: Instant) -> Result<Self> {
        let mut decoder = decode::Decoder::open(
            Rc::clone(&renderer.device),
            path,
            &renderer.gpu.decode_modifiers,
        )?;
        let current = decoder.next()?;
        Ok(Self {
            decoder,
            current,
            schedule: Schedule { due: now },
            imports: gpu::Imports::default(),
            slots: Vec::new(),
            size: (0, 0),
            frame: 0,
            awaiting_wayland: false,
        })
    }

    pub fn deadline(&self, frame_step: u32) -> Option<Instant> {
        if self.awaiting_wayland {
            return None;
        }
        let should_present = self.frame.is_multiple_of(u64::from(frame_step));
        (!should_present || self.slots.iter().any(|slot| slot.import.available()))
            .then_some(self.schedule.due)
    }

    pub fn resize<State>(
        &mut self,
        renderer: &Renderer,
        queue: &QueueHandle<State>,
        size: (i32, i32),
    ) -> Result<()>
    where
        State: Dispatch<wl_buffer::WlBuffer, Released>
            + Dispatch<zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, Arc<ImportTicket>>
            + 'static,
    {
        if self.size != size {
            self.slots = renderer.pool(queue, size)?;
            self.size = size;
        }
        Ok(())
    }

    /// Render to an available Wayland buffer without reusing one still owned by the compositor.
    pub fn draw(
        &mut self,
        frame_step: u32,
        now: Instant,
        batch: &mut Batch<'_>,
    ) -> Result<Progress> {
        for slot in &self.slots {
            slot.import.verify()?;
        }
        let Some(due) = self.deadline(frame_step) else {
            return Ok(Progress::Idle);
        };
        if now < due {
            return Ok(Progress::Idle);
        }
        if !self.frame.is_multiple_of(u64::from(frame_step)) {
            return Ok(Progress::Skip);
        }
        let slot = self
            .slots
            .iter()
            .find_map(|slot| slot.import.reserve().map(|buffer| (slot, buffer)))
            .context("no available DMA-BUF")?;
        batch.inputs.push(self.decoder.mapped()?);
        batch.gpu.draw(
            &slot.0.target,
            batch.inputs.last().context("missing mapped frame")?,
            self.current,
            self.size.0,
            self.size.1,
            &mut self.imports,
        )?;
        self.awaiting_wayland = true;
        Ok(Progress::Present(slot.1))
    }

    pub fn advance(&mut self, now: Instant) -> Result<()> {
        let next = self.decoder.next()?;
        self.schedule.advance(now, self.current, next);
        self.current = next;
        self.frame = self.frame.wrapping_add(1);
        Ok(())
    }

    pub fn resume(&mut self) {
        self.schedule.due = Instant::now();
    }

    pub fn loops(&self) -> u64 {
        self.decoder.loops
    }
}

pub enum Progress {
    Idle,
    Skip,
    Present(wl_buffer::WlBuffer),
}

struct Schedule {
    due: Instant,
}

impl Schedule {
    fn advance(&mut self, now: Instant, current: decode::FrameInfo, next: decode::FrameInfo) {
        let same_loop = current.loop_count == next.loop_count;
        let gap = same_loop
            .then(|| {
                next.timestamp?
                    .checked_sub(current.timestamp?)
                    .filter(|&delta| delta > 0)
                    .map(|delta| Duration::from_nanos(delta as u64))
            })
            .flatten()
            .unwrap_or(current.duration);
        // A display can stop sending frame callbacks while covered or powered off.
        // Resume from the current image instead of racing through a catch-up burst.
        let anchor = if now.saturating_duration_since(self.due) > Duration::from_millis(250) {
            now
        } else {
            self.due
        };
        self.due = anchor.checked_add(gap).unwrap_or(anchor);
    }
}

pub fn attach(surface: &wl_surface::WlSurface, buffer: &wl_buffer::WlBuffer, size: (i32, i32)) {
    surface.attach(Some(buffer), 0, 0);
    surface.damage_buffer(0, 0, size.0, size.1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(timestamp: Option<i64>, loop_count: u64) -> decode::FrameInfo {
        decode::FrameInfo {
            width: 64,
            height: 64,
            aspect: 1.0,
            color: color::Color {
                luma_offset: 0.0,
                luma_scale: 1.0,
                chroma_scale: 1.0,
                coefficients: [0.0; 4],
            },
            timestamp,
            duration: Duration::from_millis(33),
            loop_count,
        }
    }

    #[test]
    fn schedule_uses_pts_and_handles_wrapping_and_missing_timestamps() {
        let start = Instant::now();
        let mut schedule = Schedule { due: start };
        schedule.advance(
            start,
            frame(Some(10_000_000), 0),
            frame(Some(50_000_000), 0),
        );
        assert_eq!(schedule.due, start + Duration::from_millis(40));
        schedule.advance(
            schedule.due,
            frame(Some(50_000_000), 0),
            frame(Some(10_000_000), 1),
        );
        assert_eq!(schedule.due, start + Duration::from_millis(73));
        schedule.advance(schedule.due, frame(Some(10_000_000), 1), frame(None, 1));
        assert_eq!(schedule.due, start + Duration::from_millis(106));
    }

    #[test]
    fn schedule_resumes_without_catchup_after_visibility_pause() {
        let start = Instant::now();
        let resume = start + Duration::from_secs(3);
        let mut schedule = Schedule { due: start };
        schedule.advance(resume, frame(Some(0), 0), frame(Some(33_000_000), 0));
        assert_eq!(schedule.due, resume + Duration::from_millis(33));
    }
}
