//! Adjustment and fill layers, as the compositor applies them.
//!
//! Adjustment layers transform the backdrop in place, inside their mask. The
//! math follows the calibration records where Photoshop pins it:
//!
//! - **Legacy Brightness/Contrast** uses Photoshop's own hybrid order:
//!   brightness folds into the *input* for positive contrast
//!   (`(v + b − 127.5) × 100/(100 − c) + 127.5`), adds to the *output* for
//!   negative contrast, `c = 100` is a hard threshold at 127 and `c = 0` a
//!   plain add.
//! - **Modern Brightness/Contrast** composes a gain ray `2^(b/110)` (cubic
//!   Hermite tail, `b > 100` composition, bisection inverse below zero) with a
//!   parabolic contrast curve `β = 1 − 0.0076c`.
//! - **Curves** interpolate with a natural cubic through the control points
//!   (zero second derivative at both ends, clamped outside) — Photoshop's own
//!   interpolation — per-channel tables first, then the composite table.
//!
//! - **Exposure** applies gain and offset in linear light, then gamma, then
//!   re-encodes.
//! - **Hue/Saturation** runs Photoshop's integer pipeline (lightness per
//!   channel, then a saturation ratio on the half chroma and a hue turn in
//!   whole steps of a 1530-step wheel; colorize rebuilds from the source's
//!   lightness).
//!
//! Approximated, and documented as such: the saturation ratio is a closed
//! form where Photoshop's is a measured table, the six Hue/Saturation ranges
//! are not applied, Vibrance follows the usual falloff, Color Balance is a
//! smooth approximation, and Selective Color, Color Lookup and Content
//! Generator are not rendered.

use psd_core::adjustments::{
    AdjustmentData, AdjustmentKind, ColorBalanceValues, Curve, CurveData, CurvePoint, GradientMap,
    HueSaturationValues, Levels, PhotoFilter, PhotoFilterColor,
};
use psd_core::{
    Color, Descriptor, DescriptorValue, GradientKind, Result, SolidGradient, StopSource,
    TransparencyStop,
};

use super::ramp::{self, linear_to_srgb, srgb_to_linear, Placement, Ramp, Span, SpanBasis};
use super::{Canvas, Compositor, Content, Plane, Rect};
use crate::layer::Layer;
use crate::BitDepth;
use psd_core::effect_enums::{GradientInterpolation, GradientStyle};

/// Apply the layer's adjustment to the canvas, inside its mask.
///
/// Strength is the layer's opacity and fill (both scale an adjustment), times
/// its mask, times the clipping base when clipped, times the Blend If gate:
/// This Layer is evaluated on the adjusted output and Underlying Layer on the
/// pre-adjustment backdrop.
pub fn apply<T: BitDepth>(
    compositor: &Compositor<'_, T>,
    layer: &Layer<T>,
    canvas: &mut Canvas,
    clip_base: Option<&Plane>,
) -> Result<()> {
    let blocks = layer.adjustments()?;
    let Some(block) = blocks.iter().find(|block| {
        !matches!(
            block.kind,
            AdjustmentKind::SolidColor | AdjustmentKind::GradientFill | AdjustmentKind::PatternFill
        )
    }) else {
        return Ok(());
    };
    let generator = blocks.iter().find_map(|block| match &block.data {
        AdjustmentData::ContentGenerator(generator) => Some(generator),
        _ => None,
    });
    let gray = matches!(
        compositor.document.color_mode,
        psd_core::ColorMode::Grayscale | psd_core::ColorMode::Bitmap | psd_core::ColorMode::Duotone
    );
    let operation = Operation::new(&block.data, generator, gray);
    if !operation.supported {
        return Ok(());
    }

    let strength = compositor.opacity(layer, true);
    if strength <= 0.0 {
        return Ok(());
    }
    let restricted = compositor.channel_restrictions(layer);
    let canvas_rect = Rect::new(
        canvas.origin.1,
        canvas.origin.0,
        canvas.origin.1 + canvas.height as i32,
        canvas.origin.0 + canvas.width as i32,
    );
    let mask = compositor.mask_plane(layer, canvas_rect);
    let width = canvas.width as usize;
    let ranges = &layer.blending_ranges;
    let gated = compositor.options.blend_if && super::blend_if_active(ranges);
    for y in canvas_rect.top..canvas_rect.bottom {
        for x in canvas_rect.left..canvas_rect.right {
            let Some(index) = canvas.index(i64::from(x), i64::from(y)) else {
                continue;
            };
            let local = (y - canvas_rect.top) as usize * width + (x - canvas_rect.left) as usize;
            let mut coverage = strength * mask.as_ref().map_or(1.0, |mask| mask[local]);
            if let Some(clip_base) = clip_base {
                coverage *= clip_base.at(x, y);
            }
            if coverage <= 0.0 {
                continue;
            }
            let (color, alpha) = canvas.pixel(index);
            let adjusted = operation.apply(color);
            if gated {
                coverage *= super::blend_if_gate(ranges, color, alpha, adjusted);
                if coverage <= 0.0 {
                    continue;
                }
            }
            let mut result = color;
            for (channel, value) in result.iter_mut().enumerate() {
                if !restricted[channel] {
                    *value = color[channel] + (adjusted[channel] - color[channel]) * coverage;
                }
            }
            canvas.set(index, result, alpha);
        }
    }
    Ok(())
}

/// One adjustment, resolved once per layer.
struct Operation {
    data: AdjustmentData,
    supported: bool,
    /// Brightness, contrast and whether the legacy algorithm applies: a
    /// modern layer keeps them in its content-generator block, a legacy one
    /// in the `brit` record.
    brightness_contrast: (f32, f32, bool),
    /// A single-channel document: its one channel's Levels record and Curves
    /// curve sit where a colour document keeps the red channel's.
    gray: bool,
}

impl Operation {
    fn new(
        data: &AdjustmentData,
        generator: Option<&psd_core::adjustments::ContentGenerator>,
        gray: bool,
    ) -> Self {
        let brightness_contrast = match (data, generator) {
            (AdjustmentData::BrightnessContrast(legacy), Some(generator))
                if generator.brightness().is_some() =>
            {
                (
                    generator.brightness().unwrap_or(0.0) as f32,
                    generator.contrast().unwrap_or(0.0) as f32,
                    generator.use_legacy().unwrap_or(false) || legacy.lab_only,
                )
            }
            (AdjustmentData::BrightnessContrast(legacy), _) => (
                f32::from(legacy.brightness),
                f32::from(legacy.contrast),
                true,
            ),
            _ => (0.0, 0.0, true),
        };
        let supported = matches!(
            data,
            AdjustmentData::BrightnessContrast(_)
                | AdjustmentData::Levels(_)
                | AdjustmentData::Curves(_)
                | AdjustmentData::Exposure(_)
                | AdjustmentData::HueSaturation(_)
                | AdjustmentData::ColorBalance(_)
                | AdjustmentData::BlackAndWhite(_)
                | AdjustmentData::PhotoFilter(_)
                | AdjustmentData::ChannelMixer(_)
                | AdjustmentData::Invert { .. }
                | AdjustmentData::Posterize(_)
                | AdjustmentData::Threshold(_)
                | AdjustmentData::GradientMap(_)
                | AdjustmentData::Vibrance(_)
        );
        Self {
            data: data.clone(),
            supported,
            brightness_contrast,
            gray,
        }
    }

    fn apply(&self, color: [f32; 3]) -> [f32; 3] {
        match &self.data {
            AdjustmentData::Invert { .. } => [1.0 - color[0], 1.0 - color[1], 1.0 - color[2]],
            AdjustmentData::BrightnessContrast(_) => {
                let (brightness, contrast, legacy) = self.brightness_contrast;
                color.map(|value| brightness_contrast(value, brightness, contrast, legacy))
            }
            AdjustmentData::Levels(levels) => levels_apply(color, levels, self.gray),
            AdjustmentData::Curves(curves) => curves_apply(color, &curves.curves, self.gray),
            AdjustmentData::Exposure(exposure) => {
                // Photoshop works in linear light: the gain and the offset
                // apply there, the gamma correction follows, and the result
                // is re-encoded. (Fitted against Photoshop's own renders to
                // under one level.)
                let gain = 2f32.powf(exposure.exposure);
                let inverse_gamma = 1.0 / exposure.gamma.max(0.01);
                let gray = self.gray;
                color.map(|value| {
                    // A grayscale document's working space is gamma 1.8.
                    let linear = if gray {
                        value.powf(1.8)
                    } else {
                        srgb_to_linear(value)
                    } * gain
                        + exposure.offset;
                    let corrected = linear.max(0.0).powf(inverse_gamma);
                    if gray {
                        corrected.clamp(0.0, 1.0).powf(1.0 / 1.8)
                    } else {
                        linear_to_srgb(corrected)
                    }
                })
            }
            AdjustmentData::HueSaturation(settings) => hue_saturation(
                color,
                settings.master,
                settings.colorize,
                settings.colorization,
            ),
            AdjustmentData::ColorBalance(settings) => color_balance(color, settings),
            AdjustmentData::BlackAndWhite(settings) => black_and_white(color, &settings.descriptor),
            AdjustmentData::PhotoFilter(settings) => photo_filter(color, settings),
            AdjustmentData::ChannelMixer(settings) => {
                let mut out = [0.0; 3];
                for (channel, mix) in settings.mixes.iter().take(3).enumerate() {
                    let mut value = f32::from(mix.constant) / 100.0;
                    for (source, amount) in mix.sources.iter().take(3).enumerate() {
                        value += color[source] * f32::from(*amount) / 100.0;
                    }
                    out[channel] = value.clamp(0.0, 1.0);
                }
                if settings.monochrome {
                    let gray = luminance(out);
                    [gray, gray, gray]
                } else {
                    out
                }
            }
            AdjustmentData::Posterize(settings) => {
                let levels = f32::from(settings.levels.max(2));
                let mut out = [0.0; 3];
                for channel in 0..3 {
                    let stepped = (color[channel] * levels).floor() / (levels - 1.0);
                    out[channel] = stepped.clamp(0.0, 1.0);
                }
                out
            }
            AdjustmentData::Threshold(settings) => {
                let level = f32::from(settings.level) / 255.0;
                let value = if luminance(color) >= level { 1.0 } else { 0.0 };
                [value, value, value]
            }
            AdjustmentData::GradientMap(settings) => gradient_map(color, settings),
            AdjustmentData::Vibrance(settings) => vibrance(color, &settings.descriptor),
            _ => color,
        }
    }
}

fn luminance(color: [f32; 3]) -> f32 {
    0.299 * color[0] + 0.587 * color[1] + 0.114 * color[2]
}

fn preserve_luminosity(before: [f32; 3], after: [f32; 3]) -> [f32; 3] {
    let delta = luminance(before) - luminance(after);
    [
        (after[0] + delta).clamp(0.0, 1.0),
        (after[1] + delta).clamp(0.0, 1.0),
        (after[2] + delta).clamp(0.0, 1.0),
    ]
}

// ---------------------------------------------------------------------------
// Brightness/Contrast
// ---------------------------------------------------------------------------

fn brightness_contrast(value: f32, brightness: f32, contrast: f32, legacy: bool) -> f32 {
    if legacy {
        legacy_brightness_contrast(value, brightness, contrast)
    } else {
        modern_contrast(modern_brightness(value, brightness), contrast)
    }
}

fn legacy_brightness_contrast(value: f32, b: f32, c: f32) -> f32 {
    let v = value * 255.0;
    let result = if c == 0.0 {
        v + b
    } else if c > 0.0 && c < 100.0 {
        (v + b - 127.5) * 100.0 / (100.0 - c) + 127.5
    } else if c >= 100.0 {
        if v + b >= 127.0 {
            255.0
        } else {
            0.0
        }
    } else {
        (v - 127.5) * (100.0 + c) / 100.0 + 127.5 + b
    };
    (result.round() / 255.0).clamp(0.0, 1.0)
}

fn modern_brightness(value: f32, amount: f32) -> f32 {
    if amount == 0.0 {
        return value;
    }
    if amount > 100.0 {
        return modern_brightness_curve(modern_brightness_curve(value, 100.0), amount - 100.0);
    }
    if amount > 0.0 {
        return modern_brightness_curve(value, amount);
    }
    // The exact inverse of the positive curve, by deterministic bisection.
    let (mut low, mut high) = (0.0f32, 1.0f32);
    for _ in 0..64 {
        let mid = 0.5 * (low + high);
        if modern_brightness_curve(mid, -amount) < value {
            low = mid;
        } else {
            high = mid;
        }
    }
    0.5 * (low + high)
}

/// The gain ray `σ = 2^(amount/110)` up to output 0.5, then one cubic Hermite
/// to (1, 1) with end slope `τ = max(0.1, 1 / (1 + 12(σ − 1)))`.
fn modern_brightness_curve(value: f32, amount: f32) -> f32 {
    let sigma = 2f32.powf(amount / 110.0);
    let tau = (1.0 / (1.0 + 12.0 * (sigma - 1.0))).clamp(0.1, 1.0);
    let x0 = 0.5 / sigma;
    if value <= x0 {
        return (value * sigma).clamp(0.0, 1.0);
    }
    // A cubic Hermite from (x0, 0.5) with the ray's slope σ to (1, 1) with
    // slope τ; the tangents scale by the interval's length.
    let length = 1.0 - x0;
    let t = ((value - x0) / length).clamp(0.0, 1.0);
    let h00 = 2.0 * t * t * t - 3.0 * t * t + 1.0;
    let h10 = t * t * t - 2.0 * t * t + t;
    let h01 = -2.0 * t * t * t + 3.0 * t * t;
    let h11 = t * t * t - t * t;
    (h00 * 0.5 + h10 * sigma * length + h01 + h11 * tau * length).clamp(0.0, 1.0)
}

fn modern_contrast(value: f32, c: f32) -> f32 {
    if c == 0.0 {
        return value;
    }
    let beta = 1.0 - 0.0076 * c;
    if value <= 0.5 {
        (2.0 - 2.0 * beta) * value * value + beta * value
    } else {
        let u = 1.0 - value;
        1.0 - ((2.0 - 2.0 * beta) * u * u + beta * u)
    }
}

// ---------------------------------------------------------------------------
// Levels and Curves
// ---------------------------------------------------------------------------

fn levels_apply(color: [f32; 3], levels: &Levels, gray: bool) -> [f32; 3] {
    let map = |record: &psd_core::adjustments::LevelsRecord, value: f32| -> f32 {
        let floor = f32::from(record.input_floor);
        let ceiling = f32::from(record.input_ceiling);
        let output_floor = f32::from(record.output_floor);
        let output_ceiling = f32::from(record.output_ceiling);
        let gamma = if record.gamma == 0 {
            1.0
        } else {
            f32::from(record.gamma) / 100.0
        };
        let scaled = ((value * 255.0 - floor) / (ceiling - floor).max(1e-6)).clamp(0.0, 1.0);
        let corrected = scaled.powf(1.0 / gamma.max(1e-6));
        ((output_floor + corrected * (output_ceiling - output_floor)) / 255.0).clamp(0.0, 1.0)
    };
    // Record 0 is the composite, then R, G, B. The per-channel records apply
    // first and the composite record acts on their result. A grayscale
    // document's single channel uses record 1.
    if gray {
        let mut value = color[0];
        if let Some(record) = levels.records.get(1) {
            value = map(record, value);
        }
        if let Some(record) = levels.records.first() {
            value = map(record, value);
        }
        return [value; 3];
    }
    let mut out = color;
    for (value, record) in out.iter_mut().zip(levels.records.iter().skip(1)) {
        *value = map(record, *value);
    }
    if let Some(record) = levels.records.first() {
        for value in &mut out {
            *value = map(record, *value);
        }
    }
    out
}

fn curves_apply(color: [f32; 3], curves: &[Curve], gray: bool) -> [f32; 3] {
    let mut out = color;
    if gray {
        // The single channel's own curve (channel 1) first, then the
        // composite (channel 0).
        let mut value = color[0];
        for wanted in [1, 0] {
            for curve in curves.iter().filter(|curve| curve.channel == wanted) {
                value = sample_lut(&curve_table(curve), value);
            }
        }
        return [value; 3];
    }
    // The per-channel curves apply first, whatever their order in the file,
    // and the composite curve acts on their result.
    for curve in curves
        .iter()
        .filter(|curve| (1..=3).contains(&curve.channel))
    {
        let channel = usize::from(curve.channel - 1);
        out[channel] = sample_lut(&curve_table(curve), out[channel]);
    }
    for curve in curves
        .iter()
        .filter(|curve| !(1..=3).contains(&curve.channel))
    {
        let table = curve_table(curve);
        for value in out.iter_mut() {
            *value = sample_lut(&table, *value);
        }
    }
    out
}

/// A curve's 256-entry table.
fn curve_table(curve: &Curve) -> [f32; 256] {
    match &curve.data {
        CurveData::Points(points) => curve_lut(points),
        CurveData::Map(values) => {
            let mut table = [0.0f32; 256];
            for (index, entry) in table.iter_mut().enumerate() {
                *entry = f32::from(*values.get(index).unwrap_or(&(index as u8))) / 255.0;
            }
            table
        }
    }
}

/// A 256-entry table from a natural cubic spline through the control points.
fn curve_lut(points: &[CurvePoint]) -> [f32; 256] {
    let mut nodes: Vec<(f32, f32)> = points
        .iter()
        .map(|point| (f32::from(point.input), f32::from(point.output)))
        .collect();
    nodes.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    nodes.dedup_by(|a, b| a.0 == b.0);
    let mut table = [0.0f32; 256];
    if nodes.len() < 2 {
        for (index, entry) in table.iter_mut().enumerate() {
            *entry = index as f32 / 255.0;
        }
        return table;
    }
    let n = nodes.len();
    let mut h = vec![0.0f32; n - 1];
    for index in 0..n - 1 {
        h[index] = nodes[index + 1].0 - nodes[index].0;
    }
    let mut alpha = vec![0.0f32; n];
    for index in 1..n - 1 {
        alpha[index] = 3.0
            * ((nodes[index + 1].1 - nodes[index].1) / h[index]
                - (nodes[index].1 - nodes[index - 1].1) / h[index - 1]);
    }
    let mut l = vec![1.0f32; n];
    let mut mu = vec![0.0f32; n];
    let mut z = vec![0.0f32; n];
    for index in 1..n - 1 {
        l[index] = 2.0 * (nodes[index + 1].0 - nodes[index - 1].0) - h[index - 1] * mu[index - 1];
        mu[index] = h[index] / l[index];
        z[index] = (alpha[index] - h[index - 1] * z[index - 1]) / l[index];
    }
    let mut c = vec![0.0f32; n];
    let mut b = vec![0.0f32; n];
    let mut d = vec![0.0f32; n];
    for j in (0..n - 1).rev() {
        c[j] = z[j] - mu[j] * c[j + 1];
        b[j] = (nodes[j + 1].1 - nodes[j].1) / h[j] - h[j] * (c[j + 1] + 2.0 * c[j]) / 3.0;
        d[j] = (c[j + 1] - c[j]) / (3.0 * h[j]);
    }
    for (index, entry) in table.iter_mut().enumerate() {
        let x = index as f32;
        let value = if x <= nodes[0].0 {
            nodes[0].1
        } else if x >= nodes[n - 1].0 {
            nodes[n - 1].1
        } else {
            let mut segment = 0;
            for i in 0..n - 1 {
                if x >= nodes[i].0 && x <= nodes[i + 1].0 {
                    segment = i;
                    break;
                }
            }
            let dx = x - nodes[segment].0;
            nodes[segment].1 + b[segment] * dx + c[segment] * dx * dx + d[segment] * dx * dx * dx
        };
        *entry = (value / 255.0).clamp(0.0, 1.0);
    }
    table
}

fn sample_lut(table: &[f32; 256], value: f32) -> f32 {
    let index = (value.clamp(0.0, 1.0) * 255.0).round() as usize;
    table[index.min(255)]
}

// ---------------------------------------------------------------------------
// Colour adjustments
// ---------------------------------------------------------------------------

fn hue_saturation(
    color: [f32; 3],
    values: HueSaturationValues,
    colorize: bool,
    colorization: HueSaturationValues,
) -> [f32; 3] {
    let byte = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as i32;
    let rgb = [byte(color[0]), byte(color[1]), byte(color[2])];
    let out = if colorize {
        colorize_bytes(rgb, colorization)
    } else {
        master_bytes(rgb, values)
    };
    out.map(|value| value as f32 / 255.0)
}

/// Photoshop's lightness slider: the percent is quantised to a byte first,
/// then each channel moves toward white or black by that step.
fn lightness_byte(value: i32, percent: i32) -> i32 {
    if percent == 0 {
        return value;
    }
    let step = percent.abs() * 255 / 100;
    if percent > 0 {
        value + ((255 - value) * step + 127) / 255
    } else {
        (value * (255 - step) + 127) / 255
    }
}

/// The saturation slider's ratio on the half chroma: `1 + s` below zero and
/// `1 / (1 - s)` above, bounded at the slider's end (fitted to Photoshop).
fn saturation_ratio(percent: i32) -> f32 {
    let s = percent.clamp(-100, 100) as f32 / 100.0;
    if s <= 0.0 {
        1.0 + s
    } else {
        (1.0 / (1.0 - s).max(1.0 / 128.0)).min(128.0)
    }
}

/// A position on Photoshop's 1530-step hue wheel (six sectors of 255).
fn wheel_position(rgb: [i32; 3], max: i32, min: i32) -> f32 {
    let chroma = (max - min) as f32;
    let [r, g, b] = rgb.map(|value| value as f32);
    let hue = if max == rgb[0] {
        ((g - b) / chroma).rem_euclid(6.0)
    } else if max == rgb[1] {
        (b - r) / chroma + 2.0
    } else {
        (r - g) / chroma + 4.0
    };
    hue * 255.0
}

/// Rebuild a colour from a wheel position, a lightness and a half chroma: the
/// high channel is `light + round(half)`, the low `light − floor(half)`, and
/// the middle one interpolates by the position within its sector.
fn from_wheel(position: f32, light: i32, half: f32) -> [i32; 3] {
    let position = position.rem_euclid(1530.0);
    let sector = (position / 255.0) as usize;
    let fraction = (position - sector as f32 * 255.0) / 255.0;
    let high = light + (half + 0.5).floor() as i32;
    let low = light - half.floor() as i32;
    let rising = low + (((high - low) as f32) * fraction + 0.5).floor() as i32;
    let falling = low + (((high - low) as f32) * (1.0 - fraction) + 0.5).floor() as i32;
    let [r, g, b] = match sector {
        0 => [high, rising, low],
        1 => [falling, high, low],
        2 => [low, high, rising],
        3 => [low, falling, high],
        4 => [rising, low, high],
        _ => [high, low, falling],
    };
    [r.clamp(0, 255), g.clamp(0, 255), b.clamp(0, 255)]
}

fn master_bytes(rgb: [i32; 3], values: HueSaturationValues) -> [i32; 3] {
    // Lightness runs first and per channel, then the integer HSL stages.
    let rgb = rgb.map(|value| lightness_byte(value, i32::from(values.lightness)));
    let max = rgb[0].max(rgb[1]).max(rgb[2]);
    let min = rgb[0].min(rgb[1]).min(rgb[2]);
    if max == min {
        // Neutrals are never tinted.
        return rgb;
    }
    let light = (max + min) >> 1;
    let half = (max - min) as f32 / 2.0;
    let limit = (light.min(255 - light) as f32).max(half);
    let half = (half * saturation_ratio(i32::from(values.saturation))).min(limit);
    // The hue turns by a whole number of wheel steps.
    let steps = (f32::from(values.hue) * 4.25 + 0.5).floor();
    let position = wheel_position(rgb, max, min) + steps;
    from_wheel(position, light, half)
}

fn colorize_bytes(rgb: [i32; 3], colorization: HueSaturationValues) -> [i32; 3] {
    let max = rgb[0].max(rgb[1]).max(rgb[2]);
    let min = rgb[0].min(rgb[1]).min(rgb[2]);
    let light = lightness_byte((max + min) >> 1, i32::from(colorization.lightness));
    let saturation = f32::from(colorization.saturation).clamp(0.0, 100.0) / 100.0;
    let half = light.min(255 - light) as f32 * saturation;
    let position = f32::from(colorization.hue).rem_euclid(360.0) * 4.25;
    from_wheel(position, light, half)
}

fn color_balance(color: [f32; 3], settings: &psd_core::adjustments::ColorBalance) -> [f32; 3] {
    let mut deltas = [0.0f32; 3];
    let ranges: [(&ColorBalanceValues, f32); 3] = [
        (&settings.shadows, 0.25),
        (&settings.midtones, 0.5),
        (&settings.highlights, 0.75),
    ];
    for (values, center) in ranges {
        // A weight that peaks in the range's tonal zone and falls away.
        let distance = (luminance(color) - center).abs();
        let weight = (1.0 - distance * 2.0).max(0.0);
        deltas[0] += f32::from(values.cyan_red) / 100.0 * weight;
        deltas[1] += f32::from(values.magenta_green) / 100.0 * weight;
        deltas[2] += f32::from(values.yellow_blue) / 100.0 * weight;
    }
    let mut out = color;
    for channel in 0..3 {
        out[channel] = (color[channel] + deltas[channel] * 0.5).clamp(0.0, 1.0);
    }
    if settings.preserve_luminosity {
        out = preserve_luminosity(color, out);
    }
    out
}

fn descriptor_number(descriptor: &Descriptor, key: &str) -> Option<f64> {
    match descriptor.get(key)? {
        DescriptorValue::Double(value) => Some(*value),
        DescriptorValue::Integer(value) => Some(f64::from(*value)),
        DescriptorValue::UnitFloat { value, .. } => Some(*value),
        _ => None,
    }
}

fn descriptor_descriptor(value: &DescriptorValue) -> Option<&Descriptor> {
    match value {
        DescriptorValue::Descriptor(descriptor) => Some(descriptor),
        _ => None,
    }
}

/// A descriptor colour (`Rd  `/`Grn `/`Bl  ` doubles) as 0..1 RGB.
fn descriptor_rgb(value: &DescriptorValue) -> Option<[f32; 3]> {
    let descriptor = descriptor_descriptor(value)?;
    let channel =
        |key: &str| descriptor_number(descriptor, key).map(|value| (value / 255.0) as f32);
    Some([channel("Rd  ")?, channel("Grn ")?, channel("Bl  ")?])
}

fn black_and_white(color: [f32; 3], descriptor: &Descriptor) -> [f32; 3] {
    let weight = |key: &str, fallback: f32| -> f32 {
        descriptor_number(descriptor, key).map_or(fallback, |value| (value / 100.0) as f32)
    };
    let reds = weight("reds", 0.4);
    let yellows = weight("yellows", 0.6);
    let greens = weight("greens", 0.4);
    let cyans = weight("cyans", 0.6);
    let blues = weight("blues", 0.2);
    let magentas = weight("magentas", 0.8);

    // Each pixel takes the mix of the two primaries its hue sits between.
    let (r, g, b) = (color[0], color[1], color[2]);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let chroma = max - min;
    let gray = if chroma <= 1e-6 {
        max
    } else {
        let mut value = 0.0f32;
        // Primary shares: how far each channel is above the minimum.
        let shares = [r - min, g - min, b - min];
        let total: f32 = shares.iter().sum();
        let pair_weights = [
            (reds, yellows, greens),  // red-dominant
            (greens, yellows, cyans), // green-dominant
            (blues, cyans, magentas), // blue-dominant
        ];
        for (index, share) in shares.iter().enumerate() {
            if *share <= 0.0 {
                continue;
            }
            let (primary, secondary, _tertiary) = pair_weights[index];
            // The two secondaries split the remainder by their own shares.
            let other: Vec<(usize, f32)> = shares
                .iter()
                .enumerate()
                .filter(|(other, share)| *other != index && **share > 0.0)
                .map(|(other, share)| (other, *share))
                .collect();
            let other_total: f32 = other.iter().map(|(_, share)| *share).sum();
            let mut local = primary * share;
            for (other_index, other_share) in other {
                let secondary_weight = match (index, other_index) {
                    (0, 1) => yellows,
                    (0, 2) => magentas,
                    (1, 0) => yellows,
                    (1, 2) => cyans,
                    (2, 0) => magentas,
                    _ => cyans,
                };
                let _ = secondary;
                let portion = if other_total > 0.0 {
                    other_share / other_total
                } else {
                    0.0
                };
                local += secondary_weight * other_share * portion;
            }
            value += local;
        }
        (value / total.max(1e-6)).clamp(0.0, 1.0)
    };

    let Some(outer) = descriptor.get("tintColor").and_then(descriptor_descriptor) else {
        return [gray, gray, gray];
    };
    let amount = (descriptor_number(outer, "tint").unwrap_or(0.0) / 100.0) as f32;
    let Some(tint) = outer.get("tintColor").and_then(descriptor_rgb) else {
        return [gray, gray, gray];
    };
    let mut out = [0.0; 3];
    for channel in 0..3 {
        out[channel] = gray * (1.0 - amount) + tint[channel] * gray * amount;
    }
    out
}

fn photo_filter(color: [f32; 3], settings: &PhotoFilter) -> [f32; 3] {
    let filter = match &settings.color {
        PhotoFilterColor::Xyz(components) => {
            let scale = |value: i32| ((value as f32) / 255.0).clamp(0.0, 1.0);
            [
                scale(components[0]),
                scale(components[1]),
                scale(components[2]),
            ]
        }
        PhotoFilterColor::Color(raw) => {
            let scale = |value: u16| (f32::from(value) / 255.0).clamp(0.0, 1.0);
            [
                scale(raw.components[0]),
                scale(raw.components[1]),
                scale(raw.components[2]),
            ]
        }
    };
    let density = (settings.density as f32 / 100.0).clamp(0.0, 1.0);
    let mut out = [0.0; 3];
    for channel in 0..3 {
        out[channel] = color[channel] * (1.0 - density) + filter[channel] * density;
    }
    if settings.preserve_luminosity {
        out = preserve_luminosity(color, out);
    }
    out
}

fn vibrance(color: [f32; 3], descriptor: &Descriptor) -> [f32; 3] {
    let amount = (descriptor_number(descriptor, "vibrance").unwrap_or(0.0) / 100.0) as f32;
    let saturation = (descriptor_number(descriptor, "saturation").unwrap_or(0.0) / 100.0) as f32;
    if amount == 0.0 && saturation == 0.0 {
        return color;
    }
    let max = color[0].max(color[1]).max(color[2]);
    let min = color[0].min(color[1]).min(color[2]);
    let current = max - min;
    // Vibrance boosts the least saturated pixels most.
    let boost = amount * (1.0 - current) + saturation;
    let gray = luminance(color);
    let mut out = [0.0; 3];
    for channel in 0..3 {
        out[channel] = (gray + (color[channel] - gray) * (1.0 + boost)).clamp(0.0, 1.0);
    }
    out
}

fn gradient_map(color: [f32; 3], settings: &GradientMap) -> [f32; 3] {
    let stops: Vec<psd_core::ColorStop> = settings
        .color_stops
        .iter()
        .map(|stop| psd_core::ColorStop {
            source: StopSource::User(Color::Rgb {
                red: f64::from(stop.color.components[0].min(255)),
                green: f64::from(stop.color.components[1].min(255)),
                blue: f64::from(stop.color.components[2].min(255)),
            }),
            location: stop.location.min(4096) as i32,
            midpoint: stop.midpoint.min(100) as i32,
        })
        .collect();
    let gradient = psd_core::Gradient {
        label: settings.name.clone(),
        name: settings.name.clone(),
        kind: GradientKind::Solid(SolidGradient {
            smoothness: 4096.0,
            color_stops: stops,
            transparency_stops: Vec::<TransparencyStop>::new(),
        }),
    };
    super::effects::gradient_sample(&gradient, luminance(color))
}

/// A fill layer's content: solid color, gradient or pattern over the whole
/// canvas, with the layer's masks.
pub fn fill_content<T: BitDepth>(
    compositor: &Compositor<'_, T>,
    layer: &Layer<T>,
) -> Result<Option<Content>> {
    let blocks = layer.adjustments()?;
    let Some(block) = blocks.iter().find(|block| {
        matches!(
            block.kind,
            AdjustmentKind::SolidColor | AdjustmentKind::GradientFill | AdjustmentKind::PatternFill
        )
    }) else {
        return Ok(None);
    };
    let AdjustmentData::Fill(settings) = &block.data else {
        return Ok(None);
    };
    let rect = Rect::new(
        0,
        0,
        compositor.document.height as i32,
        compositor.document.width as i32,
    );
    let Some(mut content) =
        paint_content(compositor, layer, &settings.descriptor, block.kind, rect)
    else {
        return Ok(None);
    };
    // A shape's path is part of its content (`shapes`); a plain fill layer
    // takes every mask as coverage.
    if compositor.has_vector_path(layer) {
        content.mask = compositor.user_mask_plane(layer, rect);
        content = compositor.shape_content(layer, content);
    } else {
        content.mask = compositor.mask_plane(layer, rect);
    }
    Ok(Some(content))
}

/// One paint (solid, gradient or pattern) over `rect`, from the descriptor a
/// fill layer or a vector stroke carries.
pub(super) fn paint_content<T: BitDepth>(
    compositor: &Compositor<'_, T>,
    layer: &Layer<T>,
    descriptor: &Descriptor,
    kind: AdjustmentKind,
    rect: Rect,
) -> Option<Content> {
    let mut content = Content::new(rect);
    let width = content.width();
    let height = content.height();
    match kind {
        AdjustmentKind::SolidColor => {
            let color = descriptor
                .get("Clr ")
                .and_then(descriptor_rgb)
                .unwrap_or([0.0, 0.0, 0.0]);
            for index in 0..width * height {
                content.color[0][index] = color[0];
                content.color[1][index] = color[1];
                content.color[2][index] = color[2];
                content.alpha[index] = 1.0;
            }
        }
        AdjustmentKind::GradientFill => {
            let gradient = descriptor
                .get("Grad")
                .and_then(descriptor_descriptor)
                .and_then(psd_core::Gradient::from_descriptor);
            let enumerated = |key: &str| {
                descriptor
                    .get(key)
                    .and_then(DescriptorValue::as_enum)
                    .map(|(_, value)| value.as_bytes().to_vec())
            };
            let interpolation = enumerated("gradientsInterpolationMethod")
                .and_then(|id| GradientInterpolation::from_id(&id));
            // A gradient fill eases even a two-stop ramp.
            let ramp = gradient
                .as_ref()
                .and_then(|gradient| Ramp::new(gradient, interpolation, true));
            let placement = Placement {
                style: enumerated("Type")
                    .and_then(|id| GradientStyle::from_id(&id))
                    .unwrap_or(GradientStyle::Linear),
                angle: descriptor_number(descriptor, "Angl").unwrap_or(90.0) as f32,
                scale: descriptor_number(descriptor, "Scl ").unwrap_or(100.0) as f32,
                reverse: descriptor
                    .get("Rvrs")
                    .and_then(DescriptorValue::as_bool)
                    .unwrap_or(false),
                offset: descriptor
                    .get("Ofst")
                    .and_then(descriptor_descriptor)
                    .map_or((0.0, 0.0), |offset| {
                        (
                            descriptor_number(offset, "Hrzn").unwrap_or(0.0) as f32,
                            descriptor_number(offset, "Vrtc").unwrap_or(0.0) as f32,
                        )
                    }),
            };
            // "Align with Layer" spans the shape's own bounds (the canvas for a
            // plain fill layer).
            let aligned = descriptor
                .get("Algn")
                .and_then(DescriptorValue::as_bool)
                .unwrap_or(true);
            let span = compositor
                .shape_span(layer)
                .filter(|_| aligned)
                .unwrap_or(Span {
                    left: rect.left as f32,
                    top: rect.top as f32,
                    width: width as f32,
                    height: height as f32,
                });
            if let Some(ramp) = ramp {
                for y in 0..height {
                    for x in 0..width {
                        let position = ramp::position(
                            &placement,
                            span,
                            SpanBasis::CenterChord,
                            rect.left + x as i32,
                            rect.top + y as i32,
                        );
                        let (color, alpha) = ramp.sample(position);
                        let index = y * width + x;
                        content.color[0][index] = color[0];
                        content.color[1][index] = color[1];
                        content.color[2][index] = color[2];
                        content.alpha[index] = alpha;
                    }
                }
            }
        }
        AdjustmentKind::PatternFill => {
            let reference = descriptor
                .get("Ptrn")
                .and_then(descriptor_descriptor)
                .and_then(psd_core::PatternRef::from_descriptor);
            let phase = descriptor
                .get("phase")
                .and_then(descriptor_descriptor)
                .map(|phase| {
                    psd_core::Offset::percent(
                        descriptor_number(phase, "Hrzn").unwrap_or(0.0),
                        descriptor_number(phase, "Vrtc").unwrap_or(0.0),
                    )
                });
            let context = compositor.effect_context(layer, rect);
            let sampler = context.sampler(
                reference.as_ref(),
                descriptor_number(descriptor, "Scl "),
                descriptor_number(descriptor, "Angl"),
                descriptor.get("Algn").and_then(DescriptorValue::as_bool),
                phase,
            );
            if let Some(sampler) = sampler {
                for y in 0..height {
                    for x in 0..width {
                        let (color, alpha) =
                            sampler.sample(rect.left + x as i32, rect.top + y as i32);
                        let index = y * width + x;
                        content.color[0][index] = color[0];
                        content.color[1][index] = color[1];
                        content.color[2][index] = color[2];
                        content.alpha[index] = alpha;
                    }
                }
            }
        }
        _ => return None,
    }
    Some(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modern_brightness_contrast_follows_photoshops_curve() {
        // Samples of a Photoshop render of a gray ramp at brightness 23,
        // contrast 70.
        for (input, expected) in [(64.0, 57.0), (128.0, 157.0), (160.0, 200.0), (192.0, 229.0)] {
            let value = brightness_contrast(input / 255.0, 23.0, 70.0, false) * 255.0;
            assert!(
                (value - expected).abs() < 1.5,
                "{input}: {value} vs {expected}"
            );
        }
    }

    #[test]
    fn a_gray_descriptor_colour_is_a_percentage_of_black() {
        let color = super::super::ramp::color_rgb(&psd_core::Color::Gray { gray: 25.0 });
        assert!((color[0] - 0.75).abs() < 1e-6);
    }
}
