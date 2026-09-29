//! The legacy `lrFX` effects block, generated from a [`LayerEffects`].
//!
//! Photoshop writes every layer's effects twice: as the descriptor block (`lfx2`) that current
//! versions read, and as `lrFX`, the fixed-layout block Photoshop 5 to CS5 read. Readers that
//! only know `lrFX` still show the effects it holds, so an edit to the descriptor is followed
//! by a fresh `lrFX` rather than leaving the old one to disagree with it.
//!
//! The block holds seven records in a fixed order, always all of them: the common state
//! (`cmnS`), drop shadow (`dsdw`), inner shadow (`isdw`), outer glow (`oglw`), inner glow
//! (`iglw`), bevel (`bevl`) and solid fill (`sofi`), then two zero bytes. It can only describe
//! one of each, so the first entry of each family is written, and an effect the layer does not
//! have is written the way Photoshop writes one it does not have: Photoshop's default values,
//! switched off. Everything else `lfx2` can say (further instances, satin, gradient and pattern
//! overlays, strokes, contours, noise, textures) has no legacy record and is not mirrored.
//!
//! Layouts, offsets from the start of each record's payload (all big-endian, `8BIM` + key +
//! `u32` payload length in front):
//!
//! | record | payload |
//! |---|---|
//! | `cmnS` | version 0, visible `u8`, 2 unused bytes (7) |
//! | `dsdw`, `isdw` | version 2, size, intensity (always 0), angle, distance, colour, blend, enabled, uses global light, opacity, native colour (51) |
//! | `oglw` | version 2, size, intensity (0), colour, blend, enabled, opacity, native colour (42) |
//! | `iglw` | as `oglw`, with an inverted `u8` before the native colour (43) |
//! | `bevl` | version 2, angle, strength, size, highlight blend, shadow blend, highlight colour, shadow colour, style, highlight opacity, shadow opacity, enabled, uses global light, direction, native highlight colour, native shadow colour (78) |
//! | `sofi` | version 2, blend, colour, opacity, enabled, native colour (34) |
//!
//! Sizes, angles and distances are 16.16 fixed point, a colour is a `u16` colour space and four
//! `u16` components, a blend is `8BIM` and its four-character key, an opacity is a byte
//! (`round(percent x 2.55)`). The layout follows Adobe's format documentation, checked against
//! Photoshop-authored files: the block this module writes for the model of an `lfx2` effect
//! matches the `lrFX` Photoshop wrote beside it in every corpus document, to within one unit
//! of a colour component (Photoshop derives those from a float that this model cannot see the
//! low bits of).

use crate::color::Color;
use crate::effect_enums::{BevelDirection, BevelStyle, GlowSource};
use crate::effect_set::LayerEffects;
use crate::effects::{Bevel, ColorOverlay, Glow, GlowKind, Shadow, ShadowKind};
use crate::enums::BlendMode;
use crate::gradient::{GradientKind, StopSource};
use crate::io::BeWriter;

/// The payload of a legacy `lrFX` block for `effects`.
pub fn legacy_effects_block_data(effects: &LayerEffects) -> Vec<u8> {
    let mut writer = BeWriter::with_capacity(396);
    writer.u16(0);
    writer.u16(7);

    // Photoshop leaves the common state visible even when the layer's effects are switched off
    // as a whole (`masterFXSwitch` in the descriptor carries that), so this does too.
    record(&mut writer, b"cmnS", |w| {
        w.u32(0);
        w.u8(1);
        w.u16(0);
    });
    record(&mut writer, b"dsdw", |w| {
        shadow(w, effects.drop_shadows.first(), ShadowKind::Drop);
    });
    record(&mut writer, b"isdw", |w| {
        shadow(w, effects.inner_shadows.first(), ShadowKind::Inner);
    });
    record(&mut writer, b"oglw", |w| {
        glow(w, effects.outer_glow.as_ref(), GlowKind::Outer);
    });
    record(&mut writer, b"iglw", |w| {
        glow(w, effects.inner_glow.as_ref(), GlowKind::Inner);
    });
    record(&mut writer, b"bevl", |w| bevel(w, effects.bevel.as_ref()));
    record(&mut writer, b"sofi", |w| {
        solid_fill(w, effects.color_overlays.first());
    });

    writer.u16(0);
    writer.into_inner()
}

/// One `8BIM` record: the payload is built first so its length can lead it.
fn record(writer: &mut BeWriter, key: &[u8; 4], build: impl FnOnce(&mut BeWriter)) {
    let mut payload = BeWriter::new();
    build(&mut payload);
    let payload = payload.into_inner();
    writer.bytes(b"8BIM");
    writer.bytes(key);
    writer.u32(payload.len() as u32);
    writer.bytes(&payload);
}

/// A 16.16 fixed-point number.
fn fixed(writer: &mut BeWriter, value: f64) {
    let scaled = (value * 65536.0).round();
    writer.i32(scaled.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32);
}

/// An opacity percent as the byte the legacy records store: truncated, which is what current
/// Photoshop writes (50% is `0x7f`). Older Photoshop rounded the halves up (`0x80`); the
/// epsilon keeps a product that lands a hair under a whole number from losing a unit.
fn opacity(percent: f64) -> u8 {
    (percent * 255.0 / 100.0 + 1e-9).floor().clamp(0.0, 255.0) as u8
}

/// `8BIM` and the blend mode's key.
fn blend(writer: &mut BeWriter, mode: Option<BlendMode>, default: BlendMode) {
    writer.bytes(b"8BIM");
    writer.bytes(&mode.unwrap_or(default).as_bytes());
}

/// A percentage as a `u16` over the full range.
fn word(percent: f64) -> u16 {
    (percent * 65535.0 / 100.0).round().clamp(0.0, 65535.0) as u16
}

/// A colour as the legacy records store it: a `u16` colour space, then four `u16` components in
/// that space (Adobe's "colour" structure). RGB and CMYK are checked against Photoshop-authored
/// files; the other three follow the format documentation.
fn colour(writer: &mut BeWriter, color: Option<&Color>, default: (f64, f64, f64)) {
    let fallback = Color::Rgb {
        red: default.0,
        green: default.1,
        blue: default.2,
    };
    let (space, components): (u16, [u16; 4]) = match *color.unwrap_or(&fallback) {
        Color::Rgb { red, green, blue } => {
            let [r, g, b] =
                [red, green, blue].map(|c| (c * 257.0).round().clamp(0.0, 65535.0) as u16);
            (0, [r, g, b, 0])
        }
        Color::Hsb {
            hue,
            saturation,
            brightness,
        } => (
            1,
            [
                word(hue.rem_euclid(360.0) / 3.6),
                word(saturation),
                word(brightness),
                0,
            ],
        ),
        // Ink is stored inverted: 65535 is no ink.
        Color::Cmyk {
            cyan,
            magenta,
            yellow,
            black,
        } => (
            2,
            [cyan, magenta, yellow, black].map(|ink| 65535 - word(ink)),
        ),
        Color::Lab { lightness, a, b } => (
            7,
            [
                (lightness * 100.0).round().clamp(0.0, 10000.0) as u16,
                (a * 100.0).round().clamp(-12800.0, 12700.0) as i16 as u16,
                (b * 100.0).round().clamp(-12800.0, 12700.0) as i16 as u16,
                0,
            ],
        ),
        Color::Gray { gray } => (
            8,
            [(gray * 100.0).round().clamp(0.0, 10000.0) as u16, 0, 0, 0],
        ),
    };
    writer.u16(space);
    for value in components {
        writer.u16(value);
    }
}

fn shadow(writer: &mut BeWriter, effect: Option<&Shadow>, kind: ShadowKind) {
    let default = Shadow::new(kind);
    // No entry: Photoshop's default shadow, switched off.
    let shown = effect.unwrap_or(&default);
    let enabled = effect.is_some_and(|s| s.enabled.unwrap_or(true));
    writer.u32(2);
    fixed(writer, shown.size.or(default.size).unwrap_or(5.0));
    fixed(writer, 0.0);
    fixed(writer, shown.angle.or(default.angle).unwrap_or(120.0));
    fixed(writer, shown.distance.or(default.distance).unwrap_or(5.0));
    let colour_of = shown.color.or(default.color);
    colour(writer, colour_of.as_ref(), (0.0, 0.0, 0.0));
    blend(writer, shown.blend_mode, BlendMode::MULTIPLY);
    writer.u8(u8::from(enabled));
    writer.u8(u8::from(
        shown
            .use_global_light
            .or(default.use_global_light)
            .unwrap_or(true),
    ));
    writer.u8(opacity(shown.opacity.or(default.opacity).unwrap_or(75.0)));
    colour(writer, colour_of.as_ref(), (0.0, 0.0, 0.0));
}

/// The colour of a gradient glow's first stop, when it stores one.
fn first_stop_colour(glow: &Glow) -> Option<Color> {
    let GradientKind::Solid(gradient) = &glow.gradient.as_ref()?.kind else {
        return None;
    };
    match gradient.color_stops.first()?.source {
        StopSource::User(color) => Some(color),
        StopSource::Foreground | StopSource::Background => None,
    }
}

fn glow(writer: &mut BeWriter, effect: Option<&Glow>, kind: GlowKind) {
    let default = Glow::new(kind);
    let shown = effect.unwrap_or(&default);
    let enabled = effect.is_some_and(|g| g.enabled.unwrap_or(true));
    writer.u32(2);
    fixed(writer, shown.size.or(default.size).unwrap_or(5.0));
    fixed(writer, 0.0);
    // A gradient glow has no single colour; the legacy record gets its first colour stop's.
    let colour_of = shown
        .color
        .or_else(|| first_stop_colour(shown))
        .or(default.color);
    colour(writer, colour_of.as_ref(), (255.0, 255.0, 190.0));
    blend(writer, shown.blend_mode, BlendMode::SCREEN);
    writer.u8(u8::from(enabled));
    writer.u8(opacity(shown.opacity.or(default.opacity).unwrap_or(75.0)));
    if kind == GlowKind::Inner {
        // "Inverted": an inner glow that comes from the edge (Photoshop's default).
        let inverted = shown.source.or(default.source) != Some(GlowSource::Center);
        writer.u8(u8::from(inverted));
    }
    colour(writer, colour_of.as_ref(), (255.0, 255.0, 190.0));
}

fn bevel(writer: &mut BeWriter, effect: Option<&Bevel>) {
    let default = Bevel::default();
    let shown = effect.unwrap_or(&default);
    let enabled = effect.is_some_and(|b| b.enabled.unwrap_or(true));
    let style = shown
        .style
        .or(default.style)
        .unwrap_or(BevelStyle::InnerBevel);
    let (style_byte, halved) = match style {
        BevelStyle::OuterBevel => (1, false),
        BevelStyle::InnerBevel => (2, false),
        // Legacy readers have no stroke emboss; the closest is emboss.
        BevelStyle::Emboss | BevelStyle::StrokeEmboss => (3, true),
        BevelStyle::PillowEmboss => (4, true),
    };
    let size = shown.size.or(default.size).unwrap_or(5.0);
    // The legacy size of an emboss is half the descriptor's; strength is the depth applied to
    // it, within the legacy range of 1 to 20.
    let legacy_size = if halved { size / 2.0 } else { size };
    let depth = shown.depth.or(default.depth).unwrap_or(100.0);
    let strength = (depth / 100.0 * legacy_size).clamp(1.0, 20.0);
    writer.u32(2);
    fixed(writer, shown.angle.or(default.angle).unwrap_or(120.0));
    // Photoshop truncates this product to 16.16 rather than rounding it.
    fixed(writer, (strength * 65536.0 + 1e-6).floor() / 65536.0);
    fixed(writer, legacy_size);
    blend(writer, shown.highlight_blend_mode, BlendMode::SCREEN);
    blend(writer, shown.shadow_blend_mode, BlendMode::MULTIPLY);
    let highlight = shown.highlight_color.or(default.highlight_color);
    let shade = shown.shadow_color.or(default.shadow_color);
    colour(writer, highlight.as_ref(), (255.0, 255.0, 255.0));
    colour(writer, shade.as_ref(), (0.0, 0.0, 0.0));
    writer.u8(style_byte);
    writer.u8(opacity(
        shown
            .highlight_opacity
            .or(default.highlight_opacity)
            .unwrap_or(75.0),
    ));
    writer.u8(opacity(
        shown
            .shadow_opacity
            .or(default.shadow_opacity)
            .unwrap_or(75.0),
    ));
    writer.u8(u8::from(enabled));
    writer.u8(u8::from(
        shown
            .use_global_light
            .or(default.use_global_light)
            .unwrap_or(true),
    ));
    let down = shown.direction.or(default.direction) == Some(BevelDirection::Down);
    writer.u8(u8::from(down));
    colour(writer, highlight.as_ref(), (255.0, 255.0, 255.0));
    colour(writer, shade.as_ref(), (0.0, 0.0, 0.0));
}

fn solid_fill(writer: &mut BeWriter, effect: Option<&ColorOverlay>) {
    let default = ColorOverlay::default();
    let shown = effect.unwrap_or(&default);
    let enabled = effect.is_some_and(|o| o.enabled.unwrap_or(true));
    writer.u32(2);
    blend(writer, shown.blend_mode, BlendMode::NORMAL);
    let colour_of = shown.color.or(default.color);
    colour(writer, colour_of.as_ref(), (255.0, 0.0, 0.0));
    writer.u8(opacity(shown.opacity.or(default.opacity).unwrap_or(100.0)));
    writer.u8(u8::from(enabled));
    colour(writer, colour_of.as_ref(), (255.0, 0.0, 0.0));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gradient::{ColorStop, Gradient, SolidGradient};

    /// The payload of the record `key` in a block.
    fn payload<'a>(block: &'a [u8], key: &[u8; 4]) -> &'a [u8] {
        let mut at = 4;
        while at + 12 <= block.len() {
            let size = u32::from_be_bytes(block[at + 8..at + 12].try_into().unwrap()) as usize;
            if &block[at + 4..at + 8] == key {
                return &block[at + 12..at + 12 + size];
            }
            at += 12 + size;
        }
        panic!("no {key:?} record");
    }

    #[test]
    fn no_effects_write_all_seven_records_switched_off() {
        let block = legacy_effects_block_data(&LayerEffects::default());
        assert_eq!(block.len(), 396);
        assert_eq!(&block[..4], [0, 0, 0, 7]);
        assert_eq!(&block[block.len() - 2..], [0, 0]);
        for (key, size) in [
            (b"cmnS", 7),
            (b"dsdw", 51),
            (b"isdw", 51),
            (b"oglw", 42),
            (b"iglw", 43),
            (b"bevl", 78),
            (b"sofi", 34),
        ] {
            assert_eq!(payload(&block, key).len(), size, "{key:?}");
        }
        // Common state visible; every effect's enabled byte off.
        assert_eq!(payload(&block, b"cmnS"), [0, 0, 0, 0, 1, 0, 0]);
        assert_eq!(payload(&block, b"dsdw")[38], 0);
        assert_eq!(payload(&block, b"isdw")[38], 0);
        assert_eq!(payload(&block, b"oglw")[30], 0);
        assert_eq!(payload(&block, b"iglw")[30], 0);
        assert_eq!(payload(&block, b"bevl")[55], 0);
        assert_eq!(payload(&block, b"sofi")[23], 0);
    }

    #[test]
    fn an_effect_is_written_from_its_first_instance() {
        let mut effects = LayerEffects::default();
        let mut first = Shadow::new(ShadowKind::Drop);
        first.distance = Some(12.0);
        first.angle = Some(-45.0);
        first.opacity = Some(50.0);
        let mut second = Shadow::new(ShadowKind::Drop);
        second.distance = Some(99.0);
        effects.drop_shadows = vec![first, second];
        let block = legacy_effects_block_data(&effects);
        let record = payload(&block, b"dsdw");
        assert_eq!(&record[..4], [0, 0, 0, 2]);
        assert_eq!(
            i32::from_be_bytes(record[12..16].try_into().unwrap()),
            -45 << 16
        );
        assert_eq!(
            i32::from_be_bytes(record[16..20].try_into().unwrap()),
            12 << 16
        );
        assert_eq!(&record[30..38], b"8BIMmul ");
        assert_eq!(record[38], 1, "enabled");
        // 50% truncates to 127, as current Photoshop writes it.
        assert_eq!(record[40], 0x7f);
    }

    #[test]
    fn opacity_truncates_and_keeps_whole_percents() {
        assert_eq!(opacity(100.0), 255);
        assert_eq!(opacity(75.0), 191);
        assert_eq!(opacity(50.0), 127);
        assert_eq!(opacity(20.0), 51, "51.000000000000004 must not lose a unit");
        assert_eq!(opacity(0.0), 0);
        assert_eq!(opacity(140.0), 255);
    }

    #[test]
    fn colours_are_written_in_their_own_colour_space() {
        let write = |color: Color| {
            let mut writer = BeWriter::new();
            colour(&mut writer, Some(&color), (0.0, 0.0, 0.0));
            writer.into_inner()
        };
        // RGB: 8-bit components times 257.
        assert_eq!(
            write(Color::Rgb {
                red: 255.0,
                green: 0.0,
                blue: 148.0
            }),
            [0, 0, 0xff, 0xff, 0, 0, 0x94, 0x94, 0, 0]
        );
        // CMYK: space 2, ink inverted, so no ink at all is 0xffff.
        assert_eq!(
            write(Color::Cmyk {
                cyan: 0.0,
                magenta: 0.0,
                yellow: 0.0,
                black: 100.0
            }),
            [0, 2, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0, 0]
        );
        assert_eq!(write(Color::Gray { gray: 50.0 })[..4], [0, 8, 0x13, 0x88]);
    }

    #[test]
    fn a_gradient_glow_takes_the_colour_of_its_first_stop() {
        let mut glow = Glow::new(GlowKind::Outer);
        glow.color = None;
        glow.gradient = Some(Gradient {
            label: "Gradient".to_owned(),
            name: "Custom".to_owned(),
            kind: GradientKind::Solid(SolidGradient {
                smoothness: 4096.0,
                color_stops: vec![ColorStop {
                    source: StopSource::User(Color::Rgb {
                        red: 255.0,
                        green: 203.0,
                        blue: 45.0,
                    }),
                    location: 0,
                    midpoint: 50,
                }],
                transparency_stops: Vec::new(),
            }),
        });
        let effects = LayerEffects {
            outer_glow: Some(glow),
            ..LayerEffects::default()
        };
        let block = legacy_effects_block_data(&effects);
        let record = payload(&block, b"oglw");
        assert_eq!(
            &record[12..22],
            [0, 0, 0xff, 0xff, 0xcb, 0xcb, 0x2d, 0x2d, 0, 0]
        );
    }

    #[test]
    fn a_bevel_halves_an_emboss_size_and_bounds_its_strength() {
        let with = |style: BevelStyle, size: f64, depth: f64| {
            let bevel = Bevel {
                style: Some(style),
                size: Some(size),
                depth: Some(depth),
                ..Bevel::default()
            };
            let block = legacy_effects_block_data(&LayerEffects {
                bevel: Some(bevel),
                ..LayerEffects::default()
            });
            let record = payload(&block, b"bevl").to_vec();
            let fixed = |at: usize| i32::from_be_bytes(record[at..at + 4].try_into().unwrap());
            (fixed(8), fixed(12), record[52])
        };
        // (strength, size, style) as 16.16 and a byte.
        assert_eq!(
            with(BevelStyle::InnerBevel, 5.0, 100.0),
            (5 << 16, 5 << 16, 2)
        );
        assert_eq!(with(BevelStyle::Emboss, 10.0, 100.0), (5 << 16, 5 << 16, 3));
        assert_eq!(
            with(BevelStyle::PillowEmboss, 8.0, 100.0),
            (4 << 16, 4 << 16, 4)
        );
        assert_eq!(
            with(BevelStyle::OuterBevel, 30.0, 100.0),
            (20 << 16, 30 << 16, 1)
        );
        assert_eq!(with(BevelStyle::InnerBevel, 5.0, 1.0).0, 1 << 16);
    }
}
