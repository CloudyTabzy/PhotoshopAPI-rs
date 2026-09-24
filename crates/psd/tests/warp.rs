use std::path::{Path, PathBuf};

use psd::{LayeredFile, WarpKind};

use psd::{render_warped, ChannelKey, Raster, WarpRenderOptions};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/documents")
        .join(name)
}

#[cfg(feature = "image")]
fn rgba_raster(image: image::RgbaImage) -> Raster<u8> {
    let (width, height) = image.dimensions();
    let mut red = Vec::with_capacity((width * height) as usize);
    let mut green = Vec::with_capacity(red.capacity());
    let mut blue = Vec::with_capacity(red.capacity());
    let mut alpha = Vec::with_capacity(red.capacity());
    for pixel in image.pixels() {
        red.push(pixel[0]);
        green.push(pixel[1]);
        blue.push(pixel[2]);
        alpha.push(pixel[3]);
    }
    let mut raster = Raster::new(width as usize, height as usize).unwrap();
    raster.set_channel(ChannelKey::color(0), red).unwrap();
    raster.set_channel(ChannelKey::color(1), green).unwrap();
    raster.set_channel(ChannelKey::color(2), blue).unwrap();
    raster.set_channel(ChannelKey::ALPHA, alpha).unwrap();
    raster
}

#[cfg(feature = "image")]
fn embedded_jpeg_source(document: &LayeredFile<u8>) -> Raster<u8> {
    let linked = document.linked_layers().unwrap();
    let record = linked
        .iter()
        .find(|record| record.file_type == *b"JPEG")
        .expect("transformed fixture's embedded JPEG");
    let image = image::load_from_memory(&record.raw_file_bytes)
        .expect("decode progressive JPEG source")
        .to_rgba8();
    assert_eq!(image.dimensions(), (512, 512));
    rgba_raster(image)
}

fn quilt_descriptor_encoding(layer: &psd::Layer<u8>) -> (bool, bool, bool) {
    let data = layer.smart_object_data().unwrap().unwrap();
    let quilt = data
        .descriptor
        .get("quiltWarp")
        .unwrap()
        .as_descriptor()
        .unwrap();
    let envelope = quilt
        .get("customEnvelopeWarp")
        .unwrap()
        .as_descriptor()
        .unwrap();
    let slices = match envelope.get("quiltSliceX").unwrap() {
        psd::core::DescriptorValue::ObjectArray(array) => array,
        _ => panic!("quilt slices must be an object array"),
    };
    let (rotate_type, rotate_value) = quilt.get("warpRotate").unwrap().as_enum().unwrap();
    (
        slices.class_id.uses_implicit_length(),
        rotate_type.uses_implicit_length(),
        rotate_value.uses_implicit_length(),
    )
}

#[test]
fn smart_object_fixtures_expose_typed_normal_and_quilt_warps() {
    let document =
        LayeredFile::<u8>::read(fixture("SmartObjects/smart_objects_transformed.psd")).unwrap();
    let normal = document
        .layers()
        .filter(|layer| layer.is_smart_object())
        .filter_map(|layer| layer.warp().unwrap())
        .filter(|warp| warp.kind() == WarpKind::Normal)
        .count();
    let quilt = document
        .layers()
        .filter(|layer| layer.is_smart_object())
        .filter_map(|layer| layer.warp().unwrap())
        .filter(|warp| warp.kind() == WarpKind::Quilt)
        .count();
    assert_eq!((normal, quilt), (7, 4));

    let no_warp =
        LayeredFile::<u8>::read(fixture("SmartObjects/smart_object_file_no_warp.psd")).unwrap();
    let layer_c = no_warp.layer_by_path("C").unwrap();
    let layer_a = no_warp.layer_by_path("A").unwrap();
    let missing_envelope = layer_c.warp().unwrap().unwrap();
    let present_envelope = layer_a.warp().unwrap().unwrap();
    assert_eq!(missing_envelope.grid_dimensions(), (4, 4));
    assert_eq!(missing_envelope.control_points().len(), 16);
    assert_eq!(present_envelope.grid_dimensions(), (4, 4));
    assert_eq!(present_envelope.control_points().len(), 16);

    let legacy_c = psd::Warp::from_placed_layer(&layer_c.placed_layer().unwrap().unwrap()).unwrap();
    let legacy_a = psd::Warp::from_placed_layer(&layer_a.placed_layer().unwrap().unwrap()).unwrap();
    assert!(!legacy_c.has_custom_envelope());
    assert!(legacy_a.has_custom_envelope());
}

#[test]
#[cfg(feature = "image")]
fn transformed_smart_objects_match_photoshop_reference_renders() {
    let document =
        LayeredFile::<u8>::read(fixture("SmartObjects/smart_objects_transformed.psd")).unwrap();
    let source = embedded_jpeg_source(&document);

    // Upstream asserts a mean error below 0.005 (0.01 for the perspective
    // edge case); the port holds a tighter bound as a regression guard.
    const TOLERANCE: f64 = 0.0035;
    let cases = [
        ("control", "control.png", TOLERANCE),
        ("rotation", "rotation.png", TOLERANCE),
        // The progressive JPEG test decoder differs marginally from the
        // upstream OpenImageIO decoder on this ST-map edge case.
        ("perspective_transform", "perspective_transform.png", 0.011),
        (
            "simple_warp/no_bbox_change",
            "simple_warp/no_bbox_change.png",
            TOLERANCE,
        ),
        (
            "simple_warp/bbox_change",
            "simple_warp/bbox_change.png",
            TOLERANCE,
        ),
        (
            "simple_warp/no_bbox_change_perspective_warp",
            "simple_warp/no_bbox_change_perspective_warp.png",
            TOLERANCE,
        ),
        (
            "simple_warp/bbox_change_perspective_warp",
            "simple_warp/bbox_change_perspective_warp.png",
            TOLERANCE,
        ),
        (
            "quilt_warp/no_bbox_change",
            "quilt_warp/no_bbox_change.png",
            TOLERANCE,
        ),
        (
            "quilt_warp/bbox_change",
            "quilt_warp/bbox_change.png",
            TOLERANCE,
        ),
        (
            "quilt_warp/no_bbox_change_perspective_transform",
            "quilt_warp/no_bbox_change_perspective_transform.png",
            TOLERANCE,
        ),
        (
            "quilt_warp/bbox_change_perspective_transform",
            "quilt_warp/bbox_change_perspective_transform.png",
            TOLERANCE,
        ),
    ];
    let reference_dir = fixture("SmartObjects/reference");

    for (layer_path, reference_name, tolerance) in cases {
        let layer = document
            .layer_by_path(layer_path)
            .unwrap_or_else(|| panic!("missing layer {layer_path}"));
        let warp = layer.warp().unwrap().expect("smart-object warp");
        let rendered = render_warped(&warp, &source, WarpRenderOptions::default())
            .unwrap_or_else(|error| panic!("render {layer_path}: {error}"));

        let mut canvas = Raster::<u8>::new(512, 512).unwrap();
        for key in [
            ChannelKey::color(0),
            ChannelKey::color(1),
            ChannelKey::color(2),
            ChannelKey::ALPHA,
        ] {
            canvas.set_channel(key, vec![0; 512 * 512]).unwrap();
        }
        psd::composite_rgb(&mut canvas, &rendered)
            .unwrap_or_else(|error| panic!("composite {layer_path}: {error}"));

        let reference = image::open(reference_dir.join(reference_name))
            .unwrap_or_else(|error| panic!("open reference {reference_name}: {error}"))
            .to_rgba8();
        assert_eq!(reference.dimensions(), (512, 512), "{layer_path}");
        for (key, channel_index) in [
            (ChannelKey::color(0), 0),
            (ChannelKey::color(1), 1),
            (ChannelKey::color(2), 2),
            (ChannelKey::ALPHA, 3),
        ] {
            let actual = canvas.channel(key).unwrap();
            let mut total_error = 0.0f64;
            for (index, pixel) in reference.pixels().enumerate() {
                // PNG permits arbitrary color values where alpha is zero;
                // compare visible RGB there as zero, matching the upstream
                // image-buffer oracle's alpha-aware comparison.
                let expected = if channel_index < 3 && pixel[3] == 0 {
                    0
                } else {
                    pixel[channel_index]
                };
                total_error += f64::from(actual[index].abs_diff(expected)) / 255.0;
            }
            let mean_error = total_error / (512.0 * 512.0);
            assert!(
                mean_error < tolerance,
                "{layer_path} channel {channel_index}: mean error {mean_error:.6} >= {tolerance}"
            );
        }
    }
}

#[test]
fn modified_warp_descriptors_survive_a_document_round_trip() {
    let mut document =
        LayeredFile::<u8>::read(fixture("SmartObjects/smart_objects_transformed.psd")).unwrap();
    let layer_id = document.find_layer("control").unwrap();
    {
        let layer = document.layer_mut(layer_id).unwrap();
        for key in [
            psd::core::TaggedBlockKey::new(*b"SoLd"),
            psd::core::TaggedBlockKey::new(*b"PlLd"),
        ] {
            layer
                .blocks
                .get_mut(key)
                .expect("placed-layer block")
                .data
                .extend_from_slice(&[0xFA, 0xCE]);
        }
    }
    let mut warp = document.layer(layer_id).unwrap().warp().unwrap().unwrap();
    let point = warp.point(1, 1).unwrap();
    let changed_point = psd::Point2::new(point.x + 3.0, point.y - 2.0);
    warp.set_point(1, 1, changed_point).unwrap();
    {
        let layer = document.layer_mut(layer_id).unwrap();
        layer.set_warp(&warp).unwrap();
        for key in [
            psd::core::TaggedBlockKey::new(*b"SoLd"),
            psd::core::TaggedBlockKey::new(*b"PlLd"),
        ] {
            assert!(
                layer.blocks.get(key).unwrap().data.ends_with(&[0xFA, 0xCE]),
                "trailing payload bytes of {key:?} should survive a warp edit"
            );
        }
    }

    let bytes = document.to_bytes().unwrap();
    let back = LayeredFile::<u8>::from_bytes(&bytes).unwrap();
    let parsed = back
        .layer_by_path("control")
        .unwrap()
        .warp()
        .unwrap()
        .unwrap();
    assert_eq!(parsed.point(1, 1), Some(changed_point));
    assert_eq!(parsed.affine_transform(), warp.affine_transform());
    assert_eq!(parsed.non_affine_transform(), warp.non_affine_transform());
    let back_layer = back.layer_by_path("control").unwrap();
    for key in [
        psd::core::TaggedBlockKey::new(*b"SoLd"),
        psd::core::TaggedBlockKey::new(*b"PlLd"),
    ] {
        assert!(back_layer
            .blocks
            .get(key)
            .unwrap()
            .data
            .ends_with(&[0xFA, 0xCE]));
    }
}

#[test]
fn switching_a_layer_from_normal_to_quilt_persists_both_sold_descriptors() {
    let mut document =
        LayeredFile::<u8>::read(fixture("SmartObjects/smart_objects_transformed.psd")).unwrap();
    let layer_id = document.find_layer("control").unwrap();
    let original = document.layer(layer_id).unwrap().warp().unwrap().unwrap();
    let source_bounds = original.source_bounds();
    let mut quilt = psd::Warp::generate_default_with_grid(
        source_bounds.width().round() as usize,
        source_bounds.height().round() as usize,
        7,
        4,
    )
    .unwrap();
    quilt.set_source_bounds(source_bounds).unwrap();
    quilt
        .set_affine_transform(original.affine_transform())
        .unwrap();
    quilt
        .set_non_affine_transform(original.non_affine_transform())
        .unwrap();
    document
        .layer_mut(layer_id)
        .unwrap()
        .set_warp(&quilt)
        .unwrap();

    let mut back = LayeredFile::<u8>::from_bytes(&document.to_bytes().unwrap()).unwrap();
    let layer = back.layer_by_path("control").unwrap();
    let parsed = layer.warp().unwrap().unwrap();
    assert_eq!(parsed.kind(), WarpKind::Quilt);
    assert_eq!(parsed.grid_dimensions(), (7, 4));
    let data = layer.smart_object_data().unwrap().unwrap();
    assert!(data.descriptor.get("quiltWarp").is_some());
    let placeholder = data
        .descriptor
        .get("warp")
        .unwrap()
        .as_descriptor()
        .unwrap();
    assert_eq!(
        placeholder
            .get("warpStyle")
            .unwrap()
            .as_enum()
            .unwrap()
            .1
            .as_str(),
        "warpNone"
    );
    assert!(!placeholder.contains("customEnvelopeWarp"));

    let mut normal = psd::Warp::generate_default(
        source_bounds.width().round() as usize,
        source_bounds.height().round() as usize,
    )
    .unwrap();
    normal.set_source_bounds(source_bounds).unwrap();
    normal
        .set_affine_transform(original.affine_transform())
        .unwrap();
    normal
        .set_non_affine_transform(original.non_affine_transform())
        .unwrap();
    back.layer_mut(layer_id).unwrap().set_warp(&normal).unwrap();
    let reverted = LayeredFile::<u8>::from_bytes(&back.to_bytes().unwrap()).unwrap();
    let reverted_layer = reverted.layer_by_path("control").unwrap();
    assert_eq!(
        reverted_layer.warp().unwrap().unwrap().kind(),
        WarpKind::Normal
    );
    assert!(reverted_layer
        .smart_object_data()
        .unwrap()
        .unwrap()
        .descriptor
        .get("quiltWarp")
        .is_none());
}

#[test]
fn editing_an_existing_quilt_preserves_four_byte_key_encodings() {
    let mut document =
        LayeredFile::<u8>::read(fixture("SmartObjects/smart_objects_transformed.psd")).unwrap();
    let layer_id = document.find_layer("quilt_warp/no_bbox_change").unwrap();
    let original = document.layer(layer_id).unwrap();
    let old_encoding = quilt_descriptor_encoding(original);
    let mut warp = original.warp().unwrap().unwrap();
    let (slices_x, slices_y) = warp.quilt_slices();
    let mut slices_x = slices_x.to_vec();
    slices_x[1] += 0.5;
    let new_slice = slices_x[1];
    warp.set_quilt_slices(slices_x, slices_y.to_vec()).unwrap();
    warp.set_warp_rotate("Vrtc").unwrap();
    document
        .layer_mut(layer_id)
        .unwrap()
        .set_warp(&warp)
        .unwrap();

    let back = LayeredFile::<u8>::from_bytes(&document.to_bytes().unwrap()).unwrap();
    let layer = back.layer_by_path("quilt_warp/no_bbox_change").unwrap();
    assert_eq!(quilt_descriptor_encoding(layer), old_encoding);
    let parsed = layer.warp().unwrap().unwrap();
    assert_eq!(parsed.warp_rotate(), "Vrtc");
    assert!((parsed.quilt_slices().0[1] - new_slice).abs() < 1e-12);
}

#[test]
fn legacy_only_plld_updates_reject_lossy_non_affine_state() {
    let document =
        LayeredFile::<u8>::read(fixture("SmartObjects/smart_objects_transformed.psd")).unwrap();
    let mut legacy = document.layer_by_path("control").unwrap().clone();
    legacy
        .blocks
        .blocks
        .retain(|block| block.key.as_bytes() != *b"SoLd" && block.key.as_bytes() != *b"SoLE");
    let original_bytes = legacy
        .blocks
        .get(psd::core::TaggedBlockKey::new(*b"PlLd"))
        .unwrap()
        .data
        .clone();

    let mut lossy = legacy.warp().unwrap().unwrap();
    let mut non_affine = lossy.non_affine_transform();
    non_affine[0].x += 5.0;
    lossy.set_non_affine_transform(non_affine).unwrap();
    assert!(legacy.set_warp(&lossy).is_err());
    assert_eq!(
        legacy
            .blocks
            .get(psd::core::TaggedBlockKey::new(*b"PlLd"))
            .unwrap()
            .data,
        original_bytes
    );

    let mut editable = legacy.warp().unwrap().unwrap();
    let old_point = editable.point(1, 1).unwrap();
    let changed_point = psd::Point2::new(old_point.x + 2.0, old_point.y);
    editable.set_point(1, 1, changed_point).unwrap();
    legacy.set_warp(&editable).unwrap();
    let placed = legacy.placed_layer().unwrap().unwrap();
    let parsed = psd::Warp::from_placed_layer(&placed).unwrap();
    assert_eq!(parsed.point(1, 1), Some(changed_point));
}

#[test]
#[cfg(feature = "image")]
fn caller_supplied_source_can_update_preview_channels_and_layer_bounds() {
    let mut document =
        LayeredFile::<u8>::read(fixture("SmartObjects/smart_objects_transformed.psd")).unwrap();
    let source = embedded_jpeg_source(&document);
    let layer_id = document.find_layer("control").unwrap();
    let warp = document.layer(layer_id).unwrap().warp().unwrap().unwrap();
    let expected = warp.apply(&source, WarpRenderOptions::default()).unwrap();
    document
        .layer_mut(layer_id)
        .unwrap()
        .set_warp_and_render(&warp, &source, WarpRenderOptions::default())
        .unwrap();

    let rendered_layer = document.layer(layer_id).unwrap();
    let (left, top) = expected.origin();
    assert_eq!(
        rendered_layer.bounds,
        psd::Rect::new(
            top,
            left,
            top + expected.height() as i32,
            left + expected.width() as i32,
        )
    );
    let image = rendered_layer.image().unwrap();
    for (key, channel) in expected.channels().iter() {
        assert_eq!(image.channels.get(key), Some(channel));
    }

    let back = LayeredFile::<u8>::from_bytes(&document.to_bytes().unwrap()).unwrap();
    let back_layer = back.layer_by_path("control").unwrap();
    assert_eq!(back_layer.bounds, rendered_layer.bounds);
    assert_eq!(
        back_layer
            .image()
            .unwrap()
            .channels
            .get(ChannelKey::color(0)),
        image.channels.get(ChannelKey::color(0))
    );
}

#[test]
#[cfg(feature = "image")]
fn preview_render_rejects_unhandled_layer_channels_without_mutating_them() {
    let mut document =
        LayeredFile::<u8>::read(fixture("SmartObjects/smart_objects_transformed.psd")).unwrap();
    let source = embedded_jpeg_source(&document);
    let layer_id = document.find_layer("control").unwrap();
    let warp = document.layer(layer_id).unwrap().warp().unwrap().unwrap();
    let layer = document.layer_mut(layer_id).unwrap();

    layer
        .image_mut()
        .unwrap()
        .set_channel(ChannelKey::USER_MASK, vec![91, 92, 93]);
    let before = layer.clone();
    let error = layer
        .set_warp_and_render(&warp, &source, WarpRenderOptions::default())
        .unwrap_err();
    assert!(error.to_string().contains("mask"));
    assert_eq!(*layer, before);

    layer
        .image_mut()
        .unwrap()
        .channels
        .remove(ChannelKey::USER_MASK);
    layer.mask = Some(Default::default());
    let before = layer.clone();
    let error = layer
        .set_warp_and_render(&warp, &source, WarpRenderOptions::default())
        .unwrap_err();
    assert!(error.to_string().contains("mask"));
    assert_eq!(*layer, before);

    layer.mask = None;
    layer
        .image_mut()
        .unwrap()
        .set_channel(ChannelKey(7), vec![71, 72, 73]);
    let before = layer.clone();
    let error = layer
        .set_warp_and_render(&warp, &source, WarpRenderOptions::default())
        .unwrap_err();
    assert!(error.to_string().contains("existing layer channel"));
    assert_eq!(*layer, before);

    layer.image_mut().unwrap().channels.remove(ChannelKey(7));
    let mut extra_channel_source = source.clone();
    let source_samples = extra_channel_source.width() * extra_channel_source.height();
    extra_channel_source
        .set_channel(ChannelKey(7), vec![0; source_samples])
        .unwrap();
    let before = layer.clone();
    let error = layer
        .set_warp_and_render(&warp, &extra_channel_source, WarpRenderOptions::default())
        .unwrap_err();
    assert!(error.to_string().contains("absent from the target layer"));
    assert_eq!(*layer, before);

    let mut source_without_alpha = Raster::new(source.width(), source.height()).unwrap();
    for (key, channel) in source.channels().iter() {
        if key != ChannelKey::ALPHA {
            source_without_alpha
                .set_channel(key, channel.to_vec())
                .unwrap();
        }
    }
    let alpha_samples = layer.bounds.sample_count();
    layer
        .image_mut()
        .unwrap()
        .set_channel(ChannelKey::ALPHA, vec![255; alpha_samples]);
    let before = layer.clone();
    let error = layer
        .set_warp_and_render(&warp, &source_without_alpha, WarpRenderOptions::default())
        .unwrap_err();
    assert!(error.to_string().contains("omits the target layer's alpha"));
    assert_eq!(*layer, before);
}

#[test]
fn odd_sized_identity_warp_stays_anchored_at_the_source_origin() {
    // Upstream anchors the output at round(center − extent/2)
    // (`Util/CoordinateUtil.h`); rounding the center first drifted odd-sized
    // warps one document pixel off (see the origin placement in
    // `render_warped`). An identity 5×5 warp must stay at (0, 0), not (1, 1).
    let warp = psd::Warp::identity(5, 5).unwrap();
    let mut source = Raster::new(5, 5).unwrap();
    source
        .set_channel(ChannelKey::color(0), vec![0u8; 25])
        .unwrap();
    let rendered = render_warped(&warp, &source, WarpRenderOptions::default()).unwrap();
    assert_eq!(rendered.origin(), (0, 0));
    assert_eq!((rendered.width(), rendered.height()), (5, 5));
}

#[test]
fn mutated_warp_descriptors_fail_cleanly_in_parsing_and_geometry() {
    // Flip bytes of every fixture SoLd payload; whatever still parses as a
    // descriptor must either be rejected as a warp or yield geometry calls
    // that return (Ok or Err) without panicking.
    let document =
        LayeredFile::<u8>::read(fixture("SmartObjects/smart_objects_transformed.psd")).unwrap();
    let mut payloads: Vec<Vec<u8>> = document
        .layers()
        .filter_map(|layer| layer.blocks.get(psd::core::TaggedBlockKey::SOLD))
        .map(|block| block.data.to_vec())
        .collect();
    assert_eq!(payloads.len(), 11);

    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let (mut parsed_warps, mut rejected) = (0usize, 0usize);
    for payload in &mut payloads {
        for _ in 0..300 {
            let offset = (next() % payload.len() as u64) as usize;
            let mask = (next() % 255) as u8 + 1;
            payload[offset] ^= mask;
            let parsed = psd::core::PlacedLayerData::read(&mut psd::core::BeReader::new(payload))
                .ok()
                .and_then(|data| psd::Warp::from_placed_data(&data).ok());
            match parsed {
                Some(warp) => {
                    parsed_warps += 1;
                    let _ = warp.surface();
                    let _ = warp.mesh();
                    let _ = warp.bounds();
                    let _ = warp.control_bounds();
                    let _ = warp.tessellated_mesh(8, 8);
                    let _ = warp.no_op();
                    let _ = warp.to_descriptor();
                }
                None => rejected += 1,
            }
            payload[offset] ^= mask;
        }
    }
    // Most flips land in doubles and names, so both outcomes are exercised.
    assert!(
        parsed_warps > 0 && rejected > 0,
        "{parsed_warps} / {rejected}"
    );
}

#[test]
fn smart_object_warps_and_links_survive_a_document_round_trip() {
    for name in [
        "SmartObjects/smart_objects_transformed.psd",
        "SmartObjects/smart_object_file_no_warp.psd",
    ] {
        let document = LayeredFile::<u8>::read(fixture(name)).unwrap();
        let back = LayeredFile::<u8>::from_bytes(&document.to_bytes().unwrap()).unwrap();
        let warps = |file: &LayeredFile<u8>| -> Vec<(String, psd::Warp)> {
            file.layers()
                .filter(|layer| layer.is_smart_object())
                .map(|layer| (layer.name.clone(), layer.warp().unwrap().unwrap()))
                .collect()
        };
        let (before, after) = (warps(&document), warps(&back));
        assert!(!before.is_empty(), "{name}");
        assert_eq!(before, after, "{name}: warps changed");
        assert_eq!(
            document.linked_layers().unwrap(),
            back.linked_layers().unwrap(),
            "{name}: linked records changed"
        );
    }
}
