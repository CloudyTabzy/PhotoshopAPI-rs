use nalgebra::{Matrix3, SMatrix, SVector};

use super::Point2;
use crate::core::{PsdError, Result};

/// A projective 2D transform represented by a normalized 3×3 matrix.
#[derive(Debug, Clone, PartialEq)]
pub struct Homography {
    matrix: Matrix3<f64>,
}

impl Homography {
    pub fn identity() -> Self {
        Self {
            matrix: Matrix3::identity(),
        }
    }

    pub fn from_matrix(matrix: Matrix3<f64>) -> Result<Self> {
        let determinant = matrix.determinant();
        if !matrix.iter().all(|value| value.is_finite())
            || !determinant.is_finite()
            || determinant.abs() < 1e-14
        {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "homography matrix must be finite and non-singular",
            });
        }
        Ok(Self { matrix })
    }

    /// Build from a row-major 3×3 matrix (`rows[row][column]`), for callers
    /// that do not use `nalgebra` directly.
    pub fn from_rows(rows: [[f64; 3]; 3]) -> Result<Self> {
        Self::from_matrix(Matrix3::from_fn(|row, column| rows[row][column]))
    }

    /// The matrix as row-major rows.
    pub fn to_rows(&self) -> [[f64; 3]; 3] {
        std::array::from_fn(|row| std::array::from_fn(|column| self.matrix[(row, column)]))
    }

    pub fn translation(offset: Point2) -> Result<Self> {
        Self::from_matrix(Matrix3::new(
            1.0, 0.0, offset.x, 0.0, 1.0, offset.y, 0.0, 0.0, 1.0,
        ))
    }

    pub fn scale(factors: Point2, pivot: Point2) -> Result<Self> {
        if !factors.is_finite() || !pivot.is_finite() || factors.x == 0.0 || factors.y == 0.0 {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "homography scale factors and pivot must be finite and non-zero",
            });
        }
        Self::from_matrix(Matrix3::new(
            factors.x,
            0.0,
            pivot.x * (1.0 - factors.x),
            0.0,
            factors.y,
            pivot.y * (1.0 - factors.y),
            0.0,
            0.0,
            1.0,
        ))
    }

    pub fn rotation(angle_radians: f64, pivot: Point2) -> Result<Self> {
        if !angle_radians.is_finite() || !pivot.is_finite() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "homography rotation parameters must be finite",
            });
        }
        let (sin, cos) = angle_radians.sin_cos();
        Self::from_matrix(Matrix3::new(
            cos,
            -sin,
            pivot.x - cos * pivot.x + sin * pivot.y,
            sin,
            cos,
            pivot.y - sin * pivot.x - cos * pivot.y,
            0.0,
            0.0,
            1.0,
        ))
    }

    /// Compose two transforms so `self.then(next).apply(p)` is `next(self(p))`.
    pub fn then(&self, next: &Self) -> Result<Self> {
        Self::from_matrix(next.matrix * self.matrix)
    }

    /// Solve the projective transform taking `source` to `destination`.
    ///
    /// Quads use Photoshop's internal order: top-left, top-right,
    /// bottom-left, bottom-right. The 8×8 solve fixes the final matrix entry
    /// to 1, which avoids the ill-conditioned normal-equation eigensolve used
    /// upstream. It runs on centroid-relative coordinates: a map that sends
    /// the canvas origin to infinity (its vanishing line crosses (0, 0)) has
    /// h33 = 0 in canvas coordinates and cannot be solved with that entry
    /// fixed, whereas the centroid of a valid quad always maps to a finite
    /// point.
    pub fn between(source: [Point2; 4], destination: [Point2; 4]) -> Result<Self> {
        if !source
            .iter()
            .chain(&destination)
            .all(|point| point.is_finite())
        {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "homography contains a non-finite point",
            });
        }
        let centroid =
            |quad: &[Point2; 4]| quad.iter().fold(Point2::ZERO, |sum, point| sum + *point) * 0.25;
        let (source_center, destination_center) = (centroid(&source), centroid(&destination));
        let centered = Self::between_centered(
            source.map(|point| point - source_center),
            destination.map(|point| point - destination_center),
        )?;
        let matrix = Self::translation(destination_center)?.matrix
            * centered
            * Self::translation(-source_center)?.matrix;
        // Keep h33 = 1 whenever it is meaningfully non-zero (every map whose
        // origin stays finite); otherwise scale by the largest entry.
        let largest = matrix
            .iter()
            .fold(0.0f64, |max, value| max.max(value.abs()));
        let scale = if matrix[(2, 2)].abs() > largest * 1e-12 {
            matrix[(2, 2)]
        } else {
            largest
        };
        Self::from_matrix(matrix / scale)
    }

    /// The fixed-h33 8×8 solve on (centered) quads.
    fn between_centered(source: [Point2; 4], destination: [Point2; 4]) -> Result<Matrix3<f64>> {
        let mut a = SMatrix::<f64, 8, 8>::zeros();
        let mut b = SVector::<f64, 8>::zeros();

        for i in 0..4 {
            let src = source[i];
            let dst = destination[i];

            let row = 2 * i;
            a[(row, 0)] = src.x;
            a[(row, 1)] = src.y;
            a[(row, 2)] = 1.0;
            a[(row, 6)] = -dst.x * src.x;
            a[(row, 7)] = -dst.x * src.y;
            b[row] = dst.x;

            a[(row + 1, 3)] = src.x;
            a[(row + 1, 4)] = src.y;
            a[(row + 1, 5)] = 1.0;
            a[(row + 1, 6)] = -dst.y * src.x;
            a[(row + 1, 7)] = -dst.y * src.y;
            b[row + 1] = dst.y;
        }

        let solution = a.lu().solve(&b).ok_or(PsdError::InvalidData {
            offset: 0,
            message: "homography source quad is singular",
        })?;
        let matrix = Matrix3::new(
            solution[0],
            solution[1],
            solution[2],
            solution[3],
            solution[4],
            solution[5],
            solution[6],
            solution[7],
            1.0,
        );
        if !matrix.iter().all(|value| value.is_finite()) {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "homography solve produced non-finite coefficients",
            });
        }
        // The 8×8 system can be regular while the solved map is singular
        // (e.g. a destination quad collapsed onto a line); keep the
        // non-singular invariant every other constructor enforces.
        Ok(Self::from_matrix(matrix)?.matrix)
    }

    pub fn apply(&self, point: Point2) -> Option<Point2> {
        let transformed = self.matrix * nalgebra::Vector3::new(point.x, point.y, 1.0);
        if !transformed.z.is_finite() || transformed.z.abs() < 1e-14 {
            return None;
        }
        let result = Point2::new(transformed.x / transformed.z, transformed.y / transformed.z);
        result.is_finite().then_some(result)
    }

    pub fn transform_points(&self, points: &mut [Point2]) -> Result<()> {
        for point in points {
            *point = self.apply(*point).ok_or(PsdError::InvalidData {
                offset: 0,
                message: "homography maps a point to infinity",
            })?;
        }
        Ok(())
    }

    pub fn matrix(&self) -> &Matrix3<f64> {
        &self.matrix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_quad_maps(source: [Point2; 4], destination: [Point2; 4]) {
        let homography = Homography::between(source, destination).unwrap();
        for (from, to) in source.into_iter().zip(destination) {
            let actual = homography.apply(from).unwrap();
            assert!((actual.x - to.x).abs() < 1e-8, "{actual:?} != {to:?}");
            assert!((actual.y - to.y).abs() < 1e-8, "{actual:?} != {to:?}");
        }
    }

    #[test]
    fn homography_maps_scale_translation_skew_perspective_and_combined_quads() {
        let source = [
            Point2::new(0.0, 0.0),
            Point2::new(512.0, 0.0),
            Point2::new(0.0, 256.0),
            Point2::new(512.0, 256.0),
        ];
        let cases = [
            [
                Point2::new(0.0, 0.0),
                Point2::new(256.0, 0.0),
                Point2::new(0.0, 128.0),
                Point2::new(256.0, 128.0),
            ],
            [
                Point2::new(20.0, -10.0),
                Point2::new(532.0, -10.0),
                Point2::new(20.0, 246.0),
                Point2::new(532.0, 246.0),
            ],
            [
                Point2::new(0.0, 0.0),
                Point2::new(512.0, 30.0),
                Point2::new(0.0, 256.0),
                Point2::new(512.0, 286.0),
            ],
            [
                Point2::new(40.0, 30.0),
                Point2::new(470.0, 0.0),
                Point2::new(0.0, 230.0),
                Point2::new(512.0, 256.0),
            ],
            [
                Point2::new(70.0, -20.0),
                Point2::new(500.0, 5.0),
                Point2::new(10.0, 280.0),
                Point2::new(540.0, 245.0),
            ],
        ];
        for destination in cases {
            assert_quad_maps(source, destination);
        }
    }

    #[test]
    fn homography_rejects_singular_quads() {
        let line = [Point2::new(1.0, 1.0); 4];
        assert!(Homography::between(line, line).is_err());

        // A regular source onto a destination collapsed to a line solves the
        // 8×8 system but is not an invertible transform.
        let square = [
            Point2::new(0.0, 0.0),
            Point2::new(1.0, 0.0),
            Point2::new(0.0, 1.0),
            Point2::new(1.0, 1.0),
        ];
        let flat = [
            Point2::new(0.0, 0.0),
            Point2::new(1.0, 0.0),
            Point2::new(0.0, 0.0),
            Point2::new(1.0, 0.0),
        ];
        assert!(Homography::between(square, flat).is_err());
    }

    #[test]
    fn homography_solves_maps_that_send_the_canvas_origin_to_infinity() {
        // h33 = 0: the vanishing line x + y = 0 passes through (0, 0), so a
        // solve with h33 fixed to 1 in canvas coordinates has no solution.
        let truth =
            Homography::from_rows([[1.0, 0.0, 5.0], [0.0, 1.0, 3.0], [0.01, 0.01, 0.0]]).unwrap();
        let source = [
            Point2::new(10.0, 10.0),
            Point2::new(20.0, 10.0),
            Point2::new(10.0, 20.0),
            Point2::new(20.0, 20.0),
        ];
        let mut destination = source;
        truth.transform_points(&mut destination).unwrap();
        let solved = Homography::between(source, destination).unwrap();
        for point in [Point2::new(12.0, 17.0), Point2::new(19.5, 10.5), source[3]] {
            assert!(solved
                .apply(point)
                .unwrap()
                .approx_eq(truth.apply(point).unwrap(), 1e-9));
        }
        assert_eq!(solved.apply(Point2::ZERO), None);

        // Large canvas coordinates keep sub-nanopixel corner accuracy.
        let far = source.map(|point| point * 2000.0 + Point2::new(20_000.0, 15_000.0));
        let skewed = [
            Point2::new(40_100.0, 35_050.0),
            Point2::new(59_800.0, 34_900.0),
            Point2::new(39_900.0, 55_100.0),
            Point2::new(60_200.0, 54_950.0),
        ];
        assert_quad_maps(far, skewed);
    }

    #[test]
    fn solved_homographies_are_projective_and_compose() {
        let a = [
            Point2::new(0.0, 0.0),
            Point2::new(512.0, 0.0),
            Point2::new(0.0, 256.0),
            Point2::new(512.0, 256.0),
        ];
        let b = [
            Point2::new(40.0, 30.0),
            Point2::new(470.0, 0.0),
            Point2::new(0.0, 230.0),
            Point2::new(512.0, 256.0),
        ];
        let c = [
            Point2::new(-20.0, 10.0),
            Point2::new(300.0, -5.0),
            Point2::new(15.0, 190.0),
            Point2::new(280.0, 210.0),
        ];
        let ab = Homography::between(a, b).unwrap();
        let bc = Homography::between(b, c).unwrap();
        let ac = Homography::between(a, c).unwrap();
        let chained = ab.then(&bc).unwrap();
        let cross =
            |p: Point2, q: Point2, r: Point2| (q.x - p.x) * (r.y - p.y) - (q.y - p.y) * (r.x - p.x);
        for (x, y) in [(0.25, 0.5), (0.5, 0.5), (0.9, 0.1), (0.0, 0.75)] {
            let point = Point2::new(512.0 * x, 256.0 * y);
            // Composition matches the direct solve (up to projective scale).
            assert!(chained
                .apply(point)
                .unwrap()
                .approx_eq(ac.apply(point).unwrap(), 1e-7));
            // Lines stay lines: a point on the top edge lands on the image of
            // that edge.
            let edge = Point2::new(512.0 * x, 0.0);
            let mapped = ab.apply(edge).unwrap();
            assert!(cross(b[0], b[1], mapped).abs() < 1e-6, "{mapped:?}");
        }
    }

    #[test]
    fn affine_transforms_compose_around_pivots() {
        let point = Point2::new(2.0, 3.0);
        let pivot = Point2::new(1.0, 1.0);
        let transform = Homography::rotation(std::f64::consts::FRAC_PI_2, pivot)
            .unwrap()
            .then(&Homography::scale(Point2::new(2.0, 3.0), pivot).unwrap())
            .unwrap()
            .then(&Homography::translation(Point2::new(4.0, -2.0)).unwrap())
            .unwrap();
        let actual = transform.apply(point).unwrap();
        assert!((actual.x - 1.0).abs() < 1e-10);
        assert!((actual.y - 2.0).abs() < 1e-10);
        assert!(Homography::scale(Point2::new(0.0, 1.0), pivot).is_err());
        assert!(Homography::translation(Point2::new(f64::INFINITY, 0.0)).is_err());
        assert!(Homography::from_matrix(Matrix3::zeros()).is_err());
    }
}
