use crate::core::{PsdError, Result};

use super::{MeshVertex, Point2, QuadMesh};

/// A scanline-ordered grid of one or more adjoining cubic Bézier patches.
#[derive(Debug, Clone, PartialEq)]
pub struct BezierSurface {
    grid_width: usize,
    grid_height: usize,
    patches_x: usize,
    patches_y: usize,
    patches: Vec<[Point2; 16]>,
    slices_x: Option<Vec<f64>>,
    slices_y: Option<Vec<f64>>,
}

impl BezierSurface {
    pub fn new(points: &[Point2], grid_width: usize, grid_height: usize) -> Result<Self> {
        Self::build(points, grid_width, grid_height, None, None)
    }

    /// Construct a quilt surface whose source-UV breakpoints are not uniform.
    /// Slice values are source coordinates, as stored in Photoshop descriptors.
    pub fn with_quilt_slices(
        points: &[Point2],
        grid_width: usize,
        grid_height: usize,
        slices_x: &[f64],
        slices_y: &[f64],
    ) -> Result<Self> {
        let normalize = |slices: &[f64]| -> Result<Vec<f64>> {
            if slices.len() < 2 || !slices.iter().all(|value| value.is_finite()) {
                return Err(PsdError::InvalidData {
                    offset: 0,
                    message: "quilt slices require at least two finite values",
                });
            }
            let first = slices[0];
            let span = slices[slices.len() - 1] - first;
            if span <= 0.0 || slices.windows(2).any(|pair| pair[1] <= pair[0]) {
                return Err(PsdError::InvalidData {
                    offset: 0,
                    message: "quilt slices must be strictly increasing",
                });
            }
            Ok(slices
                .iter()
                .map(|value| ((value - first) / span).clamp(0.0, 1.0))
                .collect())
        };
        Self::build(
            points,
            grid_width,
            grid_height,
            Some(normalize(slices_x)?),
            Some(normalize(slices_y)?),
        )
    }

    fn build(
        points: &[Point2],
        grid_width: usize,
        grid_height: usize,
        slices_x: Option<Vec<f64>>,
        slices_y: Option<Vec<f64>>,
    ) -> Result<Self> {
        if grid_width < 4
            || grid_height < 4
            || !(grid_width - 4).is_multiple_of(3)
            || !(grid_height - 4).is_multiple_of(3)
            || grid_width.checked_mul(grid_height) != Some(points.len())
            || !points.iter().all(|point| point.is_finite())
        {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "invalid cubic Bézier control-point grid",
            });
        }
        if slices_x.is_some() != slices_y.is_some() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "quilt slices must be provided for both axes",
            });
        }

        let patches_x = 1 + (grid_width - 4) / 3;
        let patches_y = 1 + (grid_height - 4) / 3;
        let mut patches = Vec::with_capacity(patches_x * patches_y);
        for patch_y in 0..patches_y {
            for patch_x in 0..patches_x {
                let mut patch = [Point2::ZERO; 16];
                for y in 0..4 {
                    for x in 0..4 {
                        let index = (patch_y * 3 + y) * grid_width + patch_x * 3 + x;
                        patch[y * 4 + x] = points[index];
                    }
                }
                patches.push(patch);
            }
        }

        Ok(Self {
            grid_width,
            grid_height,
            patches_x,
            patches_y,
            patches,
            slices_x,
            slices_y,
        })
    }

    pub fn grid_width(&self) -> usize {
        self.grid_width
    }

    pub fn grid_height(&self) -> usize {
        self.grid_height
    }

    pub fn patch_dimensions(&self) -> (usize, usize) {
        (self.patches_x, self.patches_y)
    }

    pub fn patches(&self) -> &[[Point2; 16]] {
        &self.patches
    }

    /// Reverse-map UV through Photoshop's quilt slice spacing.
    pub fn bias_uv(&self, u: f64, v: f64) -> Point2 {
        match (&self.slices_x, &self.slices_y) {
            (Some(slices_x), Some(slices_y)) => Point2::new(
                reverse_lerp_slices(u, slices_x),
                reverse_lerp_slices(v, slices_y),
            ),
            _ => Point2::new(u, v),
        }
    }

    /// Evaluate the surface in world coordinates for normalized input UV.
    pub fn evaluate(&self, u: f64, v: f64) -> Point2 {
        let u = u.clamp(0.0, 1.0);
        let v = v.clamp(0.0, 1.0);
        let patch_x = ((u * self.patches_x as f64).floor() as usize).min(self.patches_x - 1);
        let patch_y = ((v * self.patches_y as f64).floor() as usize).min(self.patches_y - 1);
        let local_u = (u * self.patches_x as f64 - patch_x as f64).clamp(0.0, 1.0);
        let local_v = (v * self.patches_y as f64 - patch_y as f64).clamp(0.0, 1.0);
        evaluate_patch(
            &self.patches[patch_y * self.patches_x + patch_x],
            local_u,
            local_v,
        )
    }

    /// Tessellate into quads carrying equally-spaced texture UVs.
    pub fn mesh(&self, divisions_x: usize, divisions_y: usize) -> Result<QuadMesh> {
        self.mesh_with_origin(divisions_x, divisions_y, false)
    }

    /// Tessellate the surface and optionally translate its minimum corner to
    /// the origin. The point offset can be recovered from the original surface
    /// bounds before translation when document placement is needed.
    pub fn mesh_with_origin(
        &self,
        divisions_x: usize,
        divisions_y: usize,
        move_to_zero: bool,
    ) -> Result<QuadMesh> {
        if divisions_x < 2 || divisions_y < 2 {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "Bézier mesh requires at least two divisions per axis",
            });
        }
        let mut vertices = Vec::with_capacity(divisions_x * divisions_y);
        for y in 0..divisions_y {
            let v = y as f64 / (divisions_y - 1) as f64;
            for x in 0..divisions_x {
                let u = x as f64 / (divisions_x - 1) as f64;
                let biased = self.bias_uv(u, v);
                vertices.push(MeshVertex {
                    point: self.evaluate(biased.x, biased.y),
                    uv: Point2::new(u, v),
                });
            }
        }
        let mut mesh = QuadMesh::new(vertices, divisions_x, divisions_y)?;
        if move_to_zero {
            let offset = -mesh.bounds().min;
            mesh.offset(offset)?;
        }
        Ok(mesh)
    }
}

fn reverse_lerp_slices(value: f64, slices: &[f64]) -> f64 {
    let value = value.clamp(0.0, 1.0);
    let upper = slices
        .partition_point(|slice| *slice <= value)
        .clamp(1, slices.len() - 1);
    let lower = upper - 1;
    let t = (value - slices[lower]) / (slices[upper] - slices[lower]);
    let count = (slices.len() - 1) as f64;
    ((lower as f64 + t) / count).clamp(0.0, 1.0)
}

fn evaluate_curve(points: [Point2; 4], t: f64) -> Point2 {
    let a = points[0].lerp(points[1], t);
    let b = points[1].lerp(points[2], t);
    let c = points[2].lerp(points[3], t);
    a.lerp(b, t).lerp(b.lerp(c, t), t)
}

fn evaluate_patch(patch: &[Point2; 16], u: f64, v: f64) -> Point2 {
    let rows = [
        evaluate_curve([patch[0], patch[1], patch[2], patch[3]], u),
        evaluate_curve([patch[4], patch[5], patch[6], patch[7]], u),
        evaluate_curve([patch[8], patch[9], patch[10], patch[11]], u),
        evaluate_curve([patch[12], patch[13], patch[14], patch[15]], u),
    ];
    evaluate_curve(rows, v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane(width: usize, height: usize) -> Vec<Point2> {
        (0..height)
            .flat_map(|y| {
                (0..width).map(move |x| {
                    Point2::new(
                        x as f64 / (width - 1) as f64,
                        y as f64 / (height - 1) as f64,
                    )
                })
            })
            .collect()
    }

    #[test]
    fn regular_patch_evaluates_to_the_unit_square() {
        let surface = BezierSurface::new(&plane(4, 4), 4, 4).unwrap();
        assert_eq!(surface.evaluate(0.0, 0.0), Point2::new(0.0, 0.0));
        let center = surface.evaluate(0.37, 0.61);
        assert!((center.x - 0.37).abs() < 1e-12);
        assert!((center.y - 0.61).abs() < 1e-12);
        assert_eq!(surface.evaluate(1.0, 1.0), Point2::new(1.0, 1.0));
    }

    #[test]
    fn quilt_slice_bias_remaps_nonuniform_source_intervals() {
        let surface = BezierSurface::with_quilt_slices(
            &plane(7, 4),
            7,
            4,
            &[-0.6, 100.0, 400.0],
            &[-0.6, 200.0],
        )
        .unwrap();
        let at_slice = surface.bias_uv(100.6 / 400.6, 0.5);
        assert!((at_slice.x - 0.5).abs() < 1e-12);
        assert!((at_slice.y - 0.5).abs() < 1e-12);
        let mesh = surface.mesh(5, 3).unwrap();
        let sampled_world = surface.evaluate(surface.bias_uv(0.5, 0.5).x, 0.5);
        let sampled_uv = mesh.uv_at(sampled_world).unwrap();
        assert!((sampled_uv.x - 0.5).abs() < 1e-12);
        assert!((sampled_uv.y - 0.5).abs() < 1e-12);
    }

    #[test]
    fn mesh_can_be_translated_to_zero_without_changing_surface_bounds() {
        let points = (0..4)
            .flat_map(|y| (0..4).map(move |x| Point2::new(10.0 + x as f64, 20.0 + y as f64)))
            .collect::<Vec<_>>();
        let surface = BezierSurface::new(&points, 4, 4).unwrap();
        let world_mesh = surface.mesh(5, 5).unwrap();
        let local_mesh = surface.mesh_with_origin(5, 5, true).unwrap();
        assert_eq!(world_mesh.bounds().min, Point2::new(10.0, 20.0));
        assert_eq!(local_mesh.bounds().min, Point2::ZERO);
        assert_eq!(world_mesh.vertices()[0].uv, local_mesh.vertices()[0].uv);
    }

    #[test]
    fn invalid_patch_dimensions_and_duplicate_slices_are_rejected() {
        assert!(BezierSurface::new(&plane(5, 4), 5, 4).is_err());
        assert!(
            BezierSurface::with_quilt_slices(&plane(4, 4), 4, 4, &[0.0, 0.0], &[0.0, 1.0]).is_err()
        );
    }
}
