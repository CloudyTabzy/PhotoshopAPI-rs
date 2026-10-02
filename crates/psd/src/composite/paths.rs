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
    fill(polygons, rect, FillRule::EvenOdd)
}

/// How overlapping contours decide what is inside.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FillRule {
    /// Inside where an odd number of edges lie to the left.
    EvenOdd,
    /// Inside where the signed edge count is not zero: same-orientation
    /// pieces union.
    NonZero,
}

/// Coverage of `polygons` over `rect` (document pixels) under a fill rule.
pub(crate) fn fill(polygons: &[Polygon], rect: Rect, rule: FillRule) -> Vec<f32> {
    let width = rect.width().max(0) as usize;
    let height = rect.height().max(0) as usize;
    let mut plane = vec![0.0f32; width * height];
    if width == 0 || height == 0 {
        return plane;
    }

    // Edges as (y_min, y_max, x_at_y_min, dx/dy, direction) in rect-local
    // pixels; the direction is +1 for an edge that runs downward.
    let mut edges: Vec<(f64, f64, f64, f64, i32)> = Vec::new();
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
            let (top, bottom, x_top, x_bottom, direction) = if y0 < y1 {
                (y0, y1, x0, x1, 1)
            } else {
                (y1, y0, x1, x0, -1)
            };
            edges.push((
                top,
                bottom,
                x_top,
                (x_bottom - x_top) / (bottom - top),
                direction,
            ));
        }
    }
    edges.sort_by(|a, b| a.0.total_cmp(&b.0));

    let weight = 1.0 / SUBSAMPLES as f32;
    let mut next_edge = 0;
    let mut active: Vec<usize> = Vec::new();
    let mut crossings: Vec<(f64, i32)> = Vec::new();
    let mut spans: Vec<(f64, f64)> = Vec::new();
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
                let (top, _, x_top, slope, direction) = edges[edge];
                (x_top + (y - top) * slope, direction)
            }));
            crossings.sort_by(|a, b| a.0.total_cmp(&b.0));
            spans.clear();
            match rule {
                FillRule::EvenOdd => {
                    for pair in crossings.chunks_exact(2) {
                        spans.push((pair[0].0, pair[1].0));
                    }
                }
                FillRule::NonZero => {
                    let mut winding = 0;
                    let mut start = 0.0;
                    for &(x, direction) in &crossings {
                        let before = winding;
                        winding += direction;
                        if before == 0 && winding != 0 {
                            start = x;
                        } else if before != 0 && winding == 0 {
                            spans.push((start, x));
                        }
                    }
                }
            }
            for &(start, end) in &spans {
                let xa = start.clamp(0.0, width as f64);
                let xb = end.clamp(0.0, width as f64);
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
    for (index, group) in groups.iter().enumerate() {
        let coverage = fill_even_odd(&group.polygons, rect);
        // On an empty accumulator the first shape is exactly itself, whatever
        // its operation (Subtract alone starts from everything).
        let operation = if index == 0 && !starts_full {
            1
        } else {
            group.operation
        };
        combine(&mut accumulated, &coverage, operation);
    }
    accumulated
}

/// The three box radii Photoshop's feather approximates a Gaussian with.
///
/// Kutskir's three-box approximation, with the narrow/wide pass split: the
/// ideal box width for `sigma` over three passes is `sqrt(12 sigma^2 / 3 + 1)`,
/// rounded down to an odd width, and the passes that can take the narrower
/// width do. The reference editor pins this as Photoshop's own behaviour for
/// mask and vector-mask feather (its `mask_feather_box_radii`), and its
/// fixtures are Photoshop exports.
pub(crate) fn feather_box_radii(sigma: f64) -> [i32; 3] {
    const PASSES: f64 = 3.0;
    let ideal = (12.0 * sigma * sigma / PASSES + 1.0).sqrt();
    let mut lower = ideal.floor() as i32;
    if lower % 2 == 0 {
        lower -= 1;
    }
    lower = lower.max(1);
    let lower_d = f64::from(lower);
    let narrow =
        (12.0 * sigma * sigma - PASSES * lower_d * lower_d - 4.0 * PASSES * lower_d - 3.0 * PASSES)
            / (-4.0 * lower_d - 4.0);
    let narrow_passes = (narrow + 0.5).floor().clamp(0.0, 3.0) as i32;
    let mut radii = [0i32; 3];
    for (pass, radius) in radii.iter_mut().enumerate() {
        let width = if (pass as i32) < narrow_passes {
            lower
        } else {
            lower + 2
        };
        *radius = (width - 1) / 2;
    }
    radii
}

/// A box blur pass along rows or columns, edge-clamped.
fn feather_box_pass(
    plane: &mut [f32],
    scratch: &mut [f32],
    width: usize,
    height: usize,
    radius: i32,
    horizontal: bool,
) {
    if radius <= 0 {
        return;
    }
    let window = (2 * radius + 1) as f32;
    let (lines, length, step) = if horizontal {
        (height, width, 1usize)
    } else {
        (width, height, width)
    };
    for line in 0..lines {
        let base = if horizontal { line * width } else { line };
        let at = |index: i64| -> f32 {
            let clamped = index.clamp(0, length as i64 - 1) as usize;
            plane[base + clamped * step]
        };
        let mut sum = 0.0f32;
        for index in -radius..=radius {
            sum += at(i64::from(index));
        }
        for index in 0..length {
            scratch[base + index * step] = sum / window;
            sum += at(index as i64 + i64::from(radius) + 1) - at(index as i64 - i64::from(radius));
        }
    }
    plane.copy_from_slice(scratch);
}

/// Photoshop's feather: a three-box approximation of a Gaussian with `sigma`
/// pixels, edges replicated. The reach is the sum of the box radii, which is
/// what a caller padding the plane has to allow for.
pub(crate) fn feather_blur(plane: &mut [f32], width: usize, height: usize, sigma: f64) {
    if sigma <= 0.0 || width == 0 || height == 0 {
        return;
    }
    let mut scratch = vec![0.0f32; plane.len()];
    for radius in feather_box_radii(sigma) {
        if radius > 0 {
            feather_box_pass(plane, &mut scratch, width, height, radius, true);
            feather_box_pass(plane, &mut scratch, width, height, radius, false);
        }
    }
}

/// The reach of [`feather_blur`] in pixels: how far a padded plane must extend
/// past the painted area for the blur to see its default colour.
pub(crate) fn feather_reach(sigma: f64) -> i32 {
    feather_box_radii(sigma).iter().sum()
}

#[cfg(test)]
mod feather_tests {
    use super::*;

    #[test]
    fn the_three_box_radii_match_the_pinned_mapping() {
        // `sigma = 3` is a worked case of the pinned formula: ideal width
        // sqrt(12*9/3 + 1) = 6.08 floors to 6, goes odd to 5, the narrow-pass
        // count comes out 2, so the first two boxes are width 5 and the third
        // width 7 — radii 2, 2, 3 and a reach of 7.
        assert_eq!(feather_box_radii(3.0), [2, 2, 3]);
        assert_eq!(feather_reach(3.0), 7);
        // Every radius is odd-width and positive, and the reach grows with sigma.
        for sigma in [0.5, 1.0, 2.5, 5.0, 10.0, 25.0] {
            let radii = feather_box_radii(sigma);
            assert!(radii.iter().all(|&radius| radius >= 0), "sigma {sigma}");
            assert!(feather_reach(sigma) >= 0, "sigma {sigma}");
        }
        assert!(feather_reach(10.0) > feather_reach(3.0));
        // A box blur conserves a constant plane (edge clamping included).
        let (width, height) = (41usize, 41usize);
        let mut plane = vec![0.0f32; width * height];
        for y in 0..height {
            for x in 0..width {
                plane[y * width + x] = 1.0;
            }
        }
        feather_blur(&mut plane, width, height, 3.0);
        assert!(plane.iter().all(|value| (value - 1.0).abs() < 1e-4));
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
}
