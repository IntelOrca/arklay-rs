//! The optional SDL_GPU hardware renderer for the gameplay scene.
//!
//! The software rasterizer in [`crate::render`] stays authoritative: the
//! oracle, the goldens, `--capture`, `--trace`, `--stats` and every test draw
//! through it. This renderer is an interactive-only alternative selected with
//! `--renderer gpu`. It draws the same [`GameplayScene`] at the window's
//! resolution (see [`crate::video`]): the scene target is the viewport's size
//! times `render_scale` percent, optionally multisampled (`msaa`), and is
//! filtered into the viewport when the two differ.
//!
//!
//! * Every TMD is expanded once into a static vertex buffer
//!   ([`scene::mesh_vertices`]) and stays resident while it is drawn. An
//!   upload unused for [`EVICT_AFTER_FRAMES`] scene frames is released; a
//!   room change ([`GpuRenderer::retire`]) releases at once whatever the new
//!   room's first frame does not draw, and leaving gameplay releases
//!   everything ([`GpuRenderer::release_residency`]). Indexed texture pages,
//!   palettes and RGBA images (backgrounds, mask, shadow and effect pages) are
//!   cached the same way.
//! * Per draw, only the joint matrices, the camera, the latched lights and the
//!   tint/blend travel as uniforms; `mesh.vert` skins rigidly, lights each
//!   corner like `reference_vertex_shade` and projects through the cut camera.
//!   `mesh.frag` looks up the CLUT row in the shader, with the STP bit in the
//!   palette's alpha.
//! * Visibility is a depth buffer rather than the painter's list: masks write
//!   depth at their painter's key (a view-Z), shadows and effects test against
//!   it, opaque mesh texels write it and STP texels blend in a second pass.
//!
//! * A pack's HD replacements ([`crate::htexture`]) are looked up as an image
//!   or page is uploaded, from the hash of the pixels it would otherwise
//!   upload. A replaced image is sampled at its own resolution
//!   (`sprite.frag`'s `scale`); a replaced page keeps its indices for
//!   transparency and STP and takes each CLUT row's colours from a layer of
//!   an array texture (`mesh.frag`).
//!
//! The 2D overlays (menus, messages, fades) stay software. The session draws
//! them twice, once over a black scene and once over a white one; every
//! overlay operation is affine in the destination, so the composite shader
//! recovers `black + scene * (white - black)` exactly (see
//! `composite.frag`).
//!
//! The shaders are SPIR-V only for now, so the renderer needs a Vulkan
//! driver. Device creation failing is not an error for the engine: it warns
//! and falls back to the software renderer.

pub mod scene;

use std::collections::HashMap;
use std::ffi::{CStr, c_void};

use anyhow::{Result, bail};
use sdl3_sys::error::SDL_GetError;
use sdl3_sys::gpu::*;
use sdl3_sys::pixels::SDL_FColor;
use sdl3_sys::rect::SDL_Rect;
use sdl3_sys::video::{SDL_GetWindowSizeInPixels, SDL_Window};

use crate::htexture;
use crate::model::{Texture8, Tmd};
use crate::pack::Pack;
use crate::render::{Framebuffer, GameplayScene, root_position};
use crate::state::Image;
use crate::video;

use scene::{DrawUniforms, MeshVertex, SpriteRun, SpriteVertex};

const MESH_VERT: &[u8] = include_bytes!("shaders/mesh.vert.spv");
const MESH_FRAG: &[u8] = include_bytes!("shaders/mesh.frag.spv");
const SPRITE_VERT: &[u8] = include_bytes!("shaders/sprite.vert.spv");
const SPRITE_FRAG: &[u8] = include_bytes!("shaders/sprite.frag.spv");
const COMPOSITE_VERT: &[u8] = include_bytes!("shaders/composite.vert.spv");
const COMPOSITE_FRAG: &[u8] = include_bytes!("shaders/composite.frag.spv");

/// Scene frames an unused upload stays resident before it is released.
pub const EVICT_AFTER_FRAMES: u64 = 300;
/// The largest accepted offscreen resolution multiplier.
pub const MAX_SCALE: u32 = 8;
/// The largest scene target side, whatever the window and `render_scale`.
const MAX_SCENE_SIDE: u32 = 8192;

/// How long a present that had no swapchain image waits, so a loop held
/// back only by vsync does not spin while the window is hidden.
const HIDDEN_PRESENT_WAIT: std::time::Duration = std::time::Duration::from_millis(4);

/// What [`GpuRenderer::composite`] uploads before drawing.
enum Layers<'a> {
    /// The overlay drawn over a black scene and over a white one.
    Pair([&'a [u8]; 2]),
    /// One opaque image, composited over itself.
    Single(&'a [u8]),
    /// The single image already in `layer_black`.
    Kept,
}

const LAYER_WIDTH: u32 = 320;
const LAYER_HEIGHT: u32 = 240;

fn sdl_error() -> String {
    unsafe { CStr::from_ptr(SDL_GetError()) }
        .to_string_lossy()
        .into_owned()
}

/// A compiled shader, released when dropped; a pipeline keeps what it needs.
struct Shader {
    device: *mut SDL_GPUDevice,
    shader: *mut SDL_GPUShader,
}

impl Drop for Shader {
    fn drop(&mut self) {
        unsafe { SDL_ReleaseGPUShader(self.device, self.shader) };
    }
}

/// A cache entry made by the scene being recorded, forgotten again if the
/// recording fails before its uploads run.
#[derive(Clone, Copy)]
enum Upload {
    Mesh(usize),
    Page(usize),
    Image(usize),
}

/// A cached upload and the frame it was last drawn in.
struct Resident<T> {
    value: T,
    fingerprint: u64,
    last_used: u64,
}

/// A mesh's static vertex buffer; `buffer` is null for a mesh with no
/// drawable packet.
#[derive(Clone, Copy)]
struct MeshBuffer {
    buffer: *mut SDL_GPUBuffer,
    vertex_count: u32,
    blend_start: u32,
    /// The CLUT rows its packets read, one bit each.
    rows: u32,
}

/// An indexed page: the `R8_UINT` index texture and its palette, and the HD
/// replacements of its CLUT rows (both null without one).
#[derive(Clone, Copy)]
struct PageTextures {
    indices: *mut SDL_GPUTexture,
    palette: *mut SDL_GPUTexture,
    /// One RGBA layer per replaced row.
    hd: *mut SDL_GPUTexture,
    /// Per row, `mesh.frag`'s layer and extents.
    hd_rows: *mut SDL_GPUTexture,
}

/// An uploaded RGBA image: the original or its HD replacement, with the
/// replacement's texels per original texel.
#[derive(Clone, Copy)]
struct ImageTexture {
    texture: *mut SDL_GPUTexture,
    scale: [f32; 2],
}

/// The renderer's pipelines.
struct Pipelines {
    mesh_opaque: *mut SDL_GPUGraphicsPipeline,
    mesh_blend: *mut SDL_GPUGraphicsPipeline,
    /// The mirror copy is drawn unculled (its reflected X scale reverses the
    /// winding), like the software path.
    mesh_opaque_unculled: *mut SDL_GPUGraphicsPipeline,
    mesh_blend_unculled: *mut SDL_GPUGraphicsPipeline,
    sprite_write: *mut SDL_GPUGraphicsPipeline,
    sprite_test: *mut SDL_GPUGraphicsPipeline,
    /// Present-only: built for the swapchain's format.
    composite: *mut SDL_GPUGraphicsPipeline,
}

/// The SDL_GPU device, its pipelines and the resident uploads.
pub struct GpuRenderer {
    device: *mut SDL_GPUDevice,
    /// The claimed window; null for an offscreen renderer.
    window: *mut SDL_Window,
    /// The scene target's size as a percentage of the viewport.
    render_scale: u32,
    /// The scene target's size in pixels.
    scene_size: (u32, u32),
    /// Samples per scene pixel; above one the scene draws into
    /// `scene_samples` and resolves into `scene_colour`.
    samples: SDL_GPUSampleCount,
    depth_format: SDL_GPUTextureFormat,
    pipelines: Pipelines,
    sampler: *mut SDL_GPUSampler,
    /// The composite's sampler: the layers' sharp upscale and the scene's
    /// filtering both build on bilinear taps.
    linear: *mut SDL_GPUSampler,
    scene_colour: *mut SDL_GPUTexture,
    /// The multisampled colour target; null without MSAA.
    scene_samples: *mut SDL_GPUTexture,
    scene_depth: *mut SDL_GPUTexture,
    layer_black: *mut SDL_GPUTexture,
    layer_white: *mut SDL_GPUTexture,
    /// The persistent staging buffer of both overlay layers, cycled per
    /// present.
    layer_transfer: *mut SDL_GPUTransferBuffer,
    /// The layers' size: 320x240, or a wider aspect's canvas while a screen
    /// presents one ([`GpuRenderer::ensure_layers`]).
    layer_size: (u32, u32),
    /// The [`VideoFrame::serial`](crate::hdfilm::VideoFrame::serial) of the
    /// image `layer_black` holds alone, so a film frame shown across several
    /// presents is uploaded once.
    layer_image: Option<u64>,
    sprite_buffer: *mut SDL_GPUBuffer,
    sprite_capacity: u32,
    meshes: HashMap<usize, Resident<MeshBuffer>>,
    pages: HashMap<usize, Resident<PageTextures>>,
    images: HashMap<usize, Resident<ImageTexture>>,
    /// Bound in place of a page's replacements when it has none.
    no_hd: *mut SDL_GPUTexture,
    no_hd_rows: *mut SDL_GPUTexture,
    frame: u64,
    /// Set by [`GpuRenderer::retire`]: the next eviction keeps only what the
    /// frame that follows drew.
    retiring: bool,
    /// The entries the scene being recorded created.
    fresh: Vec<Upload>,
    /// Whether presentation uses mailbox, which never waits for vsync; the
    /// caller's frame pacing ([`crate::pacer`]) then sets the present rate.
    mailbox: bool,
}

impl GpuRenderer {
    /// Create a device, claim `window` for it and build the pipelines.
    ///
    /// The scene target follows the window: [`crate::video::fit`]'s viewport
    /// of its client area, times `render_scale` percent, with `msaa` samples
    /// per pixel (or the most the device supports below that).
    ///
    /// # Safety
    ///
    /// `window` must be a live SDL window that outlives the renderer and is
    /// not claimed by another device or an SDL renderer.
    pub unsafe fn new(window: *mut SDL_Window, render_scale: u32, msaa: u32) -> Result<Self> {
        let mut renderer = Self::create(
            (LAYER_WIDTH, LAYER_HEIGHT),
            render_scale.clamp(
                *video::RENDER_SCALE_RANGE.start(),
                *video::RENDER_SCALE_RANGE.end(),
            ),
            msaa,
        )?;
        if !unsafe { SDL_ClaimWindowForGPUDevice(renderer.device, window) } {
            bail!("SDL_ClaimWindowForGPUDevice failed: {}", sdl_error());
        }
        renderer.window = window;
        renderer.follow_window()?;
        // FIFO presentation can block indefinitely on a compositor that
        // stops sending frame callbacks (a hidden or unmapped Wayland
        // window), which would also stop the event loop. Mailbox never
        // waits; the loop's frame pacer limits presents to `frame_rate`.
        if unsafe {
            SDL_WindowSupportsGPUPresentMode(renderer.device, window, SDL_GPU_PRESENTMODE_MAILBOX)
        } && unsafe {
            SDL_SetGPUSwapchainParameters(
                renderer.device,
                window,
                SDL_GPU_SWAPCHAINCOMPOSITION_SDR,
                SDL_GPU_PRESENTMODE_MAILBOX,
            )
        } {
            renderer.mailbox = true;
        }
        let format = unsafe { SDL_GetGPUSwapchainTextureFormat(renderer.device, window) };
        renderer.pipelines.composite = renderer.composite_pipeline(format)?;
        Ok(renderer)
    }

    /// Give the window back once in-flight work is done, so an SDL renderer
    /// can claim it; presenting fails from then on.
    pub fn release_window(&mut self) {
        if !self.window.is_null() {
            unsafe {
                SDL_WaitForGPUIdle(self.device);
                SDL_ReleaseWindowFromGPUDevice(self.device, self.window);
            }
            self.window = std::ptr::null_mut();
        }
    }

    /// A renderer with no window, for tests and tools: it records scenes and
    /// reads them back with [`GpuRenderer::read_scene`].
    /// Its scene target is `scale` times 320x240 (clamped to
    /// `1..=MAX_SCALE`), single-sampled.
    pub fn offscreen(scale: u32) -> Result<Self> {
        let scale = scale.clamp(1, MAX_SCALE);
        Self::create((LAYER_WIDTH * scale, LAYER_HEIGHT * scale), 100, 1)
    }

    fn create(scene_size: (u32, u32), render_scale: u32, msaa: u32) -> Result<Self> {
        let device =
            unsafe { SDL_CreateGPUDevice(SDL_GPU_SHADERFORMAT_SPIRV, false, std::ptr::null()) };
        if device.is_null() {
            bail!("SDL_CreateGPUDevice failed: {}", sdl_error());
        }
        let mut renderer = Self {
            device,
            window: std::ptr::null_mut(),
            render_scale,
            scene_size,
            samples: SDL_GPU_SAMPLECOUNT_1,
            depth_format: SDL_GPU_TEXTUREFORMAT_D24_UNORM,
            pipelines: Pipelines {
                mesh_opaque: std::ptr::null_mut(),
                mesh_blend: std::ptr::null_mut(),
                mesh_opaque_unculled: std::ptr::null_mut(),
                mesh_blend_unculled: std::ptr::null_mut(),
                sprite_write: std::ptr::null_mut(),
                sprite_test: std::ptr::null_mut(),
                composite: std::ptr::null_mut(),
            },
            sampler: std::ptr::null_mut(),
            linear: std::ptr::null_mut(),
            scene_colour: std::ptr::null_mut(),
            scene_samples: std::ptr::null_mut(),
            scene_depth: std::ptr::null_mut(),
            layer_black: std::ptr::null_mut(),
            layer_white: std::ptr::null_mut(),
            layer_transfer: std::ptr::null_mut(),
            layer_size: (0, 0),
            layer_image: None,
            sprite_buffer: std::ptr::null_mut(),
            sprite_capacity: 0,
            meshes: HashMap::new(),
            pages: HashMap::new(),
            images: HashMap::new(),
            no_hd: std::ptr::null_mut(),
            no_hd_rows: std::ptr::null_mut(),
            frame: 0,
            retiring: false,
            fresh: Vec::new(),
            mailbox: false,
        };
        // From here a failure drops `renderer`, which releases whatever was
        // created so far.
        renderer.depth_format = if unsafe {
            SDL_GPUTextureSupportsFormat(
                device,
                SDL_GPU_TEXTUREFORMAT_D32_FLOAT,
                SDL_GPU_TEXTURETYPE_2D,
                SDL_GPU_TEXTUREUSAGE_DEPTH_STENCIL_TARGET,
            )
        } {
            SDL_GPU_TEXTUREFORMAT_D32_FLOAT
        } else {
            SDL_GPU_TEXTUREFORMAT_D24_UNORM
        };
        renderer.samples = renderer.sample_count(msaa);
        renderer.create_scene_targets()?;
        renderer.ensure_layers(LAYER_WIDTH, LAYER_HEIGHT)?;
        renderer.sampler = renderer.create_sampler(SDL_GPU_FILTER_NEAREST)?;
        renderer.linear = renderer.create_sampler(SDL_GPU_FILTER_LINEAR)?;
        renderer.no_hd = renderer.array_texture(SDL_GPU_TEXTUREFORMAT_R8G8B8A8_UNORM, 1, 1, 1)?;
        renderer.no_hd_rows = renderer.texture(
            SDL_GPU_TEXTUREFORMAT_R16G16B16A16_UINT,
            2,
            1,
            SDL_GPU_TEXTUREUSAGE_SAMPLER,
        )?;
        renderer.build_scene_pipelines()?;
        renderer.clear_scene()?;
        Ok(renderer)
    }

    /// (Re)create both overlay layers and their staging buffer at `width` x
    /// `height` unless they already have that size. The released ones stay
    /// alive until the presents still reading them finish.
    fn ensure_layers(&mut self, width: u32, height: u32) -> Result<()> {
        if self.layer_size == (width, height) && !self.layer_transfer.is_null() {
            return Ok(());
        }
        unsafe {
            for texture in [self.layer_black, self.layer_white] {
                if !texture.is_null() {
                    SDL_ReleaseGPUTexture(self.device, texture);
                }
            }
            if !self.layer_transfer.is_null() {
                SDL_ReleaseGPUTransferBuffer(self.device, self.layer_transfer);
            }
        }
        self.layer_black = std::ptr::null_mut();
        self.layer_white = std::ptr::null_mut();
        self.layer_transfer = std::ptr::null_mut();
        self.layer_size = (0, 0);
        self.layer_image = None;
        self.layer_black = self.texture(
            SDL_GPU_TEXTUREFORMAT_R8G8B8A8_UNORM,
            width,
            height,
            SDL_GPU_TEXTUREUSAGE_SAMPLER,
        )?;
        self.layer_white = self.texture(
            SDL_GPU_TEXTUREFORMAT_R8G8B8A8_UNORM,
            width,
            height,
            SDL_GPU_TEXTUREUSAGE_SAMPLER,
        )?;
        let transfer_info = SDL_GPUTransferBufferCreateInfo {
            usage: SDL_GPU_TRANSFERBUFFERUSAGE_UPLOAD,
            size: width * height * 4 * 2,
            ..Default::default()
        };
        self.layer_transfer = unsafe { SDL_CreateGPUTransferBuffer(self.device, &transfer_info) };
        if self.layer_transfer.is_null() {
            bail!("SDL_CreateGPUTransferBuffer failed: {}", sdl_error());
        }
        self.layer_size = (width, height);
        Ok(())
    }

    /// The most samples up to `msaa` both scene formats support.
    fn sample_count(&self, msaa: u32) -> SDL_GPUSampleCount {
        let counts = [
            (8, SDL_GPU_SAMPLECOUNT_8),
            (4, SDL_GPU_SAMPLECOUNT_4),
            (2, SDL_GPU_SAMPLECOUNT_2),
        ];
        for (samples, count) in counts {
            if samples <= msaa
                && unsafe {
                    SDL_GPUTextureSupportsSampleCount(
                        self.device,
                        SDL_GPU_TEXTUREFORMAT_R8G8B8A8_UNORM,
                        count,
                    ) && SDL_GPUTextureSupportsSampleCount(self.device, self.depth_format, count)
                }
            {
                return count;
            }
        }
        SDL_GPU_SAMPLECOUNT_1
    }

    fn create_sampler(&self, filter: SDL_GPUFilter) -> Result<*mut SDL_GPUSampler> {
        let info = SDL_GPUSamplerCreateInfo {
            min_filter: filter,
            mag_filter: filter,
            mipmap_mode: SDL_GPU_SAMPLERMIPMAPMODE_NEAREST,
            address_mode_u: SDL_GPU_SAMPLERADDRESSMODE_CLAMP_TO_EDGE,
            address_mode_v: SDL_GPU_SAMPLERADDRESSMODE_CLAMP_TO_EDGE,
            address_mode_w: SDL_GPU_SAMPLERADDRESSMODE_CLAMP_TO_EDGE,
            ..Default::default()
        };
        let sampler = unsafe { SDL_CreateGPUSampler(self.device, &info) };
        if sampler.is_null() {
            bail!("SDL_CreateGPUSampler failed: {}", sdl_error());
        }
        Ok(sampler)
    }

    /// Create the scene's colour, multisample and depth targets at
    /// `scene_size`.
    fn create_scene_targets(&mut self) -> Result<()> {
        let (width, height) = self.scene_size;
        self.scene_colour = self.texture(
            SDL_GPU_TEXTUREFORMAT_R8G8B8A8_UNORM,
            width,
            height,
            SDL_GPUTextureUsageFlags(
                SDL_GPU_TEXTUREUSAGE_SAMPLER.0 | SDL_GPU_TEXTUREUSAGE_COLOR_TARGET.0,
            ),
        )?;
        if self.samples != SDL_GPU_SAMPLECOUNT_1 {
            self.scene_samples = self.target(
                SDL_GPU_TEXTUREFORMAT_R8G8B8A8_UNORM,
                width,
                height,
                SDL_GPU_TEXTUREUSAGE_COLOR_TARGET,
                self.samples,
            )?;
        }
        self.scene_depth = self.target(
            self.depth_format,
            width,
            height,
            SDL_GPU_TEXTUREUSAGE_DEPTH_STENCIL_TARGET,
            self.samples,
        )?;
        Ok(())
    }

    /// Release the scene targets. SDL keeps a texture alive until the work
    /// already submitted against it is done.
    fn release_scene_targets(&mut self) {
        for texture in [
            &mut self.scene_colour,
            &mut self.scene_samples,
            &mut self.scene_depth,
        ] {
            if !texture.is_null() {
                unsafe { SDL_ReleaseGPUTexture(self.device, *texture) };
                *texture = std::ptr::null_mut();
            }
        }
    }

    /// Resize the scene targets to the window's current viewport times
    /// `render_scale`; a renderer with no window keeps its size.
    fn follow_window(&mut self) -> Result<()> {
        if self.window.is_null() {
            return Ok(());
        }
        let (mut width, mut height) = (0, 0);
        if !unsafe { SDL_GetWindowSizeInPixels(self.window, &mut width, &mut height) } {
            bail!("SDL_GetWindowSizeInPixels failed: {}", sdl_error());
        }
        // The scene covers the whole 4:3 frame a wider aspect's band is
        // cropped from.
        let band = video::shown(LAYER_WIDTH, LAYER_HEIGHT, video::aspect().centred());
        let fit = video::fit_rows(width.max(1) as u32, height.max(1) as u32, band.height);
        let (_, _, frame_w, frame_h) = video::frame_viewport(fit, band, LAYER_HEIGHT);
        let viewport = video::Viewport {
            x: 0,
            y: 0,
            width: frame_w.round().max(1.0) as u32,
            height: frame_h.round().max(1.0) as u32,
        };
        let (width, height) = video::scene_size(viewport, self.render_scale);
        let size = (width.min(MAX_SCENE_SIDE), height.min(MAX_SCENE_SIDE));
        if size == self.scene_size && !self.scene_colour.is_null() {
            return Ok(());
        }
        self.release_scene_targets();
        self.scene_size = size;
        self.create_scene_targets()?;
        self.clear_scene()
    }

    /// The scene target's size in pixels.
    pub fn scene_size(&self) -> (u32, u32) {
        self.scene_size
    }

    fn texture(
        &self,
        format: SDL_GPUTextureFormat,
        width: u32,
        height: u32,
        usage: SDL_GPUTextureUsageFlags,
    ) -> Result<*mut SDL_GPUTexture> {
        self.target(format, width, height, usage, SDL_GPU_SAMPLECOUNT_1)
    }

    /// A sampled 2D array texture of `layers` layers.
    fn array_texture(
        &self,
        format: SDL_GPUTextureFormat,
        width: u32,
        height: u32,
        layers: u32,
    ) -> Result<*mut SDL_GPUTexture> {
        let info = SDL_GPUTextureCreateInfo {
            r#type: SDL_GPU_TEXTURETYPE_2D_ARRAY,
            format,
            usage: SDL_GPU_TEXTUREUSAGE_SAMPLER,
            width,
            height,
            layer_count_or_depth: layers,
            num_levels: 1,
            sample_count: SDL_GPU_SAMPLECOUNT_1,
            ..Default::default()
        };
        let texture = unsafe { SDL_CreateGPUTexture(self.device, &info) };
        if texture.is_null() {
            bail!("SDL_CreateGPUTexture failed: {}", sdl_error());
        }
        Ok(texture)
    }

    /// [`GpuRenderer::texture`] with `samples` per pixel.
    fn target(
        &self,
        format: SDL_GPUTextureFormat,
        width: u32,
        height: u32,
        usage: SDL_GPUTextureUsageFlags,
        samples: SDL_GPUSampleCount,
    ) -> Result<*mut SDL_GPUTexture> {
        let info = SDL_GPUTextureCreateInfo {
            r#type: SDL_GPU_TEXTURETYPE_2D,
            format,
            usage,
            width,
            height,
            layer_count_or_depth: 1,
            num_levels: 1,
            sample_count: samples,
            ..Default::default()
        };
        let texture = unsafe { SDL_CreateGPUTexture(self.device, &info) };
        if texture.is_null() {
            bail!("SDL_CreateGPUTexture failed: {}", sdl_error());
        }
        Ok(texture)
    }

    fn shader(
        &self,
        code: &[u8],
        stage: SDL_GPUShaderStage,
        samplers: u32,
        uniforms: u32,
    ) -> Result<Shader> {
        let info = SDL_GPUShaderCreateInfo {
            code_size: code.len(),
            code: code.as_ptr(),
            entrypoint: c"main".as_ptr(),
            format: SDL_GPU_SHADERFORMAT_SPIRV,
            stage,
            num_samplers: samplers,
            num_uniform_buffers: uniforms,
            ..Default::default()
        };
        let shader = unsafe { SDL_CreateGPUShader(self.device, &info) };
        if shader.is_null() {
            bail!("SDL_CreateGPUShader failed: {}", sdl_error());
        }
        Ok(Shader {
            device: self.device,
            shader,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn pipeline(
        &self,
        vertex: *mut SDL_GPUShader,
        fragment: *mut SDL_GPUShader,
        stride: u32,
        attributes: &[SDL_GPUVertexAttribute],
        cull: SDL_GPUCullMode,
        blend: bool,
        depth: Option<(SDL_GPUTextureFormat, bool)>,
        colour_format: SDL_GPUTextureFormat,
        samples: SDL_GPUSampleCount,
    ) -> Result<*mut SDL_GPUGraphicsPipeline> {
        let buffer = SDL_GPUVertexBufferDescription {
            slot: 0,
            pitch: stride,
            input_rate: SDL_GPU_VERTEXINPUTRATE_VERTEX,
            instance_step_rate: 0,
        };
        let blend_state = if blend {
            SDL_GPUColorTargetBlendState {
                src_color_blendfactor: SDL_GPU_BLENDFACTOR_SRC_ALPHA,
                dst_color_blendfactor: SDL_GPU_BLENDFACTOR_ONE_MINUS_SRC_ALPHA,
                color_blend_op: SDL_GPU_BLENDOP_ADD,
                src_alpha_blendfactor: SDL_GPU_BLENDFACTOR_ZERO,
                dst_alpha_blendfactor: SDL_GPU_BLENDFACTOR_ONE,
                alpha_blend_op: SDL_GPU_BLENDOP_ADD,
                enable_blend: true,
                ..Default::default()
            }
        } else {
            SDL_GPUColorTargetBlendState::default()
        };
        let colour_target = SDL_GPUColorTargetDescription {
            format: colour_format,
            blend_state,
        };
        let (depth_format, depth_stencil_state) = match depth {
            Some((format, write)) => (
                format,
                SDL_GPUDepthStencilState {
                    compare_op: SDL_GPU_COMPAREOP_LESS_OR_EQUAL,
                    enable_depth_test: true,
                    enable_depth_write: write,
                    ..Default::default()
                },
            ),
            None => (
                SDL_GPU_TEXTUREFORMAT_INVALID,
                SDL_GPUDepthStencilState::default(),
            ),
        };
        let info = SDL_GPUGraphicsPipelineCreateInfo {
            vertex_shader: vertex,
            fragment_shader: fragment,
            vertex_input_state: SDL_GPUVertexInputState {
                vertex_buffer_descriptions: if attributes.is_empty() {
                    std::ptr::null()
                } else {
                    &buffer
                },
                num_vertex_buffers: u32::from(!attributes.is_empty()),
                vertex_attributes: attributes.as_ptr(),
                num_vertex_attributes: attributes.len() as u32,
            },
            primitive_type: SDL_GPU_PRIMITIVETYPE_TRIANGLELIST,
            rasterizer_state: SDL_GPURasterizerState {
                fill_mode: SDL_GPU_FILLMODE_FILL,
                cull_mode: cull,
                // The software path keeps a packet whose screen-space area is
                // positive in its Y-down frame: clockwise on screen.
                front_face: SDL_GPU_FRONTFACE_CLOCKWISE,
                enable_depth_clip: true,
                ..Default::default()
            },
            multisample_state: SDL_GPUMultisampleState {
                sample_count: samples,
                ..Default::default()
            },
            depth_stencil_state,
            target_info: SDL_GPUGraphicsPipelineTargetInfo {
                color_target_descriptions: &colour_target,
                num_color_targets: 1,
                depth_stencil_format: depth_format,
                has_depth_stencil_target: depth.is_some(),
                ..Default::default()
            },
            ..Default::default()
        };
        let pipeline = unsafe { SDL_CreateGPUGraphicsPipeline(self.device, &info) };
        if pipeline.is_null() {
            bail!("SDL_CreateGPUGraphicsPipeline failed: {}", sdl_error());
        }
        Ok(pipeline)
    }

    fn build_scene_pipelines(&mut self) -> Result<()> {
        let depth_format = self.depth_format;
        let attribute = |location: u32, format: SDL_GPUVertexElementFormat, offset: u32| {
            SDL_GPUVertexAttribute {
                location,
                buffer_slot: 0,
                format,
                offset,
            }
        };
        let mesh_attributes = [
            attribute(0, SDL_GPU_VERTEXELEMENTFORMAT_FLOAT3, 0),
            attribute(1, SDL_GPU_VERTEXELEMENTFORMAT_FLOAT3, 12),
            attribute(2, SDL_GPU_VERTEXELEMENTFORMAT_FLOAT2, 24),
            attribute(3, SDL_GPU_VERTEXELEMENTFORMAT_FLOAT3, 32),
            attribute(4, SDL_GPU_VERTEXELEMENTFORMAT_UINT, 44),
        ];
        let sprite_attributes = [
            attribute(0, SDL_GPU_VERTEXELEMENTFORMAT_FLOAT4, 0),
            attribute(1, SDL_GPU_VERTEXELEMENTFORMAT_FLOAT2, 16),
            attribute(2, SDL_GPU_VERTEXELEMENTFORMAT_FLOAT4, 24),
            attribute(3, SDL_GPU_VERTEXELEMENTFORMAT_UINT, 40),
        ];
        let mesh_stride = std::mem::size_of::<MeshVertex>() as u32;
        let sprite_stride = std::mem::size_of::<SpriteVertex>() as u32;
        let colour = SDL_GPU_TEXTUREFORMAT_R8G8B8A8_UNORM;

        let mesh_vert = self.shader(MESH_VERT, SDL_GPU_SHADERSTAGE_VERTEX, 0, 1)?;
        let mesh_frag = self.shader(MESH_FRAG, SDL_GPU_SHADERSTAGE_FRAGMENT, 4, 1)?;
        let sprite_vert = self.shader(SPRITE_VERT, SDL_GPU_SHADERSTAGE_VERTEX, 0, 0)?;
        let sprite_frag = self.shader(SPRITE_FRAG, SDL_GPU_SHADERSTAGE_FRAGMENT, 1, 1)?;
        let mesh = |this: &Self, cull, blend, write| {
            this.pipeline(
                mesh_vert.shader,
                mesh_frag.shader,
                mesh_stride,
                &mesh_attributes,
                cull,
                blend,
                Some((depth_format, write)),
                colour,
                this.samples,
            )
        };
        let sprite = |this: &Self, write| {
            this.pipeline(
                sprite_vert.shader,
                sprite_frag.shader,
                sprite_stride,
                &sprite_attributes,
                SDL_GPU_CULLMODE_NONE,
                true,
                Some((depth_format, write)),
                colour,
                this.samples,
            )
        };
        // Each pipeline is stored as soon as it exists, so a later failure
        // still releases it (via `Drop`).
        self.pipelines.mesh_opaque = mesh(self, SDL_GPU_CULLMODE_BACK, false, true)?;
        self.pipelines.mesh_blend = mesh(self, SDL_GPU_CULLMODE_BACK, true, false)?;
        self.pipelines.mesh_opaque_unculled = mesh(self, SDL_GPU_CULLMODE_NONE, false, true)?;
        self.pipelines.mesh_blend_unculled = mesh(self, SDL_GPU_CULLMODE_NONE, true, false)?;
        self.pipelines.sprite_write = sprite(self, true)?;
        self.pipelines.sprite_test = sprite(self, false)?;
        Ok(())
    }

    fn composite_pipeline(
        &self,
        format: SDL_GPUTextureFormat,
    ) -> Result<*mut SDL_GPUGraphicsPipeline> {
        let vertex = self.shader(COMPOSITE_VERT, SDL_GPU_SHADERSTAGE_VERTEX, 0, 0)?;
        let fragment = self.shader(COMPOSITE_FRAG, SDL_GPU_SHADERSTAGE_FRAGMENT, 3, 0)?;
        self.pipeline(
            vertex.shader,
            fragment.shader,
            0,
            &[],
            SDL_GPU_CULLMODE_NONE,
            false,
            None,
            format,
            SDL_GPU_SAMPLECOUNT_1,
        )
    }

    fn command_buffer(&self) -> Result<*mut SDL_GPUCommandBuffer> {
        let command = unsafe { SDL_AcquireGPUCommandBuffer(self.device) };
        if command.is_null() {
            bail!("SDL_AcquireGPUCommandBuffer failed: {}", sdl_error());
        }
        Ok(command)
    }

    fn submit(command: *mut SDL_GPUCommandBuffer) -> Result<()> {
        if !unsafe { SDL_SubmitGPUCommandBuffer(command) } {
            bail!("SDL_SubmitGPUCommandBuffer failed: {}", sdl_error());
        }
        Ok(())
    }

    /// Clear the scene target to black so a composite before the first scene
    /// shows nothing under the overlays.
    fn clear_scene(&self) -> Result<()> {
        let command = self.command_buffer()?;
        let target = SDL_GPUColorTargetInfo {
            texture: self.scene_colour,
            clear_color: SDL_FColor {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            load_op: SDL_GPU_LOADOP_CLEAR,
            store_op: SDL_GPU_STOREOP_STORE,
            ..Default::default()
        };
        let pass = unsafe { SDL_BeginGPURenderPass(command, &target, 1, std::ptr::null()) };
        if !pass.is_null() {
            unsafe { SDL_EndGPURenderPass(pass) };
        }
        Self::submit(command)
    }

    /// Stage `bytes` in a transient transfer buffer.
    fn stage(&self, bytes: &[u8]) -> Result<*mut SDL_GPUTransferBuffer> {
        let info = SDL_GPUTransferBufferCreateInfo {
            usage: SDL_GPU_TRANSFERBUFFERUSAGE_UPLOAD,
            size: bytes.len() as u32,
            ..Default::default()
        };
        let transfer = unsafe { SDL_CreateGPUTransferBuffer(self.device, &info) };
        if transfer.is_null() {
            bail!("SDL_CreateGPUTransferBuffer failed: {}", sdl_error());
        }
        let mapped = unsafe { SDL_MapGPUTransferBuffer(self.device, transfer, false) };
        if mapped.is_null() {
            unsafe { SDL_ReleaseGPUTransferBuffer(self.device, transfer) };
            bail!("SDL_MapGPUTransferBuffer failed: {}", sdl_error());
        }
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), mapped.cast::<u8>(), bytes.len());
            SDL_UnmapGPUTransferBuffer(self.device, transfer);
        }
        Ok(transfer)
    }

    /// Record an upload of a whole texture; the staging buffer is released
    /// once the copy has executed.
    fn upload_texture(
        &self,
        copy: *mut SDL_GPUCopyPass,
        texture: *mut SDL_GPUTexture,
        width: u32,
        height: u32,
        bytes: &[u8],
    ) -> Result<()> {
        self.upload_layer(copy, texture, 0, width, height, bytes)
    }

    /// [`GpuRenderer::upload_texture`] into the top-left `width` x `height`
    /// of an array texture's `layer`.
    fn upload_layer(
        &self,
        copy: *mut SDL_GPUCopyPass,
        texture: *mut SDL_GPUTexture,
        layer: u32,
        width: u32,
        height: u32,
        bytes: &[u8],
    ) -> Result<()> {
        let transfer = self.stage(bytes)?;
        let source = SDL_GPUTextureTransferInfo {
            transfer_buffer: transfer,
            offset: 0,
            pixels_per_row: width,
            rows_per_layer: height,
        };
        let destination = SDL_GPUTextureRegion {
            texture,
            layer,
            w: width,
            h: height,
            d: 1,
            ..Default::default()
        };
        unsafe {
            SDL_UploadToGPUTexture(copy, &source, &destination, false);
            SDL_ReleaseGPUTransferBuffer(self.device, transfer);
        }
        Ok(())
    }

    fn upload_buffer(
        &self,
        copy: *mut SDL_GPUCopyPass,
        buffer: *mut SDL_GPUBuffer,
        bytes: &[u8],
        cycle: bool,
    ) -> Result<()> {
        let transfer = self.stage(bytes)?;
        let source = SDL_GPUTransferBufferLocation {
            transfer_buffer: transfer,
            offset: 0,
        };
        let destination = SDL_GPUBufferRegion {
            buffer,
            offset: 0,
            size: bytes.len() as u32,
        };
        unsafe {
            SDL_UploadToGPUBuffer(copy, &source, &destination, cycle);
            SDL_ReleaseGPUTransferBuffer(self.device, transfer);
        }
        Ok(())
    }

    fn vertex_buffer(&self, size: u32) -> Result<*mut SDL_GPUBuffer> {
        let info = SDL_GPUBufferCreateInfo {
            usage: SDL_GPU_BUFFERUSAGE_VERTEX,
            size,
            ..Default::default()
        };
        let buffer = unsafe { SDL_CreateGPUBuffer(self.device, &info) };
        if buffer.is_null() {
            bail!("SDL_CreateGPUBuffer failed: {}", sdl_error());
        }
        Ok(buffer)
    }

    /// Make `mesh`'s vertex buffer resident.
    fn ensure_mesh(&mut self, copy: *mut SDL_GPUCopyPass, mesh: &Tmd) -> Result<MeshBuffer> {
        let key = mesh as *const Tmd as usize;
        let fingerprint = scene::mesh_fingerprint(mesh);
        if let Some(entry) = self.meshes.get_mut(&key)
            && entry.fingerprint == fingerprint
        {
            entry.last_used = self.frame;
            return Ok(entry.value);
        }
        let built = scene::mesh_vertices(mesh);
        let bytes = as_bytes(&built.vertices);
        let buffer = if bytes.is_empty() {
            std::ptr::null_mut()
        } else {
            let buffer = self.vertex_buffer(bytes.len() as u32)?;
            if let Err(err) = self.upload_buffer(copy, buffer, bytes, false) {
                unsafe { SDL_ReleaseGPUBuffer(self.device, buffer) };
                return Err(err);
            }
            buffer
        };
        let rows = built.vertices.iter().fold(0u32, |rows, vertex| {
            rows | 1u32.checked_shl((vertex.packed >> 8) & 0xFFFF).unwrap_or(0)
        });
        let value = MeshBuffer {
            buffer,
            vertex_count: built.vertices.len() as u32,
            blend_start: built.blend_start,
            rows,
        };
        self.fresh.push(Upload::Mesh(key));
        if let Some(old) = self.meshes.insert(
            key,
            Resident {
                value,
                fingerprint,
                last_used: self.frame,
            },
        ) {
            self.release_mesh(old.value);
        }
        Ok(value)
    }

    /// Make an indexed page and its palette resident, with the HD
    /// replacements `pack` carries for its CLUT rows. A missing replacement
    /// is reported only for the rows in `used`.
    fn ensure_page(
        &mut self,
        copy: *mut SDL_GPUCopyPass,
        texture: &Texture8,
        used: u32,
        pack: Option<&Pack>,
    ) -> Result<Option<PageTextures>> {
        // Like the software rasterizer, an empty page draws nothing; a
        // truncated one draws its missing texels as index 0 (transparent).
        if texture.width == 0 || texture.height == 0 {
            return Ok(None);
        }
        let key = texture as *const Texture8 as usize;
        let pack = pack.filter(|pack| !pack.htexture().is_empty());
        let fingerprint = scene::texture_fingerprint(texture);
        if let Some(entry) = self.pages.get_mut(&key)
            && entry.fingerprint == fingerprint
        {
            entry.last_used = self.frame;
            return Ok(Some(entry.value));
        }
        let indices = self.texture(
            SDL_GPU_TEXTUREFORMAT_R8_UINT,
            texture.width,
            texture.height,
            SDL_GPU_TEXTUREUSAGE_SAMPLER,
        )?;
        let (rows, palette_rgba) = scene::palette_rgba(texture);
        let palette = match self.texture(
            SDL_GPU_TEXTUREFORMAT_R8G8B8A8_UNORM,
            crate::model::PALETTE_ROW_LEN as u32,
            rows,
            SDL_GPU_TEXTUREUSAGE_SAMPLER,
        ) {
            Ok(palette) => palette,
            Err(err) => {
                unsafe { SDL_ReleaseGPUTexture(self.device, indices) };
                return Err(err);
            }
        };
        let mut value = PageTextures {
            indices,
            palette,
            hd: std::ptr::null_mut(),
            hd_rows: std::ptr::null_mut(),
        };
        let pixels = (texture.width * texture.height) as usize;
        let padded;
        let page_indices = if texture.indices.len() >= pixels {
            &texture.indices[..pixels]
        } else {
            padded = {
                let mut padded = texture.indices.clone();
                padded.resize(pixels, 0);
                padded
            };
            &padded[..]
        };
        let uploaded = self
            .upload_texture(copy, indices, texture.width, texture.height, page_indices)
            .and_then(|()| {
                self.upload_texture(
                    copy,
                    palette,
                    crate::model::PALETTE_ROW_LEN as u32,
                    rows,
                    &palette_rgba,
                )
            })
            .and_then(|()| match pack {
                Some(pack) => self.upload_page_hd(copy, &mut value, texture, used, pack),
                None => Ok(()),
            });
        if let Err(err) = uploaded {
            self.release_page(value);
            return Err(err);
        }
        self.fresh.push(Upload::Page(key));
        if let Some(old) = self.pages.insert(
            key,
            Resident {
                value,
                fingerprint,
                last_used: self.frame,
            },
        ) {
            self.release_page(old.value);
        }
        Ok(Some(value))
    }

    /// Look up the replacement of each of `texture`'s CLUT rows and upload
    /// the ones found into `page`; a miss is reported for the rows in `used`.
    fn upload_page_hd(
        &self,
        copy: *mut SDL_GPUCopyPass,
        page: &mut PageTextures,
        texture: &Texture8,
        used: u32,
        pack: &Pack,
    ) -> Result<()> {
        let table = pack.htexture();
        let row_count = texture
            .palettes
            .len()
            .div_ceil(crate::model::PALETTE_ROW_LEN)
            .max(1);
        // Rows sharing a replacement share its layer.
        let mut layers: Vec<(String, Image)> = Vec::new();
        let mut placed = vec![[0u16; 8]; row_count];
        for (row, place) in placed.iter_mut().enumerate() {
            let candidates = htexture::page_candidates(texture, row);
            let found = if used & 1u32.checked_shl(row as u32).unwrap_or(0) != 0 {
                table.find(&candidates, || {
                    format!(
                        "{}x{} model page, CLUT row {row}",
                        texture.width, texture.height
                    )
                })
            } else {
                table.find_quietly(&candidates)
            };
            let Some(found) = found else {
                continue;
            };
            let layer = match layers.iter().position(|(path, _)| *path == found.path) {
                Some(layer) => layer,
                None => match htexture::load(pack, &found.path) {
                    Ok(image) if image.width > 0 && image.height > 0 => {
                        layers.push((found.path, image));
                        layers.len() - 1
                    }
                    Ok(_) => continue,
                    Err(err) => {
                        eprintln!("htexture: {err:#}");
                        continue;
                    }
                },
            };
            let image = &layers[layer].1;
            *place = [
                layer as u16 + 1,
                found.frame[0].min(0xFFFF) as u16,
                found.frame[1].min(0xFFFF) as u16,
                0,
                image.width.min(0xFFFF) as u16,
                image.height.min(0xFFFF) as u16,
                0,
                0,
            ];
        }
        if layers.is_empty() {
            return Ok(());
        }
        let width = layers
            .iter()
            .map(|(_, image)| image.width)
            .max()
            .unwrap_or(1);
        let height = layers
            .iter()
            .map(|(_, image)| image.height)
            .max()
            .unwrap_or(1);
        page.hd = self.array_texture(
            SDL_GPU_TEXTUREFORMAT_R8G8B8A8_UNORM,
            width,
            height,
            layers.len() as u32,
        )?;
        page.hd_rows = self.texture(
            SDL_GPU_TEXTUREFORMAT_R16G16B16A16_UINT,
            2,
            row_count as u32,
            SDL_GPU_TEXTUREUSAGE_SAMPLER,
        )?;
        for (layer, (_, image)) in layers.iter().enumerate() {
            let bytes = &image.rgba[..(image.width * image.height * 4) as usize];
            self.upload_layer(
                copy,
                page.hd,
                layer as u32,
                image.width,
                image.height,
                bytes,
            )?;
        }
        let bytes: Vec<u8> = placed
            .iter()
            .flatten()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        self.upload_texture(copy, page.hd_rows, 2, row_count as u32, &bytes)
    }

    /// The HD replacement `pack` carries for `image`, and its texels per
    /// original texel.
    fn image_replacement(image: &Image, pack: &Pack) -> Option<(Image, [f32; 2])> {
        let found = pack
            .htexture()
            .find(&htexture::image_candidates(image), || {
                format!("{}x{} image", image.width, image.height)
            })?;
        match htexture::load(pack, &found.path) {
            Ok(hd)
                if hd.width > 0
                    && hd.height > 0
                    && hd.rgba.len() >= (hd.width * hd.height * 4) as usize =>
            {
                let scale = [
                    hd.width as f32 / found.frame[0].max(1) as f32,
                    hd.height as f32 / found.frame[1].max(1) as f32,
                ];
                Some((hd, scale))
            }
            Ok(_) => None,
            Err(err) => {
                eprintln!("htexture: {err:#}");
                None
            }
        }
    }

    /// Make an RGBA image, or the HD replacement `pack` carries for it,
    /// resident.
    fn ensure_image(
        &mut self,
        copy: *mut SDL_GPUCopyPass,
        image: &Image,
        pack: Option<&Pack>,
    ) -> Result<Option<ImageTexture>> {
        if image.width == 0
            || image.height == 0
            || image.rgba.len() < (image.width * image.height * 4) as usize
        {
            return Ok(None);
        }
        let key = image as *const Image as usize;
        let fingerprint = scene::image_fingerprint(image);
        if let Some(entry) = self.images.get_mut(&key)
            && entry.fingerprint == fingerprint
        {
            entry.last_used = self.frame;
            return Ok(Some(entry.value));
        }
        let replacement = pack
            .filter(|pack| !pack.htexture().is_empty())
            .and_then(|pack| Self::image_replacement(image, pack));
        let (source, scale) = match &replacement {
            Some((hd, scale)) => (hd, *scale),
            None => (image, [1.0, 1.0]),
        };
        let texture = self.texture(
            SDL_GPU_TEXTUREFORMAT_R8G8B8A8_UNORM,
            source.width,
            source.height,
            SDL_GPU_TEXTUREUSAGE_SAMPLER,
        )?;
        let bytes = &source.rgba[..(source.width * source.height * 4) as usize];
        if let Err(err) = self.upload_texture(copy, texture, source.width, source.height, bytes) {
            unsafe { SDL_ReleaseGPUTexture(self.device, texture) };
            return Err(err);
        }
        let value = ImageTexture { texture, scale };
        self.fresh.push(Upload::Image(key));
        if let Some(old) = self.images.insert(
            key,
            Resident {
                value,
                fingerprint,
                last_used: self.frame,
            },
        ) {
            unsafe { SDL_ReleaseGPUTexture(self.device, old.value.texture) };
        }
        Ok(Some(value))
    }

    fn release_mesh(&self, mesh: MeshBuffer) {
        if !mesh.buffer.is_null() {
            unsafe { SDL_ReleaseGPUBuffer(self.device, mesh.buffer) };
        }
    }

    fn release_page(&self, page: PageTextures) {
        unsafe {
            SDL_ReleaseGPUTexture(self.device, page.indices);
            SDL_ReleaseGPUTexture(self.device, page.palette);
            for texture in [page.hd, page.hd_rows] {
                if !texture.is_null() {
                    SDL_ReleaseGPUTexture(self.device, texture);
                }
            }
        }
    }

    /// Release every upload not drawn in the last [`EVICT_AFTER_FRAMES`]
    /// scene frames. SDL defers the release until in-flight work is done.
    /// After [`GpuRenderer::retire`], release every upload the frame did not
    /// draw.
    fn evict(&mut self) {
        let frame = self.frame;
        let horizon = if std::mem::take(&mut self.retiring) {
            0
        } else {
            EVICT_AFTER_FRAMES
        };
        let stale = |last_used: u64| frame.saturating_sub(last_used) > horizon;
        let meshes: Vec<usize> = self
            .meshes
            .iter()
            .filter(|(_, entry)| stale(entry.last_used))
            .map(|(&key, _)| key)
            .collect();
        for key in meshes {
            if let Some(entry) = self.meshes.remove(&key) {
                self.release_mesh(entry.value);
            }
        }
        let pages: Vec<usize> = self
            .pages
            .iter()
            .filter(|(_, entry)| stale(entry.last_used))
            .map(|(&key, _)| key)
            .collect();
        for key in pages {
            if let Some(entry) = self.pages.remove(&key) {
                self.release_page(entry.value);
            }
        }
        let images: Vec<usize> = self
            .images
            .iter()
            .filter(|(_, entry)| stale(entry.last_used))
            .map(|(&key, _)| key)
            .collect();
        for key in images {
            if let Some(entry) = self.images.remove(&key) {
                unsafe { SDL_ReleaseGPUTexture(self.device, entry.value.texture) };
            }
        }
    }

    /// Tear down the previous room's residency: the next recorded scene
    /// keeps what it draws and releases everything else.
    pub fn retire(&mut self) {
        self.retiring = true;
    }

    /// Release every resident upload: gameplay has ended, so nothing cached
    /// will be drawn soon.
    pub fn release_residency(&mut self) {
        for (_, entry) in std::mem::take(&mut self.meshes) {
            self.release_mesh(entry.value);
        }
        for (_, entry) in std::mem::take(&mut self.pages) {
            self.release_page(entry.value);
        }
        for (_, entry) in std::mem::take(&mut self.images) {
            unsafe { SDL_ReleaseGPUTexture(self.device, entry.value.texture) };
        }
    }

    /// Forget the entries a failed recording created: their uploads were
    /// cancelled with its command buffer, so the data never arrived.
    fn forget_fresh(&mut self) {
        for upload in std::mem::take(&mut self.fresh) {
            match upload {
                Upload::Mesh(key) => {
                    if let Some(entry) = self.meshes.remove(&key) {
                        self.release_mesh(entry.value);
                    }
                }
                Upload::Page(key) => {
                    if let Some(entry) = self.pages.remove(&key) {
                        self.release_page(entry.value);
                    }
                }
                Upload::Image(key) => {
                    if let Some(entry) = self.images.remove(&key) {
                        unsafe { SDL_ReleaseGPUTexture(self.device, entry.value.texture) };
                    }
                }
            }
        }
    }

    /// The number of resident meshes, pages and images.
    pub fn resident_counts(&self) -> (usize, usize, usize) {
        (self.meshes.len(), self.pages.len(), self.images.len())
    }

    /// Draw `scene` into the scene target, with the HD replacements `pack`
    /// carries. The result persists until the next call, so a frozen frame
    /// (the pause menu) keeps compositing it.
    pub fn record_scene(&mut self, scene: &GameplayScene<'_>, pack: Option<&Pack>) -> Result<()> {
        self.follow_window()?;
        self.frame += 1;
        self.fresh.clear();
        let command = self.command_buffer()?;
        let recorded = self.record_scene_into(command, scene, pack);
        let submitted = match recorded {
            Ok(()) => Self::submit(command),
            Err(err) => {
                unsafe { SDL_CancelGPUCommandBuffer(command) };
                Err(err)
            }
        };
        if submitted.is_err() {
            self.forget_fresh();
        } else {
            self.fresh.clear();
            self.evict();
        }
        submitted
    }

    fn record_scene_into(
        &mut self,
        command: *mut SDL_GPUCommandBuffer,
        scene: &GameplayScene<'_>,
        pack: Option<&Pack>,
    ) -> Result<()> {
        // Uploads first: the residency checks and this frame's sprites.
        let copy = unsafe { SDL_BeginGPUCopyPass(command) };
        if copy.is_null() {
            bail!("SDL_BeginGPUCopyPass failed: {}", sdl_error());
        }
        let uploads = (|| -> Result<_> {
            let mut meshes = Vec::with_capacity(scene.meshes.len());
            for mesh in scene.meshes {
                let buffer = self.ensure_mesh(copy, mesh.mesh)?;
                let page = self.ensure_page(copy, mesh.texture, buffer.rows, pack)?;
                meshes.push(page.map(|page| (buffer, page)));
            }
            let sprites = scene::scene_sprites(scene);
            let mut sprite_textures = HashMap::new();
            for run in sprites.before.iter().chain(&sprites.after) {
                let key = run.image as *const Image as usize;
                if let std::collections::hash_map::Entry::Vacant(slot) = sprite_textures.entry(key)
                    && let Some(texture) = self.ensure_image(copy, run.image, pack)?
                {
                    slot.insert(texture);
                }
            }
            let bytes = as_bytes(&sprites.vertices);
            if !bytes.is_empty() {
                if self.sprite_capacity < bytes.len() as u32 {
                    let capacity = (bytes.len() as u32).next_power_of_two().max(4096);
                    let buffer = self.vertex_buffer(capacity)?;
                    if !self.sprite_buffer.is_null() {
                        unsafe { SDL_ReleaseGPUBuffer(self.device, self.sprite_buffer) };
                    }
                    self.sprite_buffer = buffer;
                    self.sprite_capacity = capacity;
                }
                self.upload_buffer(copy, self.sprite_buffer, bytes, true)?;
            }
            Ok((meshes, sprites, sprite_textures))
        })();
        unsafe { SDL_EndGPUCopyPass(copy) };
        let (meshes, sprites, sprite_textures) = uploads?;

        let clear_color = SDL_FColor {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        };
        // With MSAA the samples resolve into the target the composite reads.
        let colour = if self.scene_samples.is_null() {
            SDL_GPUColorTargetInfo {
                texture: self.scene_colour,
                clear_color,
                load_op: SDL_GPU_LOADOP_CLEAR,
                store_op: SDL_GPU_STOREOP_STORE,
                cycle: true,
                ..Default::default()
            }
        } else {
            SDL_GPUColorTargetInfo {
                texture: self.scene_samples,
                clear_color,
                load_op: SDL_GPU_LOADOP_CLEAR,
                store_op: SDL_GPU_STOREOP_RESOLVE,
                resolve_texture: self.scene_colour,
                cycle: true,
                cycle_resolve_texture: true,
                ..Default::default()
            }
        };
        let depth = SDL_GPUDepthStencilTargetInfo {
            texture: self.scene_depth,
            clear_depth: 1.0,
            load_op: SDL_GPU_LOADOP_CLEAR,
            store_op: SDL_GPU_STOREOP_DONT_CARE,
            stencil_load_op: SDL_GPU_LOADOP_DONT_CARE,
            stencil_store_op: SDL_GPU_STOREOP_DONT_CARE,
            cycle: true,
            ..Default::default()
        };
        let pass = unsafe { SDL_BeginGPURenderPass(command, &colour, 1, &depth) };
        if pass.is_null() {
            bail!("SDL_BeginGPURenderPass failed: {}", sdl_error());
        }

        let draw_sprites = |runs: &[SpriteRun<'_>]| {
            if self.sprite_buffer.is_null() {
                return;
            }
            let binding = SDL_GPUBufferBinding {
                buffer: self.sprite_buffer,
                offset: 0,
            };
            for run in runs {
                let Some(&texture) = sprite_textures.get(&(run.image as *const Image as usize))
                else {
                    continue;
                };
                let pipeline = if run.depth_write {
                    self.pipelines.sprite_write
                } else {
                    self.pipelines.sprite_test
                };
                let sampler = SDL_GPUTextureSamplerBinding {
                    texture: texture.texture,
                    sampler: self.sampler,
                };
                let scale = [texture.scale[0], texture.scale[1], 0.0, 0.0];
                unsafe {
                    SDL_BindGPUGraphicsPipeline(pass, pipeline);
                    SDL_PushGPUFragmentUniformData(
                        command,
                        0,
                        scale.as_ptr().cast::<c_void>(),
                        std::mem::size_of_val(&scale) as u32,
                    );
                    SDL_BindGPUVertexBuffers(pass, 0, &binding, 1);
                    SDL_BindGPUFragmentSamplers(pass, 0, &sampler, 1);
                    SDL_DrawGPUPrimitives(pass, run.count, 1, run.first, 0);
                }
            }
        };
        draw_sprites(&sprites.before);

        // The opaque texels of every mesh, then the mirror copy, then the
        // STP texels of the blended packets in the same order.
        let lights: Vec<_> = scene
            .meshes
            .iter()
            .map(|mesh| scene.lighting.latched(root_position(mesh.joints)))
            .collect();
        let lights = &lights;
        for blend_pass in [false, true] {
            let primary = scene.meshes.iter().enumerate().map(|(index, mesh)| {
                (
                    index,
                    mesh,
                    scene::draw_uniforms(
                        scene.camera,
                        mesh,
                        &lights[index],
                        index < scene.mirror_from,
                        None,
                    ),
                    false,
                )
            });
            let mirrored =
                scene.mirror.into_iter().flat_map(|mirror| {
                    scene.meshes.iter().enumerate().skip(scene.mirror_from).map(
                        move |(index, mesh)| {
                            (
                                index,
                                mesh,
                                scene::draw_uniforms(
                                    &mirror.camera,
                                    mesh,
                                    &lights[index],
                                    false,
                                    Some(mirror),
                                ),
                                true,
                            )
                        },
                    )
                });
            for (index, mesh, uniforms, unculled) in primary.chain(mirrored) {
                let Some((buffer, page)) = meshes[index] else {
                    continue;
                };
                let (first, count) = if blend_pass {
                    (buffer.blend_start, buffer.vertex_count - buffer.blend_start)
                } else {
                    (0, buffer.vertex_count)
                };
                if buffer.buffer.is_null() || count == 0 || uniforms.flags[0] == u32::MAX {
                    continue;
                }
                let pipeline = match (blend_pass, unculled) {
                    (false, false) => self.pipelines.mesh_opaque,
                    (true, false) => self.pipelines.mesh_blend,
                    (false, true) => self.pipelines.mesh_opaque_unculled,
                    (true, true) => self.pipelines.mesh_blend_unculled,
                };
                let mut material = scene::material(mesh.blend_weight, blend_pass);
                let hd = if page.hd.is_null() {
                    [self.no_hd, self.no_hd_rows]
                } else {
                    material[2] = 1.0;
                    [page.hd, page.hd_rows]
                };
                draw_mesh(
                    command,
                    pass,
                    pipeline,
                    &uniforms,
                    material,
                    buffer.buffer,
                    [page.indices, page.palette, hd[0], hd[1]],
                    self.sampler,
                    first,
                    count,
                );
            }
        }

        draw_sprites(&sprites.after);
        unsafe { SDL_EndGPURenderPass(pass) };
        Ok(())
    }

    /// Composite the software layers over the scene and present.
    ///
    /// `black` is the frame drawn over a black scene and `white` the same
    /// frame over a white one; `None` (a screen with no gameplay scene under
    /// it) shows `black` as is.
    pub fn present(&mut self, black: &Framebuffer, white: Option<&Framebuffer>) -> Result<()> {
        if self.window.is_null() || self.pipelines.composite.is_null() {
            bail!("the GPU renderer has no window to present to");
        }
        // A 320x240 frame, or a wider aspect's full-height canvas (which
        // never shows the scene, so it has no white layer).
        let (layer_w, layer_h) = (black.width, black.height);
        let layer_bytes = layer_w * layer_h * 4;
        let canvas = layer_h == LAYER_HEIGHT && layer_w > LAYER_WIDTH && white.is_none();
        for layer in std::iter::once(black).chain(white) {
            if !(canvas || (layer.width == LAYER_WIDTH && layer.height == LAYER_HEIGHT))
                || layer.width != layer_w
                || layer.height != layer_h
                || layer.rgba.len() != layer_bytes as usize
            {
                bail!(
                    "expected a {LAYER_WIDTH}x{LAYER_HEIGHT} framebuffer, got {}x{}",
                    layer.width,
                    layer.height
                );
            }
        }
        // The whole 4:3 frame is composited into the viewport that puts its
        // visible band on the fitted rectangle, and the scissor crops the
        // rest (the band is the whole frame at 4:3).
        let shown = video::shown(black.width, black.height, black.view);
        let layers = Layers::Pair([&black.rgba, &white.unwrap_or(black).rgba]);
        self.composite(layers, (layer_w, layer_h), |width, height| {
            let fit = video::fit_size(width, height, shown.width, shown.height);
            (video::frame_viewport(fit, shown, black.height), fit)
        })
    }

    /// Present a `width` x `height` RGBA image (an HD film frame) at its own
    /// resolution where `frame` would be: letterboxed to its aspect inside
    /// the 4:3 frame's viewport and cropped to the band `frame` shows. An
    /// image whose `serial` is already uploaded is not uploaded again.
    pub fn present_image(
        &mut self,
        frame: &Framebuffer,
        rgba: &[u8],
        (width, height): (u32, u32),
        serial: u64,
    ) -> Result<()> {
        if self.window.is_null() || self.pipelines.composite.is_null() {
            bail!("the GPU renderer has no window to present to");
        }
        if width == 0
            || height == 0
            || width > crate::media::MAX_DIMENSION
            || height > crate::media::MAX_DIMENSION
            || rgba.len() != width as usize * height as usize * 4
        {
            bail!("unexpected {width}x{height} image of {} bytes", rgba.len());
        }
        let shown = video::shown(frame.width, frame.height, frame.view);
        let layers = if self.layer_image == Some(serial) && self.layer_size == (width, height) {
            Layers::Kept
        } else {
            Layers::Single(rgba)
        };
        self.composite(layers, (width, height), |target_w, target_h| {
            let fit = video::fit_size(target_w, target_h, shown.width, shown.height);
            let frame_viewport = video::frame_viewport(fit, shown, frame.height);
            (video::image_viewport(frame_viewport, width, height), fit)
        })?;
        self.layer_image = Some(serial);
        Ok(())
    }

    /// Upload the `size` layers and composite them over the scene into the
    /// viewport `place` returns for the swapchain size, clipped to its
    /// rectangle.
    fn composite(
        &mut self,
        layers: Layers<'_>,
        (layer_w, layer_h): (u32, u32),
        place: impl FnOnce(u32, u32) -> ((f32, f32, f32, f32), video::Viewport),
    ) -> Result<()> {
        let layer_bytes = layer_w * layer_h * 4;
        self.ensure_layers(layer_w, layer_h)?;
        let uploads: &[&[u8]] = match &layers {
            Layers::Pair(pair) => pair,
            Layers::Single(image) => std::slice::from_ref(image),
            Layers::Kept => &[],
        };
        if !uploads.is_empty() {
            // A layer pair replaces any single image.
            self.layer_image = None;
            // Cycling the staging buffer and the layer textures lets this
            // upload proceed while the previous present may still read them.
            let mapped =
                unsafe { SDL_MapGPUTransferBuffer(self.device, self.layer_transfer, true) };
            if mapped.is_null() {
                bail!("SDL_MapGPUTransferBuffer failed: {}", sdl_error());
            }
            unsafe {
                for (index, layer) in uploads.iter().enumerate() {
                    std::ptr::copy_nonoverlapping(
                        layer.as_ptr(),
                        mapped.cast::<u8>().add(index * layer_bytes as usize),
                        layer_bytes as usize,
                    );
                }
                SDL_UnmapGPUTransferBuffer(self.device, self.layer_transfer);
            }
        }
        let command = self.command_buffer()?;
        if !uploads.is_empty() {
            self.upload_layers(command, uploads.len(), (layer_w, layer_h))?;
        }
        // A single image is composited over itself: `black + scene * 0`.
        let white = match layers {
            Layers::Pair(_) => self.layer_white,
            Layers::Single(_) | Layers::Kept => self.layer_black,
        };
        self.composite_pass(command, white, place)?;
        Self::submit(command)
    }

    /// Upload `count` staged layers into `layer_black` (then `layer_white`).
    fn upload_layers(
        &self,
        command: *mut SDL_GPUCommandBuffer,
        count: usize,
        (layer_w, layer_h): (u32, u32),
    ) -> Result<()> {
        let layer_bytes = layer_w * layer_h * 4;
        let copy = unsafe { SDL_BeginGPUCopyPass(command) };
        if copy.is_null() {
            unsafe { SDL_CancelGPUCommandBuffer(command) };
            bail!("SDL_BeginGPUCopyPass failed: {}", sdl_error());
        }
        for (index, texture) in [self.layer_black, self.layer_white]
            .into_iter()
            .enumerate()
            .take(count)
        {
            let source = SDL_GPUTextureTransferInfo {
                transfer_buffer: self.layer_transfer,
                offset: index as u32 * layer_bytes,
                pixels_per_row: layer_w,
                rows_per_layer: layer_h,
            };
            let destination = SDL_GPUTextureRegion {
                texture,
                w: layer_w,
                h: layer_h,
                d: 1,
                ..Default::default()
            };
            unsafe { SDL_UploadToGPUTexture(copy, &source, &destination, true) };
        }
        unsafe { SDL_EndGPUCopyPass(copy) };
        Ok(())
    }

    /// Record the composite of `layer_black` and `white` over the scene into
    /// the swapchain, placed by `place`. The command buffer is cancelled on
    /// failure and left to the caller to submit otherwise.
    fn composite_pass(
        &self,
        command: *mut SDL_GPUCommandBuffer,
        white: *mut SDL_GPUTexture,
        place: impl FnOnce(u32, u32) -> ((f32, f32, f32, f32), video::Viewport),
    ) -> Result<()> {
        // Never wait for an image: with every image in flight (or a hidden
        // window) this frame is skipped and the uploads still run.
        let mut swapchain = std::ptr::null_mut();
        let (mut width, mut height) = (0, 0);
        if !unsafe {
            SDL_AcquireGPUSwapchainTexture(
                command,
                self.window,
                &mut swapchain,
                &mut width,
                &mut height,
            )
        } {
            unsafe { SDL_CancelGPUCommandBuffer(command) };
            bail!("SDL_AcquireGPUSwapchainTexture failed: {}", sdl_error());
        }
        if swapchain.is_null() {
            // Nothing to draw to: keep a vsync-paced loop from spinning.
            if !self.mailbox {
                std::thread::sleep(HIDDEN_PRESENT_WAIT);
            }
        } else {
            let target = SDL_GPUColorTargetInfo {
                texture: swapchain,
                clear_color: SDL_FColor {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 1.0,
                },
                load_op: SDL_GPU_LOADOP_CLEAR,
                store_op: SDL_GPU_STOREOP_STORE,
                ..Default::default()
            };
            let pass = unsafe { SDL_BeginGPURenderPass(command, &target, 1, std::ptr::null()) };
            if !pass.is_null() {
                let samplers = [self.layer_black, white, self.scene_colour].map(|texture| {
                    SDL_GPUTextureSamplerBinding {
                        texture,
                        sampler: self.linear,
                    }
                });
                let ((x, y, w, h), fit) = place(width, height);
                let viewport = SDL_GPUViewport {
                    x,
                    y,
                    w,
                    h,
                    min_depth: 0.0,
                    max_depth: 1.0,
                };
                let scissor = SDL_Rect {
                    x: fit.x as i32,
                    y: fit.y as i32,
                    w: fit.width as i32,
                    h: fit.height as i32,
                };
                unsafe {
                    SDL_BindGPUGraphicsPipeline(pass, self.pipelines.composite);
                    SDL_SetGPUViewport(pass, &viewport);
                    SDL_SetGPUScissor(pass, &scissor);
                    SDL_BindGPUFragmentSamplers(pass, 0, samplers.as_ptr(), 3);
                    SDL_DrawGPUPrimitives(pass, 3, 1, 0, 0);
                    SDL_EndGPURenderPass(pass);
                }
            }
        }
        Ok(())
    }

    /// Read the scene target back as RGBA rows (tests and tools).
    pub fn read_scene(&self) -> Result<Vec<u8>> {
        let (width, height) = self.scene_size();
        let size = width * height * 4;
        let info = SDL_GPUTransferBufferCreateInfo {
            usage: SDL_GPU_TRANSFERBUFFERUSAGE_DOWNLOAD,
            size,
            ..Default::default()
        };
        let transfer = unsafe { SDL_CreateGPUTransferBuffer(self.device, &info) };
        if transfer.is_null() {
            bail!("SDL_CreateGPUTransferBuffer failed: {}", sdl_error());
        }
        let result = (|| -> Result<Vec<u8>> {
            let command = self.command_buffer()?;
            let copy = unsafe { SDL_BeginGPUCopyPass(command) };
            if copy.is_null() {
                unsafe { SDL_CancelGPUCommandBuffer(command) };
                bail!("SDL_BeginGPUCopyPass failed: {}", sdl_error());
            }
            let source = SDL_GPUTextureRegion {
                texture: self.scene_colour,
                w: width,
                h: height,
                d: 1,
                ..Default::default()
            };
            let destination = SDL_GPUTextureTransferInfo {
                transfer_buffer: transfer,
                offset: 0,
                pixels_per_row: width,
                rows_per_layer: height,
            };
            unsafe {
                SDL_DownloadFromGPUTexture(copy, &source, &destination);
                SDL_EndGPUCopyPass(copy);
            }
            let fence = unsafe { SDL_SubmitGPUCommandBufferAndAcquireFence(command) };
            if fence.is_null() {
                bail!(
                    "SDL_SubmitGPUCommandBufferAndAcquireFence failed: {}",
                    sdl_error()
                );
            }
            let waited = unsafe { SDL_WaitForGPUFences(self.device, true, &fence, 1) };
            unsafe { SDL_ReleaseGPUFence(self.device, fence) };
            if !waited {
                bail!("SDL_WaitForGPUFences failed: {}", sdl_error());
            }
            let mapped = unsafe { SDL_MapGPUTransferBuffer(self.device, transfer, false) };
            if mapped.is_null() {
                bail!("SDL_MapGPUTransferBuffer failed: {}", sdl_error());
            }
            let mut out = vec![0u8; size as usize];
            unsafe {
                std::ptr::copy_nonoverlapping(mapped.cast::<u8>(), out.as_mut_ptr(), out.len());
                SDL_UnmapGPUTransferBuffer(self.device, transfer);
            }
            Ok(out)
        })();
        unsafe { SDL_ReleaseGPUTransferBuffer(self.device, transfer) };
        result
    }
}

/// Bind and draw one mesh range.
#[allow(clippy::too_many_arguments)]
fn draw_mesh(
    command: *mut SDL_GPUCommandBuffer,
    pass: *mut SDL_GPURenderPass,
    pipeline: *mut SDL_GPUGraphicsPipeline,
    uniforms: &DrawUniforms,
    material: [f32; 4],
    buffer: *mut SDL_GPUBuffer,
    textures: [*mut SDL_GPUTexture; 4],
    sampler: *mut SDL_GPUSampler,
    first: u32,
    count: u32,
) {
    let bytes = uniforms.as_bytes();
    let binding = SDL_GPUBufferBinding { buffer, offset: 0 };
    let samplers = textures.map(|texture| SDL_GPUTextureSamplerBinding { texture, sampler });
    unsafe {
        SDL_BindGPUGraphicsPipeline(pass, pipeline);
        SDL_PushGPUVertexUniformData(
            command,
            0,
            bytes.as_ptr().cast::<c_void>(),
            bytes.len() as u32,
        );
        SDL_PushGPUFragmentUniformData(
            command,
            0,
            material.as_ptr().cast::<c_void>(),
            std::mem::size_of_val(&material) as u32,
        );
        SDL_BindGPUVertexBuffers(pass, 0, &binding, 1);
        SDL_BindGPUFragmentSamplers(pass, 0, samplers.as_ptr(), samplers.len() as u32);
        SDL_DrawGPUPrimitives(pass, count, 1, first, 0);
    }
}

/// A slice of plain `repr(C)` vertices as bytes.
fn as_bytes<T: scene::Plain>(values: &[T]) -> &[u8] {
    // SAFETY: `Plain` types have no padding, so every byte is initialised.
    unsafe {
        std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), std::mem::size_of_val(values))
    }
}

impl Drop for GpuRenderer {
    fn drop(&mut self) {
        unsafe {
            SDL_WaitForGPUIdle(self.device);
        }
        self.release_residency();
        self.release_scene_targets();
        unsafe {
            let pipelines = [
                self.pipelines.mesh_opaque,
                self.pipelines.mesh_blend,
                self.pipelines.mesh_opaque_unculled,
                self.pipelines.mesh_blend_unculled,
                self.pipelines.sprite_write,
                self.pipelines.sprite_test,
                self.pipelines.composite,
            ];
            for pipeline in pipelines.into_iter().filter(|p| !p.is_null()) {
                SDL_ReleaseGPUGraphicsPipeline(self.device, pipeline);
            }
            let textures = [
                self.layer_black,
                self.layer_white,
                self.no_hd,
                self.no_hd_rows,
            ];
            for texture in textures.into_iter().filter(|t| !t.is_null()) {
                SDL_ReleaseGPUTexture(self.device, texture);
            }
            if !self.sprite_buffer.is_null() {
                SDL_ReleaseGPUBuffer(self.device, self.sprite_buffer);
            }
            if !self.layer_transfer.is_null() {
                SDL_ReleaseGPUTransferBuffer(self.device, self.layer_transfer);
            }
            for sampler in [self.sampler, self.linear] {
                if !sampler.is_null() {
                    SDL_ReleaseGPUSampler(self.device, sampler);
                }
            }
            if !self.window.is_null() {
                SDL_ReleaseWindowFromGPUDevice(self.device, self.window);
            }
            SDL_DestroyGPUDevice(self.device);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anim::Mat4x3;
    use crate::model::{TmdObject, TmdPrim};
    use crate::render::{Camera, EntityMesh, Lighting};

    /// A cube with a different palette colour on each face.
    fn cube() -> (Tmd, Texture8) {
        let s = 600i16;
        let vertices = vec![
            [-s, -s, -s],
            [s, -s, -s],
            [s, s, -s],
            [-s, s, -s],
            [-s, -s, s],
            [s, -s, s],
            [s, s, s],
            [-s, s, s],
        ];
        // Each face lists its corners anticlockwise seen from outside.
        let faces: [([u16; 4], [i16; 3]); 6] = [
            ([0, 3, 2, 1], [0, 0, -4096]),
            ([4, 5, 6, 7], [0, 0, 4096]),
            ([0, 4, 7, 3], [-4096, 0, 0]),
            ([1, 2, 6, 5], [4096, 0, 0]),
            ([0, 1, 5, 4], [0, -4096, 0]),
            ([3, 7, 6, 2], [0, 4096, 0]),
        ];
        let mut prims = Vec::new();
        for (face, (corners, _)) in faces.iter().enumerate() {
            // Face `face` samples the 8x8 cell at u = face * 8, index face + 1.
            let u = (face * 8 + 4) as u8;
            for triangle in [[0, 1, 2], [0, 2, 3]] {
                prims.push(TmdPrim {
                    vertices: triangle.map(|corner| corners[corner]),
                    normals: [face as u16; 3],
                    uv: [[u, 4]; 3],
                    clut: 480 << 6,
                    tsb: 0,
                    textured: true,
                    blend: false,
                    raw_y: false,
                    uv_nudge: [0; 3],
                    flat_color: None,
                    quad: None,
                });
            }
        }
        let mesh = Tmd {
            objects: vec![TmdObject {
                vertices,
                normals: faces.iter().map(|(_, normal)| *normal).collect(),
                prims,
            }],
        };
        let (width, height) = (64u32, 8u32);
        let indices = (0..width * height)
            .map(|i| ((i % width) / 8 + 1) as u8)
            .collect();
        let mut palettes = vec![[0u8; 4]; crate::model::PALETTE_ROW_LEN];
        let colours = [
            [200, 40, 40],
            [40, 200, 40],
            [40, 40, 200],
            [200, 200, 40],
            [200, 40, 200],
            [40, 200, 200],
        ];
        for (index, colour) in colours.iter().enumerate() {
            palettes[index + 1] = [colour[0], colour[1], colour[2], 255];
        }
        let texture = Texture8 {
            width,
            height,
            indices,
            palettes,
            stp: vec![false; crate::model::PALETTE_ROW_LEN],
        };
        (mesh, texture)
    }

    /// The GPU scene agrees with the software rasterizer on a lit, rotated
    /// cube: same faces culled, same palette lookups, close shading. Skips
    /// when the machine has no SDL video or no SPIR-V capable GPU (CI).
    #[test]
    fn gpu_scene_matches_the_software_rasterizer() {
        let _lock = crate::audio::test_lock::sdl();
        let Ok(_sdl) = crate::engine::SdlHandle::acquire() else {
            eprintln!("skipping: SDL video unavailable");
            return;
        };
        let mut gpu = match GpuRenderer::offscreen(1) {
            Ok(gpu) => gpu,
            Err(err) => {
                eprintln!("skipping: {err:#}");
                return;
            }
        };
        let (mesh, texture) = cube();
        let joints = [Mat4x3 {
            r: crate::anim::rotation_matrix(200, 500, 0),
            t: [100, -700, 0],
        }];
        let meshes = [EntityMesh {
            mesh: &mesh,
            texture: &texture,
            joints: &joints,
            tint: [255; 3],
            blend_weight: None,
            hidden_joints: 0,
            light_pos: None,
            per_joint_light: false,
            light_rotation: None,
            joint_colours: crate::render::NO_JOINT_COLOURS,
            complex_object: None,
            aliased_light: false,
        }];
        let camera = Camera::from_points([2600, -2400, 5200], [0, -700, 0], 600);
        let lighting = Lighting {
            ambient: [3000, 3000, 3000],
            lights: Default::default(),
        };
        let scene = GameplayScene {
            background: None,
            origin: [0, 0],
            colour: [0x80; 3],
            meshes: &meshes,
            shadows: &[],
            camera: &camera,
            lighting: &lighting,
            mask_layer: None,
            effect_layer: None,
            mirror_from: meshes.len(),
            mirror: None,
        };
        let mut software = Framebuffer::new();
        scene.draw(&mut software);
        gpu.record_scene(&scene, None).unwrap();
        let hardware = gpu.read_scene().unwrap();
        assert_eq!(hardware.len(), software.rgba.len());

        let mut covered = 0;
        let mut different = 0;
        for (sw, hw) in software.rgba.chunks(4).zip(hardware.chunks(4)) {
            let lit = |pixel: &[u8]| pixel[..3].iter().any(|&c| c != 0);
            if lit(sw) || lit(hw) {
                covered += 1;
                let distance = (0..3)
                    .map(|c| i32::from(sw[c]).abs_diff(i32::from(hw[c])))
                    .max()
                    .unwrap_or(0);
                if distance > 24 {
                    different += 1;
                }
            }
        }
        eprintln!("{different} of {covered} pixels differ");
        if let Ok(dir) = std::env::var("ARKLAY_GPU_TEST_DUMP") {
            std::fs::write(format!("{dir}/sw.rgba"), &software.rgba).unwrap();
            std::fs::write(format!("{dir}/hw.rgba"), &hardware).unwrap();
        }
        assert!(covered > 5000, "the cube covers only {covered} pixels");
        // Only the edges may disagree (different fill rules).
        assert!(
            different * 20 < covered,
            "{different} of {covered} pixels differ"
        );

        // The cube stays resident across a frame that does not draw it, and
        // leaves once a room change retires it.
        let resident = gpu.resident_counts();
        assert_eq!((resident.0, resident.1), (1, 1));
        let empty = GameplayScene {
            meshes: &[],
            mirror_from: 0,
            ..scene
        };
        gpu.record_scene(&empty, None).unwrap();
        assert_eq!(gpu.resident_counts(), resident);
        gpu.retire();
        gpu.record_scene(&empty, None).unwrap();
        assert_eq!(gpu.resident_counts(), (0, 0, 0));
    }

    /// A pack's `htexture/` replacements are drawn in place of the background
    /// and the model page whose pixels hash to their names, at their own
    /// resolution.
    #[test]
    fn gpu_scene_draws_hd_replacements() {
        let _lock = crate::audio::test_lock::sdl();
        let Ok(_sdl) = crate::engine::SdlHandle::acquire() else {
            eprintln!("skipping: SDL video unavailable");
            return;
        };
        let mut gpu = match GpuRenderer::offscreen(1) {
            Ok(gpu) => gpu,
            Err(err) => {
                eprintln!("skipping: {err:#}");
                return;
            }
        };
        let solid = |width: u32, height: u32, colour: fn(u32) -> [u8; 3]| Image {
            width,
            height,
            rgba: (0..width * height)
                .flat_map(|i| {
                    let [r, g, b] = colour(i % width);
                    [r, g, b, 255]
                })
                .collect(),
        };
        let background = solid(320, 240, |x| [(x / 2) as u8, 64, 64]);
        // Twice the background's resolution: blue in the left half of the
        // original's texels, green in the right.
        let hd_background = solid(
            640,
            480,
            |x| if x < 320 { [0, 0, 240] } else { [0, 240, 0] },
        );
        let (mesh, texture) = cube();
        // The cube's 64-wide page covers 64 x 256 texels of its replacement.
        let hd_page = solid(128, 512, |_| [240, 240, 240]);
        let mut writer = crate::pack::PackWriter::new();
        let background_hash = htexture::image_candidates(&background)[0].hash;
        let page_hash = htexture::page_candidates(&texture, 0)[0].hash;
        for (path, image) in [
            (
                format!("htexture/bgd/{background_hash:08X}.bmp"),
                &hd_background,
            ),
            (format!("htexture/skin/{page_hash:08X}.bmp"), &hd_page),
        ] {
            writer
                .add(&path, crate::bmp::encode_to_vec(image).unwrap())
                .unwrap();
        }
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();

        let joints = [Mat4x3 {
            r: crate::anim::rotation_matrix(200, 500, 0),
            t: [100, -700, 0],
        }];
        let meshes = [EntityMesh {
            mesh: &mesh,
            texture: &texture,
            joints: &joints,
            tint: [255; 3],
            blend_weight: None,
            hidden_joints: 0,
            light_pos: None,
            per_joint_light: false,
            light_rotation: None,
            joint_colours: crate::render::NO_JOINT_COLOURS,
            complex_object: None,
            aliased_light: false,
        }];
        let camera = Camera::from_points([2600, -2400, 5200], [0, -700, 0], 600);
        let lighting = Lighting {
            ambient: [4096, 4096, 4096],
            lights: Default::default(),
        };
        let scene = GameplayScene {
            background: Some(&background),
            origin: [0, 0],
            colour: [255; 3],
            meshes: &meshes,
            shadows: &[],
            camera: &camera,
            lighting: &lighting,
            mask_layer: None,
            effect_layer: None,
            mirror_from: meshes.len(),
            mirror: None,
        };
        // Where the cube is: what its original draw changes.
        let mut draw = |scene: &GameplayScene<'_>, pack: Option<&Pack>| {
            gpu.release_residency();
            gpu.record_scene(scene, pack).unwrap();
            gpu.read_scene().unwrap()
        };
        let original = draw(&scene, None);
        let bare = draw(
            &GameplayScene {
                meshes: &[],
                mirror_from: 0,
                ..scene
            },
            None,
        );
        let hardware = draw(&scene, Some(&pack));
        let (mut cube_pixels, mut grey) = (0, 0);
        for (index, ((hd, original), bare)) in hardware
            .chunks(4)
            .zip(original.chunks(4))
            .zip(bare.chunks(4))
            .enumerate()
        {
            let x = index % LAYER_WIDTH as usize;
            let [r, g, b] = [hd[0], hd[1], hd[2]];
            if original != bare {
                // The cube: its faces' colours are replaced by grey.
                cube_pixels += 1;
                grey += usize::from(r.abs_diff(g) < 16 && g.abs_diff(b) < 16);
            } else if x < 150 {
                assert!(b > 200 && g < 40, "{index} is {:?}", [r, g, b]);
            } else if x > 170 {
                assert!(g > 200 && b < 40, "{index} is {:?}", [r, g, b]);
            }
        }
        assert!(
            cube_pixels > 5000,
            "the cube covers only {cube_pixels} pixels"
        );
        assert!(
            grey * 10 > cube_pixels * 9,
            "{grey} of {cube_pixels} cube pixels are grey"
        );
    }
}
