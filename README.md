# wallpaper-loop

An experimental Rust video wallpaper player for Linux and Wayland. One process plays a separate muted loop on each selected output. libmpv handles decoding and renders through a shared OpenGL context. The player waits for Wayland and libmpv events and uses a Wayland frame callback to pace submissions.

## Build

Install a recent stable Rust toolchain, `pkg-config`, the Wayland client and Wayland EGL development libraries, and an OpenGL-capable EGL driver. Playback also needs `libmpv.so.2` on the system library path. The compositor must support `wlr-layer-shell` and named Wayland outputs.

```sh
cargo build --release --locked
target/release/wallpaper-loop --list-outputs
```

## Play

```sh
target/release/wallpaper-loop \
  --output DP-1 "$HOME/Videos/lake.mp4" \
  --output DP-2 "$HOME/Videos/mountains.mp4"
```

Repeat `--output NAME VIDEO` for each display. Videos loop, are muted, and fill their output with center cropping when needed. Close the process with Ctrl-C. Stop other animated wallpaper players first when comparing performance.

- `--hwdec auto` selects hardware decoding when available. Check the `decoder=` line for each output. For example, `decoder=nvdec` indicates NVIDIA hardware decoding and `decoder=no` indicates software decoding.
- `--frame-step 2` presents approximately 15 frames per second from a 30 fps source. It skips rendering and submission of alternate frames while libmpv keeps decoding at the source rate.
- `--duration 30 --stats` runs for 30 seconds and prints the presentation rate and skipped frames for each output.

The player currently uses fixed output assignments. A selected output disappearing ends the session. Fullscreen and display-power pause policies are outside this prototype.

## Desktop comparison

Three independent 2560 × 1440 H.264 loops on a Core i9-14900K, RTX 4090 and Hyprland 0.57.2. All playback used the same libmpv 0.42 libraries with NVDEC active. The comparison used the existing local mpvpaper build (`fc5ed28`, `video-sync=display-resample`) and the Rust player with its default timing.

| Player | Output rate per monitor | Player CPU | Player memory (PSS) |
| --- | ---: | ---: | ---: |
| mpvpaper | 30 fps source | 8.12% | 710 MiB |
| wallpaper-loop | 30 fps submitted | 7.49% | 598 MiB |
| wallpaper-loop `--frame-step 2` | 15 fps submitted | 6.24% | 596 MiB |

The 30 fps rows average two 16-second samples per player. The 15 fps row uses one 16-second sample. Each sample started after a five-second warm-up. CPU is user and system time from `/proc/<pid>/stat` divided by elapsed time and summed over all player processes. 100% represents one logical core. Memory is summed proportional set size from `/proc/<pid>/smaps_rollup` at the end of each sample. The compositor load and the power of the whole GPU varied during the live desktop test, so the table reports player-side results.

At the native frame rate, the measured player-side savings were about 8% CPU and 16% PSS on this workstation. Using one OpenGL context per output gave almost identical player CPU in a separate sample (7.49%).

## Technical references

- [libmpv render API](https://github.com/mpv-player/mpv/blob/master/include/mpv/render.h)
- [Wayland surface and frame callback protocol](https://wayland.freedesktop.org/docs/html/apa.html)
- [wlr-layer-shell protocol](https://wayland.app/protocols/wlr-layer-shell-unstable-v1)
- [mpvpaper, the baseline used in the desktop comparison](https://github.com/GhostNaN/mpvpaper)
