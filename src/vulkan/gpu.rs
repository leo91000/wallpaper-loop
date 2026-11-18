use std::{
    collections::VecDeque,
    ffi::c_void,
    fs::{File, OpenOptions},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::Path,
    ptr::{self, NonNull},
    rc::Rc,
};

use anyhow::{Context, Result, anyhow, ensure};
use glow::HasContext;
use khronos_egl::{self as egl, API};

use super::decode::{FrameInfo, Mapped, Plane};

pub const XR24: u32 = u32::from_le_bytes(*b"XR24");
const R8: u32 = u32::from_le_bytes(*b"R8  ");
const RG88: u32 = u32::from_le_bytes(*b"RG88");
const INVALID_MODIFIER: u64 = 0x00ff_ffff_ffff_ffff;
const RENDERING: u32 = 1 << 2;

#[link(name = "gbm")]
unsafe extern "C" {
    fn gbm_create_device(fd: i32) -> *mut c_void;
    fn gbm_device_destroy(device: *mut c_void);
    fn gbm_bo_create(
        device: *mut c_void,
        width: u32,
        height: u32,
        format: u32,
        flags: u32,
    ) -> *mut c_void;
    fn gbm_bo_create_with_modifiers2(
        device: *mut c_void,
        width: u32,
        height: u32,
        format: u32,
        modifiers: *const u64,
        count: u32,
        flags: u32,
    ) -> *mut c_void;
    fn gbm_bo_destroy(bo: *mut c_void);
    fn gbm_bo_get_fd(bo: *mut c_void) -> i32;
    fn gbm_bo_get_stride(bo: *mut c_void) -> u32;
    fn gbm_bo_get_offset(bo: *mut c_void, plane: i32) -> u32;
    fn gbm_bo_get_modifier(bo: *mut c_void) -> u64;
    fn gbm_bo_get_plane_count(bo: *mut c_void) -> i32;
}

#[link(name = "EGL")]
unsafe extern "system" {
    fn eglCreateImage(
        display: *mut c_void,
        context: *mut c_void,
        target: egl::Enum,
        buffer: *mut c_void,
        attributes: *const egl::Attrib,
    ) -> *mut c_void;
}

struct Gbm {
    raw: NonNull<c_void>,
    _file: File,
}

impl Gbm {
    fn open(node: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(node)
            .with_context(|| format!("open render node {}", node.display()))?;
        let raw = NonNull::new(unsafe { gbm_create_device(file.as_raw_fd()) })
            .context("initialize GBM")?;
        Ok(Self { raw, _file: file })
    }
}

impl Drop for Gbm {
    fn drop(&mut self) {
        unsafe { gbm_device_destroy(self.raw.as_ptr()) };
    }
}

struct Bo {
    raw: NonNull<c_void>,
    _gbm: Rc<Egl>,
}

impl Bo {
    fn allocate(device: &Rc<Egl>, width: u32, height: u32, modifiers: &[u64]) -> Result<Self> {
        let raw = device.gbm.raw.as_ptr();
        let mut bo = NonNull::new(unsafe { gbm_bo_create(raw, width, height, XR24, RENDERING) });
        if let Some(candidate) = bo {
            let modifier = unsafe { gbm_bo_get_modifier(candidate.as_ptr()) };
            if !modifiers.contains(&modifier) {
                unsafe { gbm_bo_destroy(candidate.as_ptr()) };
                bo = None;
            }
        }
        if bo.is_none() {
            bo = NonNull::new(unsafe {
                gbm_bo_create_with_modifiers2(
                    raw,
                    width,
                    height,
                    XR24,
                    modifiers.as_ptr(),
                    modifiers.len() as u32,
                    RENDERING,
                )
            });
        }
        let bo = Self {
            raw: bo.context("allocate XR24 with a compositor-supported DRM modifier")?,
            _gbm: Rc::clone(device),
        };
        ensure!(
            unsafe { gbm_bo_get_plane_count(bo.raw.as_ptr()) } == 1,
            "XR24 requires one plane"
        );
        Ok(bo)
    }

    fn export(&self) -> Result<BufferFile> {
        let raw = self.raw.as_ptr();
        let fd = unsafe { gbm_bo_get_fd(raw) };
        ensure!(fd >= 0, "export XR24: {}", std::io::Error::last_os_error());
        Ok(unsafe {
            BufferFile {
                fd: OwnedFd::from_raw_fd(fd),
                modifier: gbm_bo_get_modifier(raw),
                offset: gbm_bo_get_offset(raw, 0),
                stride: gbm_bo_get_stride(raw),
            }
        })
    }
}

impl Drop for Bo {
    fn drop(&mut self) {
        unsafe { gbm_bo_destroy(self.raw.as_ptr()) };
    }
}

pub struct BufferFile {
    pub fd: OwnedFd,
    pub modifier: u64,
    pub offset: u32,
    pub stride: u32,
}

type BindImage = unsafe extern "system" fn(u32, *mut c_void);
type QueryModifiers = unsafe extern "system" fn(
    *mut c_void,
    i32,
    i32,
    *mut u64,
    *mut egl::Boolean,
    *mut i32,
) -> egl::Boolean;

struct Egl {
    display: egl::Display,
    context: egl::Context,
    gl: glow::Context,
    bind_image: BindImage,
    gbm: Gbm,
}

impl Egl {
    fn open(node: &Path) -> Result<Rc<Self>> {
        let gbm = Gbm::open(node)?;
        const PLATFORM_GBM: egl::Enum = 0x31d7;
        let display = unsafe {
            API.get_platform_display(PLATFORM_GBM, gbm.raw.as_ptr(), &[egl::ATTRIB_NONE])
        }
        .context("open EGL on the GBM render node")?;
        API.initialize(display).context("initialize GBM EGL")?;
        let setup = Self::configure(display);
        let (context, bind_image) = match setup {
            Ok(setup) => setup,
            Err(error) => {
                let _ = API.terminate(display);
                return Err(error);
            }
        };
        // Desktop GL entry points belong to the current EGL context.
        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                API.get_proc_address(name)
                    .map_or(ptr::null(), |proc| proc as *const c_void)
            })
        };
        Ok(Rc::new(Self {
            display,
            context,
            gl,
            bind_image,
            gbm,
        }))
    }

    fn configure(display: egl::Display) -> Result<(egl::Context, BindImage)> {
        let symbol = API
            .get_proc_address("glEGLImageTargetTexture2DOES")
            .context("load EGL image entry point")?;
        let bind_image = unsafe { std::mem::transmute::<extern "system" fn(), BindImage>(symbol) };
        API.bind_api(egl::OPENGL_API)?;
        let config = API
            .choose_first_config(
                display,
                &[
                    egl::SURFACE_TYPE,
                    egl::WINDOW_BIT,
                    egl::RENDERABLE_TYPE,
                    egl::OPENGL_BIT,
                    egl::RED_SIZE,
                    8,
                    egl::GREEN_SIZE,
                    8,
                    egl::BLUE_SIZE,
                    8,
                    egl::NONE,
                ],
            )?
            .context("find GBM OpenGL config")?;
        let context = API.create_context(display, config, None, &[egl::NONE])?;
        if let Err(error) = API.make_current(display, None, None, Some(context)) {
            let _ = API.destroy_context(display, context);
            return Err(error).context("activate surfaceless GBM OpenGL");
        }
        Ok((context, bind_image))
    }

    fn modifiers(&self, format: u32) -> Result<Vec<u64>> {
        let proc = API
            .get_proc_address("eglQueryDmaBufModifiersEXT")
            .context("EGL DMA-BUF modifiers are unavailable")?;
        let query = unsafe { std::mem::transmute::<extern "system" fn(), QueryModifiers>(proc) };
        let mut count = 0;
        let display = self.display.as_ptr();
        let ok = unsafe {
            query(
                display,
                format as i32,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut count,
            )
        };
        ensure!(
            ok == egl::TRUE && (1..=4096).contains(&count),
            "EGL reported no DMA-BUF modifiers for {}",
            String::from_utf8_lossy(&format.to_le_bytes())
        );
        let mut modifiers = vec![0u64; count as usize];
        let mut external_only = vec![egl::FALSE; count as usize];
        let ok = unsafe {
            query(
                display,
                format as i32,
                count,
                modifiers.as_mut_ptr(),
                external_only.as_mut_ptr(),
                &mut count,
            )
        };
        ensure!(
            ok == egl::TRUE && (0..=modifiers.len() as i32).contains(&count),
            "enumerate EGL modifiers"
        );
        modifiers.truncate(count as usize);
        Ok(modifiers
            .into_iter()
            .zip(external_only)
            .filter_map(|(modifier, external)| {
                (external == egl::FALSE && modifier != INVALID_MODIFIER).then_some(modifier)
            })
            .collect())
    }

    fn import(self: &Rc<Self>, width: i32, height: i32, plane: Plane<'_>) -> Result<Texture> {
        const LINUX_DMA_BUF: egl::Enum = 0x3270;
        const FOURCC: egl::Attrib = 0x3271;
        const FD: egl::Attrib = 0x3272;
        const OFFSET: egl::Attrib = 0x3273;
        const PITCH: egl::Attrib = 0x3274;
        const MODIFIER_LOW: egl::Attrib = 0x3443;
        const MODIFIER_HIGH: egl::Attrib = 0x3444;
        let attributes = [
            egl::WIDTH as egl::Attrib,
            width as egl::Attrib,
            egl::HEIGHT as egl::Attrib,
            height as egl::Attrib,
            FOURCC,
            plane.format as egl::Attrib,
            FD,
            plane.fd.as_raw_fd() as egl::Attrib,
            OFFSET,
            plane.offset as egl::Attrib,
            PITCH,
            plane.pitch as egl::Attrib,
            MODIFIER_LOW,
            (plane.modifier as u32) as egl::Attrib,
            MODIFIER_HIGH,
            (plane.modifier >> 32) as egl::Attrib,
            egl::ATTRIB_NONE,
        ];
        let ptr = unsafe {
            eglCreateImage(
                self.display.as_ptr(),
                ptr::null_mut(),
                LINUX_DMA_BUF,
                ptr::null_mut(),
                attributes.as_ptr(),
            )
        };
        ensure!(
            !ptr.is_null(),
            "EGL rejected {} modifier {:#x}: {:?}",
            String::from_utf8_lossy(&plane.format.to_le_bytes()),
            plane.modifier,
            API.get_error()
        );
        let image = unsafe { egl::Image::from_ptr(ptr) };
        let gl = &self.gl;
        let name = match unsafe { gl.create_texture() } {
            Ok(name) => name,
            Err(error) => {
                let _ = API.destroy_image(self.display, image);
                return Err(anyhow!(error));
            }
        };
        let texture = Texture {
            device: Rc::clone(self),
            image,
            name,
        };
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(name));
            (self.bind_image)(glow::TEXTURE_2D, ptr);
            for (property, value) in [
                (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
            ] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, property, value as i32);
            }
        }
        self.check("bind DMA-BUF texture")?;
        Ok(texture)
    }

    fn check(&self, description: &str) -> Result<()> {
        let error = unsafe { self.gl.get_error() };
        ensure!(
            error == glow::NO_ERROR,
            "{description}: OpenGL error {error:#x}"
        );
        Ok(())
    }
}

impl Drop for Egl {
    fn drop(&mut self) {
        let _ = API.make_current(self.display, None, None, None);
        let _ = API.destroy_context(self.display, self.context);
        let _ = API.terminate(self.display);
    }
}

struct Texture {
    device: Rc<Egl>,
    image: egl::Image,
    name: glow::NativeTexture,
}

impl Drop for Texture {
    fn drop(&mut self) {
        unsafe { self.device.gl.delete_texture(self.name) };
        let _ = API.destroy_image(self.device.display, self.image);
    }
}

pub struct Target {
    framebuffer: glow::NativeFramebuffer,
    texture: Texture,
    _bo: Bo,
}

impl Drop for Target {
    fn drop(&mut self) {
        unsafe { self.texture.device.gl.delete_framebuffer(self.framebuffer) };
    }
}

pub struct Gpu {
    egl: Rc<Egl>,
    vao: glow::NativeVertexArray,
    program: glow::NativeProgram,
    crop: glow::NativeUniformLocation,
    range: glow::NativeUniformLocation,
    matrix: glow::NativeUniformLocation,
    output_modifiers: Vec<u64>,
    pub decode_modifiers: Vec<u64>,
}

impl Gpu {
    pub fn open(node: &Path, advertised: &[u64]) -> Result<Self> {
        let egl = Egl::open(node)?;
        let mut output_modifiers = egl.modifiers(XR24)?;
        output_modifiers.retain(|modifier| advertised.contains(modifier));
        ensure!(
            !output_modifiers.is_empty(),
            "EGL and Wayland share no renderable XR24 modifiers"
        );
        let chroma = egl.modifiers(RG88)?;
        let mut decode_modifiers = egl.modifiers(R8)?;
        decode_modifiers.retain(|modifier| chroma.contains(modifier));
        // This tiled NVIDIA modifier is verified for video decode and both plane views.
        // The pool can select another layout advertised by the driver if necessary.
        decode_modifiers.sort_by_key(|&modifier| modifier != 0x0300_0000_0060_6014);
        ensure!(
            !decode_modifiers.is_empty(),
            "EGL has no common modifier for the two NV12 planes"
        );
        let gl = &egl.gl;
        let (program, vao, crop, range, matrix) = unsafe {
            let program = create_program(gl)?;
            let vao = gl.create_vertex_array().map_err(|error| anyhow!(error))?;
            gl.bind_vertex_array(Some(vao));
            gl.use_program(Some(program));
            gl.uniform_1_i32(gl.get_uniform_location(program, "luma").as_ref(), 0);
            gl.uniform_1_i32(gl.get_uniform_location(program, "chroma").as_ref(), 1);
            let uniform = |name: &str| {
                gl.get_uniform_location(program, name)
                    .with_context(|| format!("find shader uniform {name}"))
            };
            (
                program,
                vao,
                uniform("crop")?,
                uniform("range")?,
                uniform("matrix")?,
            )
        };
        Ok(Self {
            egl,
            vao,
            program,
            crop,
            range,
            matrix,
            output_modifiers,
            decode_modifiers,
        })
    }

    pub fn allocate(&self, width: i32, height: i32) -> Result<(Target, BufferFile)> {
        ensure!(width > 0 && height > 0, "invalid output size");
        let bo = Bo::allocate(
            &self.egl,
            width as u32,
            height as u32,
            &self.output_modifiers,
        )?;
        let file = bo.export()?;
        let texture = self.egl.import(
            width,
            height,
            Plane {
                fd: std::os::fd::AsFd::as_fd(&file.fd),
                offset: file.offset as isize,
                pitch: file.stride as isize,
                modifier: file.modifier,
                format: XR24,
            },
        )?;
        let gl = &self.egl.gl;
        let framebuffer = unsafe { gl.create_framebuffer() }.map_err(|error| anyhow!(error))?;
        let target = Target {
            framebuffer,
            texture,
            _bo: bo,
        };
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(target.texture.name),
                0,
            );
            ensure!(
                gl.check_framebuffer_status(glow::FRAMEBUFFER) == glow::FRAMEBUFFER_COMPLETE,
                "XR24 DMA-BUF is not renderable"
            );
        }
        Ok((target, file))
    }

    pub fn draw(
        &self,
        target: &Target,
        frame: &Mapped,
        info: FrameInfo,
        width: i32,
        height: i32,
        cache: &mut Imports,
    ) -> Result<()> {
        let textures = cache.textures(&self.egl, frame, info)?;
        let (x, y) = crop_ratio(info.aspect, width as f32 / height as f32);
        let gl = &self.egl.gl;
        unsafe {
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(textures[0]));
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, Some(textures[1]));
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(target.framebuffer));
            gl.viewport(0, 0, width, height);
            gl.uniform_2_f32(Some(&self.crop), x, y);
            gl.uniform_3_f32(
                Some(&self.range),
                info.color.luma_offset,
                info.color.luma_scale,
                info.color.chroma_scale,
            );
            gl.uniform_4_f32_slice(Some(&self.matrix), &info.color.coefficients);
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
        }
        Ok(())
    }

    pub fn finish(&self) -> Result<()> {
        unsafe { self.egl.gl.finish() };
        self.egl.check("convert NV12 to XR24")
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            self.egl.gl.delete_vertex_array(self.vao);
            self.egl.gl.delete_program(self.program);
        }
    }
}

unsafe fn create_program(gl: &glow::Context) -> Result<glow::NativeProgram> {
    const VERTEX: &str = r#"#version 330
out vec2 uv;
uniform vec2 crop;
void main() {
    vec2 vertex = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    gl_Position = vec4(vertex * 2.0 - 1.0, 0.0, 1.0);
    uv = (vertex - 0.5) * crop + 0.5;
}
"#;
    const FRAGMENT: &str = r#"#version 330
in vec2 uv;
out vec4 color;
uniform sampler2D luma;
uniform sampler2D chroma;
uniform vec3 range;
uniform vec4 matrix;
void main() {
    float y = (texture(luma, uv).r - range.x) * range.y;
    vec2 cbcr = (texture(chroma, uv).rg - 128.0 / 255.0) * range.z;
    color = vec4(y + matrix.x * cbcr.y,
                 y - matrix.y * cbcr.x - matrix.z * cbcr.y,
                 y + matrix.w * cbcr.x, 1.0);
}
"#;
    unsafe {
        let program = gl.create_program().map_err(|error| anyhow!(error))?;
        for (kind, source) in [
            (glow::VERTEX_SHADER, VERTEX),
            (glow::FRAGMENT_SHADER, FRAGMENT),
        ] {
            let shader = gl.create_shader(kind).map_err(|error| anyhow!(error))?;
            gl.shader_source(shader, source);
            gl.compile_shader(shader);
            ensure!(
                gl.get_shader_compile_status(shader),
                "compile wallpaper shader: {}",
                gl.get_shader_info_log(shader)
            );
            gl.attach_shader(program, shader);
            gl.delete_shader(shader);
        }
        gl.link_program(program);
        ensure!(
            gl.get_program_link_status(program),
            "link wallpaper shader: {}",
            gl.get_program_info_log(program)
        );
        Ok(program)
    }
}

fn crop_ratio(source: f32, output: f32) -> (f32, f32) {
    ((output / source).min(1.0), (source / output).min(1.0))
}

#[derive(PartialEq, Eq)]
struct Key {
    image: u64,
    size: (i32, i32),
    planes: [(u64, isize, isize); 2],
}

#[derive(Default)]
pub struct Imports {
    generation: u64,
    entries: VecDeque<(Key, [Texture; 2])>,
}

impl Imports {
    fn textures(
        &mut self,
        egl: &Rc<Egl>,
        frame: &Mapped,
        info: FrameInfo,
    ) -> Result<[glow::NativeTexture; 2]> {
        if self.generation != frame.generation {
            self.entries.clear();
            self.generation = frame.generation;
        }
        let planes = frame.planes()?;
        let key = Key {
            image: frame.image,
            size: (info.width, info.height),
            planes: planes.map(|plane| (plane.modifier, plane.offset, plane.pitch)),
        };
        let cached = self
            .entries
            .iter()
            .position(|(candidate, _)| *candidate == key)
            .and_then(|index| self.entries.remove(index));
        let entry = match cached {
            Some(entry) => entry,
            None => {
                if self.entries.len() >= 8 {
                    self.entries.pop_front();
                }
                (
                    key,
                    [
                        egl.import(info.width, info.height, planes[0])
                            .context("import NV12 luma")?,
                        egl.import(info.width / 2, info.height / 2, planes[1])
                            .context("import NV12 chroma")?,
                    ],
                )
            }
        };
        let names = [entry.1[0].name, entry.1[1].name];
        self.entries.push_back(entry);
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vulkan::decode::{Decoder, Device};
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    #[test]
    fn center_crop_preserves_display_aspect() {
        assert_eq!(crop_ratio(2.0, 1.0), (0.5, 1.0));
        assert_eq!(crop_ratio(1.0, 2.0), (1.0, 0.5));
        assert_eq!(crop_ratio(2.0, 2.0), (1.0, 1.0));
    }

    #[test]
    #[ignore = "requires the NVIDIA Vulkan, EGL and GBM stack plus FFmpeg with libx264"]
    fn renders_asymmetric_color_chart() -> Result<()> {
        let node = std::env::var_os("WALLPAPER_TEST_RENDER_NODE").map_or_else(
            || std::path::PathBuf::from("/dev/dri/renderD128"),
            Into::into,
        );
        let scratch = tempfile::tempdir()?;
        let clip = scratch.path().join("colors.mp4");
        let mut ffmpeg = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "yuv420p",
                "-s",
                "64x64",
                "-r",
                "30",
                "-i",
                "pipe:0",
                "-c:v",
                "libx264",
                "-qp",
                "1",
                "-x264-params",
                "colorprim=bt709:transfer=bt709:colormatrix=bt709",
                "-color_primaries",
                "bt709",
                "-color_trc",
                "bt709",
                "-colorspace",
                "bt709",
                "-color_range",
                "tv",
            ])
            .arg(&clip)
            .stdin(Stdio::piped())
            .spawn()?;
        let colors = [
            [173, 42, 26],
            [63, 102, 240],
            [32, 240, 118],
            [128, 128, 128],
        ];
        let mut bytes = Vec::new();
        for (plane, side) in [64, 32, 32].into_iter().enumerate() {
            for y in 0..side {
                for x in 0..side {
                    let quadrant = (y / (side / 2)) * 2 + x / (side / 2);
                    bytes.push(colors[quadrant][plane]);
                }
            }
        }
        let write = ffmpeg
            .stdin
            .take()
            .context("open fixture pipe")?
            .write_all(&bytes);
        let status = ffmpeg.wait()?;
        write?;
        ensure!(status.success(), "encode test chart");

        let egl = Egl::open(&node)?;
        let modifiers = egl.modifiers(XR24)?;
        drop(egl);
        let gpu = Gpu::open(&node, &modifiers)?;
        let device = Device::open(&node)?;
        let mut decoder = Decoder::open(device, &clip, &gpu.decode_modifiers)?;
        let info = decoder.next()?;
        let reference = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&clip)
            .args([
                "-frames:v",
                "1",
                "-vf",
                "scale=in_color_matrix=bt709:out_range=pc",
                "-pix_fmt",
                "rgb24",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .output()?;
        ensure!(
            reference.status.success() && reference.stdout.len() == 64 * 64 * 3,
            "decode software RGB reference"
        );
        let mapped = decoder.mapped()?;
        let (target, _) = gpu.allocate(info.width, info.height)?;
        let mut imports = Imports::default();
        gpu.draw(
            &target,
            &mapped,
            info,
            info.width,
            info.height,
            &mut imports,
        )?;
        gpu.finish()?;
        let gl = &gpu.egl.gl;
        for (x, y) in [(16, 16), (48, 16), (16, 48), (48, 48)] {
            let address = (y as usize * 64 + x as usize) * 3;
            let expected = &reference.stdout[address..address + 3];
            let mut pixel = [0u8; 4];
            unsafe {
                gl.read_pixels(
                    x,
                    y,
                    1,
                    1,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelPackData::Slice(Some(&mut pixel)),
                );
            }
            gpu.egl.check("read test chart")?;
            assert!(
                pixel[..3]
                    .iter()
                    .zip(expected)
                    .all(|(&actual, &expected)| actual.abs_diff(expected) <= 4),
                "pixel at ({x}, {y}) was {pixel:?}; expected {expected:?}"
            );
        }
        Ok(())
    }
}
