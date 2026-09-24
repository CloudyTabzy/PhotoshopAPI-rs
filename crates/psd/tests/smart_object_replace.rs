#![cfg(feature = "image")]

use std::io::Cursor;
use std::path::{Path, PathBuf};

use psd::{
    core::{
        BeReader, BeWriter, DescriptorValue, LayerMask, LayerMaskData, LayerMaskFlags,
        LinkedDataKind, LinkedLayerTaggedBlock, TaggedBlock, TaggedBlockKey, UnicodeString,
    },
    ChannelKey, LayeredFile, LinkedStorage, WarpRenderOptions,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/documents")
        .join(name)
}

fn replacement_png() -> (String, Vec<u8>) {
    let path = fixture("SmartObjects/reference/control.png");
    (
        path.file_name().unwrap().to_string_lossy().into_owned(),
        std::fs::read(path).unwrap(),
    )
}

fn replacement_dimensions(bytes: &[u8]) -> (usize, usize) {
    let image = image::load_from_memory(bytes).unwrap();
    (image.width() as usize, image.height() as usize)
}

fn synthetic_png(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbaImage::from_fn(width, height, |x, y| {
        image::Rgba([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8, 255])
    });
    let mut writer = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut writer, image::ImageFormat::Png)
        .unwrap();
    writer.into_inner()
}

fn smart_object_id(layer: &psd::Layer<u8>) -> String {
    layer
        .smart_object_data()
        .unwrap()
        .unwrap()
        .descriptor
        .get("Idnt")
        .and_then(DescriptorValue::as_str)
        .unwrap()
        .to_owned()
}

#[test]
fn embedded_jpeg_source_is_resolved_by_the_layer_identity() {
    let document =
        LayeredFile::<u8>::read(fixture("SmartObjects/smart_objects_transformed.psd")).unwrap();
    let layer_id = document
        .find_layer("simple_warp/bbox_change_perspective_warp")
        .unwrap();
    let layer = document.layer(layer_id).unwrap();
    let id = smart_object_id(layer);
    let record = document
        .linked_layers()
        .unwrap()
        .into_iter()
        .find(|record| record.unique_id.value() == id)
        .unwrap();
    assert_eq!(record.kind, LinkedDataKind::Data);
    assert_eq!(record.file_type, *b"JPEG");

    let source = document.smart_object_source(layer_id).unwrap();
    assert_eq!((source.width(), source.height()), (512, 512));
    for key in [
        ChannelKey::color(0),
        ChannelKey::color(1),
        ChannelKey::color(2),
    ] {
        assert_eq!(source.channel(key).unwrap().len(), 512 * 512);
    }
}

#[test]
fn alias_source_lookup_returns_an_explicit_unsupported_error() {
    let mut document =
        LayeredFile::<u8>::read(fixture("SmartObjects/smart_objects_transformed.psd")).unwrap();
    let layer_id = document
        .find_layer("simple_warp/bbox_change_perspective_warp")
        .unwrap();
    let id = smart_object_id(document.layer(layer_id).unwrap());
    let block = document
        .document_blocks
        .as_mut()
        .unwrap()
        .get_mut(TaggedBlockKey::new(*b"lnk2"))
        .unwrap();
    let mut links = LinkedLayerTaggedBlock::read(&mut BeReader::new(&block.data)).unwrap();
    let record = links
        .layers
        .iter_mut()
        .find(|record| record.unique_id.value() == id)
        .unwrap();
    record.kind = LinkedDataKind::Alias;
    record.raw_file_bytes.clear();
    record.file_size = 0;
    let mut writer = BeWriter::new();
    links.write(&mut writer).unwrap();
    block.data = writer.into_inner();

    assert!(matches!(
        document.smart_object_source(layer_id),
        Err(psd::core::PsdError::ImageDecode(message)) if message.contains("alias")
    ));
}

#[test]
fn duplicate_linked_block_keys_are_scanned_and_ambiguous_ids_rejected() {
    let mut document =
        LayeredFile::<u8>::read(fixture("SmartObjects/smart_objects_transformed.psd")).unwrap();
    let layer_id = document
        .find_layer("simple_warp/bbox_change_perspective_warp")
        .unwrap();
    let id = smart_object_id(document.layer(layer_id).unwrap());
    let original_link_count = document.linked_layers().unwrap().len();
    let block_key = TaggedBlockKey::new(*b"lnk2");
    let block = document
        .document_blocks
        .as_ref()
        .unwrap()
        .get(block_key)
        .unwrap();
    let duplicate_count = LinkedLayerTaggedBlock::read(&mut BeReader::new(&block.data))
        .unwrap()
        .layers
        .len();
    let duplicate = TaggedBlock::new(block_key, block.data.clone());
    document.document_blocks.as_mut().unwrap().push(duplicate);

    assert_eq!(
        document.linked_layers().unwrap().len(),
        original_link_count + duplicate_count
    );
    assert!(matches!(
        document.smart_object_source(layer_id),
        Err(psd::core::PsdError::ImageDecode(message)) if message.contains("multiple")
    ));
    assert_eq!(smart_object_id(document.layer(layer_id).unwrap()), id);
}

#[test]
fn embedded_photoshop_smart_object_sources_decode_their_merged_preview() {
    let document =
        LayeredFile::<u8>::read(fixture("SmartObjects/smart_object_file_no_warp.psd")).unwrap();
    let linked = document.linked_layers().unwrap();
    let photoshop_sources: Vec<_> = linked
        .iter()
        .filter(|record| {
            record.kind == LinkedDataKind::Data
                && (record.file_type == *b"8BPS" || record.file_type == *b"8BPB")
        })
        .collect();
    assert!(
        !photoshop_sources.is_empty(),
        "expected linked PSD/PSB payloads"
    );

    for record in photoshop_sources {
        let layer_id = document
            .layers()
            .enumerate()
            .find_map(|(id, layer)| {
                (layer.is_smart_object()
                    && layer
                        .smart_object_data()
                        .ok()
                        .flatten()
                        .and_then(|data| {
                            data.descriptor
                                .get("Idnt")
                                .and_then(|v| v.as_str())
                                .map(str::to_owned)
                        })
                        .as_deref()
                        == Some(record.unique_id.value()))
                .then_some(id)
            })
            .expect("PSD/PSB linked record should be associated with a placed layer");
        let source = document.smart_object_source(layer_id).unwrap();
        assert!(source.width() > 0 && source.height() > 0);
        assert!(source.channel(ChannelKey::color(0)).is_some());
        assert!(source.channel(ChannelKey::color(1)).is_some());
        assert!(source.channel(ChannelKey::color(2)).is_some());
    }
}

#[test]
fn embedded_source_resolves_by_layer_id_and_replacement_is_idempotent() {
    let mut document =
        LayeredFile::<u8>::read(fixture("SmartObjects/smart_objects_transformed.psd")).unwrap();
    let layer_id = document
        .find_layer("simple_warp/bbox_change_perspective_warp")
        .unwrap();
    let original_source = document.smart_object_source(layer_id).unwrap();
    assert_eq!(
        (original_source.width(), original_source.height()),
        (512, 512)
    );

    let (file_name, encoded) = replacement_png();
    let replacement_size = replacement_dimensions(&encoded);
    let original_link_count = document.linked_layers().unwrap().len();
    document
        .replace_smart_object_from_bytes(layer_id, &file_name, &encoded)
        .unwrap();

    let first_layer = document.layer(layer_id).unwrap();
    let first_warp = first_layer.warp().unwrap().unwrap();
    let first_bounds = first_layer.bounds;
    let first_channels = first_layer.image().unwrap().channels.clone();
    let first_source = document.smart_object_source(layer_id).unwrap();
    assert_eq!(
        (first_source.width(), first_source.height()),
        replacement_size
    );
    let link_count_after_first = document.linked_layers().unwrap().len();
    assert!(link_count_after_first >= original_link_count);

    // Upstream checks ten repeated same-PNG replacements on one fixture. This
    // also exercises a source-size change and verifies the warp's stored source
    // bounds are rebased, preventing coordinate drift hidden by render UVs.
    for _ in 0..10 {
        document
            .replace_smart_object_from_bytes(layer_id, &file_name, &encoded)
            .unwrap();
        let layer = document.layer(layer_id).unwrap();
        assert_eq!(layer.bounds, first_bounds);
        assert!(layer
            .warp()
            .unwrap()
            .unwrap()
            .approximately_eq(&first_warp, 1e-9));
        assert_eq!(layer.image().unwrap().channels, first_channels);
        assert_eq!(
            document.linked_layers().unwrap().len(),
            link_count_after_first
        );
    }

    let layer = document.layer(layer_id).unwrap();
    let placed = layer.placed_layer().unwrap().unwrap();
    let id = smart_object_id(layer);
    assert_eq!(placed.unique_id.value(), id);
    let links = document.linked_layers().unwrap();
    let matching: Vec<_> = links
        .iter()
        .filter(|record| record.unique_id.value() == id)
        .collect();
    assert_eq!(matching.len(), 1);
    assert_eq!(matching[0].kind, LinkedDataKind::Data);
    assert_eq!(matching[0].raw_file_bytes, encoded);

    let bytes = document.to_bytes().unwrap();
    let reopened = LayeredFile::<u8>::from_bytes(&bytes).unwrap();
    let reopened_source = reopened.smart_object_source(layer_id).unwrap();
    assert_eq!(reopened_source, first_source);
    let reopened_layer = reopened.layer(layer_id).unwrap();
    assert_eq!(reopened_layer.bounds, first_bounds);
    assert_eq!(reopened_layer.image().unwrap().channels, first_channels);
    let rendered = reopened
        .layer(layer_id)
        .unwrap()
        .warp()
        .unwrap()
        .unwrap()
        .apply(&reopened_source, WarpRenderOptions::default())
        .unwrap();
    assert_eq!(
        reopened_layer.bounds,
        psd::Rect::new(
            rendered.origin().1,
            rendered.origin().0,
            rendered.origin().1 + rendered.height() as i32,
            rendered.origin().0 + rendered.width() as i32,
        )
    );
}

#[test]
fn different_size_replacement_rebases_warp_points_once() {
    let mut document =
        LayeredFile::<u8>::read(fixture("SmartObjects/smart_objects_transformed.psd")).unwrap();
    let layer_id = document
        .find_layer("simple_warp/bbox_change_perspective_warp")
        .unwrap();
    let source = synthetic_png(96, 80);
    let source_dimensions = replacement_dimensions(&source);
    let mut first = true;
    let mut previous_warp = None;
    let mut previous_bounds = None;
    let mut previous_channels = None;

    for _ in 0..2 {
        document
            .replace_smart_object_from_bytes(layer_id, "different-size.png", &source)
            .unwrap();
        let layer = document.layer(layer_id).unwrap();
        let warp = layer.warp().unwrap().unwrap();
        assert_eq!(
            (warp.source_bounds().width(), warp.source_bounds().height()),
            (source_dimensions.0 as f64, source_dimensions.1 as f64)
        );
        if first {
            previous_warp = Some(warp);
            previous_bounds = Some(layer.bounds);
            previous_channels = Some(layer.image().unwrap().channels.clone());
            first = false;
        } else {
            assert!(warp.approximately_eq(previous_warp.as_ref().unwrap(), 1.0e-9));
            assert_eq!(layer.bounds, previous_bounds.unwrap());
            assert_eq!(
                &layer.image().unwrap().channels,
                previous_channels.as_ref().unwrap()
            );
        }
    }
}

#[test]
fn external_source_resolves_by_absolute_path_after_document_roundtrip() {
    let mut document =
        LayeredFile::<u8>::read(fixture("SmartObjects/smart_objects_transformed.psd")).unwrap();
    let layer_id = document
        .find_layer("simple_warp/bbox_change_perspective_warp")
        .unwrap();
    let source_path = fixture("SmartObjects/reference/control.png");
    let replacement_size = replacement_dimensions(&std::fs::read(&source_path).unwrap());
    document
        .replace_smart_object_with_storage(layer_id, &source_path, LinkedStorage::External)
        .unwrap();

    let source = document.smart_object_source(layer_id).unwrap();
    assert_eq!((source.width(), source.height()), replacement_size);
    let id = smart_object_id(document.layer(layer_id).unwrap());
    let record = document
        .linked_layers()
        .unwrap()
        .into_iter()
        .find(|record| record.unique_id.value() == id)
        .unwrap();
    assert_eq!(record.kind, LinkedDataKind::External);
    assert!(record.raw_file_bytes.is_empty());
    assert!(record.linked_file_descriptor.is_some());

    let bytes = document.to_bytes().unwrap();
    let reopened = LayeredFile::<u8>::from_bytes(&bytes).unwrap();
    assert_eq!(reopened.smart_object_source(layer_id).unwrap(), source);
}

#[test]
fn replacement_appends_a_link_without_rewriting_unknown_records_or_block_tail() {
    let mut document =
        LayeredFile::<u8>::read(fixture("SmartObjects/smart_objects_transformed.psd")).unwrap();
    let layer_id = document
        .find_layer("simple_warp/bbox_change_perspective_warp")
        .unwrap();
    let block = document
        .document_blocks
        .as_mut()
        .unwrap()
        .get_mut(TaggedBlockKey::new(*b"lnk2"))
        .expect("embedded linked block");

    let mut unknown_record = Vec::new();
    unknown_record.extend_from_slice(&4u64.to_be_bytes());
    unknown_record.extend_from_slice(b"liZZ");
    let original = block.data.clone();
    let mut records_end = 0;
    while original.len() - records_end >= 8 {
        let size =
            u64::from_be_bytes(original[records_end..records_end + 8].try_into().unwrap()) as usize;
        let record_len = 8 + size.div_ceil(4) * 4;
        assert!(records_end + record_len <= original.len());
        records_end += record_len;
    }
    let mut protected_prefix = original[..records_end].to_vec();
    protected_prefix.extend_from_slice(&unknown_record);
    let mut block_tail = original[records_end..].to_vec();
    block_tail.extend_from_slice(&[0xDE, 0xAD, 0xBE]);
    block.data.clear();
    block.data.extend_from_slice(&protected_prefix);
    block.data.extend_from_slice(&block_tail);

    let (file_name, encoded) = replacement_png();
    document
        .replace_smart_object_from_bytes(layer_id, &file_name, &encoded)
        .unwrap();
    let updated_block = document
        .document_blocks
        .as_ref()
        .unwrap()
        .get(TaggedBlockKey::new(*b"lnk2"))
        .unwrap();
    assert!(updated_block.data.starts_with(&protected_prefix));
    assert!(updated_block.data.ends_with(&block_tail));

    let serialized = LayeredFile::<u8>::from_bytes(&document.to_bytes().unwrap()).unwrap();
    let serialized_block = serialized
        .document_blocks
        .as_ref()
        .unwrap()
        .get(TaggedBlockKey::new(*b"lnk2"))
        .unwrap();
    assert!(serialized_block.data.starts_with(&protected_prefix));
    assert!(serialized_block.data.ends_with(&block_tail));
}

#[test]
fn relative_external_links_require_and_use_explicit_document_path_context() {
    let mut document =
        LayeredFile::<u8>::read(fixture("SmartObjects/smart_objects_transformed.psd")).unwrap();
    let layer_id = document
        .find_layer("simple_warp/bbox_change_perspective_warp")
        .unwrap();
    document
        .replace_smart_object_with_storage(
            layer_id,
            fixture("SmartObjects/reference/control.png"),
            LinkedStorage::External,
        )
        .unwrap();
    let id = smart_object_id(document.layer(layer_id).unwrap());
    let block = document
        .document_blocks
        .as_mut()
        .unwrap()
        .get_mut(TaggedBlockKey::new(*b"lnkE"))
        .unwrap();
    let mut links = LinkedLayerTaggedBlock::read(&mut BeReader::new(&block.data)).unwrap();
    let record = links
        .layers
        .iter_mut()
        .find(|record| record.unique_id.value() == id)
        .unwrap();
    let descriptor = record.linked_file_descriptor.as_mut().unwrap();
    for key in ["originalPath", "fullPath", "relPath"] {
        descriptor.insert(
            key,
            DescriptorValue::String(UnicodeString::new("reference/control.png", 2).unwrap()),
        );
    }
    let mut writer = BeWriter::new();
    links.write(&mut writer).unwrap();
    block.data = writer.into_inner();

    let bytes = document.to_bytes().unwrap();
    let detached = LayeredFile::<u8>::from_bytes(&bytes).unwrap();
    assert!(matches!(
        detached.smart_object_source(layer_id),
        Err(psd::core::PsdError::ImageDecode(_))
    ));

    let contextual = LayeredFile::<u8>::from_bytes_with_source_path(
        &bytes,
        fixture("SmartObjects/context-document.psd"),
    )
    .unwrap();
    let source = contextual.smart_object_source(layer_id).unwrap();
    assert_eq!(
        (source.width(), source.height()),
        replacement_dimensions(&replacement_png().1)
    );

    let mut filename_fallback = document.clone();
    let block = filename_fallback
        .document_blocks
        .as_mut()
        .unwrap()
        .get_mut(TaggedBlockKey::new(*b"lnkE"))
        .unwrap();
    let mut links = LinkedLayerTaggedBlock::read(&mut BeReader::new(&block.data)).unwrap();
    let record = links
        .layers
        .iter_mut()
        .find(|record| record.unique_id.value() == id)
        .unwrap();
    record.file_name = UnicodeString::new("reference/control.png", 2).unwrap();
    let descriptor = record.linked_file_descriptor.as_mut().unwrap();
    for key in ["originalPath", "fullPath", "relPath"] {
        descriptor.insert(
            key,
            DescriptorValue::String(UnicodeString::new("", 2).unwrap()),
        );
    }
    let mut writer = BeWriter::new();
    links.write(&mut writer).unwrap();
    block.data = writer.into_inner();
    let fallback_bytes = filename_fallback.to_bytes().unwrap();
    assert!(LayeredFile::<u8>::from_bytes(&fallback_bytes)
        .unwrap()
        .smart_object_source(layer_id)
        .is_err());
    assert_eq!(
        LayeredFile::<u8>::from_bytes_with_source_path(
            &fallback_bytes,
            fixture("SmartObjects/context-document.psd"),
        )
        .unwrap()
        .smart_object_source(layer_id)
        .unwrap(),
        source
    );
}

#[test]
fn failed_source_replacement_is_transactional() {
    let mut document =
        LayeredFile::<u8>::read(fixture("SmartObjects/smart_objects_transformed.psd")).unwrap();
    let layer_id = document
        .find_layer("simple_warp/bbox_change_perspective_warp")
        .unwrap();
    let before = document.clone();
    assert!(document
        .replace_smart_object(layer_id, fixture("SmartObjects/missing.png"),)
        .is_err());
    assert_eq!(document, before);

    assert!(document
        .replace_smart_object(layer_id, fixture("SmartObjects"),)
        .is_err());
    assert_eq!(document, before);

    assert!(document
        .replace_smart_object_from_bytes(layer_id, "bad.bin", b"not an image")
        .is_err());
    assert_eq!(document, before);
}

#[test]
fn replacement_preserves_mask_planes_and_mask_metadata() {
    let mut document =
        LayeredFile::<u8>::read(fixture("SmartObjects/smart_objects_transformed.psd")).unwrap();
    let layer_id = document
        .find_layer("simple_warp/bbox_change_perspective_warp")
        .unwrap();
    let layer = document.layer_mut(layer_id).unwrap();
    let original_bounds = layer.bounds;
    let mask_metadata = LayerMaskData {
        pixel_mask: Some(LayerMask {
            top: original_bounds.top,
            left: original_bounds.left,
            bottom: original_bounds.bottom,
            right: original_bounds.right,
            default_color: 0,
            flags: LayerMaskFlags::from_bits(0),
            params: None,
        }),
        vector_mask: None,
    };
    layer.mask = Some(mask_metadata.clone());
    let mask_samples = vec![73; original_bounds.sample_count()];
    layer
        .image_mut()
        .unwrap()
        .set_channel(ChannelKey::USER_MASK, mask_samples.clone());

    let (file_name, encoded) = replacement_png();
    document
        .replace_smart_object_from_bytes(layer_id, file_name, &encoded)
        .unwrap();
    let layer = document.layer(layer_id).unwrap();
    assert_eq!(layer.mask, Some(mask_metadata.clone()));
    assert_eq!(
        layer.image().unwrap().channel(ChannelKey::USER_MASK),
        Some(mask_samples.as_slice())
    );

    let reopened = LayeredFile::<u8>::from_bytes(&document.to_bytes().unwrap()).unwrap();
    let reopened_layer = reopened.layer(layer_id).unwrap();
    assert_eq!(reopened_layer.mask, Some(mask_metadata));
    assert_eq!(
        reopened_layer
            .image()
            .unwrap()
            .channel(ChannelKey::USER_MASK),
        Some(mask_samples.as_slice())
    );
}
