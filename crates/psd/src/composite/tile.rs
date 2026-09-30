//! Sampling a wrap-tiled pattern in document space.
//!
//! Anchoring follows Photoshop: "Link with Layer" anchors the tile grid at the
//! layer's effects reference point (`fxrp`, which Photoshop updates when a
//! layer moves), unlinked it anchors at the document origin, and the
//! descriptor's phase adds on top in both cases. Filtering: a 100% tile with
//! no rotation is nearest (byte crisp), magnification interpolates linearly
//! between texels on the integer grid, and minification box-averages the
//! source footprint each output pixel covers. Rotation maps document to tile
//! space as `R(angle) · (p − anchor) / scale` with `R = [[cos, −sin], [sin,
//! cos]]`, so a positive angle turns the pattern counter-clockwise.

use psd_core::pattern::Pattern;

/// A pattern placed in the document.
pub(crate) struct TileSampler<'a> {
    pattern: &'a Pattern,
    width: usize,
    height: usize,
    anchor: (f64, f64),
    inverse_scale: f64,
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
        let width = pattern.bounds.w as usize;
        let height = pattern.bounds.h as usize;
        if width == 0 || height == 0 || pattern.data.len() < width * height * 4 {
            return None;
        }
        let scale = (scale / 100.0).max(0.01);
        let radians = angle.to_radians();
        let rotated = radians.abs() > 1e-9;
        let nearest = !rotated && (scale - 1.0).abs() < 1e-6;
        Some(Self {
            pattern,
            width,
            height,
            anchor,
            inverse_scale: 1.0 / scale,
            cos: radians.cos(),
            sin: radians.sin(),
            rotated,
            nearest,
            box_filter: !nearest && !rotated && scale < 1.0,
        })
    }

    fn texel(&self, x: i64, y: i64) -> [f32; 4] {
        let x = x.rem_euclid(self.width as i64) as usize;
        let y = y.rem_euclid(self.height as i64) as usize;
        let index = (y * self.width + x) * 4;
        let pixel = &self.pattern.data[index..index + 4];
        [
            f32::from(pixel[0]) / 255.0,
            f32::from(pixel[1]) / 255.0,
            f32::from(pixel[2]) / 255.0,
            f32::from(pixel[3]) / 255.0,
        ]
    }

    /// Straight colour and alpha of the pattern under pixel `(x, y)`.
    pub fn sample(&self, x: i32, y: i32) -> ([f32; 3], f32) {
        let (x, y) = (f64::from(x), f64::from(y));
        if self.box_filter {
            let u0 = (x - self.anchor.0) * self.inverse_scale;
            let u1 = (x + 1.0 - self.anchor.0) * self.inverse_scale;
            let v0 = (y - self.anchor.1) * self.inverse_scale;
            let v1 = (y + 1.0 - self.anchor.1) * self.inverse_scale;
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
                    (sum[0] / total) as f32,
                    (sum[1] / total) as f32,
                    (sum[2] / total) as f32,
                ],
                (sum[3] / total) as f32,
            );
        }
        if self.nearest {
            // 100%: the tile grid lands on pixel cells directly.
            let texel = self.texel(
                (x + 0.5 - self.anchor.0).floor() as i64,
                (y + 0.5 - self.anchor.1).floor() as i64,
            );
            return ([texel[0], texel[1], texel[2]], texel[3]);
        }
        let mut u = x - self.anchor.0;
        let mut v = y - self.anchor.1;
        if self.rotated {
            let (ru, rv) = (u * self.cos - v * self.sin, u * self.sin + v * self.cos);
            u = ru;
            v = rv;
        }
        u *= self.inverse_scale;
        v *= self.inverse_scale;
        let (floor_u, floor_v) = (u.floor(), v.floor());
        let (fx, fy) = ((u - floor_u) as f32, (v - floor_v) as f32);
        let (x0, y0) = (floor_u as i64, floor_v as i64);
        let taps = [
            self.texel(x0, y0),
            self.texel(x0 + 1, y0),
            self.texel(x0, y0 + 1),
            self.texel(x0 + 1, y0 + 1),
        ];
        let mut out = [0.0f32; 4];
        for channel in 0..4 {
            let top = taps[0][channel] + (taps[1][channel] - taps[0][channel]) * fx;
            let bottom = taps[2][channel] + (taps[3][channel] - taps[2][channel]) * fx;
            out[channel] = top + (bottom - top) * fy;
        }
        ([out[0], out[1], out[2]], out[3])
    }
}
