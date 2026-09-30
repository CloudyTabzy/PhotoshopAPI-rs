//! Rasterizing Photoshop Bézier paths into coverage planes.
//!
//! Path semantics follow what Photoshop renders, pinned by real flattens:
//! contours that share a shape-group index fill even-odd together, groups
//! combine sequentially by their lead contour's operation over the
//! accumulated coverage, open subpaths fill their implied closing chord, and
//! an empty path covers everything.

use psd_core::vector::{Subpath, VectorPath};

use crate::layer::Rect;

/// Sub-scanlines per pixel row. Horizontal coverage inside a sub-scanline is
/// exact, so this only limits the vertical resolution of sloped edges.
const SUBSAMPLES: usize = 32;

/// A flattened closed polygon in document pixels.
pub(crate) type Polygon = Vec<(f64, f64)>;

/// Flatten one subpath's Bézier segments into a polygon in pixels.
pub(crate) fn flatten_subpath(subpath: &Subpath<'_>, width: u32, height: u32) -> Polygon {
    let knots = &subpath.knots;
    let mut polygon = Polygon::new();
    if knots.is_empty() {
        return polygon;
    }
    let pixel = |point: &psd_core::vector::PathPoint| point.to_pixels(width, height);
    polygon.push(pixel(&knots[0].anchor));
    let segments = if subpath.record.closed {
        knots.len()
    } else {
        knots.len() - 1
    };
    for index in 0..segments {
        let from = knots[index];
        let to = knots[(index + 1) % knots.len()];
        let p0 = pixel(&from.anchor);
        let p1 = pixel(&from.leaving);
        let p2 = pixel(&to.preceding);
        let p3 = pixel(&to.anchor);
        let straight = p0 == p1 && p2 == p3;
        if straight {
            polygon.push(p3);
            continue;
        }
        let length = dist(p0, p1) + dist(p1, p2) + dist(p2, p3);
        let steps = ((length / 2.0).ceil() as usize).clamp(4, 512);
        for step in 1..=steps {
            let t = step as f64 / steps as f64;
            polygon.push(cubic(p0, p1, p2, p3, t));
        }
    }
    polygon
}

fn dist(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

fn cubic(p0: (f64, f64), p1: (f64, f64), p2: (f64, f64), p3: (f64, f64), t: f64) -> (f64, f64) {
    let u = 1.0 - t;
    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    (
        a * p0.0 + b * p1.0 + c * p2.0 + d * p3.0,
        a * p0.1 + b * p1.1 + c * p2.1 + d * p3.1,
    )
}

/// Coverage of the even-odd union of `polygons` over `rect` (document
/// pixels), row-major, `0.0..=1.0`.
pub(crate) fn fill_even_odd(polygons: &[Polygon], rect: Rect) -> Vec<f32> {
    let width = rect.width().max(0) as usize;
    let height = rect.height().max(0) as usize;
    let mut plane = vec![0.0f32; width * height];
    if width == 0 || height == 0 {
        return plane;
    }

    // Edges as (y_min, y_max, x_at_y_min, dx/dy) in rect-local pixels.
    let mut edges: Vec<(f64, f64, f64, f64)> = Vec::new();
    for polygon in polygons {
        let count = polygon.len();
        if count < 2 {
            continue;
        }
        for index in 0..count {
            let (x0, y0) = polygon[index];
            let (x1, y1) = polygon[(index + 1) % count];
            let (x0, y0) = (x0 - f64::from(rect.left), y0 - f64::from(rect.top));
            let (x1, y1) = (x1 - f64::from(rect.left), y1 - f64::from(rect.top));
            if y0 == y1 {
                continue;
            }
            let (top, bottom, x_top, x_bottom) = if y0 < y1 {
                (y0, y1, x0, x1)
            } else {
                (y1, y0, x1, x0)
            };
            edges.push((top, bottom, x_top, (x_bottom - x_top) / (bottom - top)));
        }
    }
    edges.sort_by(|a, b| a.0.total_cmp(&b.0));

    let weight = 1.0 / SUBSAMPLES as f32;
    let mut next_edge = 0;
    let mut active: Vec<usize> = Vec::new();
    let mut crossings: Vec<f64> = Vec::new();
    let mut diff = vec![0.0f32; width + 2];
    for row in 0..height {
        diff.fill(0.0);
        let row_data = &mut plane[row * width..(row + 1) * width];
        let mut touched = false;
        for sample in 0..SUBSAMPLES {
            let y = row as f64 + (sample as f64 + 0.5) / SUBSAMPLES as f64;
            while next_edge < edges.len() && edges[next_edge].0 <= y {
                active.push(next_edge);
                next_edge += 1;
            }
            active.retain(|&edge| edges[edge].1 > y);
            if active.is_empty() {
                continue;
            }
            crossings.clear();
            crossings.extend(active.iter().map(|&edge| {
                let (top, _, x_top, slope) = edges[edge];
                x_top + (y - top) * slope
            }));
            crossings.sort_by(|a, b| a.total_cmp(b));
            for pair in crossings.chunks_exact(2) {
                let xa = pair[0].clamp(0.0, width as f64);
                let xb = pair[1].clamp(0.0, width as f64);
                if xb <= xa {
                    continue;
                }
                touched = true;
                let ia = xa.floor() as usize;
                let ib = xb.floor() as usize;
                if ia == ib {
                    row_data[ia.min(width - 1)] += (xb - xa) as f32 * weight;
                } else {
                    row_data[ia] += ((ia + 1) as f64 - xa) as f32 * weight;
                    if ib < width {
                        row_data[ib] += (xb - ib as f64) as f32 * weight;
                    }
                    // Fully covered pixels strictly between the two ends.
                    diff[ia + 1] += weight;
                    diff[ib.min(width)] -= weight;
                }
            }
        }
        if touched {
            let mut running = 0.0f32;
            for (value, delta) in row_data.iter_mut().zip(diff.iter()) {
                running += delta;
                *value = (*value + running).clamp(0.0, 1.0);
            }
        }
    }
    plane
}

/// Combine `incoming` into `accumulated` with a Photoshop shape operation:
/// 0 exclude (xor), 1 combine (union), 2 subtract, 3 intersect.
fn combine(accumulated: &mut [f32], incoming: &[f32], operation: i16) {
    for (a, b) in accumulated.iter_mut().zip(incoming) {
        *a = match operation {
            0 => *a + *b - 2.0 * *a * *b,
            2 => *a * (1.0 - *b),
            3 => *a * *b,
            _ => *a + *b - *a * *b,
        };
    }
}

/// Coverage of a whole path (shape-group semantics) over `rect`, in document
/// pixels of a `width` × `height` canvas.
pub(crate) fn rasterize_path(path: &VectorPath, width: u32, height: u32, rect: Rect) -> Vec<f32> {
    let pixels = rect.width().max(0) as usize * rect.height().max(0) as usize;
    let Ok(subpaths) = path.subpaths() else {
        return vec![1.0; pixels];
    };
    if subpaths.is_empty() {
        return vec![1.0; pixels];
    }

    // Group contours: a lead carries the operation, continuations (0xFFFF)
    // and same-index contours join the lead's group.
    struct Group {
        operation: i16,
        index: u32,
        polygons: Vec<Polygon>,
    }
    let mut groups: Vec<Group> = Vec::new();
    for subpath in &subpaths {
        let polygon = flatten_subpath(subpath, width, height);
        let index = subpath.record.origination_index();
        let operation = subpath.record.operation;
        let continuation = operation == -1 && groups.last().is_some_and(|g| g.index == index);
        if continuation {
            if let Some(group) = groups.last_mut() {
                group.polygons.push(polygon);
            }
        } else {
            groups.push(Group {
                // An unset operation on a new group fills by parity.
                operation: if operation == -1 { 0 } else { operation },
                index,
                polygons: vec![polygon],
            });
        }
    }

    let starts_full = path.initial_fill_rule() == Some(1)
        || groups.first().is_some_and(|group| group.operation == 2);
    let mut accumulated = vec![if starts_full { 1.0 } else { 0.0 }; pixels];
    for group in &groups {
        let coverage = fill_even_odd(&group.polygons, rect);
        combine(&mut accumulated, &coverage, group.operation);
    }
    accumulated
}

/// Separable Gaussian blur with `sigma` pixels, edges replicated.
pub(crate) fn gaussian_blur(plane: &mut [f32], width: usize, height: usize, sigma: f64) {
    if sigma <= 0.0 || width == 0 || height == 0 {
        return;
    }
    let radius = (sigma * 3.0).ceil().max(1.0) as i64;
    let mut kernel: Vec<f32> = (-radius..=radius)
        .map(|offset| (-(offset * offset) as f64 / (2.0 * sigma * sigma)).exp() as f32)
        .collect();
    let total: f32 = kernel.iter().sum();
    for weight in &mut kernel {
        *weight /= total;
    }

    let mut scratch = vec![0.0f32; plane.len()];
    // Horizontal.
    for y in 0..height {
        let row = &plane[y * width..(y + 1) * width];
        let out = &mut scratch[y * width..(y + 1) * width];
        for (x, slot) in out.iter_mut().enumerate() {
            let mut sum = 0.0;
            for (k, weight) in kernel.iter().enumerate() {
                let source = (x as i64 + k as i64 - radius).clamp(0, width as i64 - 1) as usize;
                sum += weight * row[source];
            }
            *slot = sum;
        }
    }
    // Vertical.
    for y in 0..height {
        for x in 0..width {
            let mut sum = 0.0;
            for (k, weight) in kernel.iter().enumerate() {
                let source = (y as i64 + k as i64 - radius).clamp(0, height as i64 - 1) as usize;
                sum += weight * scratch[source * width + x];
            }
            plane[y * width + x] = sum;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(left: f64, top: f64, right: f64, bottom: f64) -> Polygon {
        vec![(left, top), (right, top), (right, bottom), (left, bottom)]
    }

    #[test]
    fn a_pixel_aligned_square_covers_exactly() {
        let rect = Rect::new(0, 0, 8, 8);
        let plane = fill_even_odd(&[square(2.0, 2.0, 6.0, 6.0)], rect);
        for y in 0..8 {
            for x in 0..8 {
                let inside = (2..6).contains(&x) && (2..6).contains(&y);
                assert_eq!(plane[y * 8 + x], if inside { 1.0 } else { 0.0 }, "{x},{y}");
            }
        }
    }

    #[test]
    fn a_half_pixel_edge_is_half_covered() {
        let rect = Rect::new(0, 0, 4, 4);
        let plane = fill_even_odd(&[square(1.5, 0.0, 4.0, 4.0)], rect);
        assert!((plane[1] - 0.5).abs() < 1e-3);
        assert_eq!(plane[2], 1.0);
        assert_eq!(plane[0], 0.0);
    }

    #[test]
    fn contours_of_one_group_fill_even_odd() {
        // A donut: the inner square is a hole whatever its winding.
        let rect = Rect::new(0, 0, 8, 8);
        let mut inner = square(3.0, 3.0, 5.0, 5.0);
        inner.reverse();
        let plane = fill_even_odd(&[square(1.0, 1.0, 7.0, 7.0), inner], rect);
        assert_eq!(plane[2 * 8 + 2], 1.0);
        assert_eq!(plane[4 * 8 + 4], 0.0);
    }

    #[test]
    fn the_rect_offset_is_honoured() {
        let rect = Rect::new(10, 20, 14, 24);
        let plane = fill_even_odd(&[square(21.0, 11.0, 23.0, 13.0)], rect);
        assert_eq!(plane[4 + 1], 1.0);
        assert_eq!(plane[0], 0.0);
    }

    #[test]
    fn a_blur_keeps_the_total_away_from_the_edges() {
        let mut plane = vec![0.0f32; 21 * 21];
        plane[10 * 21 + 10] = 1.0;
        gaussian_blur(&mut plane, 21, 21, 1.5);
        let total: f32 = plane.iter().sum();
        assert!((total - 1.0).abs() < 1e-4);
        assert!(plane[10 * 21 + 10] > plane[10 * 21 + 12]);
        assert!((plane[10 * 21 + 8] - plane[10 * 21 + 12]).abs() < 1e-6);
    }
}
