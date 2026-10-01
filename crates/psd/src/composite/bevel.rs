//! Bevel and emboss.
//!
//! One height field per layer, lit by a single light, shaded as a highlight
//! and a shadow that composite over the layer with their own blend modes.
//!
//! The height field is 0 outside the silhouette and 1 inside. Smooth is the
//! matte blurred with the tent kernel; Chisel Hard is an exact Euclidean
//! distance profile and Chisel Soft rounds it with a narrow blur; Soften adds a
//! further blur. A Contour sub-option reshapes the cross-section, and a
//! Texture perturbs the whole face with the pattern's luminance.
//!
//! Lighting is the Lambert term `L = N · light` with a normalised normal; the
//! highlight is the excess over the flat-face value normalised to the headroom,
//! `(L − sin alt) / (1 − sin alt)`, and the shadow the deficit normalised to
//! the floor, `(sin alt − L) / sin alt`. The normal's slope is the central
//! difference scaled by `depth × size / 2`. A Linear contour at Range `r`
//! multiplies the slope by `100 / r`. Pillow Emboss and Emboss light the smooth
//! half-size ramp, with depth floored at 25%, an unnormalised shadow deficit,
//! and (for Pillow) the interior lit in the opposite sense.

use psd_core::effect_enums::{BevelDirection, BevelStyle, BevelTechnique};
use psd_core::{Bevel, BlendMode, LayerEffects};

use super::contour::{is_linear, ContourLut};
use super::effects::{distance_transform, EffectContext, OuterPlane};
use super::Content;

/// The rect a bevel's shading can reach beyond the layer (exterior styles).
pub(crate) fn padding(bevel: &Bevel) -> f64 {
    match bevel.style {
        Some(BevelStyle::OuterBevel | BevelStyle::Emboss | BevelStyle::PillowEmboss) => {
            bevel.size.unwrap_or(5.0) + bevel.soften.unwrap_or(0.0) + 3.0
        }
        _ => 0.0,
    }
}

fn opacity(value: Option<f64>) -> f32 {
    (value.unwrap_or(100.0) / 100.0).clamp(0.0, 1.0) as f32
}

fn tent_peak(size: f64) -> usize {
    if size <= 0.0 {
        0
    } else {
        (size.round() as usize).max(2)
    }
}

/// The matte blurred with `size` as a tent (Satin's kernel).
fn tent(field: &mut [f32], width: usize, height: usize, size: f64) {
    let peak = tent_peak(size);
    if peak > 0 {
        super::effects::blur_tent(field, width, height, peak);
    }
}

/// A plain box blur of radius `radius`, edges truncated.
fn box_blur(field: &mut [f32], width: usize, height: usize, radius: usize) {
    if radius == 0 {
        return;
    }
    let mut scratch = vec![0.0f32; field.len()];
    for y in 0..height {
        for x in 0..width {
            let (from, to) = (x.saturating_sub(radius), (x + radius).min(width - 1));
            let sum: f32 = (from..=to).map(|i| field[y * width + i]).sum();
            scratch[y * width + x] = sum / (to - from + 1) as f32;
        }
    }
    for y in 0..height {
        for x in 0..width {
            let (from, to) = (y.saturating_sub(radius), (y + radius).min(height - 1));
            let sum: f32 = (from..=to).map(|i| scratch[i * width + x]).sum();
            field[y * width + x] = sum / (to - from + 1) as f32;
        }
    }
}

/// The soften blur: box passes whose radii sum to `ceil(soften)`.
fn soften_blur(field: &mut [f32], width: usize, height: usize, soften: f64) {
    let support = soften.max(0.0).ceil() as usize;
    if support == 0 {
        return;
    }
    let passes = support.min(3);
    let base = support / passes;
    let extra = support % passes;
    for pass in 0..passes {
        box_blur(field, width, height, base + usize::from(pass < extra));
    }
}

/// How many multiples of the edge mask (per unit of light altitude sine)
/// texture shading may reach; fitted to a rendered reference.
const TEXTURE_RELIEF_REACH: f32 = 4.0;

/// Build the highlight and shadow planes of a layer's bevel.
///
/// Approximation (the smooth cases match to a fraction of a level; the
/// contour, texture and large-size cases do not yet match pixel for pixel): a
/// tent-blurred height field, central-difference lighting scaled empirically,
/// nine-sample lighting for anti-aliased contours, a fitted cap on texture
/// shading, and a signed-distance field for the chisel techniques.
pub(crate) fn build(
    content: &Content,
    coverage: &[f32],
    effects: &LayerEffects,
    context: &EffectContext<'_>,
) -> Vec<OuterPlane> {
    let Some(bevel) = effects
        .bevel
        .as_ref()
        .filter(|bevel| bevel.enabled.unwrap_or(true))
    else {
        return Vec::new();
    };
    let (width, height) = (content.width(), content.height());
    let size = bevel.size.unwrap_or(5.0);
    let style = bevel.style.unwrap_or(BevelStyle::InnerBevel);
    if width == 0
        || height == 0
        || size <= 0.0
        || style == BevelStyle::StrokeEmboss
        || coverage.len() < width * height
        || (opacity(bevel.highlight_opacity) <= 0.0 && opacity(bevel.shadow_opacity) <= 0.0)
    {
        return Vec::new();
    }
    let technique = bevel.technique.unwrap_or(BevelTechnique::Smooth);
    let soften = bevel.soften.unwrap_or(0.0);
    let pillow = style == BevelStyle::PillowEmboss;
    let pillow_family = pillow || style == BevelStyle::Emboss;

    // Light: the angle is where it comes from, measured on screen.
    let angle = (180.0 - bevel.angle.unwrap_or(120.0)).to_radians() as f32;
    let altitude = bevel.altitude.unwrap_or(30.0).clamp(0.0, 90.0).to_radians() as f32;
    let horizontal = altitude.cos();
    let light = [
        -angle.cos() * horizontal,
        -angle.sin() * horizontal,
        altitude.sin(),
    ];

    // The Contour sub-option: a Linear curve at range r steepens the slope by
    // 1/r, any other curve remaps the height.
    let contour_on = bevel.use_shape.unwrap_or(false);
    let contour_range = (bevel.contour_range.unwrap_or(100.0) / 100.0).clamp(0.0001, 1.0) as f32;
    let contour = bevel.contour.as_ref();
    let mut slope_gain = 1.0f32;
    let mut contour_lut = None;
    if contour_on {
        match contour {
            Some(curve) if !is_linear(curve) => contour_lut = ContourLut::new(curve),
            _ => slope_gain = 1.0 / contour_range,
        }
    }
    let depth = (bevel.depth.unwrap_or(100.0) / 100.0) as f32;
    let normal_scale = if pillow_family {
        0.5 * depth.clamp(0.25, 10.0) * tent_peak(size * 0.5) as f32 * slope_gain
    } else {
        0.5 * depth.clamp(0.01, 10.0) * size.max(1.0) as f32 * slope_gain
    };
    let direction_up = bevel.direction.unwrap_or(BevelDirection::Up) == BevelDirection::Up;
    let direction = if direction_up { 1.0f32 } else { -1.0 };

    let pad = size + soften + 3.0;
    let texture = bevel
        .use_texture
        .unwrap_or(false)
        .then(|| {
            context.sampler(
                bevel.texture_pattern.as_ref(),
                bevel.texture_scale,
                Some(0.0),
                bevel.texture_linked,
                // An unlinked texture sits at the document origin: the stored
                // phase only offsets a texture linked to the layer.
                bevel
                    .texture_phase
                    .filter(|_| bevel.texture_linked.unwrap_or(true)),
            )
        })
        .flatten();

    // Everything below runs on the matte padded with transparent pixels.
    let (height_field, matte, pw, ph, pad_px, edge_mask) = {
        let pad_px = pad.ceil() as usize + 2;
        let (pw, ph) = (width + 2 * pad_px, height + 2 * pad_px);
        let mut matte = vec![0.0f32; pw * ph];
        for y in 0..height {
            let target = (y + pad_px) * pw + pad_px;
            matte[target..target + width].copy_from_slice(&coverage[y * width..(y + 1) * width]);
        }
        let shaping = if pillow_family { size * 0.5 } else { size };
        let mut field = matte.clone();
        match technique {
            BevelTechnique::Smooth => tent(&mut field, pw, ph, shaping),
            BevelTechnique::ChiselHard | BevelTechnique::ChiselSoft => {
                let shaping = shaping.max(0.01) as f32;
                // Signed distance, in pixels, to the 50 % coverage contour:
                // whole-pixel distances to the other side, corrected by how
                // far the pixel's own coverage sits from one half. One pixel
                // beyond `size` reaches the top or bottom of the range.
                let inside: Vec<f32> = matte.iter().map(|a| f32::from(*a >= 0.5)).collect();
                let to_inside = distance_transform(&inside, pw, ph);
                let outside: Vec<f32> = inside.iter().map(|v| 1.0 - *v).collect();
                let to_outside = distance_transform(&outside, pw, ph);
                for (index, slot) in field.iter_mut().enumerate() {
                    let alpha = matte[index].clamp(0.0, 1.0);
                    let signed = if inside[index] > 0.5 {
                        to_outside[index] - 1.0 + (alpha - 0.5)
                    } else {
                        -(to_inside[index] - 1.0) + (alpha - 0.5)
                    };
                    *slot = 0.5 + 0.5 * (signed / (shaping + 1.0)).clamp(-1.0, 1.0);
                }
                if technique == BevelTechnique::ChiselSoft {
                    box_blur(&mut field, pw, ph, 1);
                }
            }
        }
        soften_blur(&mut field, pw, ph, soften);
        for value in &mut field {
            *value = value.clamp(0.0, 1.0);
        }
        // How far a pixel sits inside the bevel's own footprint, before any
        // contour or texture reshapes the height.
        let edge_mask = field.clone();
        if let Some(lut) = &contour_lut {
            let smooth = bevel.contour_anti_aliased.unwrap_or(false);
            for value in &mut field {
                *value = lut.sample((*value / contour_range).clamp(0.0, 1.0), smooth);
            }
        }
        if let Some(sampler) = &texture {
            let invert = bevel.texture_invert.unwrap_or(false);
            let texture_depth = (bevel.texture_depth.unwrap_or(100.0) / 100.0) as f32;
            let mut bump = vec![0.0f32; pw * ph];
            for y in 0..ph {
                for x in 0..pw {
                    let document_x = content.rect.left + x as i32 - pad_px as i32;
                    let document_y = content.rect.top + y as i32 - pad_px as i32;
                    let (color, _) = sampler.sample(document_x, document_y);
                    let luminance = 0.299 * color[0] + 0.59 * color[1] + 0.111 * color[2];
                    // Bright texture pixels sink the surface (dark ones lift it
                    // when inverted). Bevels take the relief one-sided and
                    // unsmoothed; the emboss family keeps a centred, lightly
                    // smoothed relief faded toward the surface's flat parts.
                    bump[y * pw + x] = match (pillow_family, invert) {
                        (false, true) => luminance,
                        (false, false) => -luminance,
                        (true, true) => luminance - 0.5,
                        (true, false) => 0.5 - luminance,
                    };
                }
            }
            if pillow_family {
                box_blur(&mut bump, pw, ph, 1);
            }
            for index in 0..pw * ph {
                let face = if pillow_family {
                    1.0 - (field[index].clamp(0.0, 1.0) * 2.0 - 1.0).abs()
                } else {
                    1.0
                };
                field[index] += bump[index] * texture_depth * face;
            }
        }
        (field, matte, pw, ph, pad_px, edge_mask)
    };

    let gloss_lut = bevel.gloss_contour.as_ref().and_then(ContourLut::new);
    let gloss_smooth = bevel.anti_alias_gloss.unwrap_or(false);

    let highlight_paint =
        (
            super::ramp::color_rgb(bevel.highlight_color.as_ref().unwrap_or(
                &psd_core::Color::Rgb {
                    red: 255.0,
                    green: 255.0,
                    blue: 255.0,
                },
            )),
            opacity(bevel.highlight_opacity),
            bevel.highlight_blend_mode.unwrap_or(BlendMode::SCREEN),
        );
    let shadow_paint = (
        bevel
            .shadow_color
            .as_ref()
            .map(super::ramp::color_rgb)
            .unwrap_or([0.0; 3]),
        opacity(bevel.shadow_opacity),
        bevel.shadow_blend_mode.unwrap_or(BlendMode::MULTIPLY),
    );

    let textured = texture.is_some() && !pillow_family;
    let mut highlight = Content::new(content.rect);
    let mut shadow = Content::new(content.rect);
    let sample = |x: i64, y: i64| -> f32 {
        if x < 0 || y < 0 || x >= pw as i64 || y >= ph as i64 {
            0.0
        } else {
            height_field[y as usize * pw + x as usize]
        }
    };
    // An anti-aliased profile contour is evaluated on a 3 x 3 grid of
    // sub-pixel positions reaching forward from each pixel (0, 0.33 and
    // 0.66 px in both axes). The raw field is interpolated first, then the
    // contour is applied per sample, then each position is lit on its own
    // and the nine shadings are averaged.
    let supersampled =
        texture.is_none() && contour_lut.is_some() && bevel.contour_anti_aliased.unwrap_or(false);
    let steps = [0.0f32, 0.33, 0.66];
    let offsets: Vec<(f32, f32)> = if supersampled {
        steps
            .iter()
            .flat_map(|&oy| steps.iter().map(move |&ox| (ox, oy)))
            .collect()
    } else {
        vec![(0.0, 0.0)]
    };
    let raw_at = |x: i64, y: i64| -> f32 {
        if x < 0 || y < 0 || x >= pw as i64 || y >= ph as i64 {
            0.0
        } else {
            edge_mask[y as usize * pw + x as usize]
        }
    };
    let sub_sample = |fx: f32, fy: f32| -> f32 {
        let (x0, y0) = (fx.floor(), fy.floor());
        let (tx, ty) = (fx - x0, fy - y0);
        let (x0, y0) = (x0 as i64, y0 as i64);
        let value = raw_at(x0, y0) * (1.0 - tx) * (1.0 - ty)
            + raw_at(x0 + 1, y0) * tx * (1.0 - ty)
            + raw_at(x0, y0 + 1) * (1.0 - tx) * ty
            + raw_at(x0 + 1, y0 + 1) * tx * ty;
        contour_lut.as_ref().map_or(value, |lut| {
            lut.sample((value / contour_range).clamp(0.0, 1.0), true)
        })
    };

    for y in 0..height {
        for x in 0..width {
            let index = y * width + x;
            let (px, py) = ((x + pad_px) as i64, (y + pad_px) as i64);
            let matte_alpha = matte[py as usize * pw + px as usize].clamp(0.0, 1.0);
            let effect_alpha = match style {
                BevelStyle::InnerBevel | BevelStyle::StrokeEmboss => matte_alpha,
                BevelStyle::OuterBevel => 1.0 - matte_alpha,
                BevelStyle::Emboss | BevelStyle::PillowEmboss => 1.0,
            };
            if effect_alpha <= 0.0 {
                continue;
            }
            for &(ox, oy) in &offsets {
                let share = 1.0 / offsets.len() as f32;
                let (left, right, top, bottom) = if supersampled {
                    (
                        sub_sample(px as f32 - 1.0 + ox, py as f32 + oy),
                        sub_sample(px as f32 + 1.0 + ox, py as f32 + oy),
                        sub_sample(px as f32 + ox, py as f32 - 1.0 + oy),
                        sub_sample(px as f32 + ox, py as f32 + 1.0 + oy),
                    )
                } else {
                    (
                        sample(px - 1, py),
                        sample(px + 1, py),
                        sample(px, py - 1),
                        sample(px, py + 1),
                    )
                };
                let base = [(left - right) * normal_scale, (top - bottom) * normal_scale];
                let flat = left == right && top == bottom;

                let mut shade = |sign: f32, mut weight: f32| {
                    if weight <= 0.0 {
                        return;
                    }
                    let gradient = [base[0] * sign, base[1] * sign];
                    let length =
                        (gradient[0] * gradient[0] + gradient[1] * gradient[1] + 1.0).sqrt();
                    let raw = gradient[0] * light[0] + gradient[1] * light[1] + light[2];
                    let surface = raw / length.max(0.0001);
                    let split = |value: f32| -> f32 {
                        if value >= light[2] {
                            (value - light[2]) / (1.0 - light[2]).max(0.01)
                        } else {
                            -((light[2] - value) / light[2].max(0.01))
                        }
                    };
                    let mut lighting = split(surface);
                    if pillow_family && raw < light[2] {
                        lighting = -((light[2] - raw) / light[2].max(0.01));
                    }
                    if let Some(lut) = &gloss_lut {
                        // The gloss contour remaps the light value itself; flat
                        // exterior ground stays clean because a locally flat pixel
                        // weighs its shading by the matte.
                        if flat {
                            weight *= matte_alpha;
                            if weight <= 0.0 {
                                return;
                            }
                        }
                        let remapped = lut.sample(surface.clamp(0.0, 1.0), gloss_smooth);
                        lighting = split(remapped);
                        if pillow_family && remapped < light[2] {
                            let remapped_raw = lut.sample(raw.clamp(0.0, 1.0), gloss_smooth);
                            if remapped_raw < light[2] {
                                lighting = -((light[2] - remapped_raw) / light[2].max(0.01));
                            }
                        }
                    }
                    if textured {
                        // Texture relief reaches no farther than the bevel's own
                        // footprint: shading is capped by the pixel's edge mask.
                        let cap = (edge_mask[py as usize * pw + px as usize]
                            * TEXTURE_RELIEF_REACH
                            / light[2].max(0.05))
                        .clamp(0.0, 1.0);
                        lighting = lighting.clamp(-cap, cap);
                    }
                    if lighting > 0.0 {
                        let strength = lighting.clamp(0.0, 1.0) * weight * highlight_paint.1;
                        accumulate(&mut highlight, index, highlight_paint.0, strength);
                    } else if lighting < 0.0 {
                        let strength = (-lighting).clamp(0.0, 1.0) * weight * shadow_paint.1;
                        accumulate(&mut shadow, index, shadow_paint.0, strength);
                    }
                };

                if pillow {
                    // The valley mirrors the rim, and an anti-aliased edge carries
                    // both: exterior-signed shading over the backdrop fraction and
                    // the flipped interior shading over the content fraction.
                    let base_sign = if direction_up { -1.0 } else { 1.0 };
                    shade(base_sign, effect_alpha * (1.0 - matte_alpha) * share);
                    shade(-base_sign, effect_alpha * matte_alpha * share);
                } else {
                    shade(direction, effect_alpha * share);
                }
            }
        }
    }
    vec![
        OuterPlane {
            content: highlight,
            blend_mode: highlight_paint.2,
        },
        OuterPlane {
            content: shadow,
            blend_mode: shadow_paint.2,
        },
    ]
}

/// Add a shading contribution of `color` and `strength` to a plane, combining
/// with what is already there as a source-over.
fn accumulate(plane: &mut Content, index: usize, color: [f32; 3], strength: f32) {
    if strength <= 0.0 {
        return;
    }
    let existing = plane.alpha[index];
    let out = strength + existing * (1.0 - strength);
    for (channel, value) in color.iter().enumerate() {
        let old = plane.color[channel][index];
        plane.color[channel][index] = (old * existing * (1.0 - strength) + value * strength) / out;
    }
    plane.alpha[index] = out;
}
