# wallpaper-loop

A Rust video wallpaper player for Linux and Wayland. One process plays a separate muted loop on each selected output. The default backend uses libmpv. The optional Vulkan Video backend uses FFmpeg hardware decoding and a bounded pool of Wayland DMA-BUFs.

## Build

Install a recent stable Rust toolchain, `pkg-config`, the Wayland client and Wayland EGL development libraries, and an OpenGL-capable EGL driver. The compositor must support `wlr-layer-shell` and named Wayland outputs. The mpv backend needs `libmpv.so.2` on the system library path.

```sh
cargo build --release --locked
target/release/wallpaper-loop --list-outputs
```

To enable Vulkan Video, also install FFmpeg 9 development libraries with Vulkan Video and DRM support, libclang, libgbm development libraries and a Vulkan driver with hardware H.264 decoding.

```sh
cargo build --release --locked --features vulkan
```

## Play

```sh
target/release/wallpaper-loop \
  --output DP-1 "$HOME/Videos/lake.mp4" \
  --output DP-2 "$HOME/Videos/mountains.mp4"
```

Repeat `--output NAME VIDEO` for each display. Videos loop, are muted, and fill their output with center cropping when needed. Close the process with Ctrl-C. Stop other animated wallpaper players first when comparing performance.

- `--hwdec auto` selects hardware decoding when available. Check the `decoder=` line for each output. For example, `decoder=nvdec` indicates NVIDIA hardware decoding and `decoder=no` indicates software decoding.
- `--frame-step 2` presents approximately 15 frames per second from a 30 fps source. The decoder still consumes every frame.
- `--duration 30 --stats` runs for 30 seconds and prints the presentation rate and skipped frames for each output.

The player uses fixed output assignments. A selected output disappearing ends the session. Fullscreen pause policies can be managed by the session.

### Vulkan Video

```sh
target/release/wallpaper-loop --backend vulkan --stats \
  --output DP-1 "$HOME/Videos/lake.mp4" \
  --output DP-2 "$HOME/Videos/mountains.mp4"
```

The Vulkan backend supports local, even-sized 8-bit H.264 SDR videos with 4:2:0 chroma and BT.601 or BT.709 color matrices. It uses video timestamps for pacing, loops at end of stream, honors the frame's limited or full range, and center-crops to the output geometry. Use `--backend mpv` for other media formats, HDR or transformations that require libmpv's video processing.

The renderer uses the DRM render node for both decoding and presentation. On a machine with multiple render nodes, select the display GPU with `--render-node /dev/dri/renderD128` or the matching symlink in `/dev/dri/by-path`. The Wayland compositor must expose Linux DMA-BUF v3 and XR24 with a modifier that EGL can render into. Vulkan Video with an exportable NV12 pool and a pair of EGL plane views was validated on an RTX 4090 with Hyprland 0.57.2. Other driver and compositor combinations need their own validation.

On NVIDIA, Vulkan Video uses the card's hardware video decoder through the graphics driver. The Vulkan setup above is sufficient. `CUDA_DISABLE_PERF_BOOST=1` was tested on a separate CUDA-backed prototype. It lowered whole-card power from about 79–80 W to 64–65 W while increasing the prototype's CPU load. [CUDA experiment](docs/low-memory-alternatives.md#what-worked-on-the-rtx-4090)

Four output buffers per display are reused after Wayland releases them. At 1440p, their pixel payload totals about 169 MiB for three displays, plus alignment, decoder storage and driver/compositor allocations. Frames stay in GPU memory during decoding and conversion. With presentation feedback available, `--stats` reports submissions, actual display confirmations, discarded commits, completed loops and the longest interval between presentations.

A session helper can pass `--pause-socket PATH` and send Unix datagrams containing `pause` or `resume`, a tab, and a Wayland output name. A systemd service can use `Type=notify` to wait for the first submission on every selected output.

The hardware regression test generates an asymmetric H.264 color chart, then compares the Vulkan result with FFmpeg's software RGB output. It requires FFmpeg with libx264 and the same Vulkan/EGL stack as the player.

```sh
cargo test --locked --features vulkan
WALLPAPER_TEST_RENDER_NODE=/dev/dri/renderD128 \
  cargo test --release --locked --features vulkan renders_asymmetric_color_chart -- --ignored
```

On the three-display validation setup, a 140-second run presented all 4,200 frames per display at 30 fps, with zero discarded commits and 6–10 completed loops per video. The process held 147 file descriptors throughout the sampled run. The player used about 5.4% of a logical core during the sampled steady-state interval. Six desktop RGB samples from a frozen Aurora Lake frame matched the libmpv backend within one level per channel.

## Desktop comparison

These measurements use three independent 2560 × 1440 H.264 loops on a Core i9-14900K, RTX 4090 and Hyprland 0.57.2. The mpv rows used libmpv 0.42 and NVDEC. The Vulkan row was taken after installation, with all three outputs playing.

| Player | Output rate per monitor | Player CPU | Player memory (PSS) |
| --- | ---: | ---: | ---: |
| mpvpaper | 30 fps source | 8.12% | 710 MiB |
| wallpaper-loop with libmpv | 30 fps submitted | 7.49% | 598 MiB |
| wallpaper-loop with libmpv and `--frame-step 2` | 15 fps submitted | 6.24% | 596 MiB |
| wallpaper-loop with Vulkan Video | 30 fps | **5.24%** | **382 MiB** |

At 30 fps, the installed Vulkan player used about **35% less player CPU** and **46% less player RAM** than the earlier mpvpaper baseline. The two 30 fps mpv rows average two 16-second samples per player. The 15 fps row uses one 16-second sample. The installed Vulkan row uses a later 12-second sample. CPU is summed user and system time from `/proc/<pid>/stat`, with 100% representing one logical core. Memory is summed proportional set size (PSS) from `/proc/<pid>/smaps_rollup`.

In a separate prototype run, the whole RTX 4090 averaged about 64 W with bounded Vulkan playback and 84 W with the Rust libmpv backend. Whole-card readings include the compositor and other GPU activity. [Power measurements and methodology](docs/low-memory-alternatives.md#what-worked-on-the-rtx-4090)

## Research and experiments

- [Predecoded DMA-BUF replay](docs/near-zero-cpu.md) measures presentation CPU and the memory cost of caching raw frames.
- [Bounded decoding and alternative approaches](docs/low-memory-alternatives.md) covers the Vulkan and CUDA-backed experiments, the import and color findings, and ways to reduce compositor work.

## Technical references

- [libmpv render API](https://github.com/mpv-player/mpv/blob/master/include/mpv/render.h)
- [Wayland surface and frame callback protocol](https://wayland.freedesktop.org/docs/html/apa.html)
- [wlr-layer-shell protocol](https://wayland.app/protocols/wlr-layer-shell-unstable-v1)
- [mpvpaper, the baseline used in the desktop comparison](https://github.com/GhostNaN/mpvpaper)
