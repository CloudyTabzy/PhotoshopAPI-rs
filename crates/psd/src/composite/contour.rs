//! Shaping curves ("contours") of layer effects.
//!
//! A contour is a transfer curve over `0..=255`. Between its points Photoshop
//! draws a natural cubic (zero second derivative at the ends of each smooth
//! run); a *corner* point splits the curve into independent runs, which is how
//! Cone and Sawtooth get their straight segments. Outside the first and last
//! point the curve holds the end values. The curve is baked into a 256-entry
//! byte table, as Photoshop's own evaluation is.

use psd_core::Contour;

/// A contour baked into a byte table.
#[derive(Clone)]
pub(crate) struct ContourLut {
    table: [u8; 256],
}

#[derive(Clone, Copy)]
struct Node {
    x: f64,
    y: f64,
    corner: bool,
}

impl ContourLut {
    /// Bake a contour. `None` when the contour is the identity line (callers
    /// skip the remap then).
    pub fn new(contour: &Contour) -> Option<Self> {
        if is_linear(contour) {
            return None;
        }
        let mut nodes: Vec<Node> = contour
            .points
            .iter()
            .map(|point| Node {
                x: point.horizontal.clamp(0.0, 255.0),
                y: point.vertical.clamp(0.0, 255.0),
                // `Cnty` is the continuity flag: false marks a corner.
                corner: point.smooth == Some(false),
            })
            .collect();
        nodes.sort_by(|a, b| a.x.total_cmp(&b.x));
        // A repeated input keeps the last point supplied.
        let mut unique: Vec<Node> = Vec::with_capacity(nodes.len());
        for node in nodes {
            match unique.last_mut() {
                Some(last) if (last.x - node.x).abs() < 1e-9 => *last = node,
                _ => unique.push(node),
            }
        }
        let mut table = [0u8; 256];
        if unique.len() < 2 {
            for (input, slot) in table.iter_mut().enumerate() {
                *slot = input as u8;
            }
            return Some(Self { table });
        }
        let first = unique[0];
        let last = unique[unique.len() - 1];
        for (input, slot) in table.iter_mut().enumerate() {
            let x = input as f64;
            if x <= first.x {
                *slot = first.y.round() as u8;
            } else if x >= last.x {
                *slot = last.y.round() as u8;
            }
        }
        // Corner points split the curve into smooth runs.
        let mut start = 0;
        for index in 1..unique.len() {
            if unique[index].corner || index + 1 == unique.len() {
                fill_run(&mut table, &unique[start..=index]);
                start = index;
            }
        }
        Some(Self { table })
    }

    /// The curve at `t` in `0..=1`; `smooth` interpolates between table
    /// entries (the contour's anti-aliasing), otherwise the nearest entry.
    pub fn sample(&self, t: f32, smooth: bool) -> f32 {
        let scaled = t.clamp(0.0, 1.0) * 255.0;
        if !smooth {
            return f32::from(self.table[scaled.round() as usize]) / 255.0;
        }
        let low = scaled as usize;
        let high = (low + 1).min(255);
        let fraction = scaled - low as f32;
        (f32::from(self.table[low]) * (1.0 - fraction) + f32::from(self.table[high]) * fraction)
            / 255.0
    }
}

/// Whether a contour is the default straight line.
pub(crate) fn is_linear(contour: &Contour) -> bool {
    match contour.points.as_slice() {
        [] => true,
        [first, last] => {
            first.horizontal.abs() < 1e-4
                && first.vertical.abs() < 1e-4
                && (last.horizontal - 255.0).abs() < 1e-4
                && (last.vertical - 255.0).abs() < 1e-4
        }
        _ => false,
    }
}

/// Fill the table entries a run of nodes covers with a natural cubic.
fn fill_run(table: &mut [u8; 256], nodes: &[Node]) {
    let count = nodes.len();
    let mut second = vec![0.0f64; count];
    let mut work = vec![0.0f64; count];
    for index in 1..count.saturating_sub(1) {
        let (previous, current, next) = (nodes[index - 1], nodes[index], nodes[index + 1]);
        let previous_span = current.x - previous.x;
        let next_span = next.x - current.x;
        let combined = previous_span + next_span;
        let sigma = previous_span / combined;
        let pivot = sigma * second[index - 1] + 2.0;
        second[index] = (sigma - 1.0) / pivot;
        let previous_slope = (current.y - previous.y) / previous_span;
        let next_slope = (next.y - current.y) / next_span;
        work[index] =
            (6.0 * (next_slope - previous_slope) / combined - sigma * work[index - 1]) / pivot;
    }
    for upper in (1..count).rev() {
        let index = upper - 1;
        second[index] = second[index] * second[upper] + work[index];
    }

    let begin = (nodes[0].x - 1e-9).ceil().max(0.0) as usize;
    let end = (nodes[count - 1].x + 1e-9).floor().min(255.0) as usize;
    let mut upper = 1;
    for (input, slot) in table.iter_mut().enumerate().take(end + 1).skip(begin) {
        let x = input as f64;
        while upper + 1 < count && x > nodes[upper].x {
            upper += 1;
        }
        let (left, right) = (nodes[upper - 1], nodes[upper]);
        let span = right.x - left.x;
        let mut output = left.y;
        if span > 1e-9 {
            let a = (right.x - x) / span;
            let b = (x - left.x) / span;
            output = a * left.y
                + b * right.y
                + ((a * a * a - a) * second[upper - 1] + (b * b * b - b) * second[upper])
                    * span
                    * span
                    / 6.0;
        }
        *slot = output.round().clamp(0.0, 255.0) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use psd_core::ContourPoint;

    fn contour(points: &[(f64, f64, Option<bool>)]) -> Contour {
        Contour {
            name: "test".into(),
            points: points
                .iter()
                .map(|(x, y, smooth)| ContourPoint {
                    horizontal: *x,
                    vertical: *y,
                    smooth: *smooth,
                })
                .collect(),
        }
    }

    #[test]
    fn the_default_line_is_the_identity_and_needs_no_table() {
        assert!(ContourLut::new(&contour(&[(0.0, 0.0, None), (255.0, 255.0, None)])).is_none());
    }

    #[test]
    fn corner_points_make_straight_segments() {
        // Cone: up to the middle and back down, corners throughout.
        let lut = ContourLut::new(&contour(&[
            (0.0, 0.0, Some(false)),
            (128.0, 255.0, Some(false)),
            (255.0, 0.0, Some(false)),
        ]))
        .unwrap();
        assert_eq!(lut.sample(0.0, false), 0.0);
        assert_eq!(lut.sample(128.0 / 255.0, false), 1.0);
        let quarter = lut.sample(64.0 / 255.0, false);
        assert!((quarter - 0.5).abs() < 0.01, "{quarter}");
    }

    #[test]
    fn smooth_points_follow_a_cubic_through_them() {
        let lut = ContourLut::new(&contour(&[
            (0.0, 0.0, None),
            (128.0, 255.0, None),
            (255.0, 0.0, None),
        ]))
        .unwrap();
        // Passes through the middle point exactly and stays within range.
        assert_eq!(lut.sample(128.0 / 255.0, false), 1.0);
        assert!(lut.sample(64.0 / 255.0, false) > 0.5);
    }
}
