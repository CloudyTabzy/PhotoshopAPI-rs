//! Gradient ramps and gradient geometry.
//!
//! A [`Ramp`] evaluates a gradient descriptor at a position `0..=1`: colour and
//! transparency stops are independent lists, a stop's midpoint belongs to the
//! stop at its right (the destination) and remaps the segment so the midpoint
//! lands on 50%, and Classic interpolation eases each segment with a
//! Catmull-Rom spline (duplicated virtual end points) scaled by the stored
//! smoothness. Two-stop colour ramps stay linear unless the caller asks for
//! end-point smoothing, which fill layers do.
//!
//! The geometry functions map a pixel to a ramp position for the five
//! point-mapped styles, following Photoshop's measured behaviour: the axis
//! passes through the centre of pixel `floor(bounds + extent / 2)`, Linear
//! holds 0.5 at the centre, Reflected / Radial / Diamond run 0 at the centre to
//! 1 at `floor(projected span × scale / 2)`, Radial and Diamond are isotropic
//! in the angle-rotated frame, and Angle sweeps one ramp per revolution
//! clockwise from the angle direction, ignoring scale.

use psd_core::effect_enums::{GradientInterpolation, GradientStyle};
use psd_core::{Color, Gradient, GradientKind, StopSource};

/// One resolved colour stop.
#[derive(Clone, Copy)]
struct ColorStop {
    location: f32,
    midpoint: f32,
    color: [f32; 3],
}

/// One resolved transparency stop.
#[derive(Clone, Copy)]
struct AlphaStop {
    location: f32,
    midpoint: f32,
    opacity: f32,
}

/// A gradient ready to evaluate.
#[derive(Clone)]
pub(crate) struct Ramp {
    colors: Vec<ColorStop>,
    alphas: Vec<AlphaStop>,
    /// `Intr / 4096`, 1.0 for a fully smooth gradient.
    smoothness: f32,
    interpolation: GradientInterpolation,
    /// Fill layers ease even two-stop ramps.
    endpoint_smoothing: bool,
}

impl Ramp {
    /// Build a ramp. Returns `None` for gradients this renderer cannot
    /// evaluate (noise gradients).
    pub fn new(
        gradient: &Gradient,
        interpolation: Option<GradientInterpolation>,
        endpoint_smoothing: bool,
    ) -> Option<Self> {
        let solid = match &gradient.kind {
            GradientKind::Solid(solid) => solid.clone(),
            GradientKind::Noise(noise) => synthesize_noise_gradient(noise),
        };
        let mut colors: Vec<ColorStop> = solid
            .color_stops
            .iter()
            .map(|stop| ColorStop {
                location: (stop.location as f32 / 4096.0).clamp(0.0, 1.0),
                midpoint: (stop.midpoint as f32 / 100.0).clamp(0.0001, 0.9999),
                color: stop_color(&stop.source),
            })
            .collect();
        colors.sort_by(|a, b| a.location.total_cmp(&b.location));
        let mut alphas: Vec<AlphaStop> = solid
            .transparency_stops
            .iter()
            .map(|stop| AlphaStop {
                location: (stop.location as f32 / 4096.0).clamp(0.0, 1.0),
                midpoint: (stop.midpoint as f32 / 100.0).clamp(0.0001, 0.9999),
                opacity: (stop.opacity as f32 / 100.0).clamp(0.0, 1.0),
            })
            .collect();
        alphas.sort_by(|a, b| a.location.total_cmp(&b.location));
        Some(Self {
            colors,
            alphas,
            smoothness: (solid.smoothness as f32 / 4096.0).clamp(0.0, 1.0),
            interpolation: interpolation.unwrap_or(GradientInterpolation::Classic),
            endpoint_smoothing,
        })
    }

    /// Straight colour and opacity at `position` (`0..=1`).
    pub fn sample(&self, position: f32) -> ([f32; 3], f32) {
        (self.color(position), self.alpha(position))
    }

    fn color(&self, position: f32) -> [f32; 3] {
        let stops = &self.colors;
        let (Some(first), Some(last)) = (stops.first(), stops.last()) else {
            return [position; 3];
        };
        if position <= first.location {
            return first.color;
        }
        if position >= last.location {
            return last.color;
        }
        for index in 1..stops.len() {
            let right = stops[index];
            let left = stops[index - 1];
            if position <= right.location {
                let span = (right.location - left.location).max(0.0001);
                let t = remap((position - left.location) / span, right.midpoint);
                return match self.interpolation {
                    GradientInterpolation::Perceptual => {
                        let a = to_oklab(left.color);
                        let b = to_oklab(right.color);
                        from_oklab([
                            a[0] + (b[0] - a[0]) * t,
                            a[1] + (b[1] - a[1]) * t,
                            a[2] + (b[2] - a[2]) * t,
                        ])
                    }
                    GradientInterpolation::Linear => {
                        let mix = |a: f32, b: f32| {
                            let (a, b) = (srgb_to_linear(a), srgb_to_linear(b));
                            linear_to_srgb(a + (b - a) * t)
                        };
                        [
                            mix(left.color[0], right.color[0]),
                            mix(left.color[1], right.color[1]),
                            mix(left.color[2], right.color[2]),
                        ]
                    }
                    _ => {
                        let previous = if index > 1 {
                            stops[index - 2].color
                        } else {
                            left.color
                        };
                        let next = stops.get(index + 1).map_or(right.color, |s| s.color);
                        let smoothness = if stops.len() > 2 || self.endpoint_smoothing {
                            self.smoothness
                        } else {
                            0.0
                        };
                        let mut out = [0.0; 3];
                        for channel in 0..3 {
                            let linear = left.color[channel]
                                + (right.color[channel] - left.color[channel]) * t;
                            let cubic = catmull_rom(
                                previous[channel],
                                left.color[channel],
                                right.color[channel],
                                next[channel],
                                t,
                            );
                            out[channel] = (linear + (cubic - linear) * smoothness).clamp(0.0, 1.0);
                        }
                        out
                    }
                };
            }
        }
        last.color
    }

    fn alpha(&self, position: f32) -> f32 {
        let stops = &self.alphas;
        let (Some(first), Some(last)) = (stops.first(), stops.last()) else {
            return 1.0;
        };
        if position <= first.location {
            return first.opacity;
        }
        if position >= last.location {
            return last.opacity;
        }
        for index in 1..stops.len() {
            let right = stops[index];
            let left = stops[index - 1];
            if position <= right.location {
                let span = (right.location - left.location).max(0.0001);
                let t = remap((position - left.location) / span, right.midpoint);
                if self.endpoint_smoothing && self.smoothness > 0.0 {
                    let previous = if index > 1 {
                        stops[index - 2].opacity
                    } else {
                        left.opacity
                    };
                    let next = stops.get(index + 1).map_or(right.opacity, |s| s.opacity);
                    let linear = left.opacity + (right.opacity - left.opacity) * t;
                    let cubic = catmull_rom(previous, left.opacity, right.opacity, next, t);
                    return (linear + (cubic - linear) * self.smoothness).clamp(0.0, 1.0);
                }
                return left.opacity + (right.opacity - left.opacity) * t;
            }
        }
        last.opacity
    }
}

fn stop_color(source: &StopSource) -> [f32; 3] {
    match source {
        StopSource::User(color) => color_rgb(color),
        StopSource::Foreground => [0.0; 3],
        StopSource::Background => [1.0; 3],
    }
}

/// A noise gradient as ordinary stops. Photoshop's own random sequence is not
/// reproducible, so this draws a deterministic stand-in from the stored seed:
/// uniformly random colours inside the stored per-channel ranges, with more
/// stops (and so faster changes) the rougher the gradient.
fn synthesize_noise_gradient(noise: &psd_core::NoiseGradient) -> psd_core::SolidGradient {
    use psd_core::NoiseColorModel;

    let mut state = (noise.seed as u32 as u64) ^ 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        // SplitMix64, mapped to 0..1.
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    };
    let range = |index: usize, fallback: (f64, f64)| -> (f64, f64) {
        match (noise.minimum.get(index), noise.maximum.get(index)) {
            (Some(&low), Some(&high)) => (f64::from(low.min(high)), f64::from(low.max(high))),
            _ => fallback,
        }
    };
    let roughness = (f64::from(noise.roughness) / 4096.0).clamp(0.0, 1.0);
    let segments = 3 + (roughness * 40.0).round() as usize;
    let mut color_stops = Vec::new();
    let mut transparency_stops = Vec::new();
    for index in 0..=segments {
        let location = (index as f64 / segments as f64 * 4096.0).round() as i32;
        let mut channel = |slot: usize, scale: f64| {
            let (low, high) = range(slot, (0.0, 100.0));
            (low + (high - low) * next()) / 100.0 * scale
        };
        let rgb = match noise.color_model {
            NoiseColorModel::Rgb => [channel(0, 1.0), channel(1, 1.0), channel(2, 1.0)],
            NoiseColorModel::Hsb => {
                let hue = channel(0, 1.0);
                let saturation = channel(1, 1.0);
                let brightness = channel(2, 1.0);
                // The hue range is stored in degrees out of 360, not percent.
                let hue = (hue * 100.0 / 360.0).rem_euclid(1.0);
                hsb_to_rgb(hue as f32, saturation as f32, brightness as f32).map(f64::from)
            }
            NoiseColorModel::Lab => {
                let lightness = channel(0, 100.0);
                let a = channel(1, 255.0) - 128.0;
                let b = channel(2, 255.0) - 128.0;
                lab_to_rgb(lightness, a, b).map(f64::from)
            }
        };
        color_stops.push(psd_core::ColorStop {
            source: StopSource::User(Color::Rgb {
                red: rgb[0].clamp(0.0, 1.0) * 255.0,
                green: rgb[1].clamp(0.0, 1.0) * 255.0,
                blue: rgb[2].clamp(0.0, 1.0) * 255.0,
            }),
            location,
            midpoint: 50,
        });
        let opacity = if noise.add_transparency {
            let (low, high) = range(3, (0.0, 100.0));
            low + (high - low) * next()
        } else {
            100.0
        };
        transparency_stops.push(psd_core::TransparencyStop {
            opacity,
            location,
            midpoint: 50,
        });
    }
    psd_core::SolidGradient {
        smoothness: 4096.0,
        color_stops,
        transparency_stops,
    }
}

/// A descriptor colour as straight RGB `0..=1`.
pub(crate) fn color_rgb(color: &Color) -> [f32; 3] {
    match color {
        Color::Rgb { red, green, blue } => [
            (*red as f32 / 255.0).clamp(0.0, 1.0),
            (*green as f32 / 255.0).clamp(0.0, 1.0),
            (*blue as f32 / 255.0).clamp(0.0, 1.0),
        ],
        // Photoshop writes a gray as a percentage of black ink.
        Color::Gray { gray } => {
            let value = (1.0 - *gray as f32 / 100.0).clamp(0.0, 1.0);
            [value; 3]
        }
        // CMYK components are ink percentages; the profile-free conversion.
        Color::Cmyk {
            cyan,
            magenta,
            yellow,
            black,
        } => {
            let c = (*cyan as f32 / 100.0).clamp(0.0, 1.0);
            let m = (*magenta as f32 / 100.0).clamp(0.0, 1.0);
            let y = (*yellow as f32 / 100.0).clamp(0.0, 1.0);
            let k = (*black as f32 / 100.0).clamp(0.0, 1.0);
            [
                (1.0 - c) * (1.0 - k),
                (1.0 - m) * (1.0 - k),
                (1.0 - y) * (1.0 - k),
            ]
        }
        Color::Hsb {
            hue,
            saturation,
            brightness,
        } => hsb_to_rgb(
            (*hue as f32 / 360.0).rem_euclid(1.0),
            (*saturation as f32 / 100.0).clamp(0.0, 1.0),
            (*brightness as f32 / 100.0).clamp(0.0, 1.0),
        ),
        Color::Lab { lightness, a, b } => lab_to_rgb(*lightness, *a, *b),
    }
}

/// CIE L*a*b* (D50) to sRGB: XYZ, Bradford adaptation to D65, the sRGB matrix
/// and its transfer curve.
fn lab_to_rgb(lightness: f64, a: f64, b: f64) -> [f32; 3] {
    let fy = (lightness + 16.0) / 116.0;
    let fx = fy + a / 500.0;
    let fz = fy - b / 200.0;
    let epsilon = 216.0 / 24389.0;
    let kappa = 24389.0 / 27.0;
    let cube = |f: f64| {
        if f * f * f > epsilon {
            f * f * f
        } else {
            (116.0 * f - 16.0) / kappa
        }
    };
    let y = if lightness > kappa * epsilon {
        fy * fy * fy
    } else {
        lightness / kappa
    };
    let (x, z) = (cube(fx) * 0.9642, cube(fz) * 0.8249);
    // D50 to D65 (Bradford), then XYZ to linear sRGB.
    let xd = 0.955_576_6 * x - 0.023_039_3 * y + 0.063_163_6 * z;
    let yd = -0.028_289_5 * x + 1.009_941_6 * y + 0.021_007_7 * z;
    let zd = 0.012_298_2 * x - 0.020_483_0 * y + 1.329_909_8 * z;
    let linear = [
        3.240_454_2 * xd - 1.537_138_5 * yd - 0.498_531_4 * zd,
        -0.969_266_0 * xd + 1.876_010_8 * yd + 0.041_556_0 * zd,
        0.055_643_4 * xd - 0.204_025_9 * yd + 1.057_225_2 * zd,
    ];
    linear.map(|value| linear_to_srgb(value as f32))
}

fn hsb_to_rgb(h: f32, s: f32, v: f32) -> [f32; 3] {
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    match (i as i32).rem_euclid(6) {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

/// Remap a segment position so the stop's midpoint lands on 0.5.
fn remap(value: f32, midpoint: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    if (midpoint - 0.5).abs() < f32::EPSILON {
        return value;
    }
    if value <= midpoint {
        0.5 * value / midpoint
    } else {
        0.5 + 0.5 * (value - midpoint) / (1.0 - midpoint)
    }
}

fn catmull_rom(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
    let t2 = t * t;
    let t3 = t2 * t;
    0.5 * (2.0 * p1
        + (-p0 + p2) * t
        + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2
        + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t3)
}

pub(crate) fn srgb_to_linear(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

pub(crate) fn linear_to_srgb(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

fn to_oklab(color: [f32; 3]) -> [f32; 3] {
    let [r, g, b] = color.map(srgb_to_linear);
    let l = (0.412_221_47 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

fn from_oklab(lab: [f32; 3]) -> [f32; 3] {
    let l = (lab[0] + 0.396_337_78 * lab[1] + 0.215_803_76 * lab[2]).powi(3);
    let m = (lab[0] - 0.105_561_346 * lab[1] - 0.063_854_17 * lab[2]).powi(3);
    let s = (lab[0] - 0.089_484_18 * lab[1] - 1.291_485_5 * lab[2]).powi(3);
    [
        4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
        -1.268_438 * l + 2.609_757_4 * m - 0.341_319_4 * s,
        -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s,
    ]
    .map(linear_to_srgb)
}

/// Placement of a point-mapped gradient.
#[derive(Clone, Copy)]
pub(crate) struct Placement {
    pub style: GradientStyle,
    /// Degrees.
    pub angle: f32,
    /// Percent, 100 = natural size.
    pub scale: f32,
    pub reverse: bool,
    /// Offset of the centre, percent of the bounds' extent.
    pub offset: (f32, f32),
}

/// The half-open pixel bounds a gradient spans: `left, top, width, height`.
#[derive(Clone, Copy)]
pub(crate) struct Span {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}

/// How a span's length along the axis is taken.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpanBasis {
    /// Layer styles: the bounds' projection onto the axis, quantised to whole
    /// pixels and with the centre snapped to a pixel.
    Projection,
    /// Gradient fill layers: the chord through the centre, unquantised.
    CenterChord,
}

/// The ramp position of pixel `(x, y)` (document coordinates).
pub(crate) fn position(placement: &Placement, span: Span, basis: SpanBasis, x: i32, y: i32) -> f32 {
    let snap = basis == SpanBasis::Projection;
    let mut center_x = span.left + span.width * (0.5 + placement.offset.0 / 100.0);
    let mut center_y = span.top + span.height * (0.5 + placement.offset.1 / 100.0);
    if snap {
        center_x = center_x.floor() + 0.5;
        center_y = center_y.floor() + 0.5;
    }
    let px = x as f32 + 0.5;
    let py = y as f32 + 0.5;
    let radians = placement.angle.to_radians();
    let (sin, cos) = radians.sin_cos();
    let local_x = (px - center_x) * cos - (py - center_y) * sin;
    let local_y = (px - center_x) * sin + (py - center_y) * cos;

    let (abs_cos, abs_sin) = (cos.abs(), sin.abs());
    let projected = match basis {
        SpanBasis::CenterChord => {
            let along_width = if abs_cos > 1e-6 {
                span.width / abs_cos
            } else {
                f32::INFINITY
            };
            let along_height = if abs_sin > 1e-6 {
                span.height / abs_sin
            } else {
                f32::INFINITY
            };
            along_width.min(along_height).max(1.0)
        }
        SpanBasis::Projection => (abs_cos * span.width + abs_sin * span.height).max(1.0),
    };
    let scale = (placement.scale / 100.0).max(0.01);
    let raw_half = projected * scale * 0.5;
    let half_ramp = if snap {
        raw_half.floor().max(1.0)
    } else {
        raw_half.max(0.5)
    };
    let mut position = match placement.style {
        GradientStyle::Radial => (local_x * local_x + local_y * local_y).sqrt() / half_ramp,
        GradientStyle::Angle => {
            if local_x == 0.0 && local_y == 0.0 {
                // The singular centre pixel shows the quarter-sweep colour.
                0.25
            } else {
                let turns = local_y.atan2(local_x) / std::f32::consts::TAU;
                if turns < 0.0 {
                    turns + 1.0
                } else {
                    turns
                }
            }
        }
        GradientStyle::Reflected => local_x.abs() / half_ramp,
        GradientStyle::Diamond => (local_x.abs() + local_y.abs()) / half_ramp,
        _ => 0.5 + local_x / (2.0 * half_ramp),
    };
    if placement.reverse {
        position = 1.0 - position;
    }
    position.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use psd_core::{ColorStop as GradientColorStop, SolidGradient, TransparencyStop};

    fn noise(seed: i32, roughness: i32) -> Gradient {
        Gradient {
            label: "Gradient".into(),
            name: "Custom".into(),
            kind: GradientKind::Noise(psd_core::NoiseGradient {
                restrict_colors: true,
                add_transparency: false,
                color_model: psd_core::NoiseColorModel::Rgb,
                seed,
                roughness,
                minimum: vec![0, 0, 0, 0],
                maximum: vec![100, 100, 100, 100],
            }),
        }
    }

    #[test]
    fn a_noise_gradient_draws_a_deterministic_ramp_from_its_seed() {
        let first = Ramp::new(&noise(7, 2048), None, false).expect("noise gradients render");
        let again = Ramp::new(&noise(7, 2048), None, false).unwrap();
        let other = Ramp::new(&noise(8, 2048), None, false).unwrap();
        let samples = |ramp: &Ramp| -> Vec<[f32; 3]> {
            (0..=10).map(|i| ramp.sample(i as f32 / 10.0).0).collect()
        };
        assert_eq!(samples(&first), samples(&again));
        assert_ne!(samples(&first), samples(&other));
        // Opaque, and not a flat colour.
        assert_eq!(first.sample(0.5).1, 1.0);
        let reds: Vec<f32> = samples(&first).iter().map(|c| c[0]).collect();
        assert!(reds.iter().any(|r| (r - reds[0]).abs() > 0.05));
    }

    fn gradient(
        stops: &[(i32, i32, [f64; 3])],
        transparency: &[(i32, f64)],
        smoothness: f64,
    ) -> Gradient {
        Gradient {
            label: "Gradient".into(),
            name: "test".into(),
            kind: GradientKind::Solid(SolidGradient {
                smoothness,
                color_stops: stops
                    .iter()
                    .map(|(location, midpoint, rgb)| GradientColorStop {
                        source: StopSource::User(Color::Rgb {
                            red: rgb[0],
                            green: rgb[1],
                            blue: rgb[2],
                        }),
                        location: *location,
                        midpoint: *midpoint,
                    })
                    .collect(),
                transparency_stops: transparency
                    .iter()
                    .map(|(location, opacity)| TransparencyStop {
                        opacity: *opacity,
                        location: *location,
                        midpoint: 50,
                    })
                    .collect(),
            }),
        }
    }

    #[test]
    fn a_two_stop_ramp_is_linear_and_the_midpoint_moves_the_halfway_colour() {
        let plain = gradient(&[(0, 50, [0.0; 3]), (4096, 50, [255.0; 3])], &[], 4096.0);
        let ramp = Ramp::new(&plain, None, false).unwrap();
        assert!((ramp.sample(0.25).0[0] - 0.25).abs() < 1e-4);

        // The destination stop's midpoint at 25%: half the colour change has
        // happened a quarter of the way along.
        let skewed = gradient(&[(0, 50, [0.0; 3]), (4096, 25, [255.0; 3])], &[], 4096.0);
        let ramp = Ramp::new(&skewed, None, false).unwrap();
        assert!((ramp.sample(0.25).0[0] - 0.5).abs() < 1e-4);
    }

    #[test]
    fn end_point_smoothing_eases_a_two_stop_ramp() {
        let plain = gradient(&[(0, 50, [0.0; 3]), (4096, 50, [255.0; 3])], &[], 4096.0);
        let eased = Ramp::new(&plain, None, true).unwrap();
        // 0.5t + 1.5t^2 - t^3 at t = 0.25.
        let expected = 0.5 * 0.25 + 1.5 * 0.0625 - 0.015625;
        assert!((eased.sample(0.25).0[0] - expected).abs() < 1e-4);
    }

    #[test]
    fn transparency_stops_are_independent_of_colour_stops() {
        let g = gradient(
            &[(0, 50, [255.0, 0.0, 0.0]), (4096, 50, [255.0, 0.0, 0.0])],
            &[(0, 100.0), (4096, 0.0)],
            4096.0,
        );
        let ramp = Ramp::new(&g, None, false).unwrap();
        let (color, alpha) = ramp.sample(0.5);
        assert_eq!(color, [1.0, 0.0, 0.0]);
        assert!((alpha - 0.5).abs() < 1e-4);
    }

    fn placement(style: GradientStyle) -> Placement {
        Placement {
            style,
            angle: 0.0,
            scale: 100.0,
            reverse: false,
            offset: (0.0, 0.0),
        }
    }

    #[test]
    fn linear_geometry_runs_across_the_bounds() {
        let span = Span {
            left: 0.0,
            top: 0.0,
            width: 10.0,
            height: 4.0,
        };
        let linear = placement(GradientStyle::Linear);
        let first = position(&linear, span, SpanBasis::Projection, 0, 1);
        let last = position(&linear, span, SpanBasis::Projection, 9, 1);
        // The centre snaps to pixel 5's middle and the ramp spans five pixels
        // either side of it, so the last pixel sits at 0.9.
        assert_eq!(first, 0.0);
        assert!((last - 0.9).abs() < 1e-4, "{last}");
        let reversed = Placement {
            reverse: true,
            ..linear
        };
        assert_eq!(position(&reversed, span, SpanBasis::Projection, 0, 1), 1.0);
    }

    #[test]
    fn reflected_geometry_is_zero_at_the_centre() {
        let span = Span {
            left: 0.0,
            top: 0.0,
            width: 11.0,
            height: 11.0,
        };
        let reflected = placement(GradientStyle::Reflected);
        assert_eq!(position(&reflected, span, SpanBasis::Projection, 5, 5), 0.0);
        assert!(position(&reflected, span, SpanBasis::Projection, 10, 5) > 0.8);
    }
}
