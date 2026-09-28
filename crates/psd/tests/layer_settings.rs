//! Layer settings, masks, and tree edits through a write → read roundtrip.
//!
//! Ports upstream `TestBlendFill`, `TestLayer` (clipping masks, sheet
//! colors), and `TestLockedLayer`, plus the port's own regression cases.

use std::path::PathBuf;

use psd::core::{BlendMode, ColorMode, Compression, LayerColor, TaggedBlockKey, Version};
use psd::{ChannelKey, Layer, LayerKind, LayeredFile, Rect};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/documents")
        .join(name)
}

fn roundtrip<T: psd::BitDepth>(document: &LayeredFile<T>) -> LayeredFile<T> {
    LayeredFile::from_bytes(&document.to_bytes().unwrap()).unwrap()
}

fn image<T: psd::BitDepth>(name: &str, width: i32, height: i32) -> Layer<T> {
    let mut layer = Layer::new_image(name, Rect::new(0, 0, height, width));
    let samples = (width * height) as usize;
    let pixels = layer.image_mut().unwrap();
    for channel in 0..3 {
        pixels.set_channel(ChannelKey::color(channel), vec![T::from_f32(0.5); samples]);
    }
    layer
}

// Upstream TestBlendFill: "Blend fill round tripping".
#[test]
fn blend_fill_round_trips() {
    let document = LayeredFile::<u8>::read(fixture("BlendFill/blend_fill.psd")).unwrap();
    let top = document.root_children()[0];
    let fill = f32::from(document.layer(top).unwrap().fill()) / 255.0;
    assert!((fill - 0.51).abs() < 0.51 * 1e-2);
    let back = roundtrip(&document);
    assert_eq!(back.layer(back.root_children()[0]).unwrap().fill(), 130);
}

// Upstream TestLayer: "Read clipping masks" and its setting subcases.
#[test]
fn clipping_masks_read_and_write() {
    let mut document =
        LayeredFile::<u8>::read(fixture("ClippingMasks/clipping_masks.psd")).unwrap();
    let toplevel = document.find_layer("clipping_toplevel").unwrap();
    let nested = document.find_layer("group/clipping_nested").unwrap();
    assert!(document.layer(toplevel).unwrap().is_clipping_mask());
    assert!(document.layer(nested).unwrap().is_clipping_mask());

    // Upstream only writes these "invalid" placements without checking them;
    // they must at least survive a roundtrip.
    for path in ["group", "group/Layer 3", "Layer 0"] {
        let id = document.find_layer(path).unwrap();
        document.layer_mut(id).unwrap().set_clipping_mask(true);
    }
    document
        .layer_mut(toplevel)
        .unwrap()
        .set_clipping_mask(false);
    let back = roundtrip(&document);
    for path in ["group", "group/Layer 3", "Layer 0", "group/clipping_nested"] {
        assert!(
            back.layer_by_path(path).unwrap().is_clipping_mask(),
            "{path}"
        );
    }
    assert!(!back
        .layer_by_path("clipping_toplevel")
        .unwrap()
        .is_clipping_mask());
}

// Upstream TestLayer: "Roundtrip layer sheet colors psd" / "psb".
#[test]
fn sheet_colors_round_trip() {
    for name in [
        "LayerColor/layers_with_display_color.psd",
        "LayerColor/layers_with_display_color.psb",
    ] {
        let mut document = LayeredFile::<u8>::read(fixture(name)).unwrap();
        for id in document.flatten() {
            let layer = document.layer_mut(id).unwrap();
            assert_eq!(layer.display_color(), LayerColor::Violet, "{name}");
            layer.set_display_color(LayerColor::Green);
        }
        let back = roundtrip(&document);
        assert!(back
            .layers()
            .all(|layer| layer.display_color() == LayerColor::Green));
    }
}

// Upstream TestLockedLayer.
#[test]
fn locked_layers_round_trip() {
    let mut document = LayeredFile::<u16>::new(ColorMode::Rgb, 64, 64).unwrap();
    document.version = Version::Psb;
    let mut layer = image::<u16>("Layer", 64, 64);
    layer.set_locked(true);
    document.add_layer(layer);
    let mut group = Layer::new_group("Group");
    group.set_locked(true);
    document.add_layer(group);

    let back = roundtrip(&document);
    for id in back.flatten() {
        let layer = back.layer(id).unwrap();
        if !matches!(layer.kind, LayerKind::SectionDivider(_)) {
            assert!(layer.is_locked(), "{}", layer.name);
            assert!(layer.flags.transparency_protected());
        }
    }
}

#[test]
fn renaming_a_read_layer_survives_the_unicode_name_block() {
    let mut document = LayeredFile::<u8>::read(fixture("Groups/Groups_8bit.psd")).unwrap();
    let id = document.find_layer("Group/GroupedLayer").unwrap();
    document.layer_mut(id).unwrap().name = "Umbenannt – 名前".to_owned();
    let back = roundtrip(&document);
    assert!(back.find_layer("Group/Umbenannt – 名前").is_some());
    assert!(back.find_layer("Group/GroupedLayer").is_none());
    // Unedited layers keep their Photoshop-written `luni` bytes.
    let untouched = |file: &LayeredFile<u8>| {
        file.layer_by_path("Group")
            .unwrap()
            .blocks
            .get(TaggedBlockKey::LUNI)
            .unwrap()
            .data
            .clone()
    };
    assert_eq!(untouched(&back), untouched(&document));
}

#[test]
fn created_layers_keep_unicode_names() {
    let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 4, 4).unwrap();
    document.add_layer(image::<u8>("日本語 🌸", 4, 4));
    let group = document.add_layer(Layer::new_group("Gruppe ü"));
    document
        .add_layer_to_group(group, image::<u8>("Kind", 4, 4))
        .unwrap();
    let back = roundtrip(&document);
    assert!(back.find_layer("日本語 🌸").is_some());
    assert!(back.find_layer("Gruppe ü/Kind").is_some());
}

#[test]
fn group_blend_mode_and_collapse_state_follow_the_section_divider() {
    let mut document = LayeredFile::<u8>::read(fixture("Groups/Groups_8bit.psd")).unwrap();
    let group = document.find_layer("Group").unwrap();
    assert_eq!(
        document.layer(group).unwrap().blend_mode,
        BlendMode::PASSTHROUGH
    );
    let collapsed = document.find_layer("GroupTopLevel/CollapsedGroup").unwrap();
    assert!(!document.layer(collapsed).unwrap().group().unwrap().open);

    // An unedited roundtrip keeps the section divider bytes.
    let back = roundtrip(&document);
    let lsct = |file: &LayeredFile<u8>, path: &str| {
        file.layer_by_path(path)
            .unwrap()
            .blocks
            .get(TaggedBlockKey::LSCT)
            .unwrap()
            .data
            .clone()
    };
    assert_eq!(lsct(&back, "Group"), lsct(&document, "Group"));

    let layer = document.layer_mut(group).unwrap();
    layer.blend_mode = BlendMode::MULTIPLY;
    layer.group_mut().unwrap().open = false;
    document
        .layer_mut(collapsed)
        .unwrap()
        .group_mut()
        .unwrap()
        .open = true;
    let back = roundtrip(&document);
    let edited = back.layer_by_path("Group").unwrap();
    assert_eq!(edited.blend_mode, BlendMode::MULTIPLY);
    assert!(!edited.group().unwrap().open);
    assert!(
        back.layer_by_path("GroupTopLevel/CollapsedGroup")
            .unwrap()
            .group()
            .unwrap()
            .open
    );
    // Pass-through is written on `lsct` with `norm` on the record.
    let mut created = LayeredFile::<u8>::new(ColorMode::Rgb, 1, 1).unwrap();
    created.add_layer(Layer::new_group("Pass"));
    let file = created.to_photoshop_file().unwrap();
    let record = &file.layer_and_mask_info.layer_info.layer_records[1];
    assert_eq!(record.blend_mode, BlendMode::NORMAL);
    assert_eq!(
        roundtrip(&created)
            .layer_by_path("Pass")
            .unwrap()
            .blend_mode,
        BlendMode::PASSTHROUGH
    );
}

#[test]
fn group_masks_are_stored_and_written() {
    for version in [Version::Psd, Version::Psb] {
        let mut document = LayeredFile::<u16>::new(ColorMode::Rgb, 32, 32).unwrap();
        document.version = version;
        let group = document.add_layer(Layer::new_group("Masked"));
        let mask: Vec<u16> = (0..6 * 4).map(|value| value * 1000).collect();
        let layer = document.layer_mut(group).unwrap();
        layer.set_mask(mask.clone(), Rect::new(2, 3, 6, 9)).unwrap();
        layer.set_mask_density(Some(200)).unwrap();
        layer.set_mask_feather(Some(1.5)).unwrap();
        layer.set_mask_default_color(0).unwrap();
        document
            .add_layer_to_group(group, image::<u16>("Inside", 4, 4))
            .unwrap();

        let back = roundtrip(&document);
        let group = back.layer_by_path("Masked").unwrap();
        assert!(group.has_mask());
        assert_eq!(group.mask_pixels(), Some(mask.as_slice()));
        assert_eq!(group.mask_rect(), Some(Rect::new(2, 3, 6, 9)));
        assert_eq!(group.mask_density(), Some(200));
        assert_eq!(group.mask_feather(), Some(1.5));
        assert_eq!(group.mask_default_color(), Some(0));
        assert!(back.find_layer("Masked/Inside").is_some());
    }
}

#[test]
fn image_mask_settings_round_trip_on_the_masks_fixture() {
    let mut document = LayeredFile::<u8>::read(fixture("Masks/Masks_8bit.psd")).unwrap();
    let with_properties = document
        .find_layer("Pixel&VectorMaskWithProperties")
        .unwrap();
    let layer = document.layer(with_properties).unwrap();
    assert_eq!(layer.pixel_mask_key(), ChannelKey::REAL_USER_MASK);
    assert_eq!(layer.mask_density(), Some(64));
    assert_eq!(layer.mask_feather(), Some(5.0));
    assert!(layer.has_mask());
    let disabled = document.find_layer("Pixel(User)MaskDisabled").unwrap();
    assert_eq!(
        document.layer(disabled).unwrap().mask_disabled(),
        Some(true)
    );
    // A vector-only mask is not a pixel mask.
    assert!(!document.layer_by_path("VectorMask").unwrap().has_mask());

    let layer = document.layer_mut(with_properties).unwrap();
    layer.set_mask_density(Some(10)).unwrap();
    layer.set_mask_feather(None).unwrap();
    layer.set_mask_disabled(true).unwrap();
    document
        .layer_mut(disabled)
        .unwrap()
        .set_mask_disabled(false)
        .unwrap();
    let back = roundtrip(&document);
    let layer = back
        .layer_by_path("Pixel&VectorMaskWithProperties")
        .unwrap();
    assert_eq!(layer.mask_density(), Some(10));
    assert_eq!(layer.mask_feather(), None);
    assert_eq!(layer.mask_disabled(), Some(true));
    assert_eq!(
        back.layer_by_path("Pixel(User)MaskDisabled")
            .unwrap()
            .mask_disabled(),
        Some(false)
    );
}

#[test]
fn moving_a_layer_moves_its_mask_and_text() {
    let mut layer = image::<u8>("Layer", 2, 2);
    layer.set_mask(vec![255; 4], Rect::new(0, 0, 2, 2)).unwrap();
    layer.translate(10, 5).unwrap();
    assert_eq!(layer.bounds, Rect::new(5, 10, 7, 12));
    assert_eq!(layer.mask_rect(), Some(Rect::new(5, 10, 7, 12)));

    let document = LayeredFile::<u8>::read(fixture("TextLayers/TextLayers_Basic.psd")).unwrap();
    let mut text = document
        .layers()
        .find(|layer| layer.is_text_layer())
        .unwrap()
        .clone();
    let (x, y) = text.text_position().unwrap();
    text.translate(3, -4).unwrap();
    assert_eq!(text.text_position(), Some((x + 3.0, y - 4.0)));
}

#[test]
fn per_layer_compression_overrides_the_document_setting() {
    let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 8, 8).unwrap();
    document.compression = Some(Compression::Raw);
    let mut layer = image::<u8>("Zip", 8, 8);
    layer.compression = Some(Compression::Zip);
    layer.set_mask(vec![0; 64], Rect::new(0, 0, 8, 8)).unwrap();
    layer.mask_compression = Some(Compression::Rle);
    document.add_layer(layer);
    document.add_layer(image::<u8>("Default", 8, 8));

    let file = document.to_photoshop_file().unwrap();
    let info = &file.layer_and_mask_info.layer_info;
    let compressions = |index: usize| -> Vec<(i16, Compression)> {
        info.layer_records[index]
            .channels
            .iter()
            .zip(&info.channel_image_data[index].channels)
            .map(|(channel, data)| (channel.index, data.compression))
            .collect()
    };
    assert!(compressions(0).iter().all(|&(index, compression)| {
        compression
            == if index == -2 {
                Compression::Rle
            } else {
                Compression::Zip
            }
    }));
    assert!(compressions(1)
        .iter()
        .all(|&(_, compression)| compression == Compression::Raw));
    let back = roundtrip(&document);
    assert_eq!(
        back.layer_by_path("Zip").unwrap().mask_pixels(),
        Some(&[0u8; 64][..])
    );
}

#[test]
fn removing_a_text_layer_marks_the_text_cache_stale() {
    let mut document = LayeredFile::<u8>::read(fixture("TextLayers/TextLayers_Basic.psd")).unwrap();
    assert!(!document.text_cache_is_stale());
    let text = document
        .layers_with_ids()
        .find(|(_, layer)| layer.is_text_layer())
        .map(|(id, _)| id)
        .unwrap();
    document.remove_layer(text).unwrap();
    assert!(document.text_cache_is_stale());
}

/// Photoshop reads a pixel record with no transparency channel as its
/// Background layer: opaque over the whole canvas, whatever the record bounds
/// say. An authored pixel record anywhere but the canvas-covering bottom slot
/// therefore gets an all-opaque transparency channel on write, while the
/// bottom record covering exactly the canvas keeps the channel-free form.
#[test]
fn authored_pixel_records_carry_a_transparency_channel() {
    let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 8, 8).unwrap();
    let mut background = Layer::new_image("Background", Rect::new(0, 0, 8, 8));
    for channel in 0..3 {
        background
            .image_mut()
            .unwrap()
            .set_channel(ChannelKey::color(channel), vec![200u8; 64]);
    }
    document.add_layer(background);
    let mut floating = Layer::new_image("Floating", Rect::new(0, 0, 4, 4));
    for channel in 0..3 {
        floating
            .image_mut()
            .unwrap()
            .set_channel(ChannelKey::color(channel), vec![100u8; 16]);
    }
    document.add_layer(floating);

    let back = roundtrip(&document);
    let keys = |name: &str| -> Vec<i16> {
        let id = back.find_layer(name).unwrap();
        back.layer(id)
            .unwrap()
            .channels()
            .unwrap()
            .keys()
            .map(|key| key.index())
            .collect()
    };
    assert_eq!(
        keys("Background"),
        vec![0, 1, 2],
        "the canvas-covering bottom record is the one channel-free form"
    );
    assert_eq!(
        keys("Floating"),
        vec![-1, 0, 1, 2],
        "a floating pixel record gains a transparency channel"
    );
    let id = back.find_layer("Floating").unwrap();
    let alpha = back
        .layer(id)
        .unwrap()
        .channels()
        .unwrap()
        .get(ChannelKey::ALPHA)
        .expect("decoded alpha");
    assert_eq!(alpha.len(), 16);
    assert!(
        alpha.iter().all(|&value| value == 255),
        "the synthesized alpha is opaque"
    );
}
