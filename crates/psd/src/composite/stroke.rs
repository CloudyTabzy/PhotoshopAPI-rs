//! Stroking vector paths.
//!
//! A stroke is built as a union of convex pieces: a quad per segment, a wedge
//! per join and a cap at each open end (round caps and joins are discs). Every
//! piece is oriented the same way and the lot is scan-converted under the
//! non-zero rule, so overlaps union exactly. A dash pattern cuts the flattened
//! path into open pieces first, each with its own caps.

use psd_core::vector::VectorPath;

use super::effects::distance_transform;
use super::paths::{fill, flatten_subpath, FillRule, Polygon};
use crate::layer::Rect;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Cap {
    Butt,
    Round,
    Square,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Join {
    Miter,
    Round,
    Bevel,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Alignment {
    Inside,
    Center,
    Outside,
}

/// A resolved stroke style; lengths are in document pixels.
#[derive(Clone, Debug)]
pub(crate) struct StrokeStyle {
    pub width: f64,
    pub cap: Cap,
    pub join: Join,
    pub miter_limit: f64,
    pub alignment: Alignment,
    /// On/off lengths; empty for a solid stroke.
    pub dashes: Vec<f64>,
    pub dash_offset: f64,
}

type Point = (f64, f64);

fn unit(from: Point, to: Point) -> Option<Point> {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let length = (dx * dx + dy * dy).sqrt();
    (length > 1e-9).then(|| (dx / length, dy / length))
}

fn normal(direction: Point) -> Point {
    (-direction.1, direction.0)
}

fn push(out: &mut Vec<Polygon>, mut polygon: Polygon) {
    // Orient every piece the same way so the non-zero rule unions them.
    let mut area = 0.0;
    for index in 0..polygon.len() {
        let (a, b) = (polygon[index], polygon[(index + 1) % polygon.len()]);
        area += a.0 * b.1 - b.0 * a.1;
    }
    if area < 0.0 {
        polygon.reverse();
    }
    out.push(polygon);
}

fn disc(out: &mut Vec<Polygon>, center: Point, radius: f64) {
    let steps = ((std::f64::consts::TAU * radius / 0.75).ceil() as usize).clamp(8, 128);
    let polygon = (0..steps)
        .map(|step| {
            let angle = std::f64::consts::TAU * step as f64 / steps as f64;
            (
                center.0 + radius * angle.cos(),
                center.1 + radius * angle.sin(),
            )
        })
        .collect();
    push(out, polygon);
}

/// Stroke one polyline into pieces of half-width `half`.
fn stroke_polyline(
    out: &mut Vec<Polygon>,
    points: &[Point],
    closed: bool,
    half: f64,
    style: &StrokeStyle,
) {
    // Drop repeated points.
    let mut path: Vec<Point> = Vec::with_capacity(points.len());
    for &point in points {
        if path
            .last()
            .is_none_or(|last| (last.0 - point.0).abs() > 1e-9 || (last.1 - point.1).abs() > 1e-9)
        {
            path.push(point);
        }
    }
    if closed && path.len() > 1 && path[0] == path[path.len() - 1] {
        path.pop();
    }
    if path.len() == 1 {
        // A lone point strokes as its cap.
        if style.cap == Cap::Round {
            disc(out, path[0], half);
        } else if style.cap == Cap::Square {
            let p = path[0];
            push(
                out,
                vec![
                    (p.0 - half, p.1 - half),
                    (p.0 + half, p.1 - half),
                    (p.0 + half, p.1 + half),
                    (p.0 - half, p.1 + half),
                ],
            );
        }
        return;
    }
    if path.len() < 2 {
        return;
    }
    let count = path.len();
    let segments = if closed { count } else { count - 1 };
    let mut directions: Vec<Option<Point>> = Vec::with_capacity(segments);
    for index in 0..segments {
        let (a, b) = (path[index], path[(index + 1) % count]);
        directions.push(unit(a, b));
        let Some(d) = directions[index] else {
            continue;
        };
        let n = normal(d);
        push(
            out,
            vec![
                (a.0 + n.0 * half, a.1 + n.1 * half),
                (b.0 + n.0 * half, b.1 + n.1 * half),
                (b.0 - n.0 * half, b.1 - n.1 * half),
                (a.0 - n.0 * half, a.1 - n.1 * half),
            ],
        );
    }
    // Joins.
    let joins: Vec<usize> = if closed {
        (0..count).collect()
    } else {
        (1..count - 1).collect()
    };
    for vertex in joins {
        let previous = (vertex + segments - 1) % segments;
        let (Some(d0), Some(d1)) = (directions[previous], directions[vertex % segments]) else {
            continue;
        };
        join(out, path[vertex], d0, d1, half, style);
    }
    // Caps.
    if !closed {
        cap(out, path[0], directions[0], true, half, style);
        cap(
            out,
            path[count - 1],
            directions[segments - 1],
            false,
            half,
            style,
        );
    }
}

fn join(
    out: &mut Vec<Polygon>,
    vertex: Point,
    d0: Point,
    d1: Point,
    half: f64,
    style: &StrokeStyle,
) {
    let cross = d0.0 * d1.1 - d0.1 * d1.0;
    let dot = d0.0 * d1.0 + d0.1 * d1.1;
    if cross.abs() < 1e-9 && dot > 0.0 {
        return;
    }
    if style.join == Join::Round {
        disc(out, vertex, half);
        return;
    }
    // The outer side is opposite the direction the path turns.
    let side = if cross > 0.0 { -1.0 } else { 1.0 };
    let (n0, n1) = (normal(d0), normal(d1));
    let a = (vertex.0 + side * n0.0 * half, vertex.1 + side * n0.1 * half);
    let b = (vertex.0 + side * n1.0 * half, vertex.1 + side * n1.1 * half);
    if style.join == Join::Miter {
        let ratio = (2.0 / (1.0 + dot).max(1e-12)).sqrt();
        if ratio <= style.miter_limit {
            let scale = side * half / (1.0 + dot);
            let tip = (
                vertex.0 + (n0.0 + n1.0) * scale,
                vertex.1 + (n0.1 + n1.1) * scale,
            );
            push(out, vec![vertex, a, tip, b]);
            return;
        }
    }
    push(out, vec![vertex, a, b]);
}

fn cap(
    out: &mut Vec<Polygon>,
    end: Point,
    direction: Option<Point>,
    at_start: bool,
    half: f64,
    style: &StrokeStyle,
) {
    match style.cap {
        Cap::Butt => {}
        Cap::Round => disc(out, end, half),
        Cap::Square => {
            let Some(d) = direction else {
                return;
            };
            // Outward along the path's end.
            let d = if at_start { (-d.0, -d.1) } else { d };
            let n = normal(d);
            push(
                out,
                vec![
                    (end.0 + n.0 * half, end.1 + n.1 * half),
                    (
                        end.0 + n.0 * half + d.0 * half,
                        end.1 + n.1 * half + d.1 * half,
                    ),
                    (
                        end.0 - n.0 * half + d.0 * half,
                        end.1 - n.1 * half + d.1 * half,
                    ),
                    (end.0 - n.0 * half, end.1 - n.1 * half),
                ],
            );
        }
    }
}

/// Cut a polyline into the "on" pieces of a dash pattern.
fn dash(points: &[Point], closed: bool, pattern: &[f64], offset: f64) -> Vec<Vec<Point>> {
    let total: f64 = pattern.iter().sum();
    if pattern.is_empty() || total <= 1e-9 || pattern.iter().any(|v| *v < 0.0) {
        return vec![points.to_vec()];
    }
    let mut path = points.to_vec();
    if closed && path.first() != path.last() {
        if let Some(first) = path.first().copied() {
            path.push(first);
        }
    }
    // Start `offset` into the pattern.
    let mut index = 0;
    let mut remaining = pattern[0];
    let mut skip = offset.rem_euclid(total);
    while skip > 0.0 {
        if skip >= remaining {
            skip -= remaining;
            index = (index + 1) % pattern.len();
            remaining = pattern[index];
            if remaining == 0.0 && skip == 0.0 {
                break;
            }
        } else {
            remaining -= skip;
            skip = 0.0;
        }
    }
    let mut pieces: Vec<Vec<Point>> = Vec::new();
    let mut current: Vec<Point> = Vec::new();
    if index % 2 == 0 {
        current.push(path[0]);
    }
    for window in path.windows(2) {
        let (a, b) = (window[0], window[1]);
        let length = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
        if length < 1e-12 {
            continue;
        }
        let mut travelled = 0.0;
        while length - travelled > remaining {
            travelled += remaining;
            let t = travelled / length;
            let at = (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
            if index % 2 == 0 {
                current.push(at);
                pieces.push(std::mem::take(&mut current));
            } else {
                current.clear();
                current.push(at);
            }
            index = (index + 1) % pattern.len();
            remaining = pattern[index];
            if remaining == 0.0 && pattern.iter().all(|v| *v == 0.0) {
                return pieces;
            }
        }
        remaining -= length - travelled;
        if index % 2 == 0 {
            current.push(b);
        }
    }
    if index % 2 == 0 && current.len() > 1 {
        pieces.push(current);
    }
    pieces
}

/// The coverage of a path's stroke over `rect`. `fill_coverage` is the path's
/// own fill coverage over the same rect (Inside and Outside alignment clip the
/// stroke to it or to its complement).
pub(crate) fn stroke_coverage(
    path: &VectorPath,
    document: (u32, u32),
    rect: Rect,
    style: &StrokeStyle,
    fill_coverage: &[f32],
) -> Vec<f32> {
    let pixels = rect.width().max(0) as usize * rect.height().max(0) as usize;
    let Ok(subpaths) = path.subpaths() else {
        return vec![0.0; pixels];
    };
    let mut centered: Vec<Polygon> = Vec::new();
    let mut sided: Vec<Polygon> = Vec::new();
    for subpath in &subpaths {
        let closed = subpath.record.closed;
        let polyline = flatten_subpath(subpath, document.0, document.1);
        let side = closed && style.alignment != Alignment::Center;
        // Inside and Outside draw a stroke twice as wide centred on the path
        // and keep the side they want.
        let half = if side { style.width } else { style.width / 2.0 };
        let target = if side { &mut sided } else { &mut centered };
        if style.dashes.is_empty() {
            stroke_polyline(target, &polyline, closed, half, style);
        } else {
            for piece in dash(&polyline, closed, &style.dashes, style.dash_offset) {
                stroke_polyline(target, &piece, false, half, style);
            }
        }
    }
    let mut coverage = vec![0.0f32; pixels];
    if !centered.is_empty() {
        coverage = fill(&centered, rect, FillRule::NonZero);
    }
    if !sided.is_empty() {
        let plane = fill(&sided, rect, FillRule::NonZero);
        for (index, value) in plane.iter().enumerate() {
            let inside = fill_coverage.get(index).copied().unwrap_or(0.0);
            let keep = if style.alignment == Alignment::Inside {
                inside
            } else {
                1.0 - inside
            };
            let side = value * keep;
            coverage[index] = coverage[index] + side - coverage[index] * side;
        }
    }
    if subpaths.len() > 1 {
        let reach = if style.alignment == Alignment::Center {
            style.width / 2.0
        } else {
            style.width
        };
        keep_near_outline(&mut coverage, fill_coverage, rect, reach as f32);
    }
    coverage
}

/// Strokes belong to the outline of the combined shape, not to each subpath:
/// where subpaths overlap, the arcs that end up inside the shape (or, for a
/// subtraction, outside it) are not stroked. Zero the stroke wherever the
/// pixel is farther than `reach` (plus a margin that keeps anti-aliased band
/// edges intact) from the outline of `fill_coverage`.
///
/// Pixels within `reach` of the plane's edge are left alone: a band that is
/// visible there may belong to an outline that lies beyond the plane.
fn keep_near_outline(coverage: &mut [f32], fill_coverage: &[f32], rect: Rect, reach: f32) {
    let (width, height) = (rect.width().max(0) as usize, rect.height().max(0) as usize);
    if width == 0 || height == 0 || fill_coverage.len() < width * height {
        return;
    }
    let margin = 0.0f32;
    let inside: Vec<f32> = fill_coverage
        .iter()
        .take(width * height)
        .map(|value| if *value >= 0.5 { 1.0 } else { 0.0 })
        .collect();
    if inside.iter().all(|value| *value == inside[0]) {
        return;
    }
    let outside: Vec<f32> = inside.iter().map(|value| 1.0 - value).collect();
    let to_inside = distance_transform(&inside, width, height);
    let to_outside = distance_transform(&outside, width, height);
    let edge = reach + margin;
    for y in 0..height {
        for x in 0..width {
            let from_edge = x.min(y).min(width - 1 - x).min(height - 1 - y) as f32;
            if from_edge <= edge {
                continue;
            }
            let index = y * width + x;
            // Each transform is zero on its own set; the other one measures
            // how far the pixel is from the outline.
            // Centre-to-centre distances overshoot the outline by half a pixel.
            let distance = to_inside[index].max(to_outside[index]) - 0.5;
            coverage[index] *= (edge - distance + 1.0).clamp(0.0, 1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style(width: f64, cap: Cap, join: Join) -> StrokeStyle {
        StrokeStyle {
            width,
            cap,
            join,
            miter_limit: 10.0,
            alignment: Alignment::Center,
            dashes: Vec::new(),
            dash_offset: 0.0,
        }
    }

    fn cover(points: &[Point], closed: bool, style: &StrokeStyle) -> Vec<f32> {
        let mut pieces = Vec::new();
        stroke_polyline(&mut pieces, points, closed, style.width / 2.0, style);
        fill(&pieces, Rect::new(0, 0, 12, 12), FillRule::NonZero)
    }

    #[test]
    fn a_straight_stroke_covers_its_width() {
        let plane = cover(
            &[(2.0, 6.0), (10.0, 6.0)],
            false,
            &style(4.0, Cap::Butt, Join::Miter),
        );
        // Rows 4..8 are covered from x = 2 to 10, nothing beyond the butt ends.
        assert_eq!(plane[5 * 12 + 5], 1.0);
        assert_eq!(plane[3 * 12 + 5], 0.0);
        assert_eq!(plane[5 * 12 + 1], 0.0);
        assert_eq!(plane[5 * 12 + 10], 0.0);
    }

    #[test]
    fn a_square_cap_extends_by_half_the_width() {
        let plane = cover(
            &[(3.0, 6.0), (9.0, 6.0)],
            false,
            &style(4.0, Cap::Square, Join::Miter),
        );
        assert_eq!(plane[5 * 12 + 1], 1.0);
        assert_eq!(plane[5 * 12 + 10], 1.0);
    }

    #[test]
    fn a_miter_fills_the_outer_corner_and_a_bevel_cuts_it() {
        let corner = [(2.0, 2.0), (8.0, 2.0), (8.0, 8.0)];
        let miter = cover(&corner, false, &style(4.0, Cap::Butt, Join::Miter));
        let bevel = cover(&corner, false, &style(4.0, Cap::Butt, Join::Bevel));
        // The pixel at the outer corner, (9, 1).
        assert_eq!(miter[12 + 9], 1.0);
        assert!(bevel[12 + 9] < 0.6);
    }

    #[test]
    fn strokes_far_from_the_combined_outline_are_dropped() {
        // A disc of radius 12 in a 64 × 64 plane, with a stroke plane that is
        // everywhere set: only the pixels near the disc's outline survive.
        let size = 64usize;
        let rect = Rect::new(0, 0, size as i32, size as i32);
        let disc: Vec<f32> = (0..size * size)
            .map(|index| {
                let (x, y) = ((index % size) as f32 - 32.0, (index / size) as f32 - 32.0);
                if x * x + y * y <= 144.0 {
                    1.0
                } else {
                    0.0
                }
            })
            .collect();
        let mut coverage = vec![1.0f32; size * size];
        keep_near_outline(&mut coverage, &disc, rect, 1.0);
        let at = |x: usize, y: usize| coverage[y * size + x];
        // The centre is deep inside; the outline pixel stays; the corner is
        // outside the plane's edge margin and is left alone.
        assert_eq!(at(32, 32), 0.0);
        assert_eq!(at(44, 32), 1.0);
        assert_eq!(at(1, 1), 1.0);
    }

    #[test]
    fn overlapping_pieces_union_instead_of_cancelling() {
        // A closed square: every join overlaps the quads beside it.
        let square = [(2.0, 2.0), (9.0, 2.0), (9.0, 9.0), (2.0, 9.0)];
        let plane = cover(&square, true, &style(2.0, Cap::Butt, Join::Round));
        assert_eq!(plane[2 * 12 + 2], 1.0);
        assert_eq!(plane[5 * 12 + 5], 0.0);
    }

    #[test]
    fn a_dash_pattern_leaves_gaps() {
        let mut dashed = style(2.0, Cap::Butt, Join::Miter);
        dashed.dashes = vec![3.0, 2.0];
        let mut pieces = Vec::new();
        for piece in dash(&[(1.0, 6.0), (11.0, 6.0)], false, &dashed.dashes, 0.0) {
            stroke_polyline(&mut pieces, &piece, false, 1.0, &dashed);
        }
        let plane = fill(&pieces, Rect::new(0, 0, 12, 12), FillRule::NonZero);
        assert_eq!(plane[5 * 12 + 2], 1.0, "inside the first dash");
        assert_eq!(plane[5 * 12 + 4], 0.0, "in the gap");
        assert_eq!(plane[5 * 12 + 7], 1.0, "inside the second dash");
    }
}
