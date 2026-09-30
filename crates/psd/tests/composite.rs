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
