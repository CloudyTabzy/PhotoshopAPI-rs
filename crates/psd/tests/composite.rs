//! The compositor on small synthetic documents whose flattened pixels are
//! known by construction: placement, opacity, blend modes, masks, groups,
//! clipping, Blend If, channel restrictions and a few effects.

use psd::core::{
    BlendMode, BlendingRange, Color, ColorMode, ColorOverlay, LayerEffects, Shadow, ShadowKind,
    TaggedBlock, TaggedBlockKey,
};
use psd::{ChannelKey, CompositeImage, Layer, LayeredFile, Rect};

/// A document of `width` × `height` with no layers.
fn document(width: u32, height: u32) -> LayeredFile<u8> {
    LayeredFile::new(ColorMode::Rgb, width, height).unwrap()
}

/// An opaque layer of one colour covering `rect` (top, left, bottom, right).
fn solid(name: &str, rect: (i32, i32, i32, i32), color: [u8; 3]) -> Layer<u8> {
    let rect = Rect::new(rect.0, rect.1, rect.2, rect.3);
    let mut layer = Layer::new_image(name, rect);
    let samples = (rect.width() * rect.height()) as usize;
    let pixels = layer.image_mut().unwrap();
    for (channel, value) in color.iter().enumerate() {
        pixels.set_channel(ChannelKey::color(channel as u8), vec![*value; samples]);
    }
    layer
}

fn white_background(document: &mut LayeredFile<u8>) {
    let (w, h) = (document.width as i32, document.height as i32);
    document.add_layer(solid("Background", (0, 0, h, w), [255, 255, 255]));
}

fn flatten(document: &LayeredFile<u8>) -> CompositeImage {
    document.composite_rgba8().unwrap()
}

fn close(actual: [u8; 4], expected: [u8; 4]) {
    for channel in 0..4 {
        assert!(
            (i32::from(actual[channel]) - i32::from(expected[channel])).abs() <= 1,
            "{actual:?} != {expected:?}"
        );
    }
}

#[test]
fn a_layer_lands_at_its_bounds() {
    let mut doc = document(8, 6);
    doc.add_layer(solid("red", (1, 2, 4, 6), [255, 0, 0]));
    let image = flatten(&doc);
    assert_eq!(image.pixel(2, 1), [255, 0, 0, 255]);
    assert_eq!(image.pixel(5, 3), [255, 0, 0, 255]);
    assert_eq!(image.pixel(6, 3), [0, 0, 0, 0]);
    assert_eq!(image.pixel(2, 0), [0, 0, 0, 0]);
    assert_eq!(image.pixel(1, 1), [0, 0, 0, 0]);
}

#[test]
fn a_layer_partly_off_the_canvas_keeps_its_pixels_aligned() {
    let mut doc = document(4, 4);
    // A 4x1 layer whose two left columns hang off the canvas: columns 2 and 3
    // of the layer (values 30 and 40) land on canvas columns 0 and 1.
    let mut layer = Layer::new_image("shifted", Rect::new(1, -2, 2, 2));
    let pixels = layer.image_mut().unwrap();
    for channel in 0..3 {
        pixels.set_channel(ChannelKey::color(channel), vec![10, 20, 30, 40]);
    }
    doc.add_layer(layer);
    let image = flatten(&doc);
    assert_eq!(image.pixel(0, 1), [30, 30, 30, 255]);
    assert_eq!(image.pixel(1, 1), [40, 40, 40, 255]);
    assert_eq!(image.pixel(2, 1), [0, 0, 0, 0]);
}

#[test]
fn opacity_mixes_with_the_backdrop() {
    let mut doc = document(2, 2);
    white_background(&mut doc);
    let id = doc.add_layer(solid("gray", (0, 0, 2, 2), [128, 128, 128]));
    doc.layer_mut(id).unwrap().opacity = 128;
    // 50.2% of 128 over white.
    close(flatten(&doc).pixel(0, 0), [191, 191, 191, 255]);
}

#[test]
fn multiply_blends_against_the_backdrop() {
    let mut doc = document(2, 2);
    doc.add_layer(solid("base", (0, 0, 2, 2), [128, 128, 128]));
    let id = doc.add_layer(solid("top", (0, 0, 2, 2), [128, 128, 128]));
    doc.layer_mut(id).unwrap().blend_mode = BlendMode::MULTIPLY;
    close(flatten(&doc).pixel(0, 0), [64, 64, 64, 255]);
}

#[test]
fn a_blend_mode_over_a_transparent_backdrop_keeps_the_source_colour() {
    let mut doc = document(2, 2);
    let id = doc.add_layer(solid("top", (0, 0, 2, 2), [200, 100, 50]));
    doc.layer_mut(id).unwrap().blend_mode = BlendMode::MULTIPLY;
    // Nothing below to multiply with.
    assert_eq!(flatten(&doc).pixel(0, 0), [200, 100, 50, 255]);
}

#[test]
fn a_raster_mask_outside_its_rect_takes_the_default_colour() {
    let mut doc = document(4, 1);
    let id = doc.add_layer(solid("masked", (0, 0, 1, 4), [255, 0, 0]));
    let layer = doc.layer_mut(id).unwrap();
    // A two-pixel mask over columns 1..3: white at 1, black at 2.
    layer.set_mask(vec![255, 0], Rect::new(0, 1, 1, 3)).unwrap();
    layer.set_mask_default_color(0).unwrap();
    let image = flatten(&doc);
    assert_eq!(image.pixel(0, 0)[3], 0);
    assert_eq!(image.pixel(1, 0), [255, 0, 0, 255]);
    assert_eq!(image.pixel(2, 0)[3], 0);
    assert_eq!(image.pixel(3, 0)[3], 0);

    // The same mask with a white default reveals everything outside it.
    let layer = doc.layer_mut(id).unwrap();
    layer.set_mask_default_color(255).unwrap();
    let image = flatten(&doc);
    assert_eq!(image.pixel(0, 0), [255, 0, 0, 255]);
    assert_eq!(image.pixel(2, 0)[3], 0);
    assert_eq!(image.pixel(3, 0), [255, 0, 0, 255]);
}

#[test]
fn a_pass_through_group_fades_toward_its_backdrop() {
    let mut doc = document(2, 2);
    white_background(&mut doc);
    let group = doc.add_layer(Layer::new_group("group"));
    {
        let layer = doc.layer_mut(group).unwrap();
        layer.opacity = 128;
        layer.blend_mode = BlendMode::PASSTHROUGH;
    }
    doc.add_layer_to_group(group, solid("red", (0, 0, 2, 2), [255, 0, 0]))
        .unwrap();
    close(flatten(&doc).pixel(0, 0), [255, 127, 127, 255]);
}

#[test]
fn an_isolated_group_does_not_blend_its_children_with_the_backdrop() {
    let mut doc = document(2, 2);
    doc.add_layer(solid("base", (0, 0, 2, 2), [100, 100, 100]));
    let group = doc.add_layer(Layer::new_group("group"));
    doc.layer_mut(group).unwrap().blend_mode = BlendMode::NORMAL;
    let child = doc
        .add_layer_to_group(group, solid("child", (0, 0, 2, 2), [200, 100, 50]))
        .unwrap();
    doc.layer_mut(child).unwrap().blend_mode = BlendMode::MULTIPLY;
    // The child multiplies against the group's empty canvas, so it keeps its
    // own colour; the Normal group then covers the base with it.
    assert_eq!(flatten(&doc).pixel(0, 0), [200, 100, 50, 255]);
}

#[test]
fn a_clipped_layer_shows_only_where_its_base_does() {
    let mut doc = document(4, 1);
    doc.add_layer(solid("base", (0, 0, 1, 2), [0, 0, 255]));
    let id = doc.add_layer(solid("clipped", (0, 0, 1, 4), [255, 0, 0]));
    doc.layer_mut(id).unwrap().set_clipping_mask(true);
    let image = flatten(&doc);
    assert_eq!(image.pixel(0, 0), [255, 0, 0, 255]);
    assert_eq!(image.pixel(1, 0), [255, 0, 0, 255]);
    assert_eq!(image.pixel(2, 0)[3], 0);
    assert_eq!(image.pixel(3, 0)[3], 0);
}

#[test]
fn blend_if_hides_a_layer_over_dark_backdrop_pixels() {
    let mut doc = document(2, 1);
    let mut base = Layer::new_image("base", Rect::new(0, 0, 1, 2));
    let pixels = base.image_mut().unwrap();
    for channel in 0..3 {
        pixels.set_channel(ChannelKey::color(channel), vec![20, 200]);
    }
    doc.add_layer(base);
    let id = doc.add_layer(solid("top", (0, 0, 1, 2), [255, 0, 0]));
    // Underlying Layer, composite gray: black range hard-cut at 100, so a
    // backdrop darker than 100 hides the layer.
    doc.layer_mut(id).unwrap().blending_ranges.ranges[0] = BlendingRange {
        source: [0, 0, 255, 255],
        destination: [100, 100, 255, 255],
    };
    let image = flatten(&doc);
    assert_eq!(image.pixel(0, 0), [20, 20, 20, 255]);
    assert_eq!(image.pixel(1, 0), [255, 0, 0, 255]);
}

#[test]
fn an_excluded_channel_keeps_the_backdrop() {
    let mut doc = document(2, 2);
    doc.add_layer(solid("base", (0, 0, 2, 2), [10, 20, 30]));
    let id = doc.add_layer(solid("top", (0, 0, 2, 2), [200, 210, 220]));
    // `brst` lists the excluded channels: green here.
    let mut data = Vec::new();
    data.extend_from_slice(&1u32.to_be_bytes());
    doc.layer_mut(id)
        .unwrap()
        .blocks
        .push(TaggedBlock::new(TaggedBlockKey::new(*b"brst"), data));
    assert_eq!(flatten(&doc).pixel(0, 0), [200, 20, 220, 255]);
}

#[test]
fn an_adjustment_free_document_reports_its_size() {
    let doc = document(5, 3);
    let image = flatten(&doc);
    assert_eq!((image.width, image.height), (5, 3));
    assert_eq!(image.rgba.len(), 5 * 3 * 4);
}

#[test]
fn a_color_overlay_replaces_the_layer_colour() {
    let mut doc = document(3, 3);
    let id = doc.add_layer(solid("layer", (0, 0, 3, 3), [255, 0, 0]));
    let mut effects = LayerEffects::default();
    effects.color_overlays.push(ColorOverlay {
        enabled: Some(true),
        present: Some(true),
        show_in_dialog: Some(true),
        blend_mode: Some(BlendMode::NORMAL),
        color: Some(Color::Rgb {
            red: 0.0,
            green: 0.0,
            blue: 255.0,
        }),
        opacity: Some(100.0),
    });
    doc.layer_mut(id)
        .unwrap()
        .set_layer_effects(&effects)
        .unwrap();
    assert_eq!(flatten(&doc).pixel(1, 1), [0, 0, 255, 255]);
    // Switching effects off restores the layer.
    let off = doc
        .composite_rgba8_with(psd::CompositeOptions {
            effects: false,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(off.pixel(1, 1), [255, 0, 0, 255]);
}

#[test]
fn a_drop_shadow_falls_away_from_the_light() {
    let mut doc = document(40, 40);
    white_background(&mut doc);
    let id = doc.add_layer(solid("box", (10, 10, 20, 20), [255, 255, 255]));
    let mut effects = LayerEffects::default();
    let mut shadow = Shadow::new(ShadowKind::Drop);
    shadow.size = Some(0.0);
    shadow.distance = Some(6.0);
    shadow.angle = Some(120.0);
    shadow.opacity = Some(100.0);
    effects.drop_shadows.push(shadow);
    doc.layer_mut(id)
        .unwrap()
        .set_layer_effects(&effects)
        .unwrap();
    let image = flatten(&doc);
    // Light from the upper left (120 degrees): the shadow is offset by
    // (+3, +5), so it shows right of and below the box, not left of or above.
    assert_eq!(image.pixel(21, 18), [0, 0, 0, 255], "right of the box");
    assert_eq!(image.pixel(15, 23), [0, 0, 0, 255], "below the box");
    assert_eq!(image.pixel(8, 18), [255, 255, 255, 255], "left of the box");
    assert_eq!(image.pixel(12, 8), [255, 255, 255, 255], "above the box");
    // The box itself stays white: the layer hides its own shadow.
    assert_eq!(image.pixel(15, 15), [255, 255, 255, 255]);
}

#[test]
fn a_blurred_shadow_is_centred_on_the_shifted_matte() {
    let mut doc = document(41, 41);
    white_background(&mut doc);
    let id = doc.add_layer(solid("dot", (20, 20, 21, 21), [255, 255, 255]));
    let mut effects = LayerEffects::default();
    let mut shadow = Shadow::new(ShadowKind::Drop);
    shadow.size = Some(8.0);
    shadow.distance = Some(0.0);
    shadow.opacity = Some(100.0);
    shadow.layer_knocks_out = Some(false);
    effects.drop_shadows.push(shadow);
    doc.layer_mut(id)
        .unwrap()
        .set_layer_effects(&effects)
        .unwrap();
    let image = flatten(&doc);
    // Symmetric around the dot: equal darkening at equal distances.
    for offset in 1..6u32 {
        assert_eq!(
            image.pixel(20 + offset, 20),
            image.pixel(20 - offset, 20),
            "horizontal offset {offset}"
        );
        assert_eq!(
            image.pixel(20, 20 + offset),
            image.pixel(20, 20 - offset),
            "vertical offset {offset}"
        );
    }
}

/// The port's generated shape document: a filled rectangle and ellipse, a
/// stroked rectangle with no fill, and a triangle cut by a vector mask.
#[test]
fn generated_shape_layers_render_from_their_paths() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/generated/Vectors/vector_shapes_8bit.psd");
    let doc = LayeredFile::<u8>::read(path).unwrap();
    let image = flatten(&doc);
    let background = image.pixel(1, 1);
    // The red rectangle and the blue ellipse are filled from their paths.
    let red = image.pixel(10, 10);
    assert!(red[0] > 150 && red[1] < 80 && red[2] < 80, "{red:?}");
    let blue = image.pixel(46, 12);
    assert!(blue[2] > 150 && blue[0] < 80, "{blue:?}");
    // The ellipse leaves its corners to the background.
    assert_eq!(image.pixel(36, 4), background);
    // The stroked rectangle has a green band and an untouched middle.
    let band = image.pixel(6, 45);
    assert!(band[1] > 120 && band[0] < 100, "{band:?}");
    assert_eq!(image.pixel(16, 45), background);
}

#[test]
fn lazy_pixels_require_explicit_decoding_without_mutating_the_document() {
    let mut source = document(2, 1);
    let mut layer = solid("red", (0, 0, 1, 2), [255, 0, 0]);
    layer
        .image_mut()
        .unwrap()
        .set_channel(ChannelKey::ALPHA, vec![128, 255]);
    layer.set_mask(vec![255, 0], Rect::new(0, 0, 1, 2)).unwrap();
    source.add_layer(layer);
    let bytes = source.to_bytes().unwrap();
    let mut lazy = LayeredFile::<u8>::from_bytes_with_options(
        &bytes,
        psd::ReadOptions::default().with_raw_data(true),
    )
    .unwrap();
    let id = lazy.layers_with_ids().next().unwrap().0;
    let error = lazy.composite_rgba8().unwrap_err();
    assert!(error.to_string().contains("decode_layer_pixels"));
    assert!(lazy
        .layer(id)
        .unwrap()
        .channels()
        .unwrap()
        .is_raw(ChannelKey::color(0)));
    for key in [
        ChannelKey::color(0),
        ChannelKey::color(1),
        ChannelKey::color(2),
        ChannelKey::ALPHA,
    ] {
        lazy.decode_layer_channel(id, key).unwrap();
    }
    // A mask left compressed is also a rendering dependency.
    assert!(lazy.composite_rgba8().is_err());
    lazy.decode_layer_pixels(id).unwrap();
    assert_eq!(flatten(&lazy), flatten(&source));

    let mut limited = LayeredFile::<u8>::from_bytes_with_options(
        &bytes,
        psd::ReadOptions {
            total_memory_limit: Some(0),
            use_raw_data: true,
        },
    )
    .unwrap();
    assert!(limited.composite_rgba8().is_err());
    assert!(matches!(
        limited.decode_layer_pixels(id),
        Err(psd::core::PsdError::ExceededMemoryLimit { .. })
    ));
    limited.layer_mut(id).unwrap().set_visible(false);
    assert_eq!(flatten(&limited).pixel(0, 0), [0; 4]);
}

#[test]
fn clipped_groups_preserve_isolation_and_pass_through_blending() {
    for mode in [BlendMode::NORMAL, BlendMode::PASSTHROUGH] {
        let mut doc = document(4, 1);
        doc.add_layer(solid("base", (0, 0, 1, 2), [128; 3]));
        let group = doc.add_layer(Layer::new_group("clipped"));
        let layer = doc.layer_mut(group).unwrap();
        layer.set_clipping_mask(true);
        layer.blend_mode = mode;
        let child = doc
            .add_layer_to_group(group, solid("child", (0, 0, 1, 4), [128; 3]))
            .unwrap();
        doc.layer_mut(child).unwrap().blend_mode = BlendMode::MULTIPLY;
        let image = flatten(&doc);
        let gray = if mode == BlendMode::PASSTHROUGH {
            64
        } else {
            128
        };
        close(image.pixel(0, 0), [gray, gray, gray, 255]);
        assert_eq!(image.pixel(3, 0), [0; 4]);
    }
}

#[test]
fn group_clipping_scales_coverage_masks_opacity_and_effects_once() {
    for mode in [BlendMode::NORMAL, BlendMode::PASSTHROUGH] {
        let mut doc = document(3, 1);
        white_background(&mut doc);
        let mut base = solid("base", (0, 0, 1, 3), [0, 0, 255]);
        base.image_mut()
            .unwrap()
            .set_channel(ChannelKey::ALPHA, vec![255, 128, 0]);
        doc.add_layer(base);
        let group = doc.add_layer(Layer::new_group("clipped"));
        let layer = doc.layer_mut(group).unwrap();
        layer.set_clipping_mask(true);
        layer.blend_mode = mode;
        layer.opacity = 128;
        layer
            .set_mask(vec![255, 255, 255], Rect::new(0, 0, 1, 3))
            .unwrap();
        let mut effects = LayerEffects::default();
        effects
            .color_overlays
            .push(overlay([255, 0, 0], 100.0, BlendMode::NORMAL));
        layer.set_layer_effects(&effects).unwrap();
        doc.add_layer_to_group(group, solid("child", (0, 0, 1, 3), [255, 0, 0]))
            .unwrap();
        let image = flatten(&doc);
        assert_eq!(
            image.pixel(2, 0),
            [255; 4],
            "effects outside the clip base: {mode:?}"
        );
        // Use an unstyled group to pin opacity and partial base coverage.
        doc.layer_mut(group).unwrap().clear_layer_effects();
        let image = flatten(&doc);
        close(image.pixel(0, 0), [128, 0, 127, 255]);
        close(image.pixel(1, 0), [159, 95, 191, 255]);
        doc.layer_mut(group)
            .unwrap()
            .set_mask(vec![0; 3], Rect::new(0, 0, 1, 3))
            .unwrap();
        assert_eq!(flatten(&doc).pixel(0, 0), [0, 0, 255, 255]);
    }
}

fn invert_layer() -> Layer<u8> {
    let mut layer = Layer::new_adjustment("Invert", Rect::default());
    layer
        .set_adjustment(
            &psd::core::AdjustmentBlock::new(
                psd::core::AdjustmentKind::Invert,
                psd::core::AdjustmentData::Invert {
                    trailing_bytes: Vec::new(),
                },
            )
            .unwrap(),
        )
        .unwrap();
    layer
}

#[test]
fn adjustments_apply_their_blend_mode_without_changing_alpha() {
    for (mode, expected) in [(BlendMode::MULTIPLY, 64), (BlendMode::SCREEN, 191)] {
        let mut doc = document(1, 1);
        let mut base = solid("base", (0, 0, 1, 1), [128; 3]);
        base.image_mut()
            .unwrap()
            .set_channel(ChannelKey::ALPHA, vec![128]);
        doc.add_layer(base);
        let mut layer = invert_layer();
        layer.blend_mode = mode;
        doc.add_layer(layer);
        close(
            flatten(&doc).pixel(0, 0),
            [expected, expected, expected, 128],
        );
    }
    let mut doc = document(2, 1);
    doc.add_layer(solid("base", (0, 0, 1, 2), [128; 3]));
    let mut adjustment = invert_layer();
    adjustment.blend_mode = BlendMode::MULTIPLY;
    adjustment.opacity = 128;
    adjustment
        .set_mask(vec![255, 0], Rect::new(0, 0, 1, 2))
        .unwrap();
    doc.add_layer(adjustment);
    close(flatten(&doc).pixel(0, 0), [96, 96, 96, 255]);
    assert_eq!(flatten(&doc).pixel(1, 0), [128, 128, 128, 255]);
}

fn overlay(color: [u8; 3], opacity: f64, mode: BlendMode) -> ColorOverlay {
    ColorOverlay {
        enabled: Some(true),
        blend_mode: Some(mode),
        color: Some(Color::Rgb {
            red: f64::from(color[0]),
            green: f64::from(color[1]),
            blue: f64::from(color[2]),
        }),
        opacity: Some(opacity),
        ..Default::default()
    }
}

#[test]
fn independent_overlays_do_not_leak_fill_or_apply_effect_opacity_twice() {
    for fill in [0, 128] {
        for opacity in [128, 255] {
            let mut doc = document(1, 1);
            white_background(&mut doc);
            let mut layer = solid("red", (0, 0, 1, 1), [255, 0, 0]);
            layer.set_fill(fill);
            layer.opacity = opacity;
            let mut effects = LayerEffects::default();
            effects
                .color_overlays
                .push(overlay([0, 0, 255], 50.0, BlendMode::NORMAL));
            layer.set_layer_effects(&effects).unwrap();
            doc.add_layer(layer);
            let expected = match (fill, opacity) {
                (0, 128) => [191, 191, 255, 255],
                (0, 255) => [128, 128, 255, 255],
                (128, 128) => [191, 159, 223, 255],
                (128, 255) => [128, 64, 191, 255],
                _ => unreachable!(),
            };
            close(flatten(&doc).pixel(0, 0), expected);
        }
    }
}

#[test]
fn independent_overlays_keep_transparency_blend_modes_and_stacking() {
    let mut doc = document(1, 1);
    let mut layer = solid("hidden fill", (0, 0, 1, 1), [255, 0, 0]);
    layer.set_fill(0);
    let mut effects = LayerEffects::default();
    effects
        .color_overlays
        .push(overlay([0, 0, 255], 50.0, BlendMode::MULTIPLY));
    effects
        .color_overlays
        .push(overlay([0, 255, 0], 50.0, BlendMode::NORMAL));
    layer.set_layer_effects(&effects).unwrap();
    doc.add_layer(layer);
    close(flatten(&doc).pixel(0, 0), [0, 170, 85, 191]);
}

#[test]
fn reduced_fill_and_master_opacity_scale_translucent_interiors_once() {
    let mut doc = document(1, 1);
    let mut layer = solid("translucent", (0, 0, 1, 1), [255, 0, 0]);
    layer
        .image_mut()
        .unwrap()
        .set_channel(ChannelKey::ALPHA, vec![128]);
    layer.set_fill(128);
    layer.opacity = 128;
    let mut effects = LayerEffects::default();
    effects
        .color_overlays
        .push(overlay([0, 0, 255], 50.0, BlendMode::NORMAL));
    layer.set_layer_effects(&effects).unwrap();
    doc.add_layer(layer);
    close(flatten(&doc).pixel(0, 0), [85, 0, 170, 48]);
}

#[test]
fn companion_metadata_order_does_not_change_the_adjustment() {
    use psd::core::adjustments::{BrightnessContrast, ContentGenerator};
    use psd::core::{AdjustmentBlock, AdjustmentData, AdjustmentKind, Descriptor, DescriptorValue};
    let mut descriptor = Descriptor::with_class("null");
    descriptor.set("Brgh", DescriptorValue::double(20.0));
    descriptor.set("Cntr", DescriptorValue::double(0.0));
    descriptor.set("useLegacy", DescriptorValue::boolean(true));
    let companion = AdjustmentBlock::new(
        AdjustmentKind::ContentGenerator,
        AdjustmentData::ContentGenerator(ContentGenerator {
            descriptor,
            trailing_bytes: Vec::new(),
        }),
    )
    .unwrap();
    let settings = AdjustmentBlock::new(
        AdjustmentKind::BrightnessContrast,
        AdjustmentData::BrightnessContrast(BrightnessContrast {
            brightness: 0,
            contrast: 0,
            mean_value: 127,
            lab_only: false,
            trailing_bytes: Vec::new(),
        }),
    )
    .unwrap();
    for blocks in [[&companion, &settings], [&settings, &companion]] {
        let mut doc = document(1, 1);
        doc.add_layer(solid("gray", (0, 0, 1, 1), [128; 3]));
        let mut layer = Layer::new_adjustment("Brightness", Rect::default());
        for block in blocks {
            layer.blocks.push(block.to_tagged_block().unwrap());
        }
        doc.add_layer(layer);
        assert_eq!(flatten(&doc).pixel(0, 0), [148, 148, 148, 255]);
        let back = LayeredFile::<u8>::from_bytes(&doc.to_bytes().unwrap()).unwrap();
        assert_eq!(flatten(&back), flatten(&doc));
    }
}

#[test]
fn curves_use_extended_channel_records_and_ignore_non_color_channels() {
    use psd::core::adjustments::{Curve, CurveData, CurvePoint, Curves, CurvesExtension};
    use psd::core::{AdjustmentBlock, AdjustmentData, AdjustmentKind};
    let identity = Curve {
        channel: 0,
        data: CurveData::Points(vec![
            CurvePoint {
                input: 0,
                output: 0,
            },
            CurvePoint {
                input: 255,
                output: 255,
            },
        ]),
    };
    let extended = vec![
        Curve {
            channel: 0,
            data: CurveData::Map((0..=255).rev().collect()),
        },
        Curve {
            channel: 4,
            data: CurveData::Map(vec![0; 256]),
        },
    ];
    let settings = AdjustmentBlock::new(
        AdjustmentKind::Curves,
        AdjustmentData::Curves(Curves {
            is_map: true,
            version: 1,
            curves: vec![Curve {
                data: CurveData::Map((0..=255).collect()),
                ..identity
            }],
            extension: Some(CurvesExtension {
                version: 4,
                curves: extended,
            }),
            trailing_bytes: Vec::new(),
        }),
    )
    .unwrap();
    let mut doc = document(1, 1);
    doc.add_layer(solid("gray", (0, 0, 1, 1), [64; 3]));
    let mut layer = Layer::new_adjustment("Curves", Rect::default());
    layer.set_adjustment(&settings).unwrap();
    doc.add_layer(layer);
    let back = LayeredFile::<u8>::from_bytes(&doc.to_bytes().unwrap()).unwrap();
    assert_eq!(flatten(&back).pixel(0, 0), [191, 191, 191, 255]);
}

/// A single opaque RGB texel in a length-prefixed document pattern record.
fn pattern_record(color: [u8; 3]) -> Vec<u8> {
    use psd::core::{BeWriter, PascalString, UnicodeString};
    let mut body = BeWriter::new();
    body.u32(1);
    body.u32(3);
    body.i16(0);
    body.i16(0);
    UnicodeString::new("Tile", 1)
        .unwrap()
        .write(&mut body)
        .unwrap();
    PascalString::new("tile-id", 1).write(&mut body).unwrap();
    body.u32(3);
    body.u32(0);
    for value in [0, 0, 1, 1, 3] {
        body.u32(value);
    }
    for value in color {
        body.u32(1);
        body.u32(24);
        body.u32(8);
        for value in [0, 0, 1, 1] {
            body.u32(value);
        }
        body.u16(8);
        body.u8(0);
        body.u8(value);
    }
    for _ in 0..3 {
        body.u32(0);
    }
    while !body.position().is_multiple_of(4) {
        body.u8(0);
    }
    let mut record = BeWriter::new();
    record.u32(body.position() as u32);
    record.bytes(body.as_slice());
    record.into_inner()
}

#[test]
fn disabling_effects_keeps_pattern_fill_layers() {
    use psd::core::adjustments::FillSettings;
    use psd::core::{
        AdditionalLayerInfo, AdjustmentBlock, AdjustmentData, AdjustmentKind, Descriptor,
        DescriptorValue,
    };
    let mut reference = Descriptor::with_class("Ptrn");
    reference.set("Nm  ", DescriptorValue::text("Tile"));
    reference.set("Idnt", DescriptorValue::text("tile-id"));
    let mut descriptor = Descriptor::with_class("patternLayer");
    descriptor.set("Ptrn", DescriptorValue::Descriptor(reference));
    let mut layer = Layer::new_adjustment("Pattern fill", Rect::default());
    layer
        .set_adjustment(
            &AdjustmentBlock::new(
                AdjustmentKind::PatternFill,
                AdjustmentData::Fill(FillSettings {
                    descriptor,
                    trailing_bytes: Vec::new(),
                }),
            )
            .unwrap(),
        )
        .unwrap();
    let mut doc = document(2, 1);
    let mut blocks = AdditionalLayerInfo::default();
    blocks.push(TaggedBlock::new(
        TaggedBlockKey::new(*b"Patt"),
        pattern_record([10, 20, 30]),
    ));
    doc.document_blocks = Some(blocks);
    doc.add_layer(layer);
    assert_eq!(doc.patterns().len(), 1);
    let off = doc
        .composite_rgba8_with(psd::CompositeOptions {
            effects: false,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(off.pixel(0, 0), [10, 20, 30, 255]);
    assert_eq!(off, flatten(&doc));
}

#[test]
fn clipping_a_pass_through_group_also_clips_nested_group_shadows() {
    let mut doc = document(10, 3);
    white_background(&mut doc);
    doc.add_layer(solid("base", (1, 2, 2, 3), [0, 0, 255]));
    let group = doc.add_layer(Layer::new_group("clipped"));
    doc.layer_mut(group).unwrap().set_clipping_mask(true);
    doc.layer_mut(group).unwrap().blend_mode = BlendMode::PASSTHROUGH;
    let nested = doc
        .add_layer_to_group(group, Layer::new_group("nested"))
        .unwrap();
    doc.layer_mut(nested).unwrap().blend_mode = BlendMode::NORMAL;
    let mut effects = LayerEffects::default();
    let mut shadow = Shadow::new(ShadowKind::Drop);
    shadow.size = Some(0.0);
    shadow.distance = Some(4.0);
    shadow.angle = Some(180.0);
    shadow.opacity = Some(100.0);
    effects.drop_shadows.push(shadow);
    doc.layer_mut(nested)
        .unwrap()
        .set_layer_effects(&effects)
        .unwrap();
    doc.add_layer_to_group(nested, solid("red", (1, 2, 2, 3), [255, 0, 0]))
        .unwrap();
    let image = flatten(&doc);
    assert_eq!(image.pixel(2, 1), [255, 0, 0, 255]);
    assert_eq!(image.pixel(6, 1), [255; 4]);
    doc.layer_mut(group).unwrap().set_clipping_mask(false);
    assert_eq!(
        flatten(&doc).pixel(6, 1),
        [0, 0, 0, 255],
        "the unclipped shadow is present"
    );
}

#[test]
fn independent_gradient_overlays_keep_their_alpha_at_each_pixel() {
    use psd::core::{
        ColorStop, Gradient, GradientKind, GradientOverlay, SolidGradient, StopSource,
        TransparencyStop,
    };
    let blue = Color::Rgb {
        red: 0.0,
        green: 0.0,
        blue: 255.0,
    };
    let gradient = Gradient {
        label: "Blue".into(),
        name: "Blue".into(),
        kind: GradientKind::Solid(SolidGradient {
            smoothness: 4096.0,
            color_stops: vec![
                ColorStop {
                    source: StopSource::User(blue),
                    location: 0,
                    midpoint: 50,
                },
                ColorStop {
                    source: StopSource::User(blue),
                    location: 4096,
                    midpoint: 50,
                },
            ],
            transparency_stops: vec![
                TransparencyStop {
                    opacity: 0.0,
                    location: 0,
                    midpoint: 50,
                },
                TransparencyStop {
                    opacity: 100.0,
                    location: 4096,
                    midpoint: 50,
                },
            ],
        }),
    };
    let mut doc = document(4, 1);
    let mut layer = solid("red", (0, 0, 1, 4), [255, 0, 0]);
    layer.set_fill(0);
    layer
        .set_mask(vec![255, 128, 255, 255], Rect::new(0, 0, 1, 4))
        .unwrap();
    let mut effects = LayerEffects::default();
    effects.gradient_overlays.push(GradientOverlay {
        enabled: Some(true),
        gradient: Some(gradient),
        angle: Some(0.0),
        opacity: Some(50.0),
        align: Some(false),
        blend_mode: Some(BlendMode::NORMAL),
        ..Default::default()
    });
    layer.set_layer_effects(&effects).unwrap();
    doc.add_layer(layer);
    let image = flatten(&doc);
    assert_eq!(image.pixel(0, 0), [0; 4]);
    close(image.pixel(1, 0), [0, 0, 255, 16]);
    close(image.pixel(2, 0), [0, 0, 255, 64]);
    close(image.pixel(3, 0), [0, 0, 255, 96]);
}

#[test]
fn independent_inner_shadows_keep_only_their_own_paint() {
    let mut doc = document(3, 1);
    let mut layer = solid("red", (0, 0, 1, 3), [255, 0, 0]);
    layer.set_fill(0);
    let mut effects = LayerEffects::default();
    let mut shadow = Shadow::new(ShadowKind::Inner);
    shadow.color = Some(Color::Rgb {
        red: 0.0,
        green: 0.0,
        blue: 255.0,
    });
    shadow.opacity = Some(50.0);
    shadow.distance = Some(10.0);
    shadow.size = Some(0.0);
    shadow.angle = Some(0.0);
    shadow.blend_mode = Some(BlendMode::NORMAL);
    effects.inner_shadows.push(shadow);
    layer.set_layer_effects(&effects).unwrap();
    doc.add_layer(layer);
    close(flatten(&doc).pixel(1, 0), [0, 0, 255, 128]);
}

#[test]
fn grayscale_curves_apply_the_channel_table_before_the_composite_at_all_depths() {
    use psd::core::adjustments::{Curve, CurveData, Curves};
    use psd::core::{AdjustmentBlock, AdjustmentData, AdjustmentKind};
    fn check<T: psd::BitDepth>() {
        let mut doc = LayeredFile::<T>::new(ColorMode::Grayscale, 1, 1).unwrap();
        let mut base = Layer::new_image("gray", Rect::new(0, 0, 1, 1));
        base.image_mut()
            .unwrap()
            .set_channel(ChannelKey::color(0), vec![T::from_f32(0.25)]);
        doc.add_layer(base);
        let mut adjustment = Layer::new_adjustment("Curves", Rect::default());
        adjustment
            .set_adjustment(
                &AdjustmentBlock::new(
                    AdjustmentKind::Curves,
                    AdjustmentData::Curves(Curves {
                        is_map: true,
                        version: 1,
                        // Store the composite first; evaluation must still invert first,
                        // then halve. The reverse order would produce approximately 223.
                        curves: vec![
                            Curve {
                                channel: 0,
                                data: CurveData::Map((0..=255).map(|v| v / 2).collect()),
                            },
                            Curve {
                                channel: 1,
                                data: CurveData::Map((0..=255).rev().collect()),
                            },
                        ],
                        extension: None,
                        trailing_bytes: Vec::new(),
                    }),
                )
                .unwrap(),
            )
            .unwrap();
        doc.add_layer(adjustment);
        close(
            doc.composite_rgba8().unwrap().pixel(0, 0),
            [95, 95, 95, 255],
        );
    }
    check::<u8>();
    check::<u16>();
    check::<f32>();
}
