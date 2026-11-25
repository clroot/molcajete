//! The Guacamole display model: layers, buffers, drawing state and frames.
//!
//! Drawing instructions are queued as [`Op`]s and executed one complete frame
//! (everything up to a `sync`) at a time, once every image in that frame has
//! been decoded. This mirrors `Guacamole.Display` and guarantees that the user
//! never sees a half-drawn frame, while the actual pixel work happens on the
//! GPU through [`crate::gl::Renderer`].
//!
//! Images of the zstd type (`molcajete_protocol::zimage`) need no decoder of
//! the browser and nothing to wait for: they are queued as received and
//! decoded when their frame is drawn.

mod frames;
pub mod path;
mod present;

use std::collections::{HashMap, VecDeque};

use molcajete_protocol::{CompositeMode, LineCap, LineJoin, TransferFunction};

use crate::geom::{Affine, Rect};
use crate::gl::{
    Clip, CompositeItem, CompositeNode, ImageSource, Paint, Readback, Renderer, Surface,
};
use crate::image::DecodedImage;
use frames::Frames;
use molcajete_protocol::zimage;
use path::{Path, Shape};
use present::Unpresented;

/// An image a queued draw is ready to use.
pub enum QueuedImage {
    /// Decoded by the browser.
    Decoded(DecodedImage),
    /// A zstd image as it was received; decoded when it is drawn.
    Zstd(Vec<u8>),
}

impl From<DecodedImage> for QueuedImage {
    fn from(image: DecodedImage) -> Self {
        Self::Decoded(image)
    }
}

pub type ImageState = frames::ImageState<QueuedImage>;
pub type ImageSlot = frames::ImageSlot<QueuedImage>;

/// The pixel buffer of zstd images is given back after an image larger
/// than this many bytes (4 megapixels), and otherwise kept for the next.
const KEPT_PIXEL_BYTES: usize = 16 << 20;

/// Drawing operations, already decoded from instructions.
#[derive(Debug, Clone)]
pub enum DrawOp {
    Arc {
        layer: i32,
        x: i32,
        y: i32,
        radius: i32,
        start: f64,
        end: f64,
        negative: bool,
    },
    Cfill {
        mode: CompositeMode,
        layer: i32,
        color: [u8; 4],
    },
    Clip {
        layer: i32,
    },
    Close {
        layer: i32,
    },
    Copy {
        src: i32,
        rect: Rect,
        mode: CompositeMode,
        dst: i32,
        x: i32,
        y: i32,
    },
    Cstroke {
        mode: CompositeMode,
        layer: i32,
        cap: LineCap,
        join: LineJoin,
        thickness: i32,
        color: [u8; 4],
    },
    Cursor {
        hotspot_x: i32,
        hotspot_y: i32,
        src: i32,
        rect: Rect,
    },
    Curve {
        layer: i32,
        cp1: (i32, i32),
        cp2: (i32, i32),
        end: (i32, i32),
    },
    Dispose {
        layer: i32,
    },
    Distort {
        layer: i32,
        matrix: [f64; 6],
    },
    Identity {
        layer: i32,
    },
    Lfill {
        mode: CompositeMode,
        layer: i32,
        src: i32,
    },
    Line {
        layer: i32,
        x: i32,
        y: i32,
    },
    Lstroke {
        mode: CompositeMode,
        layer: i32,
        cap: LineCap,
        join: LineJoin,
        thickness: i32,
        src: i32,
    },
    Move {
        layer: i32,
        parent: i32,
        x: i32,
        y: i32,
        z: i32,
    },
    Pop {
        layer: i32,
    },
    Push {
        layer: i32,
    },
    Rect {
        layer: i32,
        rect: Rect,
    },
    Reset {
        layer: i32,
    },
    MiterLimit {
        layer: i32,
        limit: f64,
    },
    Shade {
        layer: i32,
        opacity: u8,
    },
    Size {
        layer: i32,
        width: i32,
        height: i32,
    },
    Start {
        layer: i32,
        x: i32,
        y: i32,
    },
    Transfer {
        src: i32,
        rect: Rect,
        function: TransferFunction,
        dst: i32,
        x: i32,
        y: i32,
    },
    Transform {
        layer: i32,
        matrix: [f64; 6],
    },
}

pub enum Op {
    Draw(DrawOp),
    Image {
        layer: i32,
        mode: CompositeMode,
        x: i32,
        y: i32,
        slot: ImageSlot,
    },
    Sync {
        timestamp: i64,
        frames: u32,
        received_at: f64,
    },
}

/// Notifications produced while executing frames.
#[derive(Debug, Clone)]
pub enum DisplayEvent {
    Resized {
        width: u32,
        height: u32,
    },
    /// A frame ending with `sync,timestamp` has been fully applied.
    FrameComplete {
        timestamp: i64,
        frames: u32,
        lag_ms: f64,
    },
    /// New cursor image (un-premultiplied RGBA, top row first).
    Cursor {
        hotspot_x: i32,
        hotspot_y: i32,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
}

#[derive(Clone, Debug)]
struct DrawState {
    ctm: Affine,
    clips: Vec<Shape>,
    mode: CompositeMode,
    miter_limit: f64,
}

impl Default for DrawState {
    fn default() -> Self {
        Self {
            ctm: Affine::IDENTITY,
            clips: Vec::new(),
            mode: CompositeMode::OVER,
            miter_limit: 10.0,
        }
    }
}

struct Layer {
    surface: Option<Surface>,
    width: u32,
    height: u32,
    /// Buffers grow automatically to fit whatever is drawn into them.
    autosize: bool,

    // Visible-layer properties (layer index >= 0).
    parent: Option<i32>,
    x: i32,
    y: i32,
    z: i32,
    opacity: u8,
    matrix: Affine,
    /// Insertion order, used to break ties between equal z values.
    order: u64,

    // Drawing state.
    state: DrawState,
    stack: Vec<DrawState>,
    path: Path,
    /// Mirrors `pathClosed` of the reference client: the next path
    /// instruction starts a new path.
    path_closed: bool,
    clip_dirty: bool,
    /// Changes whenever the layer's pixels may have changed.
    generation: u64,
}

impl Layer {
    fn new(width: u32, height: u32, autosize: bool) -> Self {
        Self {
            surface: None,
            width,
            height,
            autosize,
            parent: None,
            x: 0,
            y: 0,
            z: 0,
            opacity: 255,
            matrix: Affine::IDENTITY,
            order: 0,
            state: DrawState::default(),
            stack: Vec::new(),
            path: Path::default(),
            path_closed: true,
            clip_dirty: false,
            generation: 0,
        }
    }

    fn reset_state(&mut self) {
        let mode = self.state.mode;
        self.state = DrawState {
            mode,
            ..DrawState::default()
        };
        self.stack.clear();
        self.path.clear();
        self.path_closed = false;
        self.clip_dirty = true;
        if let Some(s) = &mut self.surface {
            s.drop_clip_mask();
        }
    }

    fn begin_path_if_closed(&mut self) {
        if self.path_closed {
            self.path.clear();
            self.path_closed = false;
        }
    }

    /// The clip to use for drawing, plus whether a mask must be (re)built.
    fn clip(&self) -> Clip {
        if self.state.clips.is_empty() {
            return Clip::default();
        }
        let mut rect: Option<Rect> = None;
        for shape in &self.state.clips {
            match shape {
                Shape::Rects(r) if r.len() == 1 => {
                    rect = Some(rect.map_or(r[0], |acc| acc.intersect(&r[0])));
                }
                Shape::Rects(r) if r.is_empty() => {
                    return Clip {
                        rect: Some(Rect::default()),
                        use_mask: false,
                    };
                }
                _ => {
                    return Clip {
                        rect: None,
                        use_mask: true,
                    };
                }
            }
        }
        Clip {
            rect,
            use_mask: false,
        }
    }

    fn resolve_mode(&mut self, mode: CompositeMode) -> CompositeMode {
        // Unimplemented masks leave the previous operation in effect, like
        // assigning an unknown value to `globalCompositeOperation`.
        if !matches!(mode.0, 0x0 | 0x3 | 0x5 | 0x7 | 0xD) {
            self.state.mode = mode;
        }
        self.state.mode
    }
}

#[derive(Default)]
struct SoftwareCursor {
    surface: Option<Surface>,
    width: u32,
    height: u32,
    hotspot_x: i32,
    hotspot_y: i32,
    x: i32,
    y: i32,
    visible: bool,
}

/// Sum and maximum of a duration measured several times, in milliseconds.
#[derive(Default, Clone, Copy, Debug)]
pub struct Span {
    pub count: u32,
    pub total_ms: f64,
    pub max_ms: f64,
}

impl Span {
    pub(crate) fn add(&mut self, ms: f64) {
        self.count += 1;
        self.total_ms += ms;
        self.max_ms = self.max_ms.max(ms);
    }

    pub fn average_ms(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.total_ms / self.count as f64
        }
    }
}

#[derive(Default, Clone, Copy, Debug)]
pub struct RenderStats {
    pub frames_applied: u32,
    pub composites: u32,
    pub draw_calls: u32,
    pub queued_ops: usize,
    /// Per frame: from its first instruction to its `sync` (the time its
    /// data took to arrive and be parsed).
    pub receive: Span,
    /// Per frame: from its `sync` to being applied (waiting for its images
    /// and for the frames before it).
    pub lag: Span,
    /// Per image: from handing the bytes to the decoder to the decoded image.
    pub decode: Span,
    /// Per frame: from being applied to the layers to being drawn to the
    /// canvas (waiting for the next composite, and the composite itself).
    pub present: Span,
}

pub struct Display {
    renderer: Renderer,
    layers: HashMap<i32, Layer>,
    order_counter: u64,
    queue: VecDeque<Op>,
    queued_syncs: usize,
    /// The images each queued frame still waits for.
    frames: Frames,
    /// Decoder of zstd images and the pixels of the one being drawn.
    zstd: zimage::Decoder,
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    cursor: SoftwareCursor,
    /// Scale factor from remote pixels to CSS pixels.
    scale: f64,
    device_pixel_ratio: f64,
    needs_composite: bool,
    events: Vec<DisplayEvent>,
    stats: RenderStats,
    unpresented: Unpresented,
    /// When the first instruction of the frame being received arrived.
    frame_started: Option<f64>,
    /// Source of layer generations (unique across layers and re-creations).
    generation_counter: u64,
    /// Identity of the last cursor read back: (src, rect, hotspot, generation).
    last_cursor: Option<(i32, Rect, i32, i32, u64)>,
    /// Cursor image being read back from the GPU: (readback, hotspot, size).
    pending_cursor: Option<(Readback, i32, i32, u32, u32)>,
    /// Last cursor delivered as an event, to skip identical updates.
    last_cursor_image: Option<(i32, i32, u32, u32, Vec<u8>)>,
}

fn premultiply(color: [u8; 4]) -> [f32; 4] {
    let a = color[3] as f32 / 255.0;
    [
        color[0] as f32 / 255.0 * a,
        color[1] as f32 / 255.0 * a,
        color[2] as f32 / 255.0 * a,
        a,
    ]
}

impl Display {
    pub fn new(renderer: Renderer) -> Self {
        let mut layers = HashMap::new();
        layers.insert(0, Layer::new(0, 0, false));
        Self {
            renderer,
            layers,
            order_counter: 1,
            queue: VecDeque::new(),
            queued_syncs: 0,
            frames: Frames::default(),
            zstd: zimage::Decoder::default(),
            pixels: Vec::new(),
            width: 0,
            height: 0,
            cursor: SoftwareCursor::default(),
            scale: 1.0,
            device_pixel_ratio: 1.0,
            needs_composite: true,
            events: Vec::new(),
            stats: RenderStats::default(),
            unpresented: Unpresented::default(),
            frame_started: None,
            generation_counter: 0,
            last_cursor: None,
            pending_cursor: None,
            last_cursor_image: None,
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn renderer(&self) -> &Renderer {
        &self.renderer
    }

    pub fn take_events(&mut self) -> Vec<DisplayEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn take_stats(&mut self) -> RenderStats {
        let mut stats = std::mem::take(&mut self.stats);
        stats.draw_calls = self.renderer.take_draw_calls();
        stats.queued_ops = self.queue.len();
        stats
    }

    pub fn set_scale(&mut self, scale: f64, device_pixel_ratio: f64) {
        if scale != self.scale || device_pixel_ratio != self.device_pixel_ratio {
            self.scale = scale;
            self.device_pixel_ratio = device_pixel_ratio;
            self.needs_composite = true;
        }
    }

    pub fn scale(&self) -> f64 {
        self.scale
    }

    pub fn needs_composite(&self) -> bool {
        self.needs_composite || self.pending_cursor.is_some()
    }

    pub fn invalidate(&mut self) {
        self.needs_composite = true;
    }

    /// Discards all queued work and layers (on disconnect).
    pub fn reset(&mut self) {
        self.queue.clear();
        self.queued_syncs = 0;
        self.frames = Frames::default();
        self.layers.clear();
        self.layers.insert(0, Layer::new(0, 0, false));
        self.cursor = SoftwareCursor::default();
        self.last_cursor = None;
        self.pending_cursor = None;
        self.last_cursor_image = None;
        self.unpresented = Unpresented::default();
        self.width = 0;
        self.height = 0;
        self.needs_composite = true;
    }

    // ------------------------------------------------------------------
    // Queueing
    // ------------------------------------------------------------------

    pub fn enqueue(&mut self, op: Op) {
        match &op {
            Op::Sync { received_at, .. } => {
                let received_at = *received_at;
                self.queued_syncs += 1;
                self.frames.end_frame();
                let started = self.frame_started.take().unwrap_or(received_at);
                self.stats.receive.add(received_at - started);
            }
            _ => {
                if let Op::Image { slot, .. } = &op {
                    self.frames.add_image(slot);
                }
                self.frame_started
                    .get_or_insert_with(crate::scope::performance_now);
            }
        }
        self.queue.push_back(op);
    }

    /// Records how long decoding one image took.
    pub fn image_decoded(&mut self, ms: f64) {
        self.stats.decode.add(ms);
    }

    /// Executes every complete frame whose images are ready.
    pub fn process(&mut self) {
        // This runs after every decoded image, so whether the oldest frame
        // still waits for one is counted rather than searched for.
        while self.queued_syncs > 0 && !self.frames.oldest_is_waiting() {
            while let Some(op) = self.queue.pop_front() {
                match op {
                    Op::Sync {
                        timestamp,
                        frames,
                        received_at,
                    } => {
                        self.queued_syncs -= 1;
                        self.frames.pop_oldest();
                        self.stats.frames_applied += 1;
                        self.needs_composite = true;
                        let lag_ms = crate::scope::performance_now() - received_at;
                        self.stats.lag.add(lag_ms);
                        self.unpresented.add(received_at + lag_ms);
                        self.events.push(DisplayEvent::FrameComplete {
                            timestamp,
                            frames,
                            lag_ms,
                        });
                        break;
                    }
                    Op::Draw(op) => self.execute(op),
                    Op::Image {
                        layer,
                        mode,
                        x,
                        y,
                        slot,
                    } => match slot.take() {
                        ImageState::Ready(QueuedImage::Decoded(image)) => {
                            self.draw_image(layer, mode, x, y, ImageSource::Decoded(&image));
                            image.close();
                        }
                        ImageState::Ready(QueuedImage::Zstd(data)) => {
                            self.draw_zstd_image(layer, mode, x, y, &data);
                        }
                        ImageState::Pending | ImageState::Failed => {}
                    },
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Layer management
    // ------------------------------------------------------------------

    fn ensure_layer(&mut self, index: i32) {
        if self.layers.contains_key(&index) {
            return;
        }
        let layer = if index > 0 {
            // New visible layers take the display size and sit at (0, 0)
            // inside the default layer.
            let mut layer = Layer::new(self.width, self.height, false);
            layer.surface = self
                .renderer
                .resize_surface(None, self.width, self.height, 0, 0);
            layer.parent = Some(0);
            layer.order = self.next_order();
            layer
        } else {
            Layer::new(0, 0, true)
        };
        self.layers.insert(index, layer);
    }

    /// Records that a layer's pixels changed.
    fn touch(&mut self, index: i32) {
        self.generation_counter += 1;
        let generation = self.generation_counter;
        if let Some(layer) = self.layers.get_mut(&index) {
            layer.generation = generation;
        }
    }

    fn next_order(&mut self) -> u64 {
        self.order_counter += 1;
        self.order_counter
    }

    fn layer(&mut self, index: i32) -> &mut Layer {
        self.ensure_layer(index);
        self.layers.get_mut(&index).expect("layer exists")
    }

    fn resize_layer(&mut self, index: i32, width: u32, height: u32) {
        let layer = self.layers.get_mut(&index).expect("layer exists");
        if layer.width == width && layer.height == height {
            return;
        }
        let old = layer.surface.take();
        let keep_w = layer.width.min(width);
        let keep_h = layer.height.min(height);
        layer.surface = self
            .renderer
            .resize_surface(old, width, height, keep_w, keep_h);
        self.generation_counter += 1;
        layer.generation = self.generation_counter;
        layer.width = width;
        layer.height = height;
        layer.reset_state();
        if index == 0 {
            self.width = width;
            self.height = height;
            self.events.push(DisplayEvent::Resized { width, height });
        }
        self.needs_composite = true;
    }

    /// Grows auto-sized buffers so that `rect` fits.
    fn fit(&mut self, index: i32, x: i32, y: i32, w: i32, h: i32) {
        let layer = self.layer(index);
        if !layer.autosize {
            return;
        }
        let need_w = (x + w).max(layer.width as i32).max(0) as u32;
        let need_h = (y + h).max(layer.height as i32).max(0) as u32;
        self.resize_layer(index, need_w, need_h);
    }

    /// Prepares the clip mask of a layer before drawing to it.
    fn prepare_clip(&mut self, index: i32) -> Clip {
        let layer = self.layers.get_mut(&index).expect("layer exists");
        let clip = layer.clip();
        if clip.use_mask && layer.clip_dirty {
            if let Some(surface) = layer.surface.as_mut() {
                self.renderer.build_clip_mask(surface, &layer.state.clips);
            }
            layer.clip_dirty = false;
        }
        clip
    }

    // ------------------------------------------------------------------
    // Execution
    // ------------------------------------------------------------------

    fn execute(&mut self, op: DrawOp) {
        match op {
            DrawOp::Size {
                layer,
                width,
                height,
            } => {
                self.ensure_layer(layer);
                self.resize_layer(layer, width.max(0) as u32, height.max(0) as u32);
            }
            DrawOp::Move {
                layer,
                parent,
                x,
                y,
                z,
            } => {
                if layer > 0 && parent >= 0 && layer != parent {
                    self.ensure_layer(parent);
                    let order = self.next_order();
                    let l = self.layer(layer);
                    if l.parent != Some(parent) {
                        l.parent = Some(parent);
                        l.order = order;
                    }
                    l.x = x;
                    l.y = y;
                    l.z = z;
                    self.needs_composite = true;
                }
            }
            DrawOp::Shade { layer, opacity } => {
                if layer >= 0 {
                    self.layer(layer).opacity = opacity;
                    self.needs_composite = true;
                }
            }
            DrawOp::Distort { layer, matrix } => {
                if layer >= 0 {
                    self.layer(layer).matrix = Affine::from_array(matrix);
                    self.needs_composite = true;
                }
            }
            DrawOp::Dispose { layer } => {
                if layer != 0 && self.layers.remove(&layer).is_some() {
                    // Children of a disposed layer are detached with it.
                    for l in self.layers.values_mut() {
                        if l.parent == Some(layer) {
                            l.parent = None;
                        }
                    }
                    self.needs_composite = true;
                }
            }

            DrawOp::Start { layer, x, y } => {
                self.fit(layer, x, y, 0, 0);
                let l = self.layer(layer);
                l.begin_path_if_closed();
                l.path.move_to(&l.state.ctm.clone(), x as f64, y as f64);
            }
            DrawOp::Line { layer, x, y } => {
                self.fit(layer, x, y, 0, 0);
                let l = self.layer(layer);
                l.begin_path_if_closed();
                l.path.line_to(&l.state.ctm.clone(), x as f64, y as f64);
            }
            DrawOp::Arc {
                layer,
                x,
                y,
                radius,
                start,
                end,
                negative,
            } => {
                self.fit(layer, x, y, 0, 0);
                let l = self.layer(layer);
                l.begin_path_if_closed();
                let ctm = l.state.ctm;
                l.path.arc(
                    &ctm,
                    x as f64,
                    y as f64,
                    radius as f64,
                    start,
                    end,
                    negative,
                );
            }
            DrawOp::Curve {
                layer,
                cp1,
                cp2,
                end,
            } => {
                self.fit(layer, end.0, end.1, 0, 0);
                let l = self.layer(layer);
                l.begin_path_if_closed();
                let ctm = l.state.ctm;
                l.path.curve_to(
                    &ctm,
                    (cp1.0 as f64, cp1.1 as f64),
                    (cp2.0 as f64, cp2.1 as f64),
                    (end.0 as f64, end.1 as f64),
                );
            }
            DrawOp::Rect { layer, rect } => {
                self.fit(layer, rect.x, rect.y, rect.w, rect.h);
                let l = self.layer(layer);
                l.begin_path_if_closed();
                let ctm = l.state.ctm;
                l.path.rect(
                    &ctm,
                    rect.x as f64,
                    rect.y as f64,
                    rect.w as f64,
                    rect.h as f64,
                );
            }
            DrawOp::Close { layer } => {
                let l = self.layer(layer);
                l.path.close();
                l.path_closed = true;
            }
            DrawOp::Clip { layer } => {
                let l = self.layer(layer);
                let shape = l.path.fill_shape().unwrap_or(Shape::Rects(Vec::new()));
                l.state.clips.push(shape);
                l.path_closed = true;
                l.clip_dirty = true;
            }
            DrawOp::Push { layer } => {
                let l = self.layer(layer);
                let state = l.state.clone();
                l.stack.push(state);
            }
            DrawOp::Pop { layer } => {
                let l = self.layer(layer);
                if let Some(state) = l.stack.pop() {
                    l.state = state;
                    l.clip_dirty = true;
                }
            }
            DrawOp::Reset { layer } => self.layer(layer).reset_state(),
            DrawOp::Identity { layer } => self.layer(layer).state.ctm = Affine::IDENTITY,
            DrawOp::Transform { layer, matrix } => {
                let l = self.layer(layer);
                l.state.ctm = l.state.ctm.mul(&Affine::from_array(matrix));
            }
            DrawOp::MiterLimit { layer, limit } => self.layer(layer).state.miter_limit = limit,

            DrawOp::Cfill { mode, layer, color } => {
                let l = self.layer(layer);
                let mode = l.resolve_mode(mode);
                let shape = l.path.fill_shape();
                l.path_closed = true;
                if let Some(shape) = shape {
                    self.paint_shape(layer, &shape, PaintSource::Color(color), mode);
                }
            }
            DrawOp::Cstroke {
                mode,
                layer,
                cap,
                join,
                thickness,
                color,
            } => {
                let l = self.layer(layer);
                let mode = l.resolve_mode(mode);
                let shape = l
                    .path
                    .stroke_shape(cap, join, thickness as f64, l.state.miter_limit);
                l.path_closed = true;
                if let Some(shape) = shape {
                    self.paint_shape(layer, &shape, PaintSource::Color(color), mode);
                }
            }
            DrawOp::Lfill { mode, layer, src } => {
                self.ensure_layer(src);
                let l = self.layer(layer);
                let mode = l.resolve_mode(mode);
                let shape = l.path.fill_shape();
                l.path_closed = true;
                if let Some(shape) = shape {
                    self.paint_shape(layer, &shape, PaintSource::Pattern(src), mode);
                }
            }
            DrawOp::Lstroke {
                mode,
                layer,
                cap,
                join,
                thickness,
                src,
            } => {
                self.ensure_layer(src);
                let l = self.layer(layer);
                let mode = l.resolve_mode(mode);
                let shape = l
                    .path
                    .stroke_shape(cap, join, thickness as f64, l.state.miter_limit);
                l.path_closed = true;
                if let Some(shape) = shape {
                    self.paint_shape(layer, &shape, PaintSource::Pattern(src), mode);
                }
            }

            DrawOp::Copy {
                src,
                rect,
                mode,
                dst,
                x,
                y,
            } => {
                let Some(region) = self.source_region(src, rect) else {
                    return;
                };
                self.fit(dst, x, y, region.w, region.h);
                self.touch(dst);
                let mode = self.layer(dst).resolve_mode(mode);
                let clip = self.prepare_clip(dst);
                let ctm = self.layers[&dst].state.ctm;
                let (Some(s), Some(d)) = (
                    self.layers[&src].surface.as_ref(),
                    self.layers[&dst].surface.as_ref(),
                ) else {
                    return;
                };
                self.renderer.copy(s, region, d, x, y, mode, &ctm, clip);
            }
            DrawOp::Transfer {
                src,
                rect,
                function,
                dst,
                x,
                y,
            } => {
                let Some(region) = self.source_region(src, rect) else {
                    return;
                };
                self.fit(dst, x, y, region.w, region.h);
                self.touch(dst);
                let (Some(s), Some(d)) = (
                    self.layers[&src].surface.as_ref(),
                    self.layers[&dst].surface.as_ref(),
                ) else {
                    return;
                };
                if function == TransferFunction::SRC {
                    self.renderer.put(s, region, d, x, y);
                } else if function != TransferFunction::DEST {
                    self.renderer.transfer(s, region, d, x, y, function);
                }
            }
            DrawOp::Cursor {
                hotspot_x,
                hotspot_y,
                src,
                rect,
            } => self.set_cursor(hotspot_x, hotspot_y, src, rect),
        }
    }

    /// Clips a source rectangle to the source surface (like the reference
    /// client clips to its canvas). `None` if nothing remains.
    fn source_region(&mut self, src: i32, rect: Rect) -> Option<Rect> {
        let layer = self.layer(src);
        let surface = layer.surface.as_ref()?;
        if rect.x >= surface.alloc_w as i32 || rect.y >= surface.alloc_h as i32 {
            return None;
        }
        let w = rect.w.min(surface.alloc_w as i32 - rect.x);
        let h = rect.h.min(surface.alloc_h as i32 - rect.y);
        (w > 0 && h > 0).then(|| Rect::new(rect.x, rect.y, w, h))
    }

    fn paint_shape(&mut self, layer: i32, shape: &Shape, source: PaintSource, mode: CompositeMode) {
        self.touch(layer);
        let clip = self.prepare_clip(layer);
        let l = &self.layers[&layer];
        let Some(target) = l.surface.as_ref() else {
            return;
        };
        match source {
            PaintSource::Color(color) => {
                self.renderer
                    .draw(target, shape, &Paint::Solid(premultiply(color)), mode, clip);
            }
            PaintSource::Pattern(src) => {
                if src == layer {
                    // Sampling a texture while rendering into it is not
                    // allowed; this combination is never produced by guacd.
                    return;
                }
                let src_layer = &self.layers[&src];
                let Some(src_surface) = src_layer.surface.as_ref() else {
                    return;
                };
                let Some(map) = l.state.ctm.invert() else {
                    return;
                };
                let paint = Paint::Pattern {
                    source: src_surface,
                    width: src_layer.width.max(1),
                    height: src_layer.height.max(1),
                    map,
                };
                self.renderer.draw(target, shape, &paint, mode, clip);
            }
        }
    }

    /// Decodes a zstd image and draws it, all at once: the pixels go from
    /// wasm memory into the texture.
    fn draw_zstd_image(&mut self, layer: i32, mode: CompositeMode, x: i32, y: i32, data: &[u8]) {
        let started = crate::scope::performance_now();
        let mut pixels = std::mem::take(&mut self.pixels);
        match self.zstd.decode(data, &mut pixels) {
            Ok((width, height)) => {
                self.stats
                    .decode
                    .add(crate::scope::performance_now() - started);
                let image = ImageSource::Pixels {
                    rgba: &pixels,
                    width,
                    height,
                };
                self.draw_image(layer, mode, x, y, image);
            }
            Err(err) => {
                web_sys::console::warn_1(&format!("image decode failed: {err}").into());
            }
        }
        if pixels.capacity() <= KEPT_PIXEL_BYTES {
            self.pixels = pixels;
        }
    }

    fn draw_image(&mut self, layer: i32, mode: CompositeMode, x: i32, y: i32, image: ImageSource) {
        let (width, height) = image.size();
        self.touch(layer);
        self.fit(layer, x, y, width as i32, height as i32);
        let mode = self.layer(layer).resolve_mode(mode);
        let clip = self.prepare_clip(layer);
        let l = &self.layers[&layer];
        let Some(target) = l.surface.as_ref() else {
            return;
        };
        let ctm = l.state.ctm;
        if let Err(err) = self
            .renderer
            .draw_image(image, target, x, y, mode, &ctm, clip)
        {
            web_sys::console::warn_1(&format!("image upload failed: {err:?}").into());
        }
    }

    fn set_cursor(&mut self, hotspot_x: i32, hotspot_y: i32, src: i32, rect: Rect) {
        self.ensure_layer(src);
        // Reading pixels back stalls the GPU pipeline; skip it when neither
        // the request nor the source pixels changed.
        let key = (
            src,
            rect,
            hotspot_x,
            hotspot_y,
            self.layers[&src].generation,
        );
        if self.last_cursor == Some(key) {
            return;
        }
        self.last_cursor = Some(key);
        let width = rect.w.max(0) as u32;
        let height = rect.h.max(0) as u32;
        self.cursor.hotspot_x = hotspot_x;
        self.cursor.hotspot_y = hotspot_y;
        self.cursor.width = width;
        self.cursor.height = height;
        // Always start from a cleared surface so the new image replaces the old.
        self.cursor.surface = self.renderer.resize_surface(None, width, height, 0, 0);
        if let (Some(s), Some(c)) = (
            self.layers[&src].surface.as_ref(),
            self.cursor.surface.as_ref(),
        ) {
            self.renderer.put(s, rect, c, 0, 0);
            self.pending_cursor = self
                .renderer
                .start_readback(c, Rect::new(0, 0, rect.w, rect.h))
                .map(|r| (r, hotspot_x, hotspot_y, width, height));
        }
        if self.pending_cursor.is_none() {
            self.deliver_cursor(
                hotspot_x,
                hotspot_y,
                width,
                height,
                vec![0; (width * height * 4) as usize],
            );
        }
        self.needs_composite = true;
    }

    /// Collects a finished cursor read-back, if any.
    pub fn poll_cursor(&mut self) {
        let Some((readback, hx, hy, w, h)) = &self.pending_cursor else {
            return;
        };
        let Some(pixels) = readback.poll() else {
            return;
        };
        let (hx, hy, w, h) = (*hx, *hy, *w, *h);
        self.pending_cursor = None;
        self.deliver_cursor(hx, hy, w, h, unpremultiply(pixels));
    }

    fn deliver_cursor(
        &mut self,
        hotspot_x: i32,
        hotspot_y: i32,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    ) {
        let rgba = if rgba.len() == (width * height * 4) as usize {
            rgba
        } else {
            vec![0; (width * height * 4) as usize]
        };
        if let Some((hx, hy, w, h, last)) = &self.last_cursor_image
            && (*hx, *hy, *w, *h) == (hotspot_x, hotspot_y, width, height)
            && *last == rgba
        {
            return;
        }
        self.last_cursor_image = Some((hotspot_x, hotspot_y, width, height, rgba.clone()));
        self.events.push(DisplayEvent::Cursor {
            hotspot_x,
            hotspot_y,
            width,
            height,
            rgba,
        });
    }

    /// Shows the software cursor at a server-reported position.
    pub fn show_software_cursor(&mut self, x: i32, y: i32) {
        self.cursor.x = x;
        self.cursor.y = y;
        self.cursor.visible = true;
        self.needs_composite = true;
    }

    pub fn hide_software_cursor(&mut self) {
        if self.cursor.visible {
            self.cursor.visible = false;
            self.needs_composite = true;
        }
    }

    // ------------------------------------------------------------------
    // Composite
    // ------------------------------------------------------------------

    /// Canvas backing-store size for the current display size and scale.
    pub fn canvas_size(&self) -> (u32, u32) {
        let f = self.scale * self.device_pixel_ratio;
        (
            (self.width as f64 * f).round().max(1.0) as u32,
            (self.height as f64 * f).round().max(1.0) as u32,
        )
    }

    pub fn renderer_mut(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    /// Number of queued operations not yet executed.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Premultiplied RGBA of one layer pixel (testing aid).
    pub fn debug_layer_pixel(&self, layer: i32, x: i32, y: i32) -> Option<Vec<u8>> {
        let surface = self.layers.get(&layer)?.surface.as_ref()?;
        self.renderer.read_pixels(surface, Rect::new(x, y, 1, 1))
    }

    /// Composites immediately and reads one canvas pixel (testing aid).
    pub fn debug_canvas_pixel(&mut self, x: i32, y: i32) -> Option<[u8; 4]> {
        self.composite();
        self.renderer.read_canvas_pixel(x, y)
    }

    /// Size of a layer, if it exists (testing aid).
    pub fn debug_layer_size(&self, layer: i32) -> Option<(u32, u32)> {
        self.layers.get(&layer).map(|l| (l.width, l.height))
    }

    /// Draws all visible layers onto the canvas.
    pub fn composite(&mut self) {
        self.needs_composite = false;
        let (cw, ch) = self.canvas_size();
        let canvas = self.renderer.canvas();
        if canvas.width() != cw {
            canvas.set_width(cw);
        }
        if canvas.height() != ch {
            canvas.set_height(ch);
        }
        let f = self.scale * self.device_pixel_ratio;
        let view = Affine::scale(f, f);

        let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
        for (&index, layer) in &self.layers {
            if let Some(parent) = layer.parent {
                children.entry(parent).or_default().push(index);
            }
        }
        for list in children.values_mut() {
            list.sort_by_key(|i| {
                let l = &self.layers[i];
                (l.z, l.order)
            });
        }

        let canvas_rect = Rect::new(0, 0, cw as i32, ch as i32);
        let mut items = Vec::with_capacity(self.layers.len() + 1);
        collect_items(
            &self.layers,
            &children,
            0,
            view,
            1.0,
            canvas_rect,
            &mut items,
        );

        if self.cursor.visible
            && let Some(surface) = self.cursor.surface.as_ref()
        {
            items.push(CompositeNode::Layer(CompositeItem {
                surface,
                width: self.cursor.width,
                height: self.cursor.height,
                transform: view.mul(&Affine::translate(
                    (self.cursor.x - self.cursor.hotspot_x) as f64,
                    (self.cursor.y - self.cursor.hotspot_y) as f64,
                )),
                opacity: 1.0,
                scissor: None,
            }));
        }

        let smooth = (f - f.round()).abs() > 1e-6 || f < 1.0;
        self.renderer.composite(&items, [0.0, 0.0, 0.0], smooth);
        self.stats.composites += 1;
        if !self.unpresented.is_empty() {
            self.unpresented
                .present(crate::scope::performance_now(), &mut self.stats.present);
        }
    }
}

enum PaintSource {
    Color([u8; 4]),
    Pattern(i32),
}

fn collect_items<'a>(
    layers: &'a HashMap<i32, Layer>,
    children: &HashMap<i32, Vec<i32>>,
    index: i32,
    parent_transform: Affine,
    parent_opacity: f32,
    parent_clip: Rect,
    out: &mut Vec<CompositeNode<'a>>,
) {
    let Some(layer) = layers.get(&index) else {
        return;
    };
    let (w, h) = (layer.width as f64, layer.height as f64);
    // CSS: translate(x, y) followed by the distort matrix around the layer centre.
    let local = if layer.matrix.is_identity() {
        Affine::translate(layer.x as f64, layer.y as f64)
    } else {
        Affine::translate(layer.x as f64 + w / 2.0, layer.y as f64 + h / 2.0)
            .mul(&layer.matrix)
            .mul(&Affine::translate(-w / 2.0, -h / 2.0))
    };
    let transform = if index == 0 {
        parent_transform
    } else {
        parent_transform.mul(&local)
    };
    let own_opacity = layer.opacity as f32 / 255.0;
    if own_opacity <= 0.0 || parent_opacity <= 0.0 {
        return;
    }

    // Each layer clips its children to its own bounds (`overflow: hidden`).
    let corners = [(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)].map(|(x, y)| transform.apply(x, y));
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for (x, y) in corners {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    let own = Rect::new(
        x0.floor() as i32,
        y0.floor() as i32,
        (x1.ceil() - x0.floor()) as i32,
        (y1.ceil() - y0.floor()) as i32,
    );
    let clip = parent_clip.intersect(&own);
    let kids = children.get(&index).map(Vec::as_slice).unwrap_or(&[]);

    // A translucent layer with children must be flattened before its
    // opacity is applied; otherwise opacity can simply be multiplied.
    let group = own_opacity < 1.0 && !kids.is_empty();
    let (inherited, mut nodes) = if group {
        (1.0, Vec::with_capacity(kids.len() + 1))
    } else {
        (parent_opacity * own_opacity, Vec::new())
    };
    let target = if group { &mut nodes } else { &mut *out };

    if let Some(surface) = layer.surface.as_ref() {
        target.push(CompositeNode::Layer(CompositeItem {
            surface,
            width: layer.width,
            height: layer.height,
            transform,
            opacity: inherited,
            scissor: Some(parent_clip),
        }));
    }
    for &child in kids {
        collect_items(layers, children, child, transform, inherited, clip, target);
    }

    if group {
        out.push(CompositeNode::Group {
            opacity: parent_opacity * own_opacity,
            bounds: clip,
            children: nodes,
        });
    }
}

fn unpremultiply(mut pixels: Vec<u8>) -> Vec<u8> {
    for px in pixels.as_chunks_mut::<4>().0 {
        let a = px[3] as u32;
        if a > 0 && a < 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    pixels
}
