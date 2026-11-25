//! WebGL2 renderer.
//!
//! Every Guacamole layer and buffer is an RGBA8 texture (premultiplied alpha)
//! attached to its own framebuffer. Drawing instructions become GPU draws into
//! those framebuffers, and the visible layer tree is composited onto the
//! canvas once per animation frame. Nothing is ever read back to the CPU
//! except the (small) cursor image.
//!
//! Coordinate convention: layer textures are stored top row first, i.e. layer
//! pixel `(x, y)` is texel `(x, y)` and `gl_FragCoord.xy` inside a layer
//! framebuffer equals layer pixel coordinates. Only the final composite onto
//! the canvas flips the Y axis.

mod shaders;

use molcajete_protocol::{CompositeMode, TransferFunction};
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{
    HtmlCanvasElement, WebGl2RenderingContext as GL, WebGlBuffer, WebGlFramebuffer, WebGlProgram,
    WebGlSampler, WebGlSync, WebGlTexture, WebGlUniformLocation, WebGlVertexArrayObject,
};

use crate::display::path::{Mesh, Shape};
use crate::image::DecodedImage;

#[wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    /// The same `WebGL2RenderingContext`, with a `texSubImage2D` overload
    /// taking any `TexImageSource` so that both `ImageBitmap` and WebCodecs
    /// `VideoFrame` can be uploaded.
    #[wasm_bindgen(js_name = WebGL2RenderingContext)]
    type GlUpload;

    #[wasm_bindgen(method, catch, js_name = texSubImage2D)]
    fn tex_sub_image_2d_source(
        this: &GlUpload,
        target: u32,
        level: i32,
        x: i32,
        y: i32,
        format: u32,
        ty: u32,
        source: &JsValue,
    ) -> Result<(), JsValue>;

    /// WebGL2 overload with an explicit size: uploads the top-left
    /// `width`×`height` pixels of `source`.
    #[wasm_bindgen(method, catch, js_name = texSubImage2D)]
    fn tex_sub_image_2d_sized_source(
        this: &GlUpload,
        target: u32,
        level: i32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        format: u32,
        ty: u32,
        source: &JsValue,
    ) -> Result<(), JsValue>;

    /// `CanvasRenderingContext2D.drawImage` accepting any image source
    /// (including WebCodecs `VideoFrame`).
    #[wasm_bindgen(js_name = CanvasRenderingContext2D)]
    type Context2d;

    #[wasm_bindgen(method, catch, js_name = drawImage)]
    fn draw_image_source(
        this: &Context2d,
        source: &JsValue,
        dx: f64,
        dy: f64,
    ) -> Result<(), JsValue>;
}
use crate::geom::{Affine, Rect};

/// Layer textures grow in steps of this many pixels so that buffers which
/// auto-size while being drawn do not reallocate on every operation.
const ALLOC_GRANULARITY: u32 = 64;
const FLOATS_PER_VERTEX: usize = 8;

fn round_up(v: u32, step: u32) -> u32 {
    v.div_ceil(step).max(1) * step
}

/// GPU storage of one layer or buffer.
pub struct Surface {
    gl: GL,
    tex: WebGlTexture,
    fbo: WebGlFramebuffer,
    pub alloc_w: u32,
    pub alloc_h: u32,
    /// Coverage mask of the active clip region (allocated on demand).
    clip_mask: Option<MaskTarget>,
}

impl Drop for Surface {
    fn drop(&mut self) {
        self.gl.delete_framebuffer(Some(&self.fbo));
        self.gl.delete_texture(Some(&self.tex));
    }
}

impl Surface {
    fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.alloc_w as i32, self.alloc_h as i32)
    }

    pub fn has_clip_mask(&self) -> bool {
        self.clip_mask.is_some()
    }

    pub fn drop_clip_mask(&mut self) {
        self.clip_mask = None;
    }
}

/// Single-channel render target used for coverage masks.
struct MaskTarget {
    gl: GL,
    tex: WebGlTexture,
    fbo: WebGlFramebuffer,
    w: u32,
    h: u32,
}

impl Drop for MaskTarget {
    fn drop(&mut self) {
        self.gl.delete_framebuffer(Some(&self.fbo));
        self.gl.delete_texture(Some(&self.tex));
    }
}

/// RGBA render target used as scratch space for self-overlapping copies.
struct ColorTarget {
    gl: GL,
    tex: WebGlTexture,
    fbo: WebGlFramebuffer,
    w: u32,
    h: u32,
}

impl Drop for ColorTarget {
    fn drop(&mut self) {
        self.gl.delete_framebuffer(Some(&self.fbo));
        self.gl.delete_texture(Some(&self.tex));
    }
}

/// The canvas the renderer presents to: a DOM canvas on the main thread or
/// an `OffscreenCanvas` (transferred from one) inside a worker.
#[derive(Clone)]
pub enum CanvasTarget {
    Element(HtmlCanvasElement),
    Offscreen(web_sys::OffscreenCanvas),
}

impl CanvasTarget {
    pub fn width(&self) -> u32 {
        match self {
            Self::Element(c) => c.width(),
            Self::Offscreen(c) => c.width(),
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            Self::Element(c) => c.height(),
            Self::Offscreen(c) => c.height(),
        }
    }

    pub fn set_width(&self, w: u32) {
        match self {
            Self::Element(c) => c.set_width(w),
            Self::Offscreen(c) => c.set_width(w),
        }
    }

    pub fn set_height(&self, h: u32) {
        match self {
            Self::Element(c) => c.set_height(h),
            Self::Offscreen(c) => c.set_height(h),
        }
    }

    fn context(&self, kind: &str, options: &JsValue) -> Result<Option<js_sys::Object>, JsValue> {
        match self {
            Self::Element(c) => c.get_context_with_context_options(kind, options),
            Self::Offscreen(c) => c.get_context_with_context_options(kind, options),
        }
    }
}

/// An asynchronous pixel read-back (PBO + fence): the copy is queued on the
/// GPU and collected later, so the pipeline never stalls.
pub struct Readback {
    gl: GL,
    buffer: WebGlBuffer,
    sync: WebGlSync,
    len: usize,
}

impl Readback {
    /// Returns the pixels once the GPU has finished the copy.
    pub fn poll(&self) -> Option<Vec<u8>> {
        let status = self.gl.client_wait_sync_with_u32(&self.sync, 0, 0);
        if status != GL::ALREADY_SIGNALED && status != GL::CONDITION_SATISFIED {
            return None;
        }
        let mut data = vec![0u8; self.len];
        self.gl
            .bind_buffer(GL::PIXEL_PACK_BUFFER, Some(&self.buffer));
        self.gl
            .get_buffer_sub_data_with_i32_and_u8_array(GL::PIXEL_PACK_BUFFER, 0, &mut data);
        self.gl.bind_buffer(GL::PIXEL_PACK_BUFFER, None);
        Some(data)
    }
}

impl Drop for Readback {
    fn drop(&mut self) {
        self.gl.delete_sync(Some(&self.sync));
        self.gl.delete_buffer(Some(&self.buffer));
    }
}

/// How the destination region is restricted.
#[derive(Clone, Copy, Debug, Default)]
pub struct Clip {
    /// Axis-aligned clip rectangle (scissor).
    pub rect: Option<Rect>,
    /// Use the target surface's clip mask.
    pub use_mask: bool,
}

/// What fills the shape.
pub enum Paint<'a> {
    /// Premultiplied RGBA color.
    Solid([f32; 4]),
    /// A region of another surface (or uploaded image).
    Texture(TextureSource<'a>),
    /// A whole surface repeated infinitely (`lfill` / `lstroke`).
    Pattern {
        source: &'a Surface,
        width: u32,
        height: u32,
        /// Target pixel → pattern pixel.
        map: Affine,
    },
}

pub struct TextureSource<'a> {
    tex: &'a WebGlTexture,
    tex_w: u32,
    tex_h: u32,
    /// Valid source region in source pixels.
    region: Rect,
    /// Target pixel → source pixel.
    map: Affine,
}

/// One visible layer in the final composite.
pub struct CompositeItem<'a> {
    pub surface: &'a Surface,
    pub width: u32,
    pub height: u32,
    /// Layer pixel → canvas pixel.
    pub transform: Affine,
    pub opacity: f32,
    /// Scissor in canvas pixels (top-left origin), if the item is clipped.
    pub scissor: Option<Rect>,
}

/// Node of the composite tree. A translucent layer with children is a
/// group: its subtree is flattened off-screen first and then blended with
/// the group opacity, which is how CSS `opacity` behaves.
pub enum CompositeNode<'a> {
    Layer(CompositeItem<'a>),
    Group {
        opacity: f32,
        /// Canvas pixels (top-left origin) covered by the group.
        bounds: Rect,
        children: Vec<CompositeNode<'a>>,
    },
}

struct Programs {
    basic: WebGlProgram,
    basic_proj: Option<WebGlUniformLocation>,
    basic_tex: Option<WebGlUniformLocation>,

    masked: WebGlProgram,
    m_proj: Option<WebGlUniformLocation>,
    m_pass: Option<WebGlUniformLocation>,
    m_paint: Option<WebGlUniformLocation>,
    m_color: Option<WebGlUniformLocation>,
    m_tex: Option<WebGlUniformLocation>,
    m_map: Option<WebGlUniformLocation>,
    m_tex_size: Option<WebGlUniformLocation>,
    m_src_rect: Option<WebGlUniformLocation>,
    m_use_mask: Option<WebGlUniformLocation>,
    m_mask: Option<WebGlUniformLocation>,
    m_use_clip: Option<WebGlUniformLocation>,
    m_clip: Option<WebGlUniformLocation>,

    transfer: WebGlProgram,
    t_proj: Option<WebGlUniformLocation>,
    t_src: Option<WebGlUniformLocation>,
    t_dst: Option<WebGlUniformLocation>,
    t_src_origin: Option<WebGlUniformLocation>,
    t_dst_origin: Option<WebGlUniformLocation>,
    t_target_origin: Option<WebGlUniformLocation>,
    t_function: Option<WebGlUniformLocation>,
}

/// What an image is uploaded from.
#[derive(Clone, Copy)]
pub enum ImageSource<'a> {
    /// An image decoded by the browser (`VideoFrame` or `ImageBitmap`).
    Decoded(&'a DecodedImage),
    /// Opaque RGBA pixels in wasm memory, top row first (a zstd image).
    Pixels {
        rgba: &'a [u8],
        width: u32,
        height: u32,
    },
}

impl ImageSource<'_> {
    pub fn size(&self) -> (u32, u32) {
        match self {
            Self::Decoded(image) => (image.width, image.height),
            Self::Pixels { width, height, .. } => (*width, *height),
        }
    }

    fn opaque(&self) -> bool {
        match self {
            Self::Decoded(image) => image.opaque,
            Self::Pixels { .. } => true,
        }
    }
}

pub struct Renderer {
    gl: GL,
    canvas: CanvasTarget,
    pub max_texture_size: u32,
    programs: Programs,
    _vao: WebGlVertexArrayObject,
    vbo: WebGlBuffer,
    ibo: WebGlBuffer,
    white: WebGlTexture,
    nearest: WebGlSampler,
    linear: WebGlSampler,
    scratch: [Option<ColorTarget>; 2],
    /// Off-screen targets for opacity groups, one per nesting depth.
    group_targets: Vec<ColorTarget>,
    mask: Option<MaskTarget>,
    upload: Option<(WebGlTexture, u32, u32)>,
    /// Accelerated 2D canvas used to stage decoded images; see `upload_to`.
    staging: Option<(web_sys::OffscreenCanvas, js_sys::Object, u32, u32)>,
    /// Whether images go through the staging canvas (falls back on error).
    staged_uploads: bool,
    verts: Vec<f32>,
    /// Number of draw calls issued since the last call to `take_draw_calls`.
    draw_calls: u32,
}

#[derive(Debug)]
pub struct RendererError(pub String);

impl From<JsValue> for RendererError {
    fn from(v: JsValue) -> Self {
        RendererError(format!("{v:?}"))
    }
}

fn compile(gl: &GL, kind: u32, src: &str) -> Result<web_sys::WebGlShader, RendererError> {
    let shader = gl
        .create_shader(kind)
        .ok_or_else(|| RendererError("createShader failed".into()))?;
    gl.shader_source(&shader, src);
    gl.compile_shader(&shader);
    if gl
        .get_shader_parameter(&shader, GL::COMPILE_STATUS)
        .as_bool()
        .unwrap_or(false)
    {
        Ok(shader)
    } else {
        Err(RendererError(
            gl.get_shader_info_log(&shader).unwrap_or_default(),
        ))
    }
}

fn link(gl: &GL, fragment: &str) -> Result<WebGlProgram, RendererError> {
    let vs = compile(gl, GL::VERTEX_SHADER, shaders::VERTEX)?;
    let fs = compile(gl, GL::FRAGMENT_SHADER, fragment)?;
    let program = gl
        .create_program()
        .ok_or_else(|| RendererError("createProgram failed".into()))?;
    gl.attach_shader(&program, &vs);
    gl.attach_shader(&program, &fs);
    gl.link_program(&program);
    gl.delete_shader(Some(&vs));
    gl.delete_shader(Some(&fs));
    if gl
        .get_program_parameter(&program, GL::LINK_STATUS)
        .as_bool()
        .unwrap_or(false)
    {
        Ok(program)
    } else {
        Err(RendererError(
            gl.get_program_info_log(&program).unwrap_or_default(),
        ))
    }
}

/// Projection from pixel coordinates of a `w`×`h` target to clip space.
fn projection(w: u32, h: u32, flip_y: bool) -> [f32; 9] {
    let (sy, ty) = if flip_y {
        (-2.0 / h as f32, 1.0)
    } else {
        (2.0 / h as f32, -1.0)
    };
    [2.0 / w as f32, 0.0, 0.0, 0.0, sy, 0.0, -1.0, ty, 1.0]
}

fn blend_factors(mode: CompositeMode) -> (u32, u32) {
    match mode.0 {
        0x1 => (GL::ZERO, GL::SRC_ALPHA),                // destination-in
        0x2 => (GL::ZERO, GL::ONE_MINUS_SRC_ALPHA),      // destination-out
        0x4 => (GL::DST_ALPHA, GL::ZERO),                // source-in
        0x6 => (GL::DST_ALPHA, GL::ONE_MINUS_SRC_ALPHA), // source-atop
        0x8 => (GL::ONE_MINUS_DST_ALPHA, GL::ZERO),      // source-out
        0x9 => (GL::ONE_MINUS_DST_ALPHA, GL::SRC_ALPHA), // destination-atop
        0xA => (GL::ONE_MINUS_DST_ALPHA, GL::ONE_MINUS_SRC_ALPHA), // xor
        0xB => (GL::ONE_MINUS_DST_ALPHA, GL::ONE),       // destination-over
        0xC => (GL::ONE, GL::ZERO),                      // copy
        0xF => (GL::ONE, GL::ONE),                       // lighter
        _ => (GL::ONE, GL::ONE_MINUS_SRC_ALPHA),         // source-over
    }
}

/// Composite modes for which the canvas clears everything outside the drawn
/// shape (within the clip region).
pub fn is_unbounded(mode: CompositeMode) -> bool {
    matches!(mode.0, 0x1 | 0x4 | 0x8 | 0x9 | 0xC)
}

fn pairwise_disjoint(rects: &[Rect]) -> bool {
    if rects.len() > 32 {
        return false;
    }
    rects
        .iter()
        .enumerate()
        .all(|(i, a)| rects[i + 1..].iter().all(|b| !a.overlaps(b)))
}

impl Renderer {
    pub fn new(canvas: CanvasTarget) -> Result<Self, RendererError> {
        let options = js_sys::Object::new();
        for (key, value) in [
            ("alpha", JsValue::FALSE),
            ("antialias", JsValue::FALSE),
            ("depth", JsValue::FALSE),
            ("stencil", JsValue::FALSE),
            ("premultipliedAlpha", JsValue::TRUE),
            ("preserveDrawingBuffer", JsValue::FALSE),
            // Low-latency canvas: lets Chrome bypass the compositor queue.
            ("desynchronized", JsValue::TRUE),
            ("powerPreference", JsValue::from_str("high-performance")),
        ] {
            js_sys::Reflect::set(&options, &JsValue::from_str(key), &value)?;
        }
        let gl: GL = canvas
            .context("webgl2", &options)?
            .ok_or_else(|| RendererError("WebGL2 is not available".into()))?
            .dyn_into()
            .map_err(|_| RendererError("unexpected context type".into()))?;

        let max_texture_size = gl
            .get_parameter(GL::MAX_TEXTURE_SIZE)?
            .as_f64()
            .unwrap_or(4096.0) as u32;

        let basic = link(&gl, shaders::BASIC_FRAGMENT)?;
        let masked = link(&gl, shaders::MASKED_FRAGMENT)?;
        let transfer = link(&gl, shaders::TRANSFER_FRAGMENT)?;
        let loc = |p: &WebGlProgram, n: &str| gl.get_uniform_location(p, n);
        let programs = Programs {
            basic_proj: loc(&basic, "u_proj"),
            basic_tex: loc(&basic, "u_tex"),
            m_proj: loc(&masked, "u_proj"),
            m_pass: loc(&masked, "u_pass"),
            m_paint: loc(&masked, "u_paint"),
            m_color: loc(&masked, "u_color"),
            m_tex: loc(&masked, "u_tex"),
            m_map: loc(&masked, "u_map"),
            m_tex_size: loc(&masked, "u_tex_size"),
            m_src_rect: loc(&masked, "u_src_rect"),
            m_use_mask: loc(&masked, "u_use_mask"),
            m_mask: loc(&masked, "u_mask"),
            m_use_clip: loc(&masked, "u_use_clip"),
            m_clip: loc(&masked, "u_clip"),
            t_proj: loc(&transfer, "u_proj"),
            t_src: loc(&transfer, "u_src"),
            t_dst: loc(&transfer, "u_dst"),
            t_src_origin: loc(&transfer, "u_src_origin"),
            t_dst_origin: loc(&transfer, "u_dst_origin"),
            t_target_origin: loc(&transfer, "u_target_origin"),
            t_function: loc(&transfer, "u_function"),
            basic,
            masked,
            transfer,
        };

        let vao = gl
            .create_vertex_array()
            .ok_or_else(|| RendererError("createVertexArray failed".into()))?;
        gl.bind_vertex_array(Some(&vao));
        let vbo = gl
            .create_buffer()
            .ok_or_else(|| RendererError("createBuffer failed".into()))?;
        let ibo = gl
            .create_buffer()
            .ok_or_else(|| RendererError("createBuffer failed".into()))?;
        gl.bind_buffer(GL::ARRAY_BUFFER, Some(&vbo));
        gl.bind_buffer(GL::ELEMENT_ARRAY_BUFFER, Some(&ibo));
        let stride = (FLOATS_PER_VERTEX * 4) as i32;
        gl.enable_vertex_attrib_array(0);
        gl.vertex_attrib_pointer_with_i32(0, 2, GL::FLOAT, false, stride, 0);
        gl.enable_vertex_attrib_array(1);
        gl.vertex_attrib_pointer_with_i32(1, 2, GL::FLOAT, false, stride, 8);
        gl.enable_vertex_attrib_array(2);
        gl.vertex_attrib_pointer_with_i32(2, 4, GL::FLOAT, false, stride, 16);

        let white = gl
            .create_texture()
            .ok_or_else(|| RendererError("createTexture failed".into()))?;
        gl.bind_texture(GL::TEXTURE_2D, Some(&white));
        gl.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_u8_array(
            GL::TEXTURE_2D,
            0,
            GL::RGBA8 as i32,
            1,
            1,
            0,
            GL::RGBA,
            GL::UNSIGNED_BYTE,
            Some(&[255, 255, 255, 255]),
        )?;

        let make_sampler = |filter: u32| -> Result<WebGlSampler, RendererError> {
            let s = gl
                .create_sampler()
                .ok_or_else(|| RendererError("createSampler failed".into()))?;
            gl.sampler_parameteri(&s, GL::TEXTURE_MIN_FILTER, filter as i32);
            gl.sampler_parameteri(&s, GL::TEXTURE_MAG_FILTER, filter as i32);
            gl.sampler_parameteri(&s, GL::TEXTURE_WRAP_S, GL::CLAMP_TO_EDGE as i32);
            gl.sampler_parameteri(&s, GL::TEXTURE_WRAP_T, GL::CLAMP_TO_EDGE as i32);
            Ok(s)
        };
        let nearest = make_sampler(GL::NEAREST)?;
        let linear = make_sampler(GL::LINEAR)?;
        for unit in 0..3 {
            gl.bind_sampler(unit, Some(&nearest));
        }

        gl.pixel_storei(GL::UNPACK_PREMULTIPLY_ALPHA_WEBGL, 1);
        gl.pixel_storei(GL::UNPACK_COLORSPACE_CONVERSION_WEBGL, GL::NONE as i32);
        gl.pixel_storei(GL::UNPACK_FLIP_Y_WEBGL, 0);
        gl.pixel_storei(GL::PACK_ALIGNMENT, 1);
        gl.pixel_storei(GL::UNPACK_ALIGNMENT, 1);

        Ok(Self {
            gl,
            canvas,
            max_texture_size,
            programs,
            _vao: vao,
            vbo,
            ibo,
            white,
            nearest,
            linear,
            scratch: [None, None],
            group_targets: Vec::new(),
            mask: None,
            upload: None,
            staging: None,
            staged_uploads: true,
            verts: Vec::with_capacity(1024),
            draw_calls: 0,
        })
    }

    pub fn canvas(&self) -> &CanvasTarget {
        &self.canvas
    }

    pub fn is_context_lost(&self) -> bool {
        self.gl.is_context_lost()
    }

    /// Chooses between staged (2D canvas) and direct image uploads.
    pub fn set_staged_uploads(&mut self, enabled: bool) {
        self.staged_uploads = enabled;
    }

    pub fn take_draw_calls(&mut self) -> u32 {
        std::mem::take(&mut self.draw_calls)
    }

    // ------------------------------------------------------------------
    // Resource management
    // ------------------------------------------------------------------

    fn alloc_size(&self, w: u32, h: u32) -> (u32, u32) {
        let max = self.max_texture_size;
        (
            round_up(w, ALLOC_GRANULARITY).min(max),
            round_up(h, ALLOC_GRANULARITY).min(max),
        )
    }

    fn new_color_texture(&self, w: u32, h: u32, format: u32) -> (WebGlTexture, WebGlFramebuffer) {
        let gl = &self.gl;
        let tex = gl.create_texture().expect("createTexture");
        gl.bind_texture(GL::TEXTURE_2D, Some(&tex));
        gl.tex_storage_2d(GL::TEXTURE_2D, 1, format, w as i32, h as i32);
        gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_MIN_FILTER, GL::NEAREST as i32);
        gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_MAG_FILTER, GL::NEAREST as i32);
        gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_WRAP_S, GL::CLAMP_TO_EDGE as i32);
        gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_WRAP_T, GL::CLAMP_TO_EDGE as i32);
        let fbo = gl.create_framebuffer().expect("createFramebuffer");
        gl.bind_framebuffer(GL::FRAMEBUFFER, Some(&fbo));
        gl.framebuffer_texture_2d(
            GL::FRAMEBUFFER,
            GL::COLOR_ATTACHMENT0,
            GL::TEXTURE_2D,
            Some(&tex),
            0,
        );
        gl.disable(GL::SCISSOR_TEST);
        gl.clear_color(0.0, 0.0, 0.0, 0.0);
        gl.clear(GL::COLOR_BUFFER_BIT);
        (tex, fbo)
    }

    /// Creates (or re-creates) the storage of a layer so it can hold
    /// `w`×`h` pixels. The top-left `keep_w`×`keep_h` pixels of the old
    /// surface are preserved. Returns `None` for an empty layer.
    pub fn resize_surface(
        &mut self,
        old: Option<Surface>,
        w: u32,
        h: u32,
        keep_w: u32,
        keep_h: u32,
    ) -> Option<Surface> {
        if w == 0 || h == 0 {
            return None;
        }
        let (aw, ah) = self.alloc_size(w, h);
        if old
            .as_ref()
            .is_some_and(|o| o.alloc_w == aw && o.alloc_h == ah)
        {
            return old;
        }
        let (tex, fbo) = self.new_color_texture(aw, ah, GL::RGBA8);
        let surface = Surface {
            gl: self.gl.clone(),
            tex,
            fbo,
            alloc_w: aw,
            alloc_h: ah,
            clip_mask: None,
        };
        if let Some(old) = old {
            let cw = keep_w.min(old.alloc_w).min(aw) as i32;
            let ch = keep_h.min(old.alloc_h).min(ah) as i32;
            if cw > 0 && ch > 0 {
                let gl = &self.gl;
                gl.disable(GL::SCISSOR_TEST);
                gl.bind_framebuffer(GL::READ_FRAMEBUFFER, Some(&old.fbo));
                gl.bind_framebuffer(GL::DRAW_FRAMEBUFFER, Some(&surface.fbo));
                gl.blit_framebuffer(
                    0,
                    0,
                    cw,
                    ch,
                    0,
                    0,
                    cw,
                    ch,
                    GL::COLOR_BUFFER_BIT,
                    GL::NEAREST,
                );
                gl.bind_framebuffer(GL::FRAMEBUFFER, None);
            }
        }
        Some(surface)
    }

    fn ensure_mask(&mut self, w: u32, h: u32) {
        let fits = self.mask.as_ref().is_some_and(|m| m.w >= w && m.h >= h);
        if !fits {
            let (cw, ch) = self.mask.as_ref().map(|m| (m.w, m.h)).unwrap_or((0, 0));
            let (mw, mh) = self.alloc_size(w.max(cw), h.max(ch));
            let (tex, fbo) = self.new_color_texture(mw, mh, GL::R8);
            self.mask = Some(MaskTarget {
                gl: self.gl.clone(),
                tex,
                fbo,
                w: mw,
                h: mh,
            });
        }
    }

    fn ensure_scratch(&mut self, index: usize, w: u32, h: u32) {
        let fits = self.scratch[index]
            .as_ref()
            .is_some_and(|s| s.w >= w && s.h >= h);
        if !fits {
            let (cw, ch) = self.scratch[index]
                .as_ref()
                .map(|s| (s.w, s.h))
                .unwrap_or((0, 0));
            let sw = round_up(w.max(cw), 256).min(self.max_texture_size);
            let sh = round_up(h.max(ch), 256).min(self.max_texture_size);
            let (tex, fbo) = self.new_color_texture(sw, sh, GL::RGBA8);
            self.scratch[index] = Some(ColorTarget {
                gl: self.gl.clone(),
                tex,
                fbo,
                w: sw,
                h: sh,
            });
        }
    }

    /// Copies `region` of `src` into scratch target `index` at (0, 0).
    fn copy_to_scratch(&mut self, index: usize, src_fbo: &WebGlFramebuffer, region: Rect) {
        self.ensure_scratch(index, region.w as u32, region.h as u32);
        let scratch = self.scratch[index].as_ref().expect("scratch allocated");
        let gl = &self.gl;
        gl.disable(GL::SCISSOR_TEST);
        gl.bind_framebuffer(GL::READ_FRAMEBUFFER, Some(src_fbo));
        gl.bind_texture(GL::TEXTURE_2D, Some(&scratch.tex));
        gl.copy_tex_sub_image_2d(
            GL::TEXTURE_2D,
            0,
            0,
            0,
            region.x,
            region.y,
            region.w,
            region.h,
        );
        gl.bind_framebuffer(GL::READ_FRAMEBUFFER, None);
    }

    /// Copies an image into `tex` at (x, y).
    ///
    /// Pixels in wasm memory are uploaded as they are, through a view of
    /// that memory.
    ///
    /// WebGL uploads of CPU-backed sources (`VideoFrame`s from
    /// `ImageDecoder`, most `ImageBitmap`s) are copied synchronously on the
    /// main thread. Drawing the image into an accelerated 2D canvas instead is
    /// deferred to the GPU process, and uploading from that canvas becomes a
    /// GPU-to-GPU copy, so the image is staged there first.
    fn upload_to(
        &mut self,
        tex: &WebGlTexture,
        x: i32,
        y: i32,
        image: ImageSource,
    ) -> Result<(), RendererError> {
        let image = match image {
            ImageSource::Decoded(image) => image,
            ImageSource::Pixels {
                rgba,
                width,
                height,
            } => {
                let gl = &self.gl;
                gl.bind_texture(GL::TEXTURE_2D, Some(tex));
                // The pixels are opaque, so they are premultiplied already;
                // asking for it would only make the browser convert them.
                gl.pixel_storei(GL::UNPACK_PREMULTIPLY_ALPHA_WEBGL, 0);
                let result = gl
                    .tex_sub_image_2d_with_i32_and_i32_and_u32_and_type_and_opt_u8_array(
                        GL::TEXTURE_2D,
                        0,
                        x,
                        y,
                        width as i32,
                        height as i32,
                        GL::RGBA,
                        GL::UNSIGNED_BYTE,
                        Some(rgba),
                    );
                gl.pixel_storei(GL::UNPACK_PREMULTIPLY_ALPHA_WEBGL, 1);
                return Ok(result?);
            }
        };
        let (w, h) = (image.width as i32, image.height as i32);
        if self.staged_uploads {
            match self.stage(image) {
                Ok(canvas) => {
                    self.gl.bind_texture(GL::TEXTURE_2D, Some(tex));
                    return Ok(self
                        .gl
                        .unchecked_ref::<GlUpload>()
                        .tex_sub_image_2d_sized_source(
                            GL::TEXTURE_2D,
                            0,
                            x,
                            y,
                            w,
                            h,
                            GL::RGBA,
                            GL::UNSIGNED_BYTE,
                            &canvas,
                        )?);
                }
                Err(err) => {
                    web_sys::console::warn_2(&"staged upload unavailable:".into(), &err);
                    self.staged_uploads = false;
                }
            }
        }
        self.gl.bind_texture(GL::TEXTURE_2D, Some(tex));
        self.gl
            .unchecked_ref::<GlUpload>()
            .tex_sub_image_2d_source(
                GL::TEXTURE_2D,
                0,
                x,
                y,
                GL::RGBA,
                GL::UNSIGNED_BYTE,
                &image.source,
            )?;
        Ok(())
    }

    /// Draws `image` at the origin of the staging canvas.
    fn stage(&mut self, image: &DecodedImage) -> Result<JsValue, JsValue> {
        let (w, h) = (image.width, image.height);
        let fits = self
            .staging
            .as_ref()
            .is_some_and(|(_, _, sw, sh)| *sw >= w && *sh >= h);
        if !fits {
            let (cw, ch) = self
                .staging
                .as_ref()
                .map(|(_, _, sw, sh)| (*sw, *sh))
                .unwrap_or((0, 0));
            // At least 256x256 so that Chromium accelerates the canvas.
            let sw = round_up(w.max(cw).max(256), 256).min(self.max_texture_size);
            let sh = round_up(h.max(ch).max(256), 256).min(self.max_texture_size);
            // An OffscreenCanvas works on the main thread and in workers.
            let canvas = web_sys::OffscreenCanvas::new(sw, sh)?;
            let options = js_sys::Object::new();
            js_sys::Reflect::set(&options, &"willReadFrequently".into(), &JsValue::FALSE)?;
            let ctx = canvas
                .get_context_with_context_options("2d", &options)?
                .ok_or("no 2d context")?;
            // `copy` replaces the previous image entirely.
            js_sys::Reflect::set(&ctx, &"globalCompositeOperation".into(), &"copy".into())?;
            self.staging = Some((canvas, ctx, sw, sh));
        }
        let (canvas, ctx, _, _) = self.staging.as_ref().expect("staging canvas");
        ctx.unchecked_ref::<Context2d>()
            .draw_image_source(&image.source, 0.0, 0.0)?;
        Ok(canvas.clone().into())
    }

    /// Uploads an image into the shared upload texture, which is only
    /// reallocated when it is too small.
    fn upload_image(&mut self, image: ImageSource) -> Result<(), RendererError> {
        let (w, h) = image.size();
        let fits = self
            .upload
            .as_ref()
            .is_some_and(|(_, tw, th)| *tw >= w && *th >= h);
        if !fits {
            let (cw, ch) = self
                .upload
                .as_ref()
                .map(|(_, tw, th)| (*tw, *th))
                .unwrap_or((0, 0));
            let (aw, ah) = self.alloc_size(w.max(cw), h.max(ch));
            let gl = &self.gl;
            let tex = gl.create_texture().expect("createTexture");
            gl.bind_texture(GL::TEXTURE_2D, Some(&tex));
            gl.tex_storage_2d(GL::TEXTURE_2D, 1, GL::RGBA8, aw as i32, ah as i32);
            gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_MIN_FILTER, GL::NEAREST as i32);
            gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_MAG_FILTER, GL::NEAREST as i32);
            gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_WRAP_S, GL::CLAMP_TO_EDGE as i32);
            gl.tex_parameteri(GL::TEXTURE_2D, GL::TEXTURE_WRAP_T, GL::CLAMP_TO_EDGE as i32);
            if let Some((old, _, _)) = self.upload.take() {
                gl.delete_texture(Some(&old));
            }
            self.upload = Some((tex, aw, ah));
        }
        let tex = self.upload.as_ref().expect("upload texture").0.clone();
        self.upload_to(&tex, 0, 0, image)
    }

    // ------------------------------------------------------------------
    // Low-level drawing helpers
    // ------------------------------------------------------------------

    fn bind_target(&self, fbo: Option<&WebGlFramebuffer>, w: u32, h: u32) {
        self.gl.bind_framebuffer(GL::FRAMEBUFFER, fbo);
        self.gl.viewport(0, 0, w as i32, h as i32);
    }

    fn scissor(&self, rect: Option<Rect>) {
        match rect {
            Some(r) => {
                self.gl.enable(GL::SCISSOR_TEST);
                self.gl.scissor(r.x, r.y, r.w.max(0), r.h.max(0));
            }
            None => self.gl.disable(GL::SCISSOR_TEST),
        }
    }

    fn blend(&self, mode: CompositeMode) {
        let (src, dst) = blend_factors(mode);
        if (src, dst) == (GL::ONE, GL::ZERO) {
            self.gl.disable(GL::BLEND);
        } else {
            self.gl.enable(GL::BLEND);
            self.gl.blend_func(src, dst);
        }
    }

    fn push_quad(&mut self, corners: [[f32; 2]; 4], uvs: [[f32; 2]; 4], color: [f32; 4]) {
        for i in [0usize, 1, 2, 0, 2, 3] {
            self.verts.extend_from_slice(&[
                corners[i][0],
                corners[i][1],
                uvs[i][0],
                uvs[i][1],
                color[0],
                color[1],
                color[2],
                color[3],
            ]);
        }
    }

    fn push_rect(&mut self, r: Rect, uv: [f32; 4], color: [f32; 4]) {
        let (x0, y0, x1, y1) = (r.x as f32, r.y as f32, r.right() as f32, r.bottom() as f32);
        self.push_quad(
            [[x0, y0], [x1, y0], [x1, y1], [x0, y1]],
            [
                [uv[0], uv[1]],
                [uv[2], uv[1]],
                [uv[2], uv[3]],
                [uv[0], uv[3]],
            ],
            color,
        );
    }

    fn flush_triangles(&mut self) {
        if self.verts.is_empty() {
            return;
        }
        let gl = &self.gl;
        gl.bind_buffer(GL::ARRAY_BUFFER, Some(&self.vbo));
        gl.buffer_data_with_u8_array(
            GL::ARRAY_BUFFER,
            bytemuck::cast_slice(&self.verts),
            GL::STREAM_DRAW,
        );
        gl.draw_arrays(
            GL::TRIANGLES,
            0,
            (self.verts.len() / FLOATS_PER_VERTEX) as i32,
        );
        self.verts.clear();
        self.draw_calls += 1;
    }

    fn draw_mesh(&mut self, mesh: &Mesh, color: [f32; 4]) {
        self.verts.clear();
        for v in &mesh.vertices {
            self.verts
                .extend_from_slice(&[v[0], v[1], 0.0, 0.0, color[0], color[1], color[2], color[3]]);
        }
        let gl = &self.gl;
        gl.bind_buffer(GL::ARRAY_BUFFER, Some(&self.vbo));
        gl.buffer_data_with_u8_array(
            GL::ARRAY_BUFFER,
            bytemuck::cast_slice(&self.verts),
            GL::STREAM_DRAW,
        );
        gl.bind_buffer(GL::ELEMENT_ARRAY_BUFFER, Some(&self.ibo));
        gl.buffer_data_with_u8_array(
            GL::ELEMENT_ARRAY_BUFFER,
            bytemuck::cast_slice(&mesh.indices),
            GL::STREAM_DRAW,
        );
        gl.draw_elements_with_i32(
            GL::TRIANGLES,
            mesh.indices.len() as i32,
            GL::UNSIGNED_INT,
            0,
        );
        self.verts.clear();
        self.draw_calls += 1;
    }

    fn use_basic(&self, tex: &WebGlTexture, w: u32, h: u32, flip_y: bool) {
        let gl = &self.gl;
        gl.use_program(Some(&self.programs.basic));
        gl.uniform_matrix3fv_with_f32_array(
            self.programs.basic_proj.as_ref(),
            false,
            &projection(w, h, flip_y),
        );
        gl.active_texture(GL::TEXTURE0);
        gl.bind_texture(GL::TEXTURE_2D, Some(tex));
        gl.uniform1i(self.programs.basic_tex.as_ref(), 0);
    }

    /// Rasterizes `shape` into the shared mask (1 inside, 0 outside), after
    /// clearing `region` of the mask.
    fn rasterize_mask(&mut self, shape: &Shape, w: u32, h: u32, region: Rect) {
        self.ensure_mask(w, h);
        let mask = self.mask.as_ref().expect("mask allocated");
        let (mw, mh, mfbo) = (mask.w, mask.h, mask.fbo.clone());
        self.bind_target(Some(&mfbo), mw, mh);
        self.scissor(Some(region));
        self.gl.clear_color(0.0, 0.0, 0.0, 0.0);
        self.gl.clear(GL::COLOR_BUFFER_BIT);
        self.gl.disable(GL::BLEND);
        let white = self.white.clone();
        self.use_basic(&white, mw, mh, false);
        match shape {
            Shape::Rects(rects) => {
                for r in rects {
                    self.push_rect(*r, [0.0; 4], [1.0; 4]);
                }
                self.flush_triangles();
            }
            Shape::Mesh(mesh) => self.draw_mesh(mesh, [1.0; 4]),
        }
    }

    // ------------------------------------------------------------------
    // Drawing operations
    // ------------------------------------------------------------------

    /// Fills `shape` on `target` with `paint` using the given composite mode,
    /// honoring canvas semantics for unbounded modes.
    pub fn draw(
        &mut self,
        target: &Surface,
        shape: &Shape,
        paint: &Paint,
        mode: CompositeMode,
        clip: Clip,
    ) {
        let mut region = target.bounds();
        if let Some(r) = clip.rect {
            region = region.intersect(&r);
        }
        if region.is_empty() {
            return;
        }
        let use_clip_mask = clip.use_mask && target.clip_mask.is_some();
        let unbounded = is_unbounded(mode);

        // Fast path: disjoint rectangles drawn directly as quads.
        if let Shape::Rects(rects) = shape {
            let simple_paint = !matches!(paint, Paint::Pattern { .. });
            if !use_clip_mask
                && simple_paint
                && (if unbounded {
                    rects.len() == 1
                } else {
                    pairwise_disjoint(rects)
                })
            {
                self.bind_target(Some(&target.fbo), target.alloc_w, target.alloc_h);
                if unbounded {
                    self.gl.clear_color(0.0, 0.0, 0.0, 0.0);
                    for part in region.subtract(&rects[0]) {
                        self.scissor(Some(part));
                        self.gl.clear(GL::COLOR_BUFFER_BIT);
                    }
                }
                self.scissor(Some(region));
                self.blend(mode);
                match paint {
                    Paint::Solid(color) => {
                        let white = self.white.clone();
                        self.use_basic(&white, target.alloc_w, target.alloc_h, false);
                        for r in rects {
                            self.push_rect(*r, [0.0; 4], *color);
                        }
                    }
                    Paint::Texture(src) => {
                        self.use_basic(src.tex, target.alloc_w, target.alloc_h, false);
                        let (tw, th) = (src.tex_w as f32, src.tex_h as f32);
                        for r in rects {
                            let corners = [
                                (r.x, r.y),
                                (r.right(), r.y),
                                (r.right(), r.bottom()),
                                (r.x, r.bottom()),
                            ];
                            let mut uvs = [[0.0f32; 2]; 4];
                            let mut pos = [[0.0f32; 2]; 4];
                            for (i, (x, y)) in corners.into_iter().enumerate() {
                                let (sx, sy) = src.map.apply(x as f64, y as f64);
                                uvs[i] = [sx as f32 / tw, sy as f32 / th];
                                pos[i] = [x as f32, y as f32];
                            }
                            self.push_quad(pos, uvs, [1.0; 4]);
                        }
                    }
                    Paint::Pattern { .. } => unreachable!(),
                }
                self.flush_triangles();
                return;
            }
        }

        // General path: coverage mask, then masked draw passes.
        let shape_bounds = shape.bounds().intersect(&region);
        if shape_bounds.is_empty() && !unbounded {
            return;
        }
        let mask_region = if unbounded { region } else { shape_bounds };
        self.rasterize_mask(shape, target.alloc_w, target.alloc_h, mask_region);

        let mask_tex = self.mask.as_ref().expect("mask allocated").tex.clone();
        let gl = &self.gl;
        self.bind_target(Some(&target.fbo), target.alloc_w, target.alloc_h);
        gl.use_program(Some(&self.programs.masked));
        let p = &self.programs;
        gl.uniform_matrix3fv_with_f32_array(
            p.m_proj.as_ref(),
            false,
            &projection(target.alloc_w, target.alloc_h, false),
        );
        gl.uniform1i(p.m_use_mask.as_ref(), 1);
        gl.active_texture(GL::TEXTURE1);
        gl.bind_texture(GL::TEXTURE_2D, Some(&mask_tex));
        gl.uniform1i(p.m_mask.as_ref(), 1);
        gl.uniform1i(p.m_use_clip.as_ref(), use_clip_mask as i32);
        gl.active_texture(GL::TEXTURE2);
        match &target.clip_mask {
            Some(clip_mask) if use_clip_mask => {
                gl.bind_texture(GL::TEXTURE_2D, Some(&clip_mask.tex))
            }
            _ => gl.bind_texture(GL::TEXTURE_2D, Some(&self.white)),
        }
        gl.uniform1i(p.m_clip.as_ref(), 2);
        gl.uniform1i(p.m_tex.as_ref(), 0);
        gl.active_texture(GL::TEXTURE0);
        gl.bind_texture(GL::TEXTURE_2D, Some(&self.white));

        if unbounded {
            gl.uniform1i(p.m_pass.as_ref(), 1);
            gl.uniform1i(p.m_paint.as_ref(), 0);
            gl.disable(GL::BLEND);
            self.scissor(Some(region));
            self.push_rect(region, [0.0; 4], [0.0; 4]);
            self.flush_triangles();
        }
        if shape_bounds.is_empty() {
            return;
        }

        let gl = &self.gl;
        let p = &self.programs;
        gl.uniform1i(p.m_pass.as_ref(), 0);
        match paint {
            Paint::Solid(color) => {
                gl.uniform1i(p.m_paint.as_ref(), 0);
                gl.uniform4f(p.m_color.as_ref(), color[0], color[1], color[2], color[3]);
            }
            Paint::Texture(src) => {
                gl.uniform1i(p.m_paint.as_ref(), 1);
                gl.uniform4f(p.m_color.as_ref(), 1.0, 1.0, 1.0, 1.0);
                gl.bind_texture(GL::TEXTURE_2D, Some(src.tex));
                gl.uniform_matrix3fv_with_f32_array(p.m_map.as_ref(), false, &src.map.to_mat3());
                gl.uniform2f(p.m_tex_size.as_ref(), src.tex_w as f32, src.tex_h as f32);
                gl.uniform4f(
                    p.m_src_rect.as_ref(),
                    src.region.x as f32,
                    src.region.y as f32,
                    src.region.right() as f32,
                    src.region.bottom() as f32,
                );
            }
            Paint::Pattern {
                source,
                width,
                height,
                map,
            } => {
                gl.uniform1i(p.m_paint.as_ref(), 2);
                gl.uniform4f(p.m_color.as_ref(), 1.0, 1.0, 1.0, 1.0);
                gl.bind_texture(GL::TEXTURE_2D, Some(&source.tex));
                gl.uniform_matrix3fv_with_f32_array(p.m_map.as_ref(), false, &map.to_mat3());
                gl.uniform2f(
                    p.m_tex_size.as_ref(),
                    source.alloc_w as f32,
                    source.alloc_h as f32,
                );
                gl.uniform4f(
                    p.m_src_rect.as_ref(),
                    0.0,
                    0.0,
                    *width as f32,
                    *height as f32,
                );
            }
        }
        self.blend(mode);
        self.scissor(Some(shape_bounds));
        self.push_rect(shape_bounds, [0.0; 4], [1.0; 4]);
        self.flush_triangles();
    }

    /// `copy`: draws a region of `src` onto `dst` (which may be the same
    /// surface) through the current transform and clip.
    #[allow(clippy::too_many_arguments)]
    pub fn copy(
        &mut self,
        src: &Surface,
        region: Rect,
        dst: &Surface,
        dx: i32,
        dy: i32,
        mode: CompositeMode,
        ctm: &Affine,
        clip: Clip,
    ) {
        let region = region.intersect(&src.bounds());
        if region.is_empty() {
            return;
        }
        let same = std::ptr::eq(src, dst);
        let (tex, tex_w, tex_h, src_region) = if same {
            self.copy_to_scratch(0, &src.fbo, region);
            let s = self.scratch[0].as_ref().expect("scratch allocated");
            (s.tex.clone(), s.w, s.h, Rect::new(0, 0, region.w, region.h))
        } else {
            (src.tex.clone(), src.alloc_w, src.alloc_h, region)
        };
        self.draw_textured(&tex, tex_w, tex_h, src_region, dst, dx, dy, mode, ctm, clip);
    }

    /// Draws an image at (x, y).
    #[allow(clippy::too_many_arguments)]
    pub fn draw_image(
        &mut self,
        image: ImageSource,
        dst: &Surface,
        x: i32,
        y: i32,
        mode: CompositeMode,
        ctm: &Affine,
        clip: Clip,
    ) -> Result<(), RendererError> {
        let (w, h) = image.size();
        let (w, h) = (w as i32, h as i32);
        let opaque = image.opaque();
        let rect = Rect::new(x, y, w, h);
        let plain = ctm.is_identity() && !clip.use_mask && clip.rect.is_none();
        let inside = rect.intersect(&dst.bounds()) == rect;

        // Fastest path: upload straight into the layer texture.
        if plain
            && inside
            && (mode == CompositeMode::SRC || (mode == CompositeMode::OVER && opaque))
        {
            if mode == CompositeMode::SRC {
                self.bind_target(Some(&dst.fbo), dst.alloc_w, dst.alloc_h);
                self.gl.clear_color(0.0, 0.0, 0.0, 0.0);
                for part in dst.bounds().subtract(&rect) {
                    self.scissor(Some(part));
                    self.gl.clear(GL::COLOR_BUFFER_BIT);
                }
            }
            let tex = dst.tex.clone();
            return self.upload_to(&tex, x, y, image);
        }

        self.upload_image(image)?;
        let (tex, tw, th) = self.upload.clone().expect("upload texture");
        self.draw_textured(
            &tex,
            tw,
            th,
            Rect::new(0, 0, w, h),
            dst,
            x,
            y,
            mode,
            ctm,
            clip,
        );
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_textured(
        &mut self,
        tex: &WebGlTexture,
        tex_w: u32,
        tex_h: u32,
        region: Rect,
        dst: &Surface,
        dx: i32,
        dy: i32,
        mode: CompositeMode,
        ctm: &Affine,
        clip: Clip,
    ) {
        let dst_rect = Rect::new(dx, dy, region.w, region.h);
        // user space (dx + u, dy + v) <-> source (region.x + u, region.y + v)
        let user_to_src = Affine::translate((region.x - dx) as f64, (region.y - dy) as f64);
        let (shape, map) = if ctm.is_translation() && ctm.e.fract() == 0.0 && ctm.f.fract() == 0.0 {
            let (tx, ty) = (ctm.e as i32, ctm.f as i32);
            let r = Rect::new(dst_rect.x + tx, dst_rect.y + ty, dst_rect.w, dst_rect.h);
            (
                Shape::Rects(vec![r]),
                user_to_src.mul(&Affine::translate(-ctm.e, -ctm.f)),
            )
        } else {
            let Some(inv) = ctm.invert() else { return };
            let corners = [
                (dst_rect.x, dst_rect.y),
                (dst_rect.right(), dst_rect.y),
                (dst_rect.right(), dst_rect.bottom()),
                (dst_rect.x, dst_rect.bottom()),
            ]
            .map(|(x, y)| {
                let (tx, ty) = ctm.apply(x as f64, y as f64);
                [tx, ty]
            });
            (Shape::Mesh(Mesh::quad(corners)), user_to_src.mul(&inv))
        };
        let paint = Paint::Texture(TextureSource {
            tex,
            tex_w,
            tex_h,
            region,
            map,
        });
        self.draw(dst, &shape, &paint, mode, clip);
    }

    /// Raw copy of pixels including alpha (`transfer` with SRC), ignoring
    /// transform, clip and composite mode like canvas `putImageData`.
    pub fn put(&mut self, src: &Surface, region: Rect, dst: &Surface, dx: i32, dy: i32) {
        let region = region.intersect(&src.bounds());
        if region.is_empty() {
            return;
        }
        let target = Rect::new(dx, dy, region.w, region.h).intersect(&dst.bounds());
        if target.is_empty() {
            return;
        }
        let region = Rect::new(
            region.x + (target.x - dx),
            region.y + (target.y - dy),
            target.w,
            target.h,
        );
        let gl = self.gl.clone();
        if std::ptr::eq(src, dst) {
            self.copy_to_scratch(0, &src.fbo, region);
            let s = self.scratch[0].as_ref().expect("scratch allocated");
            gl.bind_framebuffer(GL::READ_FRAMEBUFFER, Some(&s.fbo));
            gl.bind_framebuffer(GL::DRAW_FRAMEBUFFER, Some(&dst.fbo));
            gl.blit_framebuffer(
                0,
                0,
                target.w,
                target.h,
                target.x,
                target.y,
                target.right(),
                target.bottom(),
                GL::COLOR_BUFFER_BIT,
                GL::NEAREST,
            );
        } else {
            gl.disable(GL::SCISSOR_TEST);
            gl.bind_framebuffer(GL::READ_FRAMEBUFFER, Some(&src.fbo));
            gl.bind_framebuffer(GL::DRAW_FRAMEBUFFER, Some(&dst.fbo));
            gl.blit_framebuffer(
                region.x,
                region.y,
                region.right(),
                region.bottom(),
                target.x,
                target.y,
                target.right(),
                target.bottom(),
                GL::COLOR_BUFFER_BIT,
                GL::NEAREST,
            );
        }
        gl.bind_framebuffer(GL::FRAMEBUFFER, None);
    }

    /// `transfer`: applies a raster operation between `src` and `dst`.
    #[allow(clippy::too_many_arguments)]
    pub fn transfer(
        &mut self,
        src: &Surface,
        region: Rect,
        dst: &Surface,
        dx: i32,
        dy: i32,
        function: TransferFunction,
    ) {
        let region = region.intersect(&src.bounds());
        if region.is_empty() {
            return;
        }
        let target = Rect::new(dx, dy, region.w, region.h).intersect(&dst.bounds());
        if target.is_empty() {
            return;
        }
        let region = Rect::new(
            region.x + (target.x - dx),
            region.y + (target.y - dy),
            target.w,
            target.h,
        );

        // Snapshot the destination (and the source, when it is the same surface).
        self.copy_to_scratch(0, &dst.fbo, target);
        let (src_tex, src_origin) = if std::ptr::eq(src, dst) {
            self.copy_to_scratch(1, &src.fbo, region);
            (
                self.scratch[1].as_ref().expect("scratch").tex.clone(),
                (0, 0),
            )
        } else {
            (src.tex.clone(), (region.x, region.y))
        };
        let dst_copy = self.scratch[0].as_ref().expect("scratch").tex.clone();

        self.bind_target(Some(&dst.fbo), dst.alloc_w, dst.alloc_h);
        self.scissor(Some(target));
        self.gl.disable(GL::BLEND);
        let gl = &self.gl;
        let p = &self.programs;
        gl.use_program(Some(&p.transfer));
        gl.uniform_matrix3fv_with_f32_array(
            p.t_proj.as_ref(),
            false,
            &projection(dst.alloc_w, dst.alloc_h, false),
        );
        gl.active_texture(GL::TEXTURE0);
        gl.bind_texture(GL::TEXTURE_2D, Some(&src_tex));
        gl.uniform1i(p.t_src.as_ref(), 0);
        gl.active_texture(GL::TEXTURE1);
        gl.bind_texture(GL::TEXTURE_2D, Some(&dst_copy));
        gl.uniform1i(p.t_dst.as_ref(), 1);
        gl.uniform2i(p.t_src_origin.as_ref(), src_origin.0, src_origin.1);
        gl.uniform2i(p.t_dst_origin.as_ref(), 0, 0);
        gl.uniform2i(p.t_target_origin.as_ref(), target.x, target.y);
        gl.uniform1i(p.t_function.as_ref(), function.0 as i32);
        gl.active_texture(GL::TEXTURE0);
        self.push_rect(target, [0.0; 4], [1.0; 4]);
        self.flush_triangles();
    }

    /// Rebuilds the clip mask of `surface` as the intersection of `shapes`.
    pub fn build_clip_mask(&mut self, surface: &mut Surface, shapes: &[Shape]) {
        let (w, h) = (surface.alloc_w, surface.alloc_h);
        let needs_alloc = surface
            .clip_mask
            .as_ref()
            .is_none_or(|m| m.w != w || m.h != h);
        if needs_alloc {
            let (tex, fbo) = self.new_color_texture(w, h, GL::R8);
            surface.clip_mask = Some(MaskTarget {
                gl: self.gl.clone(),
                tex,
                fbo,
                w,
                h,
            });
        }
        let cfbo = surface.clip_mask.as_ref().expect("clip mask").fbo.clone();
        let full = Rect::new(0, 0, w as i32, h as i32);

        // Start fully visible, then multiply in each shape's coverage.
        self.bind_target(Some(&cfbo), w, h);
        self.scissor(None);
        self.gl.clear_color(1.0, 1.0, 1.0, 1.0);
        self.gl.clear(GL::COLOR_BUFFER_BIT);
        for shape in shapes {
            self.rasterize_mask(shape, w, h, full);
            let mask = self.mask.as_ref().expect("mask");
            let (mtex, mw, mh) = (mask.tex.clone(), mask.w, mask.h);
            self.bind_target(Some(&cfbo), w, h);
            self.scissor(None);
            self.gl.enable(GL::BLEND);
            self.gl.blend_func(GL::ZERO, GL::SRC_COLOR);
            self.use_basic(&mtex, w, h, false);
            self.push_rect(
                full,
                [0.0, 0.0, w as f32 / mw as f32, h as f32 / mh as f32],
                [1.0; 4],
            );
            self.flush_triangles();
        }
    }

    /// Reads premultiplied RGBA pixels (top row first).
    pub fn read_pixels(&self, surface: &Surface, region: Rect) -> Option<Vec<u8>> {
        let region = region.intersect(&surface.bounds());
        if region.is_empty() {
            return None;
        }
        let mut data = vec![0u8; (region.w * region.h * 4) as usize];
        self.gl
            .bind_framebuffer(GL::FRAMEBUFFER, Some(&surface.fbo));
        self.gl
            .read_pixels_with_opt_u8_array(
                region.x,
                region.y,
                region.w,
                region.h,
                GL::RGBA,
                GL::UNSIGNED_BYTE,
                Some(&mut data),
            )
            .ok()?;
        self.gl.bind_framebuffer(GL::FRAMEBUFFER, None);
        Some(data)
    }

    /// Starts an asynchronous read of premultiplied RGBA pixels.
    pub fn start_readback(&self, surface: &Surface, region: Rect) -> Option<Readback> {
        let region = region.intersect(&surface.bounds());
        if region.is_empty() {
            return None;
        }
        let gl = &self.gl;
        let len = (region.w * region.h * 4) as usize;
        let buffer = gl.create_buffer()?;
        gl.bind_buffer(GL::PIXEL_PACK_BUFFER, Some(&buffer));
        gl.buffer_data_with_i32(GL::PIXEL_PACK_BUFFER, len as i32, GL::STREAM_READ);
        gl.bind_framebuffer(GL::FRAMEBUFFER, Some(&surface.fbo));
        let read = gl.read_pixels_with_i32(
            region.x,
            region.y,
            region.w,
            region.h,
            GL::RGBA,
            GL::UNSIGNED_BYTE,
            0,
        );
        gl.bind_buffer(GL::PIXEL_PACK_BUFFER, None);
        gl.bind_framebuffer(GL::FRAMEBUFFER, None);
        if read.is_err() {
            gl.delete_buffer(Some(&buffer));
            return None;
        }
        let Some(sync) = gl.fence_sync(GL::SYNC_GPU_COMMANDS_COMPLETE, 0) else {
            gl.delete_buffer(Some(&buffer));
            return None;
        };
        gl.flush();
        Some(Readback {
            gl: gl.clone(),
            buffer,
            sync,
            len,
        })
    }

    /// Reads one pixel of the canvas (top-left origin). Only meaningful in
    /// the same task as `composite`, before the browser presents the frame.
    pub fn read_canvas_pixel(&self, x: i32, y: i32) -> Option<[u8; 4]> {
        let h = self.canvas.height() as i32;
        let mut px = [0u8; 4];
        self.gl.bind_framebuffer(GL::FRAMEBUFFER, None);
        self.gl
            .read_pixels_with_opt_u8_array(
                x,
                h - 1 - y,
                1,
                1,
                GL::RGBA,
                GL::UNSIGNED_BYTE,
                Some(&mut px),
            )
            .ok()?;
        Some(px)
    }

    // ------------------------------------------------------------------
    // Final composite
    // ------------------------------------------------------------------

    /// Draws the visible layer tree onto the canvas.
    pub fn composite(&mut self, nodes: &[CompositeNode], background: [f32; 3], smooth: bool) {
        let (cw, ch) = (self.canvas.width(), self.canvas.height());
        if cw == 0 || ch == 0 {
            return;
        }
        let gl = self.gl.clone();
        self.bind_target(None, cw, ch);
        gl.disable(GL::SCISSOR_TEST);
        gl.clear_color(background[0], background[1], background[2], 1.0);
        gl.clear(GL::COLOR_BUFFER_BIT);
        gl.bind_sampler(0, Some(if smooth { &self.linear } else { &self.nearest }));
        self.composite_nodes(nodes, None, 0, cw, ch);
        gl.disable(GL::SCISSOR_TEST);
        gl.bind_sampler(0, Some(&self.nearest));
    }

    /// Canvas-pixel scissor (top-left origin) on a target with `ch` rows.
    fn canvas_scissor(&self, rect: Option<Rect>, ch: u32) -> bool {
        match rect {
            Some(r) if r.is_empty() => false,
            Some(r) => {
                self.gl.enable(GL::SCISSOR_TEST);
                self.gl.scissor(r.x, ch as i32 - r.bottom(), r.w, r.h);
                true
            }
            None => {
                self.gl.disable(GL::SCISSOR_TEST);
                true
            }
        }
    }

    fn composite_nodes(
        &mut self,
        nodes: &[CompositeNode],
        fbo: Option<&WebGlFramebuffer>,
        depth: usize,
        cw: u32,
        ch: u32,
    ) {
        for node in nodes {
            match node {
                CompositeNode::Layer(item) => {
                    if item.opacity <= 0.0 || item.width == 0 || item.height == 0 {
                        continue;
                    }
                    self.bind_target(fbo, cw, ch);
                    if !self.canvas_scissor(item.scissor, ch) {
                        continue;
                    }
                    self.gl.enable(GL::BLEND);
                    self.gl.blend_func(GL::ONE, GL::ONE_MINUS_SRC_ALPHA);
                    self.use_basic(&item.surface.tex, cw, ch, true);
                    let (w, h) = (item.width as f64, item.height as f64);
                    let corners = [(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)].map(|(x, y)| {
                        let (tx, ty) = item.transform.apply(x, y);
                        [tx as f32, ty as f32]
                    });
                    let u1 = item.width as f32 / item.surface.alloc_w as f32;
                    let v1 = item.height as f32 / item.surface.alloc_h as f32;
                    let a = item.opacity;
                    self.push_quad(
                        corners,
                        [[0.0, 0.0], [u1, 0.0], [u1, v1], [0.0, v1]],
                        [a, a, a, a],
                    );
                    self.flush_triangles();
                }
                CompositeNode::Group {
                    opacity,
                    bounds,
                    children,
                } => {
                    let bounds = bounds.intersect(&Rect::new(0, 0, cw as i32, ch as i32));
                    if *opacity <= 0.0 || bounds.is_empty() {
                        continue;
                    }
                    // Flatten the subtree into an off-screen target that uses
                    // exactly the canvas coordinate system.
                    self.ensure_group_target(depth, cw, ch);
                    let target = &self.group_targets[depth];
                    let (gfbo, gtex, gw, gh) =
                        (target.fbo.clone(), target.tex.clone(), target.w, target.h);
                    self.bind_target(Some(&gfbo), cw, ch);
                    self.canvas_scissor(Some(bounds), ch);
                    self.gl.clear_color(0.0, 0.0, 0.0, 0.0);
                    self.gl.clear(GL::COLOR_BUFFER_BIT);
                    self.composite_nodes(children, Some(&gfbo), depth + 1, cw, ch);

                    // Blend the flattened group into the parent target.
                    self.bind_target(fbo, cw, ch);
                    self.canvas_scissor(Some(bounds), ch);
                    self.gl.enable(GL::BLEND);
                    self.gl.blend_func(GL::ONE, GL::ONE_MINUS_SRC_ALPHA);
                    self.gl.bind_sampler(0, Some(&self.nearest));
                    self.use_basic(&gtex, cw, ch, true);
                    let (x0, y0, x1, y1) = (bounds.x, bounds.y, bounds.right(), bounds.bottom());
                    // Rows of the group target are stored bottom-up like the canvas.
                    let uv =
                        |x: i32, y: i32| [x as f32 / gw as f32, (ch as i32 - y) as f32 / gh as f32];
                    let a = *opacity;
                    self.push_quad(
                        [
                            [x0 as f32, y0 as f32],
                            [x1 as f32, y0 as f32],
                            [x1 as f32, y1 as f32],
                            [x0 as f32, y1 as f32],
                        ],
                        [uv(x0, y0), uv(x1, y0), uv(x1, y1), uv(x0, y1)],
                        [a, a, a, a],
                    );
                    self.flush_triangles();
                }
            }
        }
    }

    fn ensure_group_target(&mut self, depth: usize, w: u32, h: u32) {
        while self.group_targets.len() <= depth {
            let (tex, fbo) = self.new_color_texture(w, h, GL::RGBA8);
            self.group_targets.push(ColorTarget {
                gl: self.gl.clone(),
                tex,
                fbo,
                w,
                h,
            });
        }
        let current = &self.group_targets[depth];
        if current.w < w || current.h < h {
            let (nw, nh) = (w.max(current.w), h.max(current.h));
            let (tex, fbo) = self.new_color_texture(nw, nh, GL::RGBA8);
            self.group_targets[depth] = ColorTarget {
                gl: self.gl.clone(),
                tex,
                fbo,
                w: nw,
                h: nh,
            };
        }
    }
}
