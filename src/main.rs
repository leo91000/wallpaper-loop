#[cfg(feature = "vulkan")]
mod control;
mod desktop;
mod gpu;
mod mpv;
#[cfg(feature = "vulkan")]
mod vulkan;

use std::{
    collections::HashSet,
    ffi::OsString,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use calloop::{EventLoop, channel};
use calloop_wayland_source::WaylandSource;
use clap::Parser;
use wayland_client::{Connection, globals::registry_queue_init};

use desktop::{Desktop, MpvRenderer, Renderer};

#[derive(Clone, Copy, clap::ValueEnum)]
enum Backend {
    Mpv,
    Vulkan,
}

#[derive(Parser)]
#[command(version, about = "Play looping videos as Wayland desktop wallpapers")]
struct Args {
    /// Assign a local video to an output; repeat for multiple outputs.
    #[arg(short, long, value_names = ["NAME", "VIDEO"], num_args = 2, required_unless_present = "list_outputs", conflicts_with = "list_outputs")]
    output: Vec<OsString>,

    /// List Wayland output names.
    #[arg(long)]
    list_outputs: bool,

    /// Playback backend. Vulkan requires a build with --features vulkan.
    #[arg(long, value_enum, default_value = "mpv")]
    backend: Backend,

    /// DRM render node for the Vulkan backend.
    #[arg(long)]
    render_node: Option<PathBuf>,

    /// Unix datagram socket for per-output Vulkan pause and resume messages.
    #[arg(long)]
    pause_socket: Option<PathBuf>,

    /// libmpv hardware decoding mode (for example auto, nvdec, vaapi, or no).
    #[arg(long, default_value = "auto")]
    hwdec: String,

    /// Present every Nth video frame. At 30 fps, 2 presents about 15 fps.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
    frame_step: u32,

    /// Exit after this many seconds.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    duration: Option<u32>,

    /// Print per-output frame counts at exit.
    #[arg(long)]
    stats: bool,
}

fn streams(args: &Args) -> Result<Vec<(String, PathBuf)>> {
    let mut used = HashSet::new();
    args.output
        .chunks_exact(2)
        .map(|pair| {
            let name = pair[0].to_str().context("output name must be UTF-8")?;
            ensure!(used.insert(name), "duplicate output {name}");
            let path = PathBuf::from(&pair[1]);
            ensure!(
                path.is_file(),
                "video is not a regular file: {}",
                path.display()
            );
            Ok((name.to_owned(), path.canonicalize()?))
        })
        .collect()
}

fn run(args: Args) -> Result<()> {
    let streams = streams(&args)?;
    if args.render_node.is_some() {
        ensure!(
            matches!(args.backend, Backend::Vulkan),
            "--render-node requires --backend vulkan"
        );
    }
    if args.pause_socket.is_some() {
        ensure!(
            matches!(args.backend, Backend::Vulkan),
            "--pause-socket requires --backend vulkan"
        );
    }
    if matches!(args.backend, Backend::Vulkan) {
        ensure!(
            args.hwdec == "auto",
            "--hwdec controls only the mpv backend"
        );
        #[cfg(not(feature = "vulkan"))]
        anyhow::bail!("build with --features vulkan to use --backend vulkan");
    }
    let connection = Connection::connect_to_env().context("connect to Wayland")?;
    let (globals, mut queue) = registry_queue_init::<Desktop>(&connection)?;
    let mut desktop = Desktop::default();
    let handle = queue.handle();
    desktop.connect_outputs(&globals, &handle);
    queue.roundtrip(&mut desktop)?;

    if args.list_outputs {
        for name in desktop.output_names() {
            println!("{name}");
        }
        return Ok(());
    }

    #[cfg(feature = "vulkan")]
    let dmabuf = if matches!(args.backend, Backend::Vulkan) {
        use wayland_protocols::wp::{
            linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
            presentation_time::client::wp_presentation::WpPresentation,
        };
        if args.stats {
            desktop.presentation = globals
                .bind::<WpPresentation, _, _>(&handle, 1..=1, ())
                .ok();
        }
        let dmabuf: ZwpLinuxDmabufV1 = globals
            .bind(&handle, 3..=3, ())
            .context("Wayland compositor requires Linux DMA-BUF version 3")?;
        queue.roundtrip(&mut desktop)?;
        Some(dmabuf)
    } else {
        None
    };
    desktop.map(&globals, &handle, streams)?;

    let mut reactor = EventLoop::<Desktop>::try_new()?;
    #[cfg(feature = "vulkan")]
    if let Some(path) = args.pause_socket.as_deref() {
        control::PauseSocket::install(path, reactor.handle())?;
    }
    #[cfg(feature = "vulkan")]
    let mut notify = if matches!(args.backend, Backend::Vulkan) {
        control::notify_address()?
    } else {
        None
    };
    WaylandSource::new(connection.clone(), queue)
        .insert(reactor.handle())
        .map_err(|e| e.error)?;
    let (wake, updates) = channel::channel();
    reactor
        .handle()
        .insert_source(updates, |_, _, _| ())
        .map_err(|e| e.error)?;
    let (stop, signals) = channel::channel();
    ctrlc::set_handler(move || {
        let _ = stop.send(());
    })?;
    reactor
        .handle()
        .insert_source(signals, |_, _, state| state.stop())
        .map_err(|e| e.error)?;

    let renderer = match args.backend {
        Backend::Mpv => Renderer::Mpv(MpvRenderer {
            display: gpu::Display::new(connection.clone())?,
            api: Rc::new(mpv::Api::load()?),
            wake,
            hwdec: args.hwdec,
            frame_step: args.frame_step,
        }),
        #[cfg(feature = "vulkan")]
        Backend::Vulkan => Renderer::Vulkan(vulkan::Renderer::new(
            args.render_node.as_deref(),
            &desktop.drm_modifiers,
            dmabuf.context("missing Wayland DMA-BUF factory")?,
            args.frame_step,
        )?),
        #[cfg(not(feature = "vulkan"))]
        Backend::Vulkan => unreachable!("Vulkan feature checked before connecting"),
    };
    let deadline = args
        .duration
        .map(|seconds| Instant::now() + Duration::from_secs(u64::from(seconds)));
    while !desktop.stopping() {
        let remaining = deadline.map(|end| end.saturating_duration_since(Instant::now()));
        if remaining == Some(Duration::ZERO) {
            desktop.verify_presentation()?;
            break;
        }
        #[cfg(feature = "vulkan")]
        let remaining = match (remaining, desktop.next_deadline(&renderer)) {
            (remaining, None) => remaining,
            (remaining, Some(next)) => {
                let frame = next.saturating_duration_since(Instant::now());
                Some(remaining.map_or(frame, |limit| frame.min(limit)))
            }
        };
        reactor
            .dispatch(remaining, &mut desktop)
            .context("dispatch Wayland events")?;
        if !desktop.stopping() {
            desktop.step(&handle, &renderer)?;
            #[cfg(feature = "vulkan")]
            if let Some(address) = &notify
                && desktop.all_outputs_submitted()
            {
                connection
                    .flush()
                    .context("submit initial wallpaper frames")?;
                control::notify_ready(address)?;
                notify = None;
            }
        }
    }
    if args.stats {
        #[cfg(feature = "vulkan")]
        {
            let until = Instant::now() + Duration::from_millis(120);
            while desktop.pending_presentation() {
                let remaining = until.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                reactor
                    .dispatch(remaining, &mut desktop)
                    .context("collect presentation feedback")?;
            }
        }
        desktop.print_stats();
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    if let Err(error) = run(Args::parse()) {
        eprintln!("wallpaper-loop: {error:#}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
