# Low-memory animated wallpaper options

The goal is to play the three existing 2560 × 1440 videos at 30 fps per output while reducing the player's CPU and GPU memory use. The [immutable replay](near-zero-cpu.md#live-desktop-experiment) measured 0.047% of one logical core for the player with timed batches, after decoding and uploading a short cache. Streaming the original videos adds decoding and buffer reuse to that workload.

## Memory budget

One tightly packed 1440p NV12 frame is 5.273 MiB. One XR24 frame is 14.063 MiB. These figures cover image payload only. Hardware tiling and alignment, decoder reference images, presentation buffers that are still in use, GPU contexts, and compositor allocations also matter. The installed H.264 clips each declare four reference and two reorder frames in the SPS, observed with `ffmpeg -c:v copy -bsf:v trace_headers`. The four-frame buffering field includes reordering demand. Re-encoding with fewer references and B-frames is an independent memory and latency experiment, with a bitrate or quality trade-off. [DRM format definitions](https://github.com/torvalds/linux/blob/v6.19/include/uapi/drm/drm_fourcc.h), [FFmpeg trace_headers](https://ffmpeg.org/ffmpeg-bitstream-filters.html#trace_005fheaders), [NVDEC buffer sizing](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.1/nvdec-video-decoder-api-prog-guide/index.html)

| Output buffers per monitor | NV12, three monitors | XR24, three monitors |
| ---: | ---: | ---: |
| 4 | 63.3 MiB | 168.8 MiB |
| 8 | 126.6 MiB | 337.5 MiB |
| 12 | 189.8 MiB | 506.3 MiB |
| 24 | 379.7 MiB | 1,012.5 MiB |

Twelve distinct frames queued 400 ms ahead require at least twelve images per output, plus headroom for current and retired frames. The earlier experiment used 24. For a streaming player with a smaller pool, start with a modest queue and coalesce submissions across all monitors. Stop decoding when the pool is full and stop enqueuing when the layer is hidden. Use presentation feedback to tune the actual queue depth. [Wayland DMA-BUF](https://wayland.app/protocols/linux-dmabuf-v1), [FIFO](https://wayland.app/protocols/fifo-v1), [Commit timing](https://wayland.app/protocols/commit-timing-v1)

## Keep the original video

### Vulkan Video with exportable decode surfaces

Decode into a pool of NV12 surfaces allocated with a DRM modifier and DMA-BUF export enabled. Import each reusable buffer into Wayland once and pass acquire and release synchronization along with commits. Query the H.264 profile's format and usage with `vkGetPhysicalDeviceVideoFormatPropertiesKHR`. Then query the exact format, modifier, video usage and external handle type together via `vkGetPhysicalDeviceImageFormatProperties2`. Intersect that result with the surface's DMA-BUF feedback. The decoded picture buffer (DPB) can share output surfaces or require distinct allocation according to driver capabilities. An exportable decode pool requires both decode and export on the same format, usage and modifier tuple. [Video format capability query](https://docs.vulkan.org/refpages/latest/refpages/source/vkGetPhysicalDeviceVideoFormatPropertiesKHR.html), [Vulkan Video and DPB](https://docs.vulkan.org/spec/latest/chapters/videocoding.html), [DRM modifiers in Vulkan](https://docs.vulkan.org/refpages/latest/refpages/source/VK_EXT_image_drm_format_modifier.html), [DMA-BUF export in Vulkan](https://docs.vulkan.org/refpages/latest/refpages/source/VK_EXT_external_memory_dma_buf.html)

The local GStreamer H.264 Vulkan decoder advertises NV12 `VulkanImage` output on the RTX 4090. `waylandsink` advertises `DMABuf` and system-memory input. An export bridge must therefore own the Vulkan allocation or map its exact image layout. FFmpeg 9.0.2's Vulkan decoder selects optimal tiling unless a modifier-tiling frames context is supplied. Its DRM export path queries the image modifier, exports memory fds and handles synchronization. An `hwmap` failure with the default pool exercises that allocation strategy. Query video-decode-plus-export capabilities before deciding whether to supply a custom pool or copy to a separate exportable image. [GStreamer Vulkan decoder](https://gstreamer.freedesktop.org/documentation/vulkan/vulkanh264dec.html), [GStreamer waylandsink](https://gstreamer.freedesktop.org/documentation/waylandsink/index.html), [FFmpeg decoder allocation](https://github.com/FFmpeg/FFmpeg/blob/n9.0.2/libavcodec/vulkan_decode.c#L1238-L1251), [FFmpeg export implementation](https://github.com/FFmpeg/FFmpeg/blob/n9.0.2/libavutil/hwcontext_vulkan.c#L4199-L4297)

### NVIDIA NVDEC with one GPU copy into an exportable pool

The NVDEC API maps an output frame as CUDA memory with a device pointer and pitch. The compositor needs a shareable image with a known fourcc, modifier, plane layout and synchronization. NVIDIA documents address-range DMA-BUF export for allocations created through specific CUDA allocation APIs, with alignment constraints. That contract alone gives no basis for exporting a pointer returned by `cuvidMapVideoFrame` as a Wayland image. A separate pool allocated with GBM and imported through EGL gives ownership of the display-side buffer layout. Register each image with CUDA once and use asynchronous plane copies from NVDEC outputs. A single NV12 copy at 90 aggregate frames/s moves about 475 MiB/s of payload, around 949 MiB/s for the read and write combined. With XR24 output, a GPU conversion reads NV12 and writes XR24, roughly 1,740 MiB/s of payload traffic before alignment or caching. [NVDEC mapping API](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.1/nvdec-video-decoder-api-prog-guide/index.html), [CUDA memory export contract](https://docs.nvidia.com/cuda/archive/13.4.2/cuda-driver-api/cuda_driver_api/group__CUDA__MEM.html), [CUDA/EGL interop](https://docs.nvidia.com/cuda/cuda-driver-api/cuda_driver_api/group__CUDA__EGL.html), [DMA-BUF exchange](https://docs.kernel.org/userspace-api/dma-buf-alloc-exchange.html)

The `gst-cuda-dmabuf` project is a reference for GBM pool setup, EGL registration, NV12 copies and XR24 shader conversion. Its NV12 path copies both planes on the GPU and then synchronizes the stream. Keeping those copies GPU-local removes CPU pixel transfers. Further gains may come from reusing descriptor objects and handing the compositor a fence for a batch rather than waiting on each copy. When using Wayland explicit synchronization, wait for the release of each buffer before rewriting it. Release timeline signals may complete out of order, so independent buffer timelines avoid treating an earlier in-flight buffer as released. [Reference CUDA/EGL registration](https://github.com/Ericky14/gst-cuda-dmabuf/blob/67a964c7411f262c09d77b425c9f3790abc020b1/src/cuda_egl_interop.c#L284-L314), [Reference NV12 copy](https://github.com/Ericky14/gst-cuda-dmabuf/blob/67a964c7411f262c09d77b425c9f3790abc020b1/src/buffer_transform.c#L506-L562), [Wayland explicit synchronization](https://wayland.app/protocols/linux-drm-syncobj-v1), [DMA-BUF synchronization](https://docs.kernel.org/next/driver-api/dma-buf.html)

The third-party NVIDIA VA-API translation driver's direct backend copies from CUDA into exportable backing storage and fills a PRIME descriptor. It reaches private NVIDIA kernel interfaces, which its maintainers describe as unstable. The experiment used the signed Arch build of version 0.0.18, extracted into a temporary directory and loaded only by the test process. The Vulkan Video path used the existing NVIDIA Vulkan driver. [Direct backend design](https://github.com/elFarto/nvidia-vaapi-driver/blob/443050726fdb30888a2ced1cea1c5d036ea3b15c/README.md#direct-backend), [Direct copies and PRIME descriptor](https://github.com/elFarto/nvidia-vaapi-driver/blob/443050726fdb30888a2ced1cea1c5d036ea3b15c/src/direct/direct-export-buf.c#L1083-L1265)

An independent power experiment is setting `CUDA_DISABLE_PERF_BOOST=1` only for the wallpaper process. NVIDIA documents this as a Linux CUDA setting that leaves P-state choice to the driver's heuristics and may increase latency. The installed CUDA library contains the setting name. Recheck actual frame cadence and whole-card power with and without it. [NVIDIA CUDA environment variables](https://docs.nvidia.com/cuda/archive/13.4.2/cuda-programming-guide/05-appendices/environment-variables.html#cuda-disable-perf-boost)

## Change the representation of the scene

**Procedural or 2.5D wallpaper.** Store a static 1440p picture, masks and parameters per monitor. Animate selected regions in a fragment shader, for example with a flow map, noise distortion or depth-aware parallax. Three BC7 base images would occupy 10.5 MiB before masks and output surfaces. A native background pass can sample them while compositing the desktop and skip client submissions. A shader inside an ordinary Wayland client still needs output buffers and submissions. In both cases frame cadence, power and perceived motion need measurement. This option creates new animated assets and is most suitable for effects that can be reconstructed from a few textures. [BC7 block layout](https://docs.vulkan.org/spec/latest/chapters/formats.html#formats-definition), [Hyprland background and texture passes](https://github.com/hyprwm/Hyprland/blob/11cbc6c577b84ffb1edf4df3fc5b6128c3ae6947/src/render/Renderer.cpp#L1186-L1268)

**Frame interpolation.** Decode a lower-rate source, then interpolate to 30 fps using an optical flow engine or a shader. NVIDIA's FRUC uses optical flow plus CUDA and supports NV12 buffers on Linux. It requires consecutive frames and writes synthesized images to output storage. This trades decode rate for GPU computation, frame delay and possible visible artifacts. It has a better rationale for a bandwidth constraint than for these already hardware-decodable H.264 files. [NVIDIA FRUC](https://docs.nvidia.com/video-technologies/optical-flow-sdk/nvfruc-programming-guide/index.html)

**Full texture animation in BC1 or BC7.** Every 4 × 4 block of a BC1 image uses 8 bytes and every BC7 block uses 16 bytes. Caching all 1,665 frames would still use **2.86 GiB in BC1** or **5.72 GiB in BC7**, before mipmaps, alignment, or render targets. The runtime can sample the textures directly, at the cost of per-frame render work and a lossy offline conversion. The formats are convenient for short artwork or posters and do not exploit temporal similarity between video frames. The existing compressed H.264 bitstream already does. [Vulkan compressed image formats](https://docs.vulkan.org/spec/latest/chapters/formats.html#formats-definition), [Khronos KTX2 texture formats](https://github.khronos.org/KTX-Software/pyktx/docs/pyktx.ktx_texture2.html)

## What worked on the RTX 4090

FFmpeg 9.0.2 decoded H.264 with CUDA and Vulkan. Its default Vulkan frames used optimal tiling. Both Vulkan and VA-API failed the CLI's `hwmap=derive_device=drm` step. The C API could export the VA-API frames directly with `av_hwframe_map`. It exported 60 consecutive frames as tiled R8 and RG88 objects. With a custom Vulkan frames pool, the same API exported 60 Vulkan frames in one DMA-BUF each, with separate luma and chroma offsets. The tested NVIDIA tile modifier was `0x0300000000606014`. A Vulkan capability query advertised DMA-BUF export for five tiled NV12 modifiers with video decode destination usage.

The live Wayland registry omitted NV12 while advertising R8, RG88 and XR24. Hyprland still accepted a request to create a combined NV12 buffer. In the red/blue fixture, both bands appeared as dark red, with RGB samples `81,0,0` and `41,0,0` taken from the exposed wallpaper edge. Importing the two planes as EGL textures and converting them to XR24 made full-color presentation possible. The two-band fixture let two shader errors cancel out. A comparison with an actual video frame revealed the row and chroma inversions in the Vulkan prototype. The Rust backend corrects both and verifies them with an asymmetric chart and a software-decoded RGB reference. The prototype retains the original shader and fixed limited-range BT.709 conversion so the historical measurements remain reproducible. [Verified Rust backend and test](../README.md#vulkan-video)

The [bounded playback prototype](../experiments/bounded-video.c) rotates four XR24 buffers per monitor and reuses an output buffer only after Wayland releases it. The combined output payload is **168.8 MiB**, plus decoder frames, driver allocations and compositor storage. The shader samples the luma and chroma textures from DMA-BUFs and uses one GPU completion wait per three-output update. Full frames stay on the GPU. VA-API reuse keys on the DMA-BUF file identity and layout. Vulkan exports yielded a fresh file identity per frame, so Vulkan reuse keys on the native image, decoder pool generation and layout. The Vulkan test with that cache reused 4,812 plane imports out of 4,860.

| Mode | Player CPU | Player RAM (PSS) | Whole GPU memory | Whole GPU power |
| --- | ---: | ---: | ---: | ---: |
| Static control | <0.01% | 126 MiB | 3,278 MiB | 60.6 W |
| Current Rust player with libmpv/NVDEC | 7.65% | 638 MiB | 5,496 MiB | 84.4 W |
| Bounded NVDEC via VA-API, two runs | 2.22–2.25% | 332–334 MiB | 4,172–4,360 MiB | 78.9–80.2 W |
| Bounded NVDEC with CUDA boost disabled, two runs | 3.89–3.94% | 331–335 MiB | 4,158–4,313 MiB | 64.1–64.7 W |
| Bounded Vulkan Video, native image cache | 4.47% | 264 MiB | 3,695 MiB | 64.2 W |

Each playback run lasted 27 seconds with three 1440p30 outputs and used the same full H.264 files. It included an 18-second CPU and GPU measurement window after the layers had mapped and warmed for three seconds. CPU is summed thread runtime from `/proc/<pid>/task/<tid>/schedstat`, expressed as percent of one logical core. GPU memory and power are whole-card readings sampled every second on the live desktop. The static control ran immediately after the cached Vulkan test. Its memory delta to Vulkan was **417 MiB**. Treat cross-run GPU readings as approximate because background workloads can vary.

Every bounded run crossed each clip's loop boundary once. The cached Vulkan test confirmed 810 of 810 presentations per screen, with a maximum inter-frame gap of 42 ms. The VA-API runs confirmed 809–810 presentations per screen with no pool skips and maximum gaps up to 49 ms. Without native image reuse, Vulkan needed 7.59% of a core and still presented all 810 frames per screen.

Vulkan Video with the native image cache gives a useful balance for these files. It preserves their full length and frame rate, uses the existing graphics driver, and keeps both memory and GPU power near the static control in this session. The VA-API path used less player CPU at the cost of extra driver dependence and memory. To lower compositor work further, the remaining paths are native YUV sampling in Hyprland or a compatible hardware overlay. [Earlier compositor investigation](near-zero-cpu.md#scan-out-the-background-on-a-display-plane)

### How much of the existing artwork moves?

For a separate estimate of whether a poster plus animated regions is practical, the test downsampled every frame to 160 × 90 grayscale using area averaging. It compared each location with the first frame and counted locations that stayed within eight levels on the 0–255 luma scale over the entire clip. The stable-area fractions were **75.4% for Aurora Lake**, **67.6% for Blue Mountain Horizons**, and **81.7% for Tranquil Japan Lake View**. This low-resolution luma measure can miss fine motion, color changes and visible seams. It supports trying a poster and procedural ripples for the Japanese lake scene as a distinct artwork option.

### Reproduce the bounded test

Build the C experiment with FFmpeg development libraries, GBM, EGL, OpenGL, Wayland, `wayland-scanner`, and Vulkan headers. Generate client headers and private protocol code for `linux-dmabuf-v1.xml`, `presentation-time.xml`, `xdg-shell.xml` and `wlr-layer-shell-unstable-v1.xml` into a scratch directory. The expected output names are `linux-dmabuf.{h,c}`, `presentation-time.{h,c}`, `xdg-shell.{h,c}` and `layer.{h,c}`.

```sh
cc -O2 -Wall -Wextra -Werror -DWITH_VULKAN -I"$scratch" \
  experiments/bounded-video.c \
  "$scratch/linux-dmabuf.c" "$scratch/presentation-time.c" \
  "$scratch/xdg-shell.c" "$scratch/layer.c" \
  -o "$scratch/bounded-video" \
  $(pkg-config --cflags --libs wayland-client gbm egl gl libavcodec libavformat libavutil)

PROBE_VULKAN=1 PROBE_CACHE_INPUTS=1 \
  "$scratch/bounded-video" /dev/dri/renderD128 27 \
  DP-1 "$HOME/Videos/left.mp4" DP-2 "$HOME/Videos/right.mp4" DP-3 "$HOME/Videos/center.mp4"
```

Use the monitor names from the active Wayland session and 2560 × 1440, 30 fps local files. This prototype requires outputs at scale 1 and a compatible NVIDIA DRM modifier. Stop other wallpaper players during measurement and restart them afterward. For VA-API, omit `PROBE_VULKAN` and point `LIBVA_DRIVERS_PATH` at the NVIDIA driver, set `LIBVA_DRIVER_NAME=nvidia`, and select `NVD_BACKEND=direct`.
