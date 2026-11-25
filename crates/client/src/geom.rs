//! Basic geometry: integer rectangles and 2D affine transforms.

/// Integer rectangle in layer pixel coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    /// Normalizes negative width/height (canvas `rect()` accepts them).
    pub fn normalized(self) -> Self {
        let (x, w) = if self.w < 0 {
            (self.x + self.w, -self.w)
        } else {
            (self.x, self.w)
        };
        let (y, h) = if self.h < 0 {
            (self.y + self.h, -self.h)
        } else {
            (self.y, self.h)
        };
        Self { x, y, w, h }
    }

    pub fn intersect(&self, other: &Rect) -> Rect {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = self.right().min(other.right());
        let y1 = self.bottom().min(other.bottom());
        Rect::new(x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
    }

    pub fn union(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = self.right().max(other.right());
        let y1 = self.bottom().max(other.bottom());
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }

    pub fn overlaps(&self, other: &Rect) -> bool {
        !self.intersect(other).is_empty()
    }

    /// Parts of `self` not covered by `hole` (at most four rectangles).
    pub fn subtract(&self, hole: &Rect) -> impl Iterator<Item = Rect> {
        let hole = self.intersect(hole);
        let parts = if hole.is_empty() {
            [*self, Rect::default(), Rect::default(), Rect::default()]
        } else {
            [
                // Band above the hole (full width).
                Rect::new(self.x, self.y, self.w, hole.y - self.y),
                // Band below the hole (full width).
                Rect::new(self.x, hole.bottom(), self.w, self.bottom() - hole.bottom()),
                // Left and right of the hole, limited to the hole's rows.
                Rect::new(self.x, hole.y, hole.x - self.x, hole.h),
                Rect::new(hole.right(), hole.y, self.right() - hole.right(), hole.h),
            ]
        };
        parts.into_iter().filter(|r| !r.is_empty())
    }
}

/// 2D affine transform using the canvas `setTransform(a, b, c, d, e, f)`
/// convention: `x' = a*x + c*y + e`, `y' = b*x + d*y + f`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Affine {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Default for Affine {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Affine {
    pub const IDENTITY: Affine = Affine {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    pub fn from_array(m: [f64; 6]) -> Self {
        Self {
            a: m[0],
            b: m[1],
            c: m[2],
            d: m[3],
            e: m[4],
            f: m[5],
        }
    }

    pub fn translate(x: f64, y: f64) -> Self {
        Self {
            e: x,
            f: y,
            ..Self::IDENTITY
        }
    }

    pub fn scale(sx: f64, sy: f64) -> Self {
        Self {
            a: sx,
            d: sy,
            ..Self::IDENTITY
        }
    }

    /// `self * rhs`: applies `rhs` first, then `self` (canvas `transform()`
    /// semantics when `self` is the current matrix).
    pub fn mul(&self, rhs: &Affine) -> Affine {
        Affine {
            a: self.a * rhs.a + self.c * rhs.b,
            b: self.b * rhs.a + self.d * rhs.b,
            c: self.a * rhs.c + self.c * rhs.d,
            d: self.b * rhs.c + self.d * rhs.d,
            e: self.a * rhs.e + self.c * rhs.f + self.e,
            f: self.b * rhs.e + self.d * rhs.f + self.f,
        }
    }

    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    pub fn invert(&self) -> Option<Affine> {
        let det = self.a * self.d - self.b * self.c;
        if det.abs() < 1e-12 {
            return None;
        }
        let inv = 1.0 / det;
        Some(Affine {
            a: self.d * inv,
            b: -self.b * inv,
            c: -self.c * inv,
            d: self.a * inv,
            e: (self.c * self.f - self.d * self.e) * inv,
            f: (self.b * self.e - self.a * self.f) * inv,
        })
    }

    pub fn is_identity(&self) -> bool {
        *self == Self::IDENTITY
    }

    /// Only translation (identity linear part).
    pub fn is_translation(&self) -> bool {
        self.a == 1.0 && self.b == 0.0 && self.c == 0.0 && self.d == 1.0
    }

    /// Maps axis-aligned rectangles to axis-aligned rectangles.
    pub fn is_axis_aligned(&self) -> bool {
        self.b == 0.0 && self.c == 0.0
    }

    /// Column-major 3x3 matrix for GLSL `mat3`.
    pub fn to_mat3(&self) -> [f32; 9] {
        [
            self.a as f32,
            self.b as f32,
            0.0,
            self.c as f32,
            self.d as f32,
            0.0,
            self.e as f32,
            self.f as f32,
            1.0,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtract_covers_complement_exactly() {
        let outer = Rect::new(0, 0, 10, 10);
        let hole = Rect::new(2, 3, 4, 5);
        let parts: Vec<_> = outer.subtract(&hole).collect();
        let area: i32 = parts.iter().map(|r| r.w * r.h).sum();
        assert_eq!(area, 100 - 20);
        for (i, a) in parts.iter().enumerate() {
            assert!(!a.overlaps(&hole));
            for b in &parts[i + 1..] {
                assert!(!a.overlaps(b));
            }
        }
    }

    #[test]
    fn subtract_without_overlap_returns_self() {
        let outer = Rect::new(0, 0, 10, 10);
        let parts: Vec<_> = outer.subtract(&Rect::new(20, 20, 5, 5)).collect();
        assert_eq!(parts, vec![outer]);
    }

    #[test]
    fn affine_compose_and_invert() {
        let m = Affine::translate(10.0, 5.0).mul(&Affine::scale(2.0, 3.0));
        assert_eq!(m.apply(1.0, 1.0), (12.0, 8.0));
        let inv = m.invert().unwrap();
        let (x, y) = inv.apply(12.0, 8.0);
        assert!((x - 1.0).abs() < 1e-9 && (y - 1.0).abs() < 1e-9);
    }
}
