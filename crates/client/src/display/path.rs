//! Canvas-style path construction and tessellation.
//!
//! Points are transformed by the current transform when they are added, as
//! in the HTML canvas. Arcs and Bézier curves are flattened into polylines;
//! filling and stroking are tessellated into triangles with lyon.

use lyon_tessellation::math::point;
use lyon_tessellation::path::Path as LyonPath;
use lyon_tessellation::{
    BuffersBuilder, FillOptions, FillTessellator, FillVertex, StrokeOptions, StrokeTessellator,
    StrokeVertex, VertexBuffers,
};

use crate::geom::{Affine, Rect};
use molcajete_protocol::{LineCap, LineJoin};

#[derive(Clone, Debug, Default)]
struct SubPath {
    points: Vec<[f64; 2]>,
    closed: bool,
    /// Created by `rect()` and still axis-aligned with integer corners.
    rect: Option<Rect>,
}

/// The current path of a layer.
#[derive(Clone, Debug, Default)]
pub struct Path {
    subpaths: Vec<SubPath>,
}

/// Triangle mesh in layer pixel coordinates.
#[derive(Clone, Debug, Default)]
pub struct Mesh {
    pub vertices: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
    pub bounds: Rect,
}

impl Mesh {
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    /// Two triangles covering a (possibly transformed) quad.
    pub fn quad(corners: [[f64; 2]; 4]) -> Mesh {
        let vertices: Vec<[f32; 2]> = corners.iter().map(|p| [p[0] as f32, p[1] as f32]).collect();
        let bounds = bounds_of(corners.iter().copied());
        Mesh {
            vertices,
            indices: vec![0, 1, 2, 0, 2, 3],
            bounds,
        }
    }
}

/// Geometry to be drawn.
#[derive(Clone, Debug)]
pub enum Shape {
    /// Axis-aligned integer rectangles (they may overlap).
    Rects(Vec<Rect>),
    Mesh(Mesh),
}

impl Shape {
    pub fn bounds(&self) -> Rect {
        match self {
            Shape::Rects(rects) => rects.iter().fold(Rect::default(), |acc, r| acc.union(r)),
            Shape::Mesh(mesh) => mesh.bounds,
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Shape::Rects(rects) => rects.iter().all(Rect::is_empty),
            Shape::Mesh(mesh) => mesh.is_empty(),
        }
    }
}

fn bounds_of(points: impl Iterator<Item = [f64; 2]>) -> Rect {
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for [x, y] in points {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    if x0 > x1 {
        return Rect::default();
    }
    let (x0, y0) = (x0.floor() as i32, y0.floor() as i32);
    let (x1, y1) = (x1.ceil() as i32, y1.ceil() as i32);
    Rect::new(x0, y0, x1 - x0, y1 - y0)
}

impl Path {
    pub fn clear(&mut self) {
        self.subpaths.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.subpaths.iter().all(|s| s.points.len() < 2)
    }

    fn current(&mut self) -> Option<&mut SubPath> {
        self.subpaths.last_mut()
    }

    fn ensure_subpath(&mut self, p: [f64; 2]) {
        if self.subpaths.is_empty() {
            self.move_to_transformed(p);
        }
    }

    fn move_to_transformed(&mut self, p: [f64; 2]) {
        self.subpaths.push(SubPath {
            points: vec![p],
            closed: false,
            rect: None,
        });
    }

    fn line_to_transformed(&mut self, p: [f64; 2]) {
        if self.subpaths.is_empty() {
            self.move_to_transformed(p);
            return;
        }
        let sub = self.current().expect("subpath exists");
        sub.rect = None;
        sub.points.push(p);
    }

    pub fn move_to(&mut self, ctm: &Affine, x: f64, y: f64) {
        let (x, y) = ctm.apply(x, y);
        self.move_to_transformed([x, y]);
    }

    pub fn line_to(&mut self, ctm: &Affine, x: f64, y: f64) {
        let (x, y) = ctm.apply(x, y);
        self.line_to_transformed([x, y]);
    }

    pub fn close(&mut self) {
        let Some(sub) = self.current() else { return };
        if sub.points.is_empty() {
            return;
        }
        sub.closed = true;
        let first = sub.points[0];
        // Per the canvas spec, a new subpath starts at the closed one's origin.
        self.move_to_transformed(first);
    }

    pub fn rect(&mut self, ctm: &Affine, x: f64, y: f64, w: f64, h: f64) {
        let corners = [
            ctm.apply(x, y),
            ctm.apply(x + w, y),
            ctm.apply(x + w, y + h),
            ctm.apply(x, y + h),
        ];
        let points: Vec<[f64; 2]> = corners.iter().map(|&(x, y)| [x, y]).collect();
        let integral = points
            .iter()
            .all(|p| p[0].fract() == 0.0 && p[1].fract() == 0.0);
        let rect = (ctm.is_axis_aligned() && integral).then(|| {
            let b = bounds_of(points.iter().copied());
            b.normalized()
        });
        self.subpaths.push(SubPath {
            points,
            closed: true,
            rect,
        });
        // A new subpath begins at (x, y).
        let (sx, sy) = ctm.apply(x, y);
        self.move_to_transformed([sx, sy]);
    }

    /// Canvas `arc(x, y, radius, start, end, anticlockwise)`.
    #[allow(clippy::too_many_arguments)]
    pub fn arc(
        &mut self,
        ctm: &Affine,
        cx: f64,
        cy: f64,
        radius: f64,
        start: f64,
        end: f64,
        anticlockwise: bool,
    ) {
        let radius = radius.abs();
        let tau = std::f64::consts::TAU;
        let mut sweep = end - start;
        if !anticlockwise {
            if sweep >= tau {
                sweep = tau;
            } else {
                sweep = sweep.rem_euclid(tau);
            }
        } else if -sweep >= tau {
            sweep = -tau;
        } else {
            sweep = -((-sweep).rem_euclid(tau));
        }

        // Flatten with segments of at most ~2px (scaled by the transform).
        let scale = (ctm.a * ctm.d - ctm.b * ctm.c).abs().sqrt().max(1e-6);
        let arc_len = sweep.abs() * radius * scale;
        let segments = ((arc_len / 2.0).ceil() as usize).clamp(1, 1024);

        let point_at = |angle: f64| {
            let (x, y) = ctm.apply(cx + radius * angle.cos(), cy + radius * angle.sin());
            [x, y]
        };
        let first = point_at(start);
        if self.subpaths.is_empty() {
            self.move_to_transformed(first);
        } else {
            self.line_to_transformed(first);
        }
        for i in 1..=segments {
            let t = start + sweep * (i as f64 / segments as f64);
            self.line_to_transformed(point_at(t));
        }
    }

    /// Canvas `bezierCurveTo`.
    pub fn curve_to(&mut self, ctm: &Affine, cp1: (f64, f64), cp2: (f64, f64), end: (f64, f64)) {
        let c1 = ctm.apply(cp1.0, cp1.1);
        let c2 = ctm.apply(cp2.0, cp2.1);
        let e = ctm.apply(end.0, end.1);
        self.ensure_subpath([c1.0, c1.1]);
        let p0 = *self
            .subpaths
            .last()
            .and_then(|s| s.points.last())
            .expect("subpath has a point");
        let p0 = (p0[0], p0[1]);
        let len = dist(p0, c1) + dist(c1, c2) + dist(c2, e);
        let segments = ((len / 3.0).ceil() as usize).clamp(1, 512);
        for i in 1..=segments {
            let t = i as f64 / segments as f64;
            let mt = 1.0 - t;
            let a = mt * mt * mt;
            let b = 3.0 * mt * mt * t;
            let c = 3.0 * mt * t * t;
            let d = t * t * t;
            let x = a * p0.0 + b * c1.0 + c * c2.0 + d * e.0;
            let y = a * p0.1 + b * c1.1 + c * c2.1 + d * e.1;
            self.line_to_transformed([x, y]);
        }
    }

    /// Bounding box of every point, used for buffer autosizing.
    pub fn bounds(&self) -> Rect {
        bounds_of(self.subpaths.iter().flat_map(|s| s.points.iter().copied()))
    }

    /// The fill geometry. Paths made only of integer axis-aligned rectangles
    /// stay as rectangles so the renderer can take its fast path.
    pub fn fill_shape(&self) -> Option<Shape> {
        let drawable: Vec<&SubPath> = self
            .subpaths
            .iter()
            .filter(|s| s.points.len() >= 2)
            .collect();
        if drawable.is_empty() {
            return None;
        }
        if drawable.iter().all(|s| s.rect.is_some()) {
            let rects: Vec<Rect> = drawable
                .iter()
                .filter_map(|s| s.rect)
                .filter(|r| !r.is_empty())
                .collect();
            return (!rects.is_empty()).then_some(Shape::Rects(rects));
        }
        let path = self.to_lyon(true)?;
        let mut buffers: VertexBuffers<[f32; 2], u32> = VertexBuffers::new();
        let options = FillOptions::non_zero().with_tolerance(0.1);
        FillTessellator::new()
            .tessellate_path(
                &path,
                &options,
                &mut BuffersBuilder::new(&mut buffers, |v: FillVertex| v.position().to_array()),
            )
            .ok()?;
        mesh_from(buffers)
    }

    pub fn stroke_shape(
        &self,
        cap: LineCap,
        join: LineJoin,
        thickness: f64,
        miter_limit: f64,
    ) -> Option<Shape> {
        let path = self.to_lyon(false)?;
        let cap = match cap {
            LineCap::Butt => lyon_tessellation::LineCap::Butt,
            LineCap::Round => lyon_tessellation::LineCap::Round,
            LineCap::Square => lyon_tessellation::LineCap::Square,
        };
        let join = match join {
            LineJoin::Bevel => lyon_tessellation::LineJoin::Bevel,
            LineJoin::Miter => lyon_tessellation::LineJoin::Miter,
            LineJoin::Round => lyon_tessellation::LineJoin::Round,
        };
        let options = StrokeOptions::default()
            .with_line_width(thickness.max(0.0) as f32)
            .with_line_cap(cap)
            .with_line_join(join)
            .with_miter_limit((miter_limit as f32).max(1.0))
            .with_tolerance(0.1);
        let mut buffers: VertexBuffers<[f32; 2], u32> = VertexBuffers::new();
        StrokeTessellator::new()
            .tessellate_path(
                &path,
                &options,
                &mut BuffersBuilder::new(&mut buffers, |v: StrokeVertex| v.position().to_array()),
            )
            .ok()?;
        mesh_from(buffers)
    }

    fn to_lyon(&self, close_all: bool) -> Option<LyonPath> {
        let mut builder = LyonPath::builder();
        let mut any = false;
        for sub in &self.subpaths {
            if sub.points.len() < 2 {
                continue;
            }
            any = true;
            let p0 = sub.points[0];
            builder.begin(point(p0[0] as f32, p0[1] as f32));
            for p in &sub.points[1..] {
                builder.line_to(point(p[0] as f32, p[1] as f32));
            }
            builder.end(close_all || sub.closed);
        }
        any.then(|| builder.build())
    }
}

fn mesh_from(buffers: VertexBuffers<[f32; 2], u32>) -> Option<Shape> {
    if buffers.indices.is_empty() {
        return None;
    }
    let bounds = bounds_of(buffers.vertices.iter().map(|v| [v[0] as f64, v[1] as f64]));
    Some(Shape::Mesh(Mesh {
        vertices: buffers.vertices,
        indices: buffers.indices,
        bounds,
    }))
}

fn dist(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_paths_stay_rectangles() {
        let mut p = Path::default();
        p.rect(&Affine::IDENTITY, 1.0, 2.0, 3.0, 4.0);
        p.rect(&Affine::translate(10.0, 0.0), 0.0, 0.0, 5.0, 5.0);
        match p.fill_shape().unwrap() {
            Shape::Rects(r) => assert_eq!(r, vec![Rect::new(1, 2, 3, 4), Rect::new(10, 0, 5, 5)]),
            other => panic!("expected rects, got {other:?}"),
        }
    }

    #[test]
    fn negative_rect_dimensions_are_normalized() {
        let mut p = Path::default();
        p.rect(&Affine::IDENTITY, 10.0, 10.0, -4.0, -2.0);
        match p.fill_shape().unwrap() {
            Shape::Rects(r) => assert_eq!(r, vec![Rect::new(6, 8, 4, 2)]),
            other => panic!("expected rects, got {other:?}"),
        }
    }

    #[test]
    fn rotated_rect_becomes_mesh() {
        let mut p = Path::default();
        let rot = Affine::from_array([0.0, 1.0, -1.0, 0.0, 0.0, 0.0]);
        p.rect(&rot, 0.0, 0.0, 10.0, 5.0);
        assert!(matches!(p.fill_shape().unwrap(), Shape::Mesh(_)));
    }

    #[test]
    fn full_circle_fill_covers_expected_area() {
        let mut p = Path::default();
        p.arc(
            &Affine::IDENTITY,
            50.0,
            50.0,
            20.0,
            0.0,
            std::f64::consts::TAU,
            false,
        );
        let Shape::Mesh(mesh) = p.fill_shape().unwrap() else {
            panic!("mesh expected")
        };
        let area: f64 = mesh
            .indices
            .chunks(3)
            .map(|t| {
                let [a, b, c] = [t[0], t[1], t[2]].map(|i| mesh.vertices[i as usize]);
                (((b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1])) as f64).abs() / 2.0
            })
            .sum();
        let expected = std::f64::consts::PI * 400.0;
        assert!((area - expected).abs() / expected < 0.01, "area {area}");
        assert_eq!(mesh.bounds, Rect::new(30, 30, 40, 40));
    }

    #[test]
    fn stroke_produces_triangles() {
        let mut p = Path::default();
        p.move_to(&Affine::IDENTITY, 0.0, 0.0);
        p.line_to(&Affine::IDENTITY, 100.0, 0.0);
        let shape = p
            .stroke_shape(LineCap::Butt, LineJoin::Miter, 4.0, 10.0)
            .unwrap();
        assert_eq!(shape.bounds(), Rect::new(0, -2, 100, 4));
    }

    #[test]
    fn close_starts_new_subpath_at_origin() {
        let mut p = Path::default();
        p.move_to(&Affine::IDENTITY, 0.0, 0.0);
        p.line_to(&Affine::IDENTITY, 10.0, 0.0);
        p.line_to(&Affine::IDENTITY, 10.0, 10.0);
        p.close();
        p.line_to(&Affine::IDENTITY, 0.0, 10.0);
        // second subpath goes from (0,0) to (0,10)
        assert_eq!(p.subpaths.len(), 2);
        assert_eq!(p.subpaths[1].points, vec![[0.0, 0.0], [0.0, 10.0]]);
    }
}
