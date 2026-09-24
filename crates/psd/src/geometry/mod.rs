//! Geometry used by the smart-object warp and render engine.
//!
//! These are owned Rust values rather than wrappers around the upstream
//! Eigen/Eigen-backed geometry classes. The mesh keeps a compact uniform
//! spatial index; the upstream uses an octree for the same face lookup.

mod bezier;
mod homography;
mod mesh;

pub use bezier::BezierSurface;
pub use homography::Homography;
pub use mesh::{MeshVertex, QuadMesh};

/// A point in two-dimensional document or source-image coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Point2 {
    pub x: f64,
    pub y: f64,
}

impl Point2 {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn lerp(self, other: Self, t: f64) -> Self {
        Self::new(
            self.x + (other.x - self.x) * t,
            self.y + (other.y - self.y) * t,
        )
    }

    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }

    pub fn distance(self, other: Self) -> f64 {
        (self - other).length()
    }

    pub fn dot(self, other: Self) -> f64 {
        self.x * other.x + self.y * other.y
    }

    pub fn cross(self, other: Self) -> f64 {
        self.x * other.y - self.y * other.x
    }

    pub fn length(self) -> f64 {
        self.dot(self).sqrt()
    }

    pub fn component_mul(self, other: Self) -> Self {
        Self::new(self.x * other.x, self.y * other.y)
    }

    pub fn component_div(self, other: Self) -> Option<Self> {
        if other.x == 0.0 || other.y == 0.0 {
            None
        } else {
            Some(Self::new(self.x / other.x, self.y / other.y))
        }
    }

    pub fn approx_eq(self, other: Self, epsilon: f64) -> bool {
        (self.x - other.x).abs() <= epsilon && (self.y - other.y).abs() <= epsilon
    }
}

impl std::ops::Add for Point2 {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self::new(self.x + rhs.x, self.y + rhs.y)
    }
}

impl std::ops::Sub for Point2 {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self::new(self.x - rhs.x, self.y - rhs.y)
    }
}

impl std::ops::Mul<f64> for Point2 {
    type Output = Self;

    fn mul(self, rhs: f64) -> Self::Output {
        Self::new(self.x * rhs, self.y * rhs)
    }
}

impl std::ops::Div<f64> for Point2 {
    type Output = Self;

    fn div(self, rhs: f64) -> Self::Output {
        Self::new(self.x / rhs, self.y / rhs)
    }
}

impl std::ops::Neg for Point2 {
    type Output = Self;

    fn neg(self) -> Self::Output {
        Self::new(-self.x, -self.y)
    }
}

impl std::ops::AddAssign for Point2 {
    fn add_assign(&mut self, rhs: Self) {
        self.x += rhs.x;
        self.y += rhs.y;
    }
}

impl std::ops::SubAssign for Point2 {
    fn sub_assign(&mut self, rhs: Self) {
        self.x -= rhs.x;
        self.y -= rhs.y;
    }
}

/// Axis-aligned bounds in the same coordinate system as [`Point2`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BoundingBox {
    pub min: Point2,
    pub max: Point2,
}

impl BoundingBox {
    pub const fn new(min: Point2, max: Point2) -> Self {
        Self { min, max }
    }

    pub fn from_points(points: &[Point2]) -> Option<Self> {
        Self::from_points_iter(points.iter().copied())
    }

    /// Same as [`from_points`](Self::from_points) without materializing the
    /// points (mesh construction passes a mapped iterator).
    pub fn from_points_iter(points: impl IntoIterator<Item = Point2>) -> Option<Self> {
        let mut points = points.into_iter();
        let first = points.next()?;
        if !first.is_finite() {
            return None;
        }
        let mut bounds = Self::new(first, first);
        for point in points {
            if !point.is_finite() {
                return None;
            }
            bounds.min.x = bounds.min.x.min(point.x);
            bounds.min.y = bounds.min.y.min(point.y);
            bounds.max.x = bounds.max.x.max(point.x);
            bounds.max.y = bounds.max.y.max(point.y);
        }
        Some(bounds)
    }

    pub fn width(self) -> f64 {
        self.max.x - self.min.x
    }

    pub fn height(self) -> f64 {
        self.max.y - self.min.y
    }

    pub fn size(self) -> Point2 {
        Point2::new(self.width(), self.height())
    }

    pub fn center(self) -> Point2 {
        Point2::new(
            (self.min.x + self.max.x) * 0.5,
            (self.min.y + self.max.y) * 0.5,
        )
    }

    pub fn contains(self, point: Point2) -> bool {
        point.x >= self.min.x
            && point.x <= self.max.x
            && point.y >= self.min.y
            && point.y <= self.max.y
    }

    /// Whether the boxes overlap, including a shared edge or corner.
    pub fn intersects(self, other: Self) -> bool {
        !(other.max.x < self.min.x
            || other.min.x > self.max.x
            || other.max.y < self.min.y
            || other.min.y > self.max.y)
    }

    pub fn intersection(self, other: Self) -> Option<Self> {
        let min = Point2::new(self.min.x.max(other.min.x), self.min.y.max(other.min.y));
        let max = Point2::new(self.max.x.min(other.max.x), self.max.y.min(other.max.y));
        (min.x <= max.x && min.y <= max.y).then_some(Self::new(min, max))
    }

    /// Return corners ordered top-left, top-right, bottom-left, bottom-right.
    pub fn as_quad(self) -> [Point2; 4] {
        [
            self.min,
            Point2::new(self.max.x, self.min.y),
            Point2::new(self.min.x, self.max.y),
            self.max,
        ]
    }

    pub fn offset(&mut self, offset: Point2) {
        self.min += offset;
        self.max += offset;
    }

    /// Expand by `amount`; a negative amount contracts the bounds.
    pub fn pad(&mut self, amount: f64) {
        self.min.x -= amount;
        self.min.y -= amount;
        self.max.x += amount;
        self.max.y += amount;
    }

    pub fn include(&mut self, other: Self) {
        self.min.x = self.min.x.min(other.min.x);
        self.min.y = self.min.y.min(other.min.y);
        self.max.x = self.max.x.max(other.max.x);
        self.max.y = self.max.y.max(other.max.y);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_support_component_math_and_approximate_comparison() {
        let point = Point2::new(3.0, 4.0);
        assert_eq!(point.length(), 5.0);
        assert_eq!(point.distance(Point2::ZERO), 5.0);
        assert_eq!(point.dot(Point2::new(2.0, 1.0)), 10.0);
        assert_eq!(
            point.component_mul(Point2::new(2.0, 3.0)),
            Point2::new(6.0, 12.0)
        );
        assert_eq!(
            point.component_div(Point2::new(3.0, 2.0)),
            Some(Point2::new(1.0, 2.0))
        );
        assert_eq!(point.component_div(Point2::new(0.0, 1.0)), None);
        assert!(point.approx_eq(Point2::new(3.0 + 1e-8, 4.0), 1e-7));
    }

    #[test]
    fn bounds_intersect_pad_offset_and_emit_photoshop_corner_order() {
        let mut bounds = BoundingBox::new(Point2::new(1.0, 2.0), Point2::new(5.0, 8.0));
        assert!(bounds.intersects(BoundingBox::new(
            Point2::new(5.0, 4.0),
            Point2::new(6.0, 9.0)
        )));
        assert_eq!(
            bounds.intersection(BoundingBox::new(
                Point2::new(4.0, 6.0),
                Point2::new(9.0, 10.0)
            )),
            Some(BoundingBox::new(
                Point2::new(4.0, 6.0),
                Point2::new(5.0, 8.0)
            ))
        );
        assert_eq!(
            bounds.as_quad(),
            [
                Point2::new(1.0, 2.0),
                Point2::new(5.0, 2.0),
                Point2::new(1.0, 8.0),
                Point2::new(5.0, 8.0),
            ]
        );
        bounds.pad(1.0);
        bounds.offset(Point2::new(-1.0, 2.0));
        assert_eq!(
            bounds,
            BoundingBox::new(Point2::new(-1.0, 3.0), Point2::new(5.0, 11.0))
        );
    }
}
