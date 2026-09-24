use crate::core::{PsdError, Result};

use super::{BoundingBox, Homography, Point2};

/// A mesh vertex with its world-space location and source UV coordinate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeshVertex {
    pub point: Point2,
    pub uv: Point2,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct MeshFace {
    vertices: [MeshVertex; 4],
    bounds: BoundingBox,
}

/// A regular quad tessellation with a uniform 2D face index.
///
/// Upstream stores the same quads in an octree. A uniform spatial index is a
/// better fit for the regularly tessellated Bézier surface and makes queries
/// deterministic without a recursive pointer tree.
#[derive(Debug, Clone, PartialEq)]
pub struct QuadMesh {
    width: usize,
    height: usize,
    vertices: Vec<MeshVertex>,
    faces: Vec<MeshFace>,
    bounds: BoundingBox,
    bins_x: usize,
    bins_y: usize,
    bins: Vec<Vec<usize>>,
}

impl QuadMesh {
    pub fn from_points(points: &[Point2], width: usize, height: usize) -> Result<Self> {
        if width < 2 || height < 2 || width.checked_mul(height) != Some(points.len()) {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "mesh point count does not match its dimensions",
            });
        }
        let vertices = points
            .iter()
            .enumerate()
            .map(|(index, point)| {
                let x = index % width;
                let y = index / width;
                MeshVertex {
                    point: *point,
                    uv: Point2::new(
                        x as f64 / (width - 1) as f64,
                        y as f64 / (height - 1) as f64,
                    ),
                }
            })
            .collect();
        Self::new(vertices, width, height)
    }

    pub fn new(vertices: Vec<MeshVertex>, width: usize, height: usize) -> Result<Self> {
        if width < 2
            || height < 2
            || width.checked_mul(height) != Some(vertices.len())
            || !vertices
                .iter()
                .all(|vertex| vertex.point.is_finite() && vertex.uv.is_finite())
        {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "invalid quad mesh vertices or dimensions",
            });
        }
        let bounds = BoundingBox::from_points_iter(vertices.iter().map(|v| v.point)).ok_or(
            PsdError::InvalidData {
                offset: 0,
                message: "quad mesh has no finite vertices",
            },
        )?;
        if bounds.width() <= 0.0 || bounds.height() <= 0.0 {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "quad mesh has degenerate bounds",
            });
        }

        let mut faces = Vec::with_capacity((width - 1) * (height - 1));
        for y in 0..height - 1 {
            for x in 0..width - 1 {
                let top_left = y * width + x;
                let top_right = top_left + 1;
                let bottom_left = top_left + width;
                let bottom_right = bottom_left + 1;
                let corners = [
                    vertices[top_left],
                    vertices[top_right],
                    vertices[bottom_left],
                    vertices[bottom_right],
                ];
                let face_bounds = BoundingBox::from_points(&corners.map(|vertex| vertex.point))
                    .expect("validated mesh vertices are finite");
                faces.push(MeshFace {
                    vertices: corners,
                    bounds: face_bounds,
                });
            }
        }

        let face_count = faces.len().max(1) as f64;
        let aspect = (bounds.width() / bounds.height()).clamp(1.0 / 4096.0, 4096.0);
        let bins_x = ((face_count.sqrt() * aspect.sqrt()).ceil() as usize).clamp(1, 4096);
        let bins_y = ((face_count.sqrt() / aspect.sqrt()).ceil() as usize).clamp(1, 4096);
        let mut mesh = Self {
            width,
            height,
            vertices,
            faces,
            bounds,
            bins_x,
            bins_y,
            bins: vec![Vec::new(); bins_x * bins_y],
        };
        mesh.index_faces();
        Ok(mesh)
    }

    pub fn bounds(&self) -> BoundingBox {
        self.bounds
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn vertices(&self) -> &[MeshVertex] {
        &self.vertices
    }

    pub fn vertex(&self, index: usize) -> Option<MeshVertex> {
        self.vertices.get(index).copied()
    }

    pub fn face_count(&self) -> usize {
        self.faces.len()
    }

    /// Return a face's corners in scanline order: top-left, top-right,
    /// bottom-left, bottom-right.
    pub fn face(&self, index: usize) -> Option<[MeshVertex; 4]> {
        self.faces.get(index).map(|face| face.vertices)
    }

    /// Replace mesh geometry while preserving its grid topology and UV
    /// coordinates. Rebuilds face bounds and the spatial index atomically.
    pub fn set_vertices(&mut self, vertices: Vec<MeshVertex>) -> Result<()> {
        if vertices.len() != self.vertices.len() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "replacement mesh vertices must preserve the grid dimensions",
            });
        }
        *self = Self::new(vertices, self.width, self.height)?;
        Ok(())
    }

    /// Translate every world-space vertex while preserving texture UVs.
    pub fn offset(&mut self, offset: Point2) -> Result<()> {
        let vertices = self
            .vertices
            .iter()
            .map(|vertex| MeshVertex {
                point: vertex.point + offset,
                uv: vertex.uv,
            })
            .collect();
        *self = Self::new(vertices, self.width, self.height)?;
        Ok(())
    }

    /// Transform world-space vertices while preserving texture UVs.
    pub fn transform(&mut self, transform: &Homography) -> Result<()> {
        let mut vertices = self.vertices.clone();
        for vertex in &mut vertices {
            vertex.point = transform.apply(vertex.point).ok_or(PsdError::InvalidData {
                offset: 0,
                message: "mesh transform maps a vertex to infinity",
            })?;
        }
        *self = Self::new(vertices, self.width, self.height)?;
        Ok(())
    }

    /// Return texture UV at a world-space point, or `None` outside the mesh.
    pub fn uv_at(&self, point: Point2) -> Option<Point2> {
        if !self.bounds.contains(point) {
            return None;
        }
        let x = self.bin_coordinate(point.x, self.bounds.min.x, self.bounds.width(), self.bins_x);
        let y = self.bin_coordinate(
            point.y,
            self.bounds.min.y,
            self.bounds.height(),
            self.bins_y,
        );
        for &face_index in &self.bins[y * self.bins_x + x] {
            let face = self.faces[face_index];
            if !face.bounds.contains(point) {
                continue;
            }
            let [top_left, top_right, bottom_left, bottom_right] = face.vertices;
            if let Some(weights) =
                barycentric(point, top_left.point, top_right.point, bottom_right.point)
            {
                return Some(interpolate_uv([top_left, top_right, bottom_right], weights));
            }
            if let Some(weights) =
                barycentric(point, top_left.point, bottom_left.point, bottom_right.point)
            {
                return Some(interpolate_uv(
                    [top_left, bottom_left, bottom_right],
                    weights,
                ));
            }
        }
        None
    }

    fn bin_coordinate(&self, value: f64, min: f64, span: f64, bins: usize) -> usize {
        (((value - min) / span * bins as f64).floor() as usize).min(bins - 1)
    }

    fn index_faces(&mut self) {
        for (face_index, face) in self.faces.iter().enumerate() {
            let min_x = self.bin_coordinate(
                face.bounds.min.x,
                self.bounds.min.x,
                self.bounds.width(),
                self.bins_x,
            );
            let max_x = self.bin_coordinate(
                face.bounds.max.x,
                self.bounds.min.x,
                self.bounds.width(),
                self.bins_x,
            );
            let min_y = self.bin_coordinate(
                face.bounds.min.y,
                self.bounds.min.y,
                self.bounds.height(),
                self.bins_y,
            );
            let max_y = self.bin_coordinate(
                face.bounds.max.y,
                self.bounds.min.y,
                self.bounds.height(),
                self.bins_y,
            );
            for y in min_y..=max_y {
                for x in min_x..=max_x {
                    self.bins[y * self.bins_x + x].push(face_index);
                }
            }
        }
    }
}

fn cross(a: Point2, b: Point2, c: Point2) -> f64 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

fn barycentric(point: Point2, a: Point2, b: Point2, c: Point2) -> Option<[f64; 3]> {
    let denominator = cross(a, b, c);
    if denominator.abs() < 1e-14 {
        return None;
    }
    let wa = cross(point, b, c) / denominator;
    let wb = cross(a, point, c) / denominator;
    let wc = cross(a, b, point) / denominator;
    let epsilon = 1e-10;
    (wa >= -epsilon && wb >= -epsilon && wc >= -epsilon).then_some([wa, wb, wc])
}

fn interpolate_uv(vertices: [MeshVertex; 3], weights: [f64; 3]) -> Point2 {
    vertices[0].uv * weights[0] + vertices[1].uv * weights[1] + vertices[2].uv * weights[2]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regular_quad_mesh_interpolates_uv_and_rejects_points_outside() {
        let points = [
            Point2::new(10.0, 20.0),
            Point2::new(20.0, 20.0),
            Point2::new(10.0, 30.0),
            Point2::new(20.0, 30.0),
        ];
        let mesh = QuadMesh::from_points(&points, 2, 2).unwrap();
        let uv = mesh.uv_at(Point2::new(14.0, 27.0)).unwrap();
        assert!((uv.x - 0.4).abs() < 1e-12);
        assert!((uv.y - 0.7).abs() < 1e-12);
        assert_eq!(mesh.uv_at(Point2::new(21.0, 27.0)), None);
        assert_eq!(mesh.face_count(), 1);
        assert_eq!(mesh.vertex(3).unwrap().point, Point2::new(20.0, 30.0));
        assert_eq!(mesh.face(0).unwrap()[2].uv, Point2::new(0.0, 1.0));
    }

    #[test]
    fn triangulated_face_interpolates_in_both_halves() {
        let points = [
            Point2::new(0.0, 0.0),
            Point2::new(2.0, 0.0),
            Point2::new(0.0, 2.0),
            Point2::new(2.0, 2.0),
        ];
        let mesh = QuadMesh::from_points(&points, 2, 2).unwrap();
        let uv = mesh.uv_at(Point2::new(0.25, 1.5)).unwrap();
        assert!((uv.x - 0.125).abs() < 1e-12);
        assert!((uv.y - 0.75).abs() < 1e-12);
    }

    #[test]
    fn mesh_transform_rebuilds_bounds_and_spatial_index_without_changing_uvs() {
        let points = [
            Point2::new(0.0, 0.0),
            Point2::new(2.0, 0.0),
            Point2::new(0.0, 2.0),
            Point2::new(2.0, 2.0),
        ];
        let mut mesh = QuadMesh::from_points(&points, 2, 2).unwrap();
        let transform = Homography::translation(Point2::new(10.0, -3.0)).unwrap();
        mesh.transform(&transform).unwrap();
        assert_eq!(
            mesh.bounds(),
            BoundingBox::new(Point2::new(10.0, -3.0), Point2::new(12.0, -1.0))
        );
        assert_eq!(
            mesh.uv_at(Point2::new(10.5, -2.0)),
            Some(Point2::new(0.25, 0.5))
        );
        mesh.offset(Point2::new(-1.0, 2.0)).unwrap();
        assert_eq!(
            mesh.bounds(),
            BoundingBox::new(Point2::new(9.0, -1.0), Point2::new(11.0, 1.0))
        );
    }

    #[test]
    fn multi_face_index_inverts_affine_meshes_including_mirrored_ones() {
        // An affine map sends each mesh triangle linearly, so `uv_at` must
        // invert it exactly everywhere, across many faces and index bins.
        let (width, height) = (9, 6);
        for matrix in [
            [[3.0, 1.2, 40.0], [-0.8, 2.5, -15.0]],
            // Mirrored along x (negative determinant, reversed winding).
            [[-4.0, 0.3, 500.0], [0.2, 1.5, 7.0]],
        ] {
            let map = |uv: Point2| {
                let (x, y) = (uv.x * 100.0, uv.y * 60.0);
                Point2::new(
                    matrix[0][0] * x + matrix[0][1] * y + matrix[0][2],
                    matrix[1][0] * x + matrix[1][1] * y + matrix[1][2],
                )
            };
            let points: Vec<_> = (0..height)
                .flat_map(|y| {
                    (0..width).map(move |x| {
                        Point2::new(
                            x as f64 / (width - 1) as f64,
                            y as f64 / (height - 1) as f64,
                        )
                    })
                })
                .map(map)
                .collect();
            let mesh = QuadMesh::from_points(&points, width, height).unwrap();
            assert!(
                mesh.bins_x * mesh.bins_y > 1,
                "index must span several bins"
            );
            for step_y in 0..=23 {
                for step_x in 0..=31 {
                    let uv = Point2::new(step_x as f64 / 31.0, step_y as f64 / 23.0);
                    let found = mesh
                        .uv_at(map(uv))
                        .unwrap_or_else(|| panic!("{uv:?} missed"));
                    assert!(found.approx_eq(uv, 1e-9), "{found:?} != {uv:?}");
                }
            }
            // Outside the sheared parallelogram: just past an edge, and in a
            // corner of its bounding box that the shape does not cover.
            assert_eq!(mesh.uv_at(map(Point2::new(-0.02, 0.5))), None);
            let corner = mesh.bounds().min + Point2::new(1.0, 1.0);
            assert!(mesh.bounds().contains(corner));
            assert_eq!(mesh.uv_at(corner), None);
        }
    }

    #[test]
    fn replacing_vertices_rebuilds_the_face_index_and_preserves_uvs() {
        let points = [
            Point2::new(0.0, 0.0),
            Point2::new(2.0, 0.0),
            Point2::new(0.0, 2.0),
            Point2::new(2.0, 2.0),
        ];
        let mut mesh = QuadMesh::from_points(&points, 2, 2).unwrap();
        let vertices = mesh
            .vertices()
            .iter()
            .map(|vertex| MeshVertex {
                point: vertex.point + Point2::new(5.0, 4.0),
                uv: vertex.uv,
            })
            .collect();
        mesh.set_vertices(vertices).unwrap();
        assert_eq!(
            mesh.uv_at(Point2::new(6.0, 5.0)),
            Some(Point2::new(0.5, 0.5))
        );
        assert!(mesh
            .set_vertices(vec![MeshVertex {
                point: Point2::ZERO,
                uv: Point2::ZERO
            }])
            .is_err());
    }
}
