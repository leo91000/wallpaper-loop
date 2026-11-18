mod desktop;
mod gpu;
mod mpv;

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

use desktop::{Desktop, Renderer};

#[derive(Parser)]
#[command(version, about = "Play looping videos as Wayland desktop wallpapers")]
struct Args {
    /// Assign a local video to an output; repeat for multiple outputs.
    #[arg(short, long, value_names = ["NAME", "VIDEO"], num_args = 2, required_unless_present = "list_outputs", conflicts_with = "list_outputs")]
    output: Vec<OsString>,

    /// List Wayland output names.
    #[arg(long)]
    list_outputs: bool,

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

    let display = gpu::Display::new(connection.clone())?;
    let api = Rc::new(mpv::Api::load()?);
    desktop.map(&globals, &handle, streams)?;

    let mut reactor = EventLoop::<Desktop>::try_new()?;
    WaylandSource::new(connection, queue)
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

    let renderer = Renderer {
        display,
        api,
        wake,
        hwdec: args.hwdec,
        frame_step: args.frame_step,
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
        reactor.dispatch(remaining, &mut desktop)?;
        if !desktop.stopping() {
            desktop.step(&handle, &renderer)?;
        }
    }
    if args.stats {
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
