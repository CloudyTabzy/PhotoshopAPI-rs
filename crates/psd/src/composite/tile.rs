//! Sampling a wrap-tiled pattern in document space.
//!
//! Anchoring follows Photoshop: "Link with Layer" anchors the tile grid at the
//! layer's effects reference point (`fxrp`, which Photoshop updates when a
//! layer moves), unlinked it anchors at the document origin, and the
//! descriptor's phase adds on top in both cases. Filtering: a 100% tile with
//! no rotation is nearest (byte crisp). Other scales use byte-quantized
//! bilinear interpolation, with up to three rounded 2x2 reductions when
//! minifying. Unrotated odd tile dimensions keep the original footprint average until
//! their native reduction and phase convention is known. Rotation maps document
//! to tile space as `R(angle) · (p − anchor) / scale` with `R = [[cos, −sin], [sin,
//! cos]]`, so a positive angle turns the pattern counter-clockwise.

use psd_core::pattern::Pattern;
use std::borrow::Cow;

/// A pattern placed in the document.
pub(crate) struct TileSampler<'a> {
    pixels: Cow<'a, [u8]>,
    width: usize,
    height: usize,
    anchor: (f64, f64),
    scale: f64,
    cos: f64,
    sin: f64,
    rotated: bool,
    nearest: bool,
    box_filter: bool,
}

impl<'a> TileSampler<'a> {
    /// `scale` is a percentage, `angle` degrees; `anchor` already includes the
    /// phase and, when linked, the layer's reference point.
    pub fn new(pattern: &'a Pattern, scale: f64, angle: f64, anchor: (f64, f64)) -> Option<Self> {
        let mut width = pattern.bounds.w as usize;
        let mut height = pattern.bounds.h as usize;
        let bytes = width.checked_mul(height)?.checked_mul(4)?;
        if width == 0
            || height == 0
            || pattern.data.len() < bytes
            || !scale.is_finite()
            || !angle.is_finite()
            || !anchor.0.is_finite()
            || !anchor.1.is_finite()
        {
            return None;
        }
        let mut scale = (scale / 100.0).max(0.01);
        let mut pixels = Cow::Borrowed(&pattern.data[..bytes]);
        // Keep the residual scale in (0.5, 1], subject to the level limit.
        // A rendered 47% tile uses one
        // rounded reduction and the same Q8 bilinear kernel as magnification.
        // Reducing an odd tile would change its repeat period; retain the
        // footprint average for that case rather than stretching the pattern.
        for _ in 0..3 {
            if scale > 0.5
                || width == 1 && height == 1
                || width > 1 && !width.is_multiple_of(2)
                || height > 1 && !height.is_multiple_of(2)
            {
                break;
            }
            pixels = Cow::Owned(reduce(&pixels, width, height));
            width = (width / 2).max(1);
            height = (height / 2).max(1);
            scale *= 2.0;
        }
        let radians = angle.to_radians();
        let rotated = radians.abs() > 1e-9;
        let nearest = !rotated && (scale - 1.0).abs() < 1e-6;
        Some(Self {
            pixels,
            width,
            height,
            anchor,
            scale,
            cos: radians.cos(),
            sin: radians.sin(),
            rotated,
            nearest,
            box_filter: !nearest
                && !rotated
                && scale < 1.0
                && (width > 1 && !width.is_multiple_of(2)
                    || height > 1 && !height.is_multiple_of(2)),
        })
    }

    fn texel(&self, x: i64, y: i64) -> [i32; 4] {
        let x = x.rem_euclid(self.width as i64) as usize;
        let y = y.rem_euclid(self.height as i64) as usize;
        let index = (y * self.width + x) * 4;
        let pixel = &self.pixels[index..index + 4];
        [
            i32::from(pixel[0]),
            i32::from(pixel[1]),
            i32::from(pixel[2]),
            i32::from(pixel[3]),
        ]
    }

    /// Straight colour and alpha of the pattern under pixel `(x, y)`.
    pub fn sample(&self, x: i32, y: i32) -> ([f32; 3], f32) {
        let (x, y) = (f64::from(x), f64::from(y));
        if self.width == 1 && self.height == 1 {
            let texel = self.texel(0, 0).map(|value| value as f32 / 255.0);
            return ([texel[0], texel[1], texel[2]], texel[3]);
        }
        if self.box_filter {
            let u0 = ((x - self.anchor.0) / self.scale).rem_euclid(self.width as f64);
            let u1 = u0 + 1.0 / self.scale;
            let v0 = ((y - self.anchor.1) / self.scale).rem_euclid(self.height as f64);
            let v1 = v0 + 1.0 / self.scale;
            let mut sum = [0.0f64; 4];
            let mut total = 0.0f64;
            for ty in v0.floor() as i64..=(v1.ceil() as i64 - 1) {
                let cover_y = v1.min(ty as f64 + 1.0) - v0.max(ty as f64);
                if cover_y <= 0.0 {
                    continue;
                }
                for tx in u0.floor() as i64..=(u1.ceil() as i64 - 1) {
                    let cover_x = u1.min(tx as f64 + 1.0) - u0.max(tx as f64);
                    if cover_x <= 0.0 {
                        continue;
                    }
                    let weight = cover_x * cover_y;
                    let texel = self.texel(tx, ty);
                    for channel in 0..4 {
                        sum[channel] += weight * f64::from(texel[channel]);
                    }
                    total += weight;
                }
            }
            if total <= 0.0 {
                return ([0.0; 3], 0.0);
            }
            return (
                [
                    (sum[0] / total / 255.0) as f32,
                    (sum[1] / total / 255.0) as f32,
                    (sum[2] / total / 255.0) as f32,
                ],
                (sum[3] / total / 255.0) as f32,
            );
        }
        if self.nearest {
            // 100%: the tile grid lands on pixel cells directly.
            let texel = self
                .texel(
                    (x + 0.5 - self.anchor.0)
                        .rem_euclid(self.width as f64)
                        .floor() as i64,
                    (y + 0.5 - self.anchor.1)
                        .rem_euclid(self.height as f64)
                        .floor() as i64,
                )
                .map(|value| value as f32 / 255.0);
            return ([texel[0], texel[1], texel[2]], texel[3]);
        }
        let mut u = x - self.anchor.0;
        let mut v = y - self.anchor.1;
        if self.rotated {
            let (ru, rv) = (u * self.cos - v * self.sin, u * self.sin + v * self.cos);
            u = ru;
            v = rv;
        }
        u /= self.scale;
        v /= self.scale;
        if !u.is_finite() || !v.is_finite() {
            return ([0.0; 3], 0.0);
        }
        let (floor_u, floor_v) = (u.floor(), v.floor());
        let (fx, fy) = (
            ((u - floor_u) * 256.0).floor() as i32,
            ((v - floor_v) * 256.0).floor() as i32,
        );
        let (x0, y0) = (
            floor_u.rem_euclid(self.width as f64) as i64,
            floor_v.rem_euclid(self.height as f64) as i64,
        );
        let taps = [
            self.texel(x0, y0),
            self.texel(x0 + 1, y0),
            self.texel(x0, y0 + 1),
            self.texel(x0 + 1, y0 + 1),
        ];
        let mut out = [0.0f32; 4];
        for channel in 0..4 {
            let top = taps[0][channel] * 256 + (taps[1][channel] - taps[0][channel]) * fx;
            let bottom = taps[2][channel] * 256 + (taps[3][channel] - taps[2][channel]) * fx;
            out[channel] = ((top * 256 + (bottom - top) * fy + 32768) >> 16) as f32 / 255.0;
        }
        ([out[0], out[1], out[2]], out[3])
    }
}

/// One rounded reduction of each independent byte plane, including alpha.
fn reduce(pixels: &[u8], width: usize, height: usize) -> Vec<u8> {
    let (next_width, next_height) = ((width / 2).max(1), (height / 2).max(1));
    let mut out = vec![0; next_width * next_height * 4];
    for y in 0..next_height {
        for x in 0..next_width {
            let first = (y * 2 * width + x * 2) * 4;
            let right = if width > 1 { 4 } else { 0 };
            let below = if height > 1 { width * 4 } else { 0 };
            for channel in 0..4 {
                let sum = u16::from(pixels[first + channel])
                    + u16::from(pixels[first + right + channel])
                    + u16::from(pixels[first + below + channel])
                    + u16::from(pixels[first + below + right + channel]);
                out[(y * next_width + x) * 4 + channel] = ((sum + 2) / 4) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use psd_core::pattern::PatternBounds;

    fn pattern(width: usize, height: usize, gray: &[u8]) -> Pattern {
        Pattern {
            id: "tile".into(),
            name: "tile".into(),
            x: 0.0,
            y: 0.0,
            bounds: PatternBounds {
                x: 0.0,
                y: 0.0,
                w: width as f64,
                h: height as f64,
            },
            data: gray
                .iter()
                .flat_map(|&value| [value, value, value, 255])
                .collect(),
        }
    }

    fn byte(sampler: &TileSampler<'_>, x: i32, y: i32) -> u8 {
        (sampler.sample(x, y).0[0] * 255.0).round() as u8
    }

    #[test]
    fn bilinear_sampling_quantizes_positions_and_rounds_only_after_both_axes() {
        let tile = pattern(2, 1, &[0, 255]);
        let sampler = TileSampler::new(&tile, 250.0, 0.0, (0.0, 0.0)).unwrap();
        // Q8 fraction floor(0.8 * 256) = 204; continuous interpolation
        // would produce 204 instead of the native byte result 203.
        assert_eq!(byte(&sampler, 2, 0), 203);
        assert_eq!(byte(&sampler, -1, 0), byte(&sampler, 4, 0));
        let tile = pattern(2, 2, &[0, 1, 1, 1]);
        let sampler = TileSampler::new(&tile, 300.0, 0.0, (0.0, 0.0)).unwrap();
        // Two rounded lerp stages would lose this nearly-half-byte value.
        assert_eq!(byte(&sampler, 1, 1), 1);
        assert_eq!(sampler.sample(1, 1).1, 1.0);
    }

    #[test]
    fn minification_uses_a_reduced_tile_with_the_original_repeat_period() {
        let tile = pattern(
            4,
            4,
            &[
                0, 0, 80, 80, 0, 0, 80, 80, 160, 160, 240, 240, 160, 160, 240, 240,
            ],
        );
        let sampler = TileSampler::new(&tile, 47.0, 0.0, (0.0, 0.0)).unwrap();
        assert_eq!(byte(&sampler, 1, 1), 225);
        let tile = pattern(4, 2, &[0, 0, 255, 255, 0, 0, 255, 255]);
        let sampler = TileSampler::new(&tile, 25.0, 0.0, (0.0, 0.0)).unwrap();
        for x in -5..=5 {
            assert_eq!(byte(&sampler, x, 0), 128);
        }
        let tile = pattern(3, 1, &[0, 128, 255]);
        let sampler = TileSampler::new(&tile, 25.0, 0.0, (0.0, 0.0)).unwrap();
        for x in -5..=5 {
            assert_eq!(sampler.sample(x, 0), sampler.sample(x + 3, 0));
        }
    }

    #[test]
    fn reduction_averages_colour_and_alpha_planes_with_half_up_rounding() {
        let mut tile = pattern(2, 2, &[0, 1, 0, 1]);
        for (pixel, alpha) in tile.data.chunks_exact_mut(4).zip([0, 128, 255, 64]) {
            pixel[3] = alpha;
        }
        let sampler = TileSampler::new(&tile, 25.0, 0.0, (0.0, 0.0)).unwrap();
        assert_eq!(byte(&sampler, 0, 0), 1);
        assert_eq!(sampler.sample(0, 0).1, 112.0 / 255.0);
    }

    #[test]
    fn invalid_geometry_is_rejected_and_extreme_positions_do_not_overflow() {
        let mut tile = pattern(2, 1, &[0, 255]);
        for scale in [f64::NAN, f64::INFINITY] {
            assert!(TileSampler::new(&tile, scale, 0.0, (0.0, 0.0)).is_none());
        }
        assert!(TileSampler::new(&tile, 100.0, f64::INFINITY, (0.0, 0.0)).is_none());
        assert!(TileSampler::new(&tile, 100.0, 0.0, (f64::NAN, 0.0)).is_none());
        let sampler = TileSampler::new(&tile, 250.0, 0.0, (0.0, 0.0)).unwrap();
        for x in [i32::MIN, i32::MAX] {
            assert!(sampler.sample(x, x).0.iter().all(|value| value.is_finite()));
        }
        tile.bounds.w = f64::MAX;
        assert!(TileSampler::new(&tile, 100.0, 0.0, (0.0, 0.0)).is_none());
    }
}
