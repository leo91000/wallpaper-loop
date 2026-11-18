# How close to zero CPU can an animated wallpaper get?

The current Rust player keeps three independent video loops in one process and submits an EGL buffer for each new frame. The earlier comparison measured 7.49% of one logical core for the player at 30 fps per display. A predecoded playback experiment can isolate how much CPU remains when the player only schedules and submits buffers. A comparison with a static wallpaper is needed to estimate the remaining compositor cost. [Previous measurements](../README.md#desktop-comparison)

## What the workstation supports

The installed session runs Hyprland 0.57.2 (commit `11cbc6c`) and Aquamarine 0.16.0. Read-only inspection with `hyprctl -i 0 version`, `pacman -Q aquamarine`, and `modetest -M nvidia-drm -p` showed three active 2560 × 1440 CRTCs. Each has a primary plane and an unused overlay plane. For all three overlays, NVIDIA advertises XR24, AR24, and NV12, including the linear modifier, and exposes alpha and pixel-blend properties. The listing did not expose an adjustable `zpos` property. Plane capability listings establish advertised formats. A successful atomic test of the complete output state would establish whether the driver accepts a particular plane assignment and mode combination. [KMS plane properties](https://docs.kernel.org/gpu/drm-kms.html#standard-plane-properties), [Atomic state checks](https://docs.kernel.org/gpu/drm-kms.html#atomic-mode-setting)

The live Wayland registry advertised `zwp_linux_dmabuf_v1` v5, `wp_presentation` v2, `wp_fifo_manager_v1` v1 and `wp_commit_timing_manager_v1` v1. This was checked by running `WAYLAND_DEBUG=client wallpaper-loop --list-outputs` in the session. `hyprctl -i 0 getoption render:direct_scanout -j` reported the default value of 0. [Hyprland protocol registration](https://github.com/hyprwm/Hyprland/blob/11cbc6c577b84ffb1edf4df3fc5b6128c3ae6947/src/managers/ProtocolManager.cpp#L220-L229), [Direct scanout config definition](https://github.com/hyprwm/Hyprland/blob/11cbc6c577b84ffb1edf4df3fc5b6128c3ae6947/src/config/values/ConfigValues.cpp#L666)

## What each approach changes

### Cache immutable frames and replay them as DMA-BUFs

The client can decode and import a small frame cache upfront. During replay it selects an existing `wl_buffer`, attaches it and commits the surface. Safe reuse follows Wayland's buffer lifetime rules. This path can remove the client's steady-state decoding and EGL render loop. The compositor still imports or samples each frame as needed, and its scanout feedback only indicates a possible scanout candidate. On this Hyprland version each mapped background-layer commit also sets the blur framebuffer dirty and damages the layer's whole geometry. The compositor's render pass can cull regions under opaque content, with exceptions such as live blur. [Wayland surface and buffer protocol](https://wayland.freedesktop.org/docs/html/apa.html), [Linux DMA-BUF specification](https://wayland.app/protocols/linux-dmabuf-v1), [Hyprland layer commit](https://github.com/hyprwm/Hyprland/blob/11cbc6c577b84ffb1edf4df3fc5b6128c3ae6947/src/desktop/view/LayerSurface.cpp#L316-L339), [Hyprland occlusion and blur](https://github.com/hyprwm/Hyprland/blob/11cbc6c577b84ffb1edf4df3fc5b6128c3ae6947/src/render/pass/Pass.cpp#L41-L100)

There are 1,665 frames across the three installed 30 fps loops, counted with `ffprobe`. A full cache of their 2560 × 1440 frames has an unpadded pixel payload of about **8.6 GiB in NV12** or **22.9 GiB in XRGB8888**, before driver alignment and other allocations. A short cache is a useful bound on playback CPU. A bounded rolling DMA-BUF pool, with NVDEC refilling it at source rate, would keep the full loops practical. Whether NVIDIA can export the decoder's actual surface layout in a format Hyprland will import is a separate question from whether the DRM planes advertise NV12. An explicit GPU conversion into exportable buffers is another design to measure. [DRM pixel format definitions](https://github.com/torvalds/linux/blob/v6.19/include/uapi/drm/drm_fourcc.h), [DMA-BUF frame layout and modifiers](https://gstreamer.freedesktop.org/documentation/additional/design/dmabuf.html)

FIFO and commit timing let the client attach, timestamp and commit several prefetched buffers during one wakeup. A FIFO barrier keeps successive visible updates from latching in the same refresh, and commit timing gives an earliest presentation time in the compositor's presentation clock. Hyprland locks timed surface states, releasing them from a timer or on an upcoming flip. This shifts scheduling to Hyprland. The experiment below compares one-at-a-time submission with batches of 12 frames. [FIFO specification](https://wayland.app/protocols/fifo-v1), [Commit timing specification](https://wayland.app/protocols/commit-timing-v1), [Hyprland commit timers](https://github.com/hyprwm/Hyprland/blob/11cbc6c577b84ffb1edf4df3fc5b6128c3ae6947/src/protocols/CommitTiming.cpp#L43-L85)

### Draw the background inside Hyprland

Hyprland already has a native texture pass for its static background. Its renderer builds the background, then emits `RENDER_POST_WALLPAPER` with a render context before bottom layers. A C++ prototype plugin can add a texture pass there using the matching Hyprland headers. A native feature could put the texture source directly into `renderBackground`. In either case the compositor would need to track which frame is due and damage visible regions so it schedules output updates. Hardware decoding or preloading should stay outside the hot render callback. This can remove the separate Wayland surface and client-side draw/submit costs. The GPU still has to sample the wallpaper with the rest of the scene. The plugin runs inside Hyprland and uses its version-sensitive C++ ABI, so an isolated compositor would be the appropriate first test. [Hyprland background pass and hook](https://github.com/hyprwm/Hyprland/blob/11cbc6c577b84ffb1edf4df3fc5b6128c3ae6947/src/render/Renderer.cpp#L1186-L1268), [Native background texture](https://github.com/hyprwm/Hyprland/blob/11cbc6c577b84ffb1edf4df3fc5b6128c3ae6947/src/render/Renderer.cpp#L1464-L1509), [Hyprland plugin documentation](https://wiki.hypr.land/hyprland-plugins/development/getting-started/)

### Scan out the background on a display plane

Hyprland 0.57.2 selects its direct-scanout candidate from the solitary fullscreen window. An ordinary background layer follows the compositor's render pass, even when its DMA-BUF is compatible with the GPU. Aquamarine 0.16.0 programs the main and cursor planes in its output commit path. Using the additional plane would require coordinated compositor and backend changes. A possible experiment would try the wallpaper on a lower plane and an alpha-capable desktop buffer above it, using atomic test-only checks for the exact buffers, blend modes and display configuration. Effects that sample the wallpaper, workspace capture, color transforms and lock transitions would need a composited fallback. The hardware listing confirms an unused plane on each active head, with successful blending and the required stacking still open. [Hyprland scanout candidate selection](https://github.com/hyprwm/Hyprland/blob/11cbc6c577b84ffb1edf4df3fc5b6128c3ae6947/src/output/Monitor.cpp#L1696-L1829), [Hyprland scanout buffer requirements](https://github.com/hyprwm/Hyprland/blob/11cbc6c577b84ffb1edf4df3fc5b6128c3ae6947/src/output/Monitor.cpp#L1953-L1986), [Aquamarine plane commits](https://github.com/hyprwm/aquamarine/blob/b77d67648336223a6c8fb88844ae316e7b72b574/src/backend/drm/impl/Atomic.cpp#L278-L370)

A KMS plane scans out its selected framebuffer on every refresh. Changing the framebuffer involves a commit. The documented atomic API commits a new plane state, with fences and completion events for that update. A prerecorded sequence still requires software to advance the selected framebuffer on this path. Direct scanout would remove composition of wallpaper-only changes if the plane configuration supports it. It would leave the scheduling and KMS submission work. [KMS planes and scanout](https://docs.kernel.org/gpu/drm-kms.html#plane-abstraction), [DRM atomic UAPI](https://github.com/torvalds/linux/blob/v6.19/include/uapi/drm/drm_mode.h)

## Live desktop experiment

The [Rust experiment](../experiments/dmabuf-replay.rs) decoded the first 24 frames of each existing wallpaper before measuring playback. It uploaded the frames once into NVIDIA GBM XR24 buffers through EGL, then exported them as Wayland DMA-BUFs. The device provided a CPU-mappable linear buffer during an allocation probe, while the live compositor advertised the NVIDIA tiled modifier used by the successful experiment. The upload context and decoder were closed before playback. The resulting cache held 72 immutable buffers and repeated 0.8 seconds of each video at native resolution and cadence.

| Mode | Player CPU | Hyprland CPU | Whole GPU power | Whole GPU memory |
| --- | ---: | ---: | ---: | ---: |
| Static, two controls | 0.00% | 4.24–5.83% | 59.2–59.6 W | 2,482–2,686 MiB |
| wallpaper-loop with NVDEC | 7.41% | 7.82% | 81.0 W | 5,133 MiB |
| Predecoded DMA-BUFs, one frame per submission cycle | 0.17% | 6.57% | 60.3 W | 3,700 MiB |
| Predecoded DMA-BUFs, batches of 12 | Below tick resolution | 7.07% | 60.6 W | 3,752 MiB |

Each row used a 12-second steady-state sample, following a two-second warm-up after the three layers mapped. CPU came from tick deltas in `/proc/<pid>/stat`, where 100% is one logical core. The samples resolved about 0.08 percentage points of CPU. NVIDIA metrics are whole-card readings at one-second intervals on a live desktop. Other activity can change these values.

In a second run, `CLOCK_PROCESS_CPUTIME_ID` measured **31.11 ms of CPU over 17.32 s (0.180%)** with one frame per cycle and **8.06 ms over 17.30 s (0.047%)** with timed batches. Hyprland used 6.24% and 5.90% of a core respectively during the sampled windows. A batch of 12 at 30 fps wakes the player roughly every 400 ms.

Both replay modes received `wp_presentation.presented` for all 510 updates on every output during 17 seconds of playback, in both runs, with zero discarded buffers and an average near 30 fps. The longest intervals were about 38–42 ms, depending on the output and run. All feedback arrived with the `zero_copy` flag unset. A preliminary single-screen batch run with the other wallpaper player still active presented 140 of 150 updates. [Presentation time specification](https://wayland.app/protocols/presentation-time)

With a short cache, client CPU was close to zero and GPU power was close to the static control in this run. Hyprland still composited the changing layers. A full-length cache has a much higher memory cost. A bounded rolling decoder pool and native Hyprland damage tracking address different parts of that remaining trade-off.

### Reproduce the replay

Use a Wayland session offering layer shell, presentation feedback, and Linux DMA-BUF. The batch mode also needs FIFO and commit timing. The standalone script requires nightly Rust, FFmpeg, and development libraries for GBM, EGL, OpenGL and Wayland. Use outputs configured at 2560 × 1440 and scale 1. Stop other wallpaper players during measurement and restart them afterward.

```sh
cargo +nightly -Zscript run --release --manifest-path experiments/dmabuf-replay.rs -- \
  /dev/dri/renderD128 17 24 12 \
  DP-1 "$HOME/Videos/left.mp4" \
  DP-2 "$HOME/Videos/right.mp4" \
  DP-3 "$HOME/Videos/center.mp4"
```

The arguments are the render node, measured duration in seconds, cached frame count, batch size, and one video per named output. Set the batch size to `1` for per-frame submission. Playback CPU timing starts after `ready`, and the final report includes presentation counts and the maximum interval between presentations.
