//! Photoshop's blend modes, as the compositor applies them.
//!
//! The separable modes follow Adobe's PDF blend-mode specification, and the
//! handful whose 8-bit rounding Photoshop pins are computed in the byte domain
//! with the same rounding: Color Burn and Color Dodge round the quotient to
//! nearest half-up and resolve their division corner by the *destination*
//! (`d == 255` stays 255 even at `s == 0`; `d == 0` stays 0 even at
//! `s == 255`), Exclusion rounds the `s × d / 255` product before doubling, and
//! Divide rounds to nearest. The four non-separable modes (Hue, Saturation,
//! Color, Luminosity) use the spec's `set_lum`/`set_sat` algorithm over
//! luminance weights 0.3/0.59/0.11.
//!
//! Everything here is straight (un-premultiplied) color in 0..=1. Callers that
//! want Photoshop's byte-exact rounding pass `byte_domain: true`; 16- and
//! 32-bit documents blend in float throughout.

use psd_core::BlendMode;

/// Whether a mode's result depends on the other channels (Hue, Saturation,
/// Color, Luminosity, Darker Color, Lighter Color).
pub fn is_non_separable(mode: BlendMode) -> bool {
    matches!(
        mode,
        BlendMode::HUE
            | BlendMode::SATURATION
            | BlendMode::COLOR
            | BlendMode::LUMINOSITY
            | BlendMode::DARKER_COLOR
            | BlendMode::LIGHTER_COLOR
    )
}

/// Blend one pixel: `cb` is the backdrop, `cs` the source, both straight RGB.
pub fn blend(mode: BlendMode, cb: [f32; 3], cs: [f32; 3], byte_domain: bool) -> [f32; 3] {
    if is_non_separable(mode) {
        return non_separable(mode, cb, cs);
    }
    let mut out = [0.0; 3];
    for (channel, value) in out.iter_mut().enumerate() {
        *value = separable(mode, cb[channel], cs[channel], byte_domain);
    }
    out
}

fn separable(mode: BlendMode, cb: f32, cs: f32, byte_domain: bool) -> f32 {
    if byte_domain {
        let d = to_byte(cb);
        let s = to_byte(cs);
        match mode {
            BlendMode::COLOR_BURN => return from_byte(color_burn_byte(d, s)),
            BlendMode::COLOR_DODGE => return from_byte(color_dodge_byte(d, s)),
            BlendMode::EXCLUSION => return from_byte(exclusion_byte(d, s)),
            BlendMode::DIVIDE => return from_byte(divide_byte(d, s)),
            _ => {}
        }
    }
    match mode {
        BlendMode::NORMAL | BlendMode::PASSTHROUGH | BlendMode::DISSOLVE => cs,
        BlendMode::MULTIPLY => cb * cs,
        BlendMode::SCREEN => cb + cs - cb * cs,
        BlendMode::OVERLAY => hard_light(cs, cb),
        BlendMode::DARKEN => cb.min(cs),
        BlendMode::LIGHTEN => cb.max(cs),
        BlendMode::COLOR_BURN => 1.0 - (1.0 - cb).min(1.0) / cs.max(1e-9),
        BlendMode::COLOR_DODGE => (cb / (1.0 - cs).max(1e-9)).min(1.0),
        BlendMode::LINEAR_DODGE => (cb + cs).min(1.0),
        BlendMode::LINEAR_BURN => (cb + cs - 1.0).max(0.0),
        BlendMode::HARD_LIGHT => hard_light(cb, cs),
        BlendMode::SOFT_LIGHT => soft_light(cb, cs),
        BlendMode::VIVID_LIGHT => vivid_light(cb, cs),
        BlendMode::LINEAR_LIGHT => linear_light(cb, cs),
        BlendMode::PIN_LIGHT => pin_light(cb, cs),
        BlendMode::HARD_MIX => hard_mix(cb, cs),
        BlendMode::DIFFERENCE => (cb - cs).abs(),
        BlendMode::EXCLUSION => cb + cs - 2.0 * cb * cs,
        BlendMode::SUBTRACT => (cb - cs).max(0.0),
        BlendMode::DIVIDE => (cb / cs.max(1e-9)).min(1.0),
        // Non-separable modes are handled above; a stray mode falls back to
        // source-over the way Photoshop treats a key it does not know.
        _ => cs,
    }
}

fn hard_light(cb: f32, cs: f32) -> f32 {
    if cs <= 0.5 {
        cb * (2.0 * cs)
    } else {
        let doubled = 2.0 * cs - 1.0;
        cb + doubled - cb * doubled
    }
}

fn soft_light(cb: f32, cs: f32) -> f32 {
    let d = if cb <= 0.25 {
        ((16.0 * cb - 12.0) * cb + 4.0) * cb
    } else {
        cb.sqrt()
    };
    if cs <= 0.5 {
        cb - (1.0 - 2.0 * cs) * cb * (1.0 - cb)
    } else {
        cb + (2.0 * cs - 1.0) * (d - cb)
    }
}

fn vivid_light(cb: f32, cs: f32) -> f32 {
    if cs <= 0.0 {
        0.0
    } else if cs <= 0.5 {
        1.0 - ((1.0 - cb) / (2.0 * cs)).min(1.0)
    } else if cs >= 1.0 {
        1.0
    } else {
        (cb / (2.0 * (1.0 - cs))).min(1.0)
    }
}

fn linear_light(cb: f32, cs: f32) -> f32 {
    if cs <= 0.5 {
        (cb + 2.0 * cs - 1.0).max(0.0)
    } else {
        (cb + 2.0 * cs - 1.0).min(1.0)
    }
}

fn pin_light(cb: f32, cs: f32) -> f32 {
    if cs <= 0.5 {
        cb.min(2.0 * cs)
    } else {
        cb.max(2.0 * cs - 1.0)
    }
}

fn hard_mix(cb: f32, cs: f32) -> f32 {
    let total = cb + cs;
    if total > 1.0 || (total == 1.0 && cb > 0.5) {
        1.0
    } else {
        0.0
    }
}

// ---------------------------------------------------------------------------
// Byte-domain helpers (Photoshop's 8-bit rounding)
// ---------------------------------------------------------------------------

fn to_byte(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn from_byte(value: u8) -> f32 {
    f32::from(value) / 255.0
}

/// `round(num / den)` half-up in integer arithmetic.
fn div_round_half_up(num: u32, den: u32) -> u32 {
    (2 * num + den) / (2 * den)
}

fn color_burn_byte(d: u8, s: u8) -> u8 {
    if d == 255 {
        return 255;
    }
    if s == 0 {
        return 0;
    }
    let quotient = div_round_half_up(u32::from(255 - d) * 255, u32::from(s));
    (255 - quotient.min(255)) as u8
}

fn color_dodge_byte(d: u8, s: u8) -> u8 {
    if d == 0 {
        return 0;
    }
    if s == 255 {
        return 255;
    }
    div_round_half_up(u32::from(d) * 255, u32::from(255 - s)).min(255) as u8
}

fn exclusion_byte(d: u8, s: u8) -> u8 {
    let product = div_round_half_up(u32::from(s) * u32::from(d), 255);
    (u32::from(d) + u32::from(s))
        .saturating_sub(2 * product)
        .min(255) as u8
}

fn divide_byte(d: u8, s: u8) -> u8 {
    if s == 0 {
        return if d == 0 { 0 } else { 255 };
    }
    div_round_half_up(u32::from(d) * 255, u32::from(s)).min(255) as u8
}

// ---------------------------------------------------------------------------
// Non-separable modes (PDF set_lum / set_sat)
// ---------------------------------------------------------------------------

fn lum(color: [f32; 3]) -> f32 {
    0.3 * color[0] + 0.59 * color[1] + 0.11 * color[2]
}

fn sat(color: [f32; 3]) -> f32 {
    color[0].max(color[1]).max(color[2]) - color[0].min(color[1]).min(color[2])
}

fn clip_color(mut color: [f32; 3]) -> [f32; 3] {
    let l = lum(color);
    let min = color[0].min(color[1]).min(color[2]);
    let max = color[0].max(color[1]).max(color[2]);
    if min < 0.0 {
        for channel in &mut color {
            *channel = l + (*channel - l) * l / (l - min).max(1e-9);
        }
    }
    if max > 1.0 {
        for channel in &mut color {
            *channel = l + (*channel - l) * (1.0 - l) / (max - l).max(1e-9);
        }
    }
    for channel in &mut color {
        *channel = channel.clamp(0.0, 1.0);
    }
    color
}

fn set_lum(color: [f32; 3], l: f32) -> [f32; 3] {
    let d = l - lum(color);
    clip_color([color[0] + d, color[1] + d, color[2] + d])
}

fn set_sat(color: [f32; 3], s: f32) -> [f32; 3] {
    let mut sorted = color;
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let (min, mid, max) = (sorted[0], sorted[1], sorted[2]);
    let mut out = [0.0; 3];
    if max > min {
        out[0] = (mid - min) * s / (max - min);
        out[1] = s;
        out[2] = 0.0;
    }
    // Reassign the computed (mid, max, min) triple back to the channels that
    // held them, which is what the spec's index arithmetic does.
    let mut result = [0.0; 3];
    for (channel, value) in color.iter().enumerate() {
        let computed = if *value == max {
            out[1]
        } else if *value == min {
            out[2]
        } else {
            out[0]
        };
        result[channel] = computed;
    }
    if max == min {
        return [0.0, 0.0, 0.0];
    }
    result
}

fn non_separable(mode: BlendMode, cb: [f32; 3], cs: [f32; 3]) -> [f32; 3] {
    match mode {
        BlendMode::HUE => set_lum(set_sat(cs, sat(cb)), lum(cb)),
        BlendMode::SATURATION => set_lum(set_sat(cb, sat(cs)), lum(cb)),
        BlendMode::COLOR => set_lum(cs, lum(cb)),
        BlendMode::LUMINOSITY => set_lum(cb, lum(cs)),
        BlendMode::DARKER_COLOR => {
            if lum(cs) < lum(cb) {
                cs
            } else {
                cb
            }
        }
        BlendMode::LIGHTER_COLOR => {
            if lum(cs) > lum(cb) {
                cs
            } else {
                cb
            }
        }
        _ => cs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn separable_modes_follow_the_spec() {
        let cases: &[(BlendMode, f32, f32, f32)] = &[
            (BlendMode::MULTIPLY, 0.5, 0.5, 0.25),
            (BlendMode::SCREEN, 0.5, 0.5, 0.75),
            (BlendMode::DARKEN, 0.2, 0.7, 0.2),
            (BlendMode::LIGHTEN, 0.2, 0.7, 0.7),
            (BlendMode::LINEAR_DODGE, 0.6, 0.6, 1.0),
            (BlendMode::LINEAR_BURN, 0.6, 0.6, 0.2),
            (BlendMode::DIFFERENCE, 0.8, 0.3, 0.5),
            (BlendMode::SUBTRACT, 0.3, 0.8, 0.0),
            (BlendMode::HARD_MIX, 0.6, 0.6, 1.0),
            (BlendMode::HARD_MIX, 0.4, 0.4, 0.0),
            (BlendMode::OVERLAY, 0.25, 0.5, 0.25),
            (BlendMode::HARD_LIGHT, 0.5, 0.75, 0.75),
            (BlendMode::PIN_LIGHT, 0.8, 0.3, 0.6),
            (BlendMode::VIVID_LIGHT, 0.5, 0.75, 1.0),
            (BlendMode::LINEAR_LIGHT, 0.5, 0.75, 1.0),
        ];
        for &(mode, cb, cs, expected) in cases {
            let got = separable(mode, cb, cs, false);
            assert!(
                close(got, expected),
                "{mode:?} cb={cb} cs={cs}: {got} != {expected}"
            );
        }
    }

    #[test]
    fn soft_light_matches_the_pdf_formula() {
        // Below 0.25 the D term is a cubic; above it, a square root.
        assert!(close(soft_light(0.2, 0.3), 0.2 - 0.4 * 0.2 * 0.8));
        let d = 0.5f32.sqrt();
        assert!(close(soft_light(0.5, 0.75), 0.5 + 0.5 * (d - 0.5)));
    }

    #[test]
    fn byte_domain_corners_follow_the_destination() {
        // Color Burn: d = 255 stays 255 even at s = 0.
        assert_eq!(color_burn_byte(255, 0), 255);
        // Color Dodge: d = 0 stays 0 even at s = 255.
        assert_eq!(color_dodge_byte(0, 255), 0);
        // And the ordinary corners.
        assert_eq!(color_burn_byte(0, 255), 0);
        assert_eq!(color_dodge_byte(255, 0), 255);
        assert_eq!(color_burn_byte(128, 0), 0);
    }

    #[test]
    fn byte_domain_rounds_half_up() {
        // (2a + b) / (2b): the nearest-half-up quotient, not a floor.
        assert_eq!(div_round_half_up(1, 2), 1);
        assert_eq!(div_round_half_up(3, 2), 2);
        assert_eq!(div_round_half_up(5, 2), 3);
        // Divide rounds to nearest, and 0/0 is black.
        assert_eq!(divide_byte(128, 255), 128);
        assert_eq!(divide_byte(0, 0), 0);
        assert_eq!(divide_byte(10, 0), 255);
        // Exclusion rounds the product before doubling.
        assert_eq!(exclusion_byte(255, 1), 254);
        assert_eq!(exclusion_byte(128, 128), 128);
    }

    #[test]
    fn non_separable_modes_keep_the_expected_channel() {
        let backdrop = [0.8, 0.2, 0.2];
        let source = [0.2, 0.2, 0.8];
        // Luminosity keeps the backdrop's hue/saturation and takes the
        // source's luminance.
        let result = non_separable(BlendMode::LUMINOSITY, backdrop, source);
        assert!(close(lum(result), lum(source)));
        // Color takes the source's hue and saturation with the backdrop's
        // luminance.
        let result = non_separable(BlendMode::COLOR, backdrop, source);
        assert!(close(lum(result), lum(backdrop)));
        assert!(close(sat(result), sat(source)));
        // Hue keeps the backdrop's saturation.
        let result = non_separable(BlendMode::HUE, backdrop, source);
        assert!(close(sat(result), sat(backdrop)));
    }

    #[test]
    fn a_saturated_source_stays_in_range() {
        for mode in [BlendMode::COLOR, BlendMode::HUE, BlendMode::SATURATION] {
            let result = non_separable(mode, [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
            for channel in result {
                assert!((0.0..=1.0).contains(&channel), "{mode:?} gave {result:?}");
            }
        }
    }
}
