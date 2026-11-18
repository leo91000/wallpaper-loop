/* Fixed-size experiment for hardware decoding into reusable Wayland buffers. */
#define _POSIX_C_SOURCE 200809L
#define GL_GLEXT_PROTOTYPES
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GL/gl.h>
#include <errno.h>
#include <fcntl.h>
#include <gbm.h>
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_drm.h>
#ifdef WITH_VULKAN
#include <libavutil/hwcontext_vulkan.h>
#endif
#include "layer.h"
#include "linux-dmabuf.h"
#include "presentation-time.h"
#include <poll.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>
#include <wayland-client.h>

enum { W = 2560, H = 1440, SLOTS = 4, NS = 1000000000 };
#define FOURCC(a, b, c, d)                                                                         \
    ((uint32_t)(a) | (uint32_t)(b) << 8 | (uint32_t)(c) << 16 | (uint32_t)(d) << 24)
#define XR24 FOURCC('X', 'R', '2', '4')
#define NV12 FOURCC('N', 'V', '1', '2')
#define R8 FOURCC('R', '8', ' ', ' ')
#define RG88 FOURCC('R', 'G', '8', '8')
#define GR88 FOURCC('G', 'R', '8', '8')

static void must(bool ok, const char *message) {
    if (!ok) {
        fprintf(stderr, "%s\n", message);
        exit(1);
    }
}
static int av(int result, const char *operation) {
    if (result < 0) {
        fprintf(stderr, "%s: %s\n", operation, av_err2str(result));
        exit(1);
    }
    return result;
}
static int64_t now(clockid_t clock) {
    struct timespec t;
    must(clock_gettime(clock, &t) == 0, "clock_gettime");
    return (int64_t)t.tv_sec * NS + t.tv_nsec;
}
struct Decoder {
    AVFormatContext *file;
    AVCodecContext *codec;
    AVPacket *packet;
    AVFrame *frame, *drm;
    int video;
    enum AVPixelFormat pixel_format;
    bool draining;
    unsigned loops;
    unsigned pool_generation;
};
static enum AVPixelFormat select_format(AVCodecContext *ctx, const enum AVPixelFormat *options) {
    struct Decoder *d = ctx->opaque;
    for (; *options != AV_PIX_FMT_NONE; options++) {
        if (*options != d->pixel_format)
            continue;
#ifdef WITH_VULKAN
        if (*options == AV_PIX_FMT_VULKAN) {
            AVBufferRef *pool = NULL;
            av(avcodec_get_hw_frames_parameters(ctx, ctx->hw_device_ctx, *options, &pool),
               "get Vulkan pool parameters");
            AVHWFramesContext *frames = (AVHWFramesContext *)pool->data;
            AVVulkanFramesContext *native = frames->hwctx;
            static const uint64_t drm_modifier = UINT64_C(0x0300000000606014);
            // This prototype keeps these small extension records alive for the process lifetime.
            VkImageDrmFormatModifierListCreateInfoEXT *mod = av_malloc(sizeof *mod);
            must(mod != NULL, "allocate Vulkan modifier extension");
            *mod = (VkImageDrmFormatModifierListCreateInfoEXT){
                .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT,
                .pNext = native->create_pnext,
                .drmFormatModifierCount = 1,
                .pDrmFormatModifiers = &drm_modifier,
            };
            native->tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT;
            native->create_pnext = mod;
            av(av_hwframe_ctx_init(pool), "initialize exportable Vulkan pool");
            av_buffer_unref(&ctx->hw_frames_ctx);
            ctx->hw_frames_ctx = pool;
            d->pool_generation++;
        }
#endif
        return *options;
    }
    return AV_PIX_FMT_NONE;
}
static void open_video(struct Decoder *d, AVBufferRef *device, const char *path) {
    const AVCodec *codec;
    av(avformat_open_input(&d->file, path, NULL, NULL), "open video");
    av(avformat_find_stream_info(d->file, NULL), "stream info");
    d->video =
        av(av_find_best_stream(d->file, AVMEDIA_TYPE_VIDEO, -1, -1, &codec, 0), "select stream");
    AVStream *stream = d->file->streams[d->video];
    must(stream->codecpar->width == W && stream->codecpar->height == H &&
             av_cmp_q(av_guess_frame_rate(d->file, stream, NULL), (AVRational){30, 1}) == 0,
         "expected 1440p30");
    d->codec = avcodec_alloc_context3(codec);
    d->packet = av_packet_alloc();
    d->frame = av_frame_alloc();
    d->drm = av_frame_alloc();
    must(d->codec && d->packet && d->frame && d->drm, "allocate decoder objects");
    av(avcodec_parameters_to_context(d->codec, stream->codecpar), "decoder parameters");
    d->codec->hw_device_ctx = av_buffer_ref(device);
    const AVHWDeviceContext *hardware = (const AVHWDeviceContext *)device->data;
    d->pixel_format =
        hardware->type == AV_HWDEVICE_TYPE_VULKAN ? AV_PIX_FMT_VULKAN : AV_PIX_FMT_VAAPI;
    d->codec->get_format = select_format;
    d->codec->opaque = d;
    d->codec->thread_count = 1;
    av(avcodec_open2(d->codec, codec, NULL), "open VAAPI decoder");
}
static const AVDRMFrameDescriptor *decode(struct Decoder *d) {
    av_frame_unref(d->drm);
    av_frame_unref(d->frame);
    for (;;) {
        int result = avcodec_receive_frame(d->codec, d->frame);
        if (result == 0)
            break;
        if (result == AVERROR_EOF && d->draining) {
            av(av_seek_frame(d->file, d->video, 0, AVSEEK_FLAG_BACKWARD), "loop video");
            avcodec_flush_buffers(d->codec);
            d->draining = false;
            d->loops++;
            continue;
        }
        if (result != AVERROR(EAGAIN))
            av(result, "receive frame");
        for (;;) {
            result = av_read_frame(d->file, d->packet);
            if (result < 0 || d->packet->stream_index == d->video)
                break;
            av_packet_unref(d->packet);
        }
        if (result == AVERROR_EOF) {
            av(avcodec_send_packet(d->codec, NULL), "drain video");
            d->draining = true;
        } else {
            av(result, "read video");
            av(avcodec_send_packet(d->codec, d->packet), "submit packet");
            av_packet_unref(d->packet);
        }
    }
    d->drm->format = AV_PIX_FMT_DRM_PRIME;
    av(av_hwframe_map(d->drm, d->frame, AV_HWFRAME_MAP_READ | AV_HWFRAME_MAP_DIRECT),
       "export DRM PRIME");
    const AVDRMFrameDescriptor *desc = (const void *)d->drm->data[0];
    must(desc->nb_layers == 2 && desc->layers[0].format == R8 &&
             (desc->layers[1].format == RG88 || desc->layers[1].format == GR88) &&
             desc->layers[0].nb_planes == 1 && desc->layers[1].nb_planes == 1,
         "expected two NV12 planes");
    return desc;
}

struct Slot {
    struct gbm_bo *bo;
    EGLImage image;
    GLuint tex, fbo;
    struct wl_buffer *buffer;
    AVFrame *source;
    bool busy;
};
struct Screen {
    const char *name;
    struct Decoder decoder;
    struct wl_surface *surface;
    struct zwlr_layer_surface_v1 *role;
    struct Slot slots[SLOTS];
    uint32_t width, height;
    bool closed;
    unsigned presented, discarded, skipped, zero_copy;
    int64_t last_flip, max_gap;
};
struct ImportedPlane {
    dev_t dev;
    ino_t inode;
    const struct Decoder *vk_owner;
    uint64_t vk_image;
    unsigned vk_generation;
    ptrdiff_t offset, pitch;
    uint64_t modifier, last_use;
    uint32_t format;
    GLuint texture;
    EGLImage image;
};
struct Monitor {
    struct wl_output *output;
    char name[64];
};
static struct {
    struct wl_display *display;
    struct wl_compositor *compositor;
    struct zwlr_layer_shell_v1 *shell;
    struct zwp_linux_dmabuf_v1 *dmabuf;
    struct wp_presentation *presentation;
    struct Monitor monitors[16];
    unsigned monitor_count;
    struct gbm_device *gbm;
    EGLDisplay egl;
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC bind_image;
    GLuint input[2];
    GLint swap_uv;
    bool reuse;
    struct ImportedPlane imported[48];
    uint64_t uses, hits, misses;
} app;

static void output_name(void *data, struct wl_output *proxy, const char *name) {
    (void)proxy;
    struct Monitor *monitor = data;
    snprintf(monitor->name, sizeof monitor->name, "%s", name);
}
static void geometry(void *d, struct wl_output *o, int32_t x, int32_t y, int32_t w, int32_t h,
                     int32_t subpixel, const char *make, const char *model, int32_t transform) {
    (void)d;
    (void)o;
    (void)x;
    (void)y;
    (void)w;
    (void)h;
    (void)subpixel;
    (void)make;
    (void)model;
    (void)transform;
}
static void mode(void *d, struct wl_output *o, uint32_t flags, int32_t w, int32_t h,
                 int32_t refresh) {
    (void)d;
    (void)o;
    (void)flags;
    (void)w;
    (void)h;
    (void)refresh;
}
static void done(void *d, struct wl_output *o) {
    (void)d;
    (void)o;
}
static void scale(void *d, struct wl_output *o, int32_t factor) {
    (void)d;
    (void)o;
    (void)factor;
}
static void description(void *d, struct wl_output *o, const char *text) {
    (void)d;
    (void)o;
    (void)text;
}
static const struct wl_output_listener monitor_events = {geometry, mode,        done,
                                                         scale,    output_name, description};
static void global(void *data, struct wl_registry *registry, uint32_t id, const char *iface,
                   uint32_t version) {
    (void)data;
    if (!strcmp(iface, "wl_compositor") && version >= 4)
        app.compositor = wl_registry_bind(registry, id, &wl_compositor_interface, 4);
    else if (!strcmp(iface, "zwlr_layer_shell_v1"))
        app.shell = wl_registry_bind(registry, id, &zwlr_layer_shell_v1_interface, 1);
    else if (!strcmp(iface, "zwp_linux_dmabuf_v1") && version >= 3)
        app.dmabuf = wl_registry_bind(registry, id, &zwp_linux_dmabuf_v1_interface, 3);
    else if (!strcmp(iface, "wp_presentation"))
        app.presentation = wl_registry_bind(registry, id, &wp_presentation_interface, 1);
    else if (!strcmp(iface, "wl_output") && version >= 4 && app.monitor_count < 16) {
        struct Monitor *monitor = &app.monitors[app.monitor_count++];
        monitor->output = wl_registry_bind(registry, id, &wl_output_interface, 4);
        wl_output_add_listener(monitor->output, &monitor_events, monitor);
    }
}
static void removed(void *d, struct wl_registry *r, uint32_t id) {
    (void)d;
    (void)r;
    (void)id;
}
static const struct wl_registry_listener registry_events = {global, removed};
static void roundtrip(void) {
    must(wl_display_roundtrip(app.display) >= 0, "Wayland roundtrip");
}
static void flush(void) {
    must(wl_display_flush(app.display) >= 0 || errno == EAGAIN, "Wayland flush");
}
static void connect_wayland(void) {
    app.display = wl_display_connect(NULL);
    must(app.display != NULL, "connect Wayland");
    wl_registry_add_listener(wl_display_get_registry(app.display), &registry_events, NULL);
    roundtrip();
    roundtrip();
    must(app.compositor && app.shell && app.dmabuf && app.presentation,
         "missing Wayland interface");
}
static void configure(void *data, struct zwlr_layer_surface_v1 *role, uint32_t serial, uint32_t w,
                      uint32_t h) {
    struct Screen *screen = data;
    screen->width = w;
    screen->height = h;
    zwlr_layer_surface_v1_ack_configure(role, serial);
}
static void closed(void *data, struct zwlr_layer_surface_v1 *role) {
    (void)role;
    ((struct Screen *)data)->closed = true;
}
static const struct zwlr_layer_surface_v1_listener layer_events = {configure, closed};
static void release_buffer(void *data, struct wl_buffer *buffer) {
    struct Slot *slot = data;
    slot->busy = false;
    if (slot->source) {
        wl_buffer_destroy(buffer);
        slot->buffer = NULL;
        av_frame_unref(slot->source);
    }
}
static const struct wl_buffer_listener buffer_events = {release_buffer};
static void presented(void *data, struct wp_presentation_feedback *feedback, uint32_t high,
                      uint32_t low, uint32_t ns, uint32_t refresh, uint32_t seq_hi, uint32_t seq_lo,
                      uint32_t flags) {
    (void)refresh;
    (void)seq_hi;
    (void)seq_lo;
    struct Screen *screen = data;
    int64_t time = (((int64_t)high << 32) | low) * NS + ns;
    if (screen->last_flip && time - screen->last_flip > screen->max_gap)
        screen->max_gap = time - screen->last_flip;
    screen->last_flip = time;
    screen->presented++;
    screen->zero_copy += !!(flags & WP_PRESENTATION_FEEDBACK_KIND_ZERO_COPY);
    wp_presentation_feedback_destroy(feedback);
}
static void discarded(void *data, struct wp_presentation_feedback *feedback) {
    ((struct Screen *)data)->discarded++;
    wp_presentation_feedback_destroy(feedback);
}
static void sync_output(void *d, struct wp_presentation_feedback *f, struct wl_output *o) {
    (void)d;
    (void)f;
    (void)o;
}
static const struct wp_presentation_feedback_listener present_events = {sync_output, presented,
                                                                        discarded};

static void pump(void) {
    must(wl_display_dispatch_pending(app.display) >= 0, "Wayland dispatch");
    flush();
    if (wl_display_prepare_read(app.display) == 0) {
        struct pollfd fd = {.fd = wl_display_get_fd(app.display), .events = POLLIN};
        if (poll(&fd, 1, 0) > 0)
            must(wl_display_read_events(app.display) == 0, "Wayland read");
        else
            wl_display_cancel_read(app.display);
    }
    must(wl_display_dispatch_pending(app.display) >= 0, "Wayland events");
}
static void open_layer(struct Screen *screen) {
    struct wl_output *output = NULL;
    for (unsigned i = 0; i < app.monitor_count; i++)
        if (!strcmp(screen->name, app.monitors[i].name))
            output = app.monitors[i].output;
    must(output != NULL, "output not found");
    screen->surface = wl_compositor_create_surface(app.compositor);
    struct wl_region *empty = wl_compositor_create_region(app.compositor);
    wl_surface_set_input_region(screen->surface, empty);
    wl_region_destroy(empty);
    struct wl_region *opaque = wl_compositor_create_region(app.compositor);
    wl_region_add(opaque, 0, 0, W, H);
    wl_surface_set_opaque_region(screen->surface, opaque);
    wl_region_destroy(opaque);
    screen->role = zwlr_layer_shell_v1_get_layer_surface(app.shell, screen->surface, output,
                                                         ZWLR_LAYER_SHELL_V1_LAYER_BACKGROUND,
                                                         "bounded-nvdec-probe");
    zwlr_layer_surface_v1_add_listener(screen->role, &layer_events, screen);
    zwlr_layer_surface_v1_set_size(screen->role, 0, 0);
    zwlr_layer_surface_v1_set_exclusive_zone(screen->role, -1);
    zwlr_layer_surface_v1_set_anchor(screen->role, 15);
    wl_surface_commit(screen->surface);
}

static EGLImage import(uint32_t format, int w, int h, int fd, ptrdiff_t offset, ptrdiff_t pitch,
                       uint64_t mod) {
    const EGLAttrib attrs[] = {EGL_WIDTH,
                               w,
                               EGL_HEIGHT,
                               h,
                               EGL_LINUX_DRM_FOURCC_EXT,
                               format,
                               EGL_DMA_BUF_PLANE0_FD_EXT,
                               fd,
                               EGL_DMA_BUF_PLANE0_OFFSET_EXT,
                               offset,
                               EGL_DMA_BUF_PLANE0_PITCH_EXT,
                               pitch,
                               EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                               (uint32_t)mod,
                               EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                               mod >> 32,
                               EGL_NONE};
    EGLImage image = eglCreateImage(app.egl, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, attrs);
    if (image == EGL_NO_IMAGE)
        fprintf(stderr, "EGL import %.4s: %#x\n", (char *)&format, eglGetError());
    must(image != EGL_NO_IMAGE, "DMA-BUF import failed");
    return image;
}
static GLuint shader(GLenum type, const char *source) {
    GLuint id = glCreateShader(type);
    glShaderSource(id, 1, &source, NULL);
    glCompileShader(id);
    GLint ok;
    glGetShaderiv(id, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024];
        glGetShaderInfoLog(id, sizeof log, NULL, log);
        fprintf(stderr, "%s\n", log);
    }
    must(ok, "compile shader");
    return id;
}
static void open_gpu(const char *node) {
    int fd = open(node, O_RDWR | O_CLOEXEC);
    must(fd >= 0, "open render node");
    app.gbm = gbm_create_device(fd);
    must(app.gbm != NULL, "start GBM");
    app.egl = eglGetPlatformDisplay(EGL_PLATFORM_GBM_KHR, app.gbm, NULL);
    must(app.egl != EGL_NO_DISPLAY && eglInitialize(app.egl, NULL, NULL) &&
             eglBindAPI(EGL_OPENGL_API),
         "start EGL");
    const EGLint attrs[] = {EGL_SURFACE_TYPE,
                            EGL_WINDOW_BIT,
                            EGL_RENDERABLE_TYPE,
                            EGL_OPENGL_BIT,
                            EGL_RED_SIZE,
                            8,
                            EGL_GREEN_SIZE,
                            8,
                            EGL_BLUE_SIZE,
                            8,
                            EGL_NONE};
    EGLConfig config;
    EGLint count;
    must(eglChooseConfig(app.egl, attrs, &config, 1, &count) && count, "EGL config");
    EGLContext gl = eglCreateContext(app.egl, config, EGL_NO_CONTEXT, NULL);
    must(gl != EGL_NO_CONTEXT && eglMakeCurrent(app.egl, EGL_NO_SURFACE, EGL_NO_SURFACE, gl),
         "activate GL");
    app.bind_image =
        (PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    must(app.bind_image != NULL, "bind EGL image function");
    GLuint program = glCreateProgram();
    glAttachShader(program,
                   shader(GL_VERTEX_SHADER,
                          "#version 330\nout vec2 uv;"
                          "void main(){uv=vec2(float((gl_VertexID<<1)&2),float(gl_VertexID&2));"
                          "gl_Position=vec4(uv*2.0-1.0,0,1);uv.y=1.0-uv.y;}"));
    glAttachShader(program,
                   shader(GL_FRAGMENT_SHADER,
                          "#version 330\nin vec2 uv;out vec4 p;uniform sampler2D luma;uniform "
                          "sampler2D chroma;uniform bool swap_uv;"
                          "void main(){float y=(texture(luma,uv).r-16.0/255.0)*1.16438;"
                          "vec2 c=texture(chroma,uv).rg-vec2(128.0/255.0);if(swap_uv)c=c.gr;"
                          "p=vec4(y+1.79274*c.y,y-0.21325*c.x-0.53291*c.y,y+2.11240*c.x,1);}"));
    glLinkProgram(program);
    GLint ok;
    glGetProgramiv(program, GL_LINK_STATUS, &ok);
    must(ok, "link shader");
    glUseProgram(program);
    glUniform1i(glGetUniformLocation(program, "luma"), 0);
    glUniform1i(glGetUniformLocation(program, "chroma"), 1);
    app.swap_uv = glGetUniformLocation(program, "swap_uv");
    glGenTextures(2, app.input);
    glViewport(0, 0, W, H);
}
static void allocate(struct Slot *slot) {
    slot->bo = gbm_bo_create(app.gbm, W, H, XR24, GBM_BO_USE_RENDERING);
    must(slot->bo && gbm_bo_get_plane_count(slot->bo) == 1, "allocate RGB output");
    int fd = gbm_bo_get_fd(slot->bo);
    must(fd >= 0, "get export fd");
    uint32_t pitch = gbm_bo_get_stride(slot->bo), offset = gbm_bo_get_offset(slot->bo, 0);
    uint64_t mod = gbm_bo_get_modifier(slot->bo);
    slot->image = import(XR24, W, H, fd, offset, pitch, mod);
    glGenTextures(1, &slot->tex);
    glBindTexture(GL_TEXTURE_2D, slot->tex);
    app.bind_image(GL_TEXTURE_2D, slot->image);
    glGenFramebuffers(1, &slot->fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, slot->fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, slot->tex, 0);
    must(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE, "RGB framebuffer");
    struct zwp_linux_buffer_params_v1 *params = zwp_linux_dmabuf_v1_create_params(app.dmabuf);
    zwp_linux_buffer_params_v1_add(params, fd, 0, offset, pitch, mod >> 32, (uint32_t)mod);
    slot->buffer = zwp_linux_buffer_params_v1_create_immed(params, W, H, XR24, 0);
    zwp_linux_buffer_params_v1_destroy(params);
    wl_buffer_add_listener(slot->buffer, &buffer_events, slot);
    close(fd);
}
static void define_input(GLuint tex, EGLImage image) {
    glBindTexture(GL_TEXTURE_2D, tex);
    app.bind_image(GL_TEXTURE_2D, image);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
}
static GLuint cached_plane(int i, uint32_t format, const AVDRMPlaneDescriptor *plane,
                           const AVDRMObjectDescriptor *object, const struct Decoder *decoder) {
    struct stat stat = {0};
    const struct Decoder *owner = NULL;
    (void)decoder;
    uint64_t image = 0;
    unsigned generation = 0;
#ifdef WITH_VULKAN
    if (decoder->pixel_format == AV_PIX_FMT_VULKAN) {
        const AVVkFrame *frame = (const void *)decoder->frame->data[0];
        image = (uintptr_t)frame->img[0];
        owner = decoder;
        generation = decoder->pool_generation;
    }
#endif
    if (!owner)
        must(!fstat(object->fd, &stat), "identify source DMA-BUF");
    struct ImportedPlane *oldest = &app.imported[0];
    for (size_t j = 0; j < sizeof app.imported / sizeof *app.imported; j++) {
        struct ImportedPlane *p = &app.imported[j];
        if (p->texture && p->dev == stat.st_dev && p->inode == stat.st_ino &&
            p->vk_owner == owner && p->vk_image == image && p->vk_generation == generation &&
            p->offset == plane->offset && p->pitch == plane->pitch &&
            p->modifier == object->format_modifier && p->format == format) {
            p->last_use = ++app.uses;
            app.hits++;
            return p->texture;
        }
        if (p->last_use < oldest->last_use)
            oldest = p;
    }
    if (oldest->texture) {
        glDeleteTextures(1, &oldest->texture);
        eglDestroyImage(app.egl, oldest->image);
    }
    *oldest = (struct ImportedPlane){.dev = stat.st_dev,
                                     .inode = stat.st_ino,
                                     .offset = plane->offset,
                                     .pitch = plane->pitch,
                                     .modifier = object->format_modifier,
                                     .format = format,
                                     .last_use = ++app.uses,
                                     .vk_owner = owner,
                                     .vk_image = image,
                                     .vk_generation = generation};
    oldest->image = import(format, W >> i, H >> i, object->fd, plane->offset, plane->pitch,
                           object->format_modifier);
    glGenTextures(1, &oldest->texture);
    define_input(oldest->texture, oldest->image);
    app.misses++;
    return oldest->texture;
}
static void draw(struct Slot *slot, const AVDRMFrameDescriptor *desc, const struct Decoder *decoder,
                 EGLImage images[2]) {
    for (int i = 0; i < 2; i++) {
        const AVDRMLayerDescriptor *layer = &desc->layers[i];
        const AVDRMPlaneDescriptor *plane = &layer->planes[0];
        const AVDRMObjectDescriptor *object = &desc->objects[plane->object_index];
        // Import the two byte lanes through the driver's supported RG88 view.
        // The shader reads the little-endian NV12 lanes in U,V order.
        uint32_t view_format = i == 0 ? R8 : RG88;
        glActiveTexture(GL_TEXTURE0 + i);
        if (app.reuse)
            glBindTexture(GL_TEXTURE_2D, cached_plane(i, view_format, plane, object, decoder));
        else {
            images[i] = import(view_format, W >> i, H >> i, object->fd, plane->offset, plane->pitch,
                               object->format_modifier);
            define_input(app.input[i], images[i]);
        }
    }
    glUniform1i(app.swap_uv, 1);
    glBindFramebuffer(GL_FRAMEBUFFER, slot->fbo);
    glDrawArrays(GL_TRIANGLES, 0, 3);
}

static void import_created(void *data, struct zwp_linux_buffer_params_v1 *params,
                           struct wl_buffer *buffer) {
    (void)params;
    *(int *)data = 1;
    wl_buffer_destroy(buffer);
}
static void import_failed(void *data, struct zwp_linux_buffer_params_v1 *params) {
    (void)params;
    *(int *)data = -1;
}
static const struct zwp_linux_buffer_params_v1_listener import_events = {import_created,
                                                                         import_failed};
static struct zwp_linux_buffer_params_v1 *nv12_params(const AVDRMFrameDescriptor *desc) {
    struct zwp_linux_buffer_params_v1 *params = zwp_linux_dmabuf_v1_create_params(app.dmabuf);
    for (int i = 0; i < 2; i++) {
        const AVDRMPlaneDescriptor *plane = &desc->layers[i].planes[0];
        const AVDRMObjectDescriptor *object = &desc->objects[plane->object_index];
        uint64_t mod = object->format_modifier;
        zwp_linux_buffer_params_v1_add(params, object->fd, i, plane->offset, plane->pitch,
                                       mod >> 32, (uint32_t)mod);
    }
    return params;
}
static void probe_nv12(const AVDRMFrameDescriptor *desc) {
    int imported = 0;
    struct zwp_linux_buffer_params_v1 *params = nv12_params(desc);
    zwp_linux_buffer_params_v1_add_listener(params, &import_events, &imported);
    zwp_linux_buffer_params_v1_create(params, W, H, NV12, 0);
    roundtrip();
    printf("NV12 asynchronous Wayland import result=%d\n", imported);
    zwp_linux_buffer_params_v1_destroy(params);
}
static void attach_nv12(struct Slot *slot, AVFrame *frame, const AVDRMFrameDescriptor *desc) {
    must(slot->source && !slot->buffer, "NV12 slot must be available");
    av(av_frame_ref(slot->source, frame), "retain decoded frame");
    struct zwp_linux_buffer_params_v1 *params = nv12_params(desc);
    slot->buffer = zwp_linux_buffer_params_v1_create_immed(params, W, H, NV12, 0);
    zwp_linux_buffer_params_v1_destroy(params);
    wl_buffer_add_listener(slot->buffer, &buffer_events, slot);
}

int main(int argc, char **argv) {
    must(argc == 4 || argc == 5 || argc == 7 || argc == 9,
         "usage: stream NODE SECONDS OUTPUT VIDEO [OUTPUT VIDEO ...] or stream NODE probe VIDEO");
    connect_wayland();
    AVBufferRef *device = NULL;
    bool vulkan = getenv("PROBE_VULKAN") != NULL;
#ifndef WITH_VULKAN
    must(!vulkan, "this build has no Vulkan headers");
#endif
    av(av_hwdevice_ctx_create(&device, vulkan ? AV_HWDEVICE_TYPE_VULKAN : AV_HWDEVICE_TYPE_VAAPI,
                              vulkan ? "0" : argv[1], NULL, 0),
       "open hardware decoder");
    if (argc == 4) {
        struct Decoder decoder = {0};
        open_video(&decoder, device, argv[3]);
        probe_nv12(decode(&decoder));
        return 0;
    }
    int seconds = atoi(argv[2]), count = (argc - 3) / 2;
    must(seconds >= 2 && seconds <= 80, "use 2–80 seconds");
    struct Screen screens[3] = {0};
    bool direct = getenv("PROBE_DIRECT_NV12") != NULL;
    app.reuse = getenv("PROBE_CACHE_INPUTS") != NULL;
    if (!direct)
        open_gpu(argv[1]);
    for (int i = 0; i < count; i++) {
        struct Screen *s = &screens[i];
        s->name = argv[3 + i * 2];
        open_layer(s);
        open_video(&s->decoder, device, argv[4 + i * 2]);
        for (int j = 0; j < SLOTS; j++) {
            if (direct) {
                s->slots[j].source = av_frame_alloc();
                must(s->slots[j].source != NULL, "allocate frame lease");
            } else
                allocate(&s->slots[j]);
        }
    }
    roundtrip();
    for (int i = 0; i < count; i++)
        must(screens[i].width == W && screens[i].height == H, "output is not 1440p scale 1");
    int64_t start = now(CLOCK_MONOTONIC), cpu = now(CLOCK_PROCESS_CPUTIME_ID);
    for (int frame = 0; frame < seconds * 30; frame++) {
        int64_t deadline = start + (int64_t)frame * NS / 30;
        struct timespec due = {deadline / NS, deadline % NS};
        while (clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &due, NULL) == EINTR) {
        }
        pump();
        struct Slot *selected[3] = {0};
        EGLImage temporary[3][2];
        for (int i = 0; i < count; i++) {
            struct Screen *s = &screens[i];
            must(!s->closed, "layer closed");
            const AVDRMFrameDescriptor *desc = decode(&s->decoder);
            for (int j = 0; j < SLOTS; j++)
                if (!s->slots[j].busy) {
                    selected[i] = &s->slots[j];
                    break;
                }
            if (!selected[i])
                s->skipped++;
            else if (direct)
                attach_nv12(selected[i], s->decoder.drm, desc);
            else
                draw(selected[i], desc, &s->decoder, temporary[i]);
        }
        if (!direct) {
            glFinish();
            must(glGetError() == GL_NO_ERROR, "OpenGL draw error");
        }
        for (int i = 0; i < count; i++) {
            if (!selected[i])
                continue;
            struct Screen *s = &screens[i];
            if (!direct && !app.reuse)
                for (int p = 0; p < 2; p++)
                    eglDestroyImage(app.egl, temporary[i][p]);
            struct wp_presentation_feedback *fb =
                wp_presentation_feedback(app.presentation, s->surface);
            wp_presentation_feedback_add_listener(fb, &present_events, s);
            wl_surface_attach(s->surface, selected[i]->buffer, 0, 0);
            wl_surface_damage_buffer(s->surface, 0, 0, W, H);
            wl_surface_commit(s->surface);
            selected[i]->busy = true;
        }
        flush();
        if (!frame) {
            printf("ready: bounded %s pipeline %s\n", vulkan ? "Vulkan" : "NVDEC",
                   direct ? "NV12" : "XR24");
            fflush(stdout);
        }
    }
    nanosleep(&(struct timespec){0, 100000000}, NULL);
    pump();
    printf("input_cache_hits=%lu misses=%lu\n", (unsigned long)app.hits, (unsigned long)app.misses);
    double elapsed = (now(CLOCK_MONOTONIC) - start) / 1e9,
           cpu_ms = (now(CLOCK_PROCESS_CPUTIME_ID) - cpu) / 1e6;
    printf("elapsed=%.3fs cpu_ms=%.3f cpu_percent=%.3f\n", elapsed, cpu_ms, cpu_ms / elapsed / 10);
    for (int i = 0; i < count; i++) {
        struct Screen *s = &screens[i];
        printf("%s: presented=%u discarded=%u skipped=%u wraps=%u max_gap_ms=%.2f zero_copy=%u\n",
               s->name, s->presented, s->discarded, s->skipped, s->decoder.loops, s->max_gap / 1e6,
               s->zero_copy);
    }
    return 0;
}
