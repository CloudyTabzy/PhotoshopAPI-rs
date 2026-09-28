use psd::core::{
    BeReader, BeWriter, ColorMode, Compression, LayerMask, PhotoshopFile, PsdError, Version,
};
use psd::{ChannelKey, Layer, LayeredFile, ReadOptions, Rect, DEFAULT_TOTAL_MEMORY_LIMIT};

fn document_bytes(layer_count: usize) -> Vec<u8> {
    let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 2, 2).unwrap();
    for index in 0..layer_count {
        let mut layer = Layer::new_image(format!("Layer {index}"), Rect::new(0, 0, 2, 2));
        layer
            .image_mut()
            .unwrap()
            .channels
            .insert(ChannelKey::color(0), vec![1, 2, 3, 4]);
        document.add_layer(layer);
    }
    let file = document.to_photoshop_file().unwrap();
    let mut writer = BeWriter::new();
    file.write(&mut writer).unwrap();
    writer.into_inner()
}

fn document_bytes_with_compressions(version: Version) -> Vec<u8> {
    let codecs = [
        Compression::Raw,
        Compression::Rle,
        Compression::Zip,
        Compression::ZipPrediction,
    ];
    let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 8, 8).unwrap();
    document.version = version;
    for (index, compression) in codecs.into_iter().enumerate() {
        let mut layer = Layer::new_image(format!("Layer {index}"), Rect::new(0, 0, 8, 8));
        layer.compression = Some(compression);
        layer.image_mut().unwrap().channels.insert(
            ChannelKey::color(0),
            (0..64).map(|pixel| pixel as u8).collect(),
        );
        layer
            .image_mut()
            .unwrap()
            .channels
            .insert(ChannelKey::ALPHA, vec![255; 64]);
        document.add_layer(layer);
    }
    document.to_bytes().unwrap()
}

fn channel_payloads(bytes: &[u8]) -> Vec<Vec<(Compression, Vec<u8>)>> {
    let mut reader = BeReader::new(bytes);
    let file = PhotoshopFile::read(&mut reader).unwrap();
    file.layer_and_mask_info
        .layer_info
        .channel_image_data
        .iter()
        .map(|layer| {
            layer
                .channels
                .iter()
                .map(|channel| (channel.compression, channel.data.clone()))
                .collect()
        })
        .collect()
}

fn bytes_with_large_layer(width: i32, height: i32) -> Vec<u8> {
    bytes_with_layer_rect(0, 0, width, height)
}

fn bytes_with_layer_rect(left: i32, top: i32, right: i32, bottom: i32) -> Vec<u8> {
    let bytes = document_bytes(1);
    let mut reader = psd::core::BeReader::new(&bytes);
    let mut file = psd::core::PhotoshopFile::read(&mut reader).unwrap();
    let record = &mut file.layer_and_mask_info.layer_info.layer_records[0];
    record.left = left;
    record.top = top;
    record.right = right;
    record.bottom = bottom;
    let mut writer = BeWriter::new();
    file.write(&mut writer).unwrap();
    writer.into_inner()
}

fn bytes_with_large_mask() -> Vec<u8> {
    let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 2, 2).unwrap();
    let mut layer = Layer::new_image("Masked", Rect::new(0, 0, 2, 2));
    layer
        .image_mut()
        .unwrap()
        .channels
        .insert(ChannelKey::USER_MASK, vec![1, 2, 3, 4]);
    layer.mask = Some(psd::core::LayerMaskData {
        pixel_mask: Some(LayerMask {
            top: 0,
            left: 0,
            bottom: 2,
            right: 2,
            default_color: 0,
            flags: Default::default(),
            params: None,
        }),
        vector_mask: None,
    });
    document.add_layer(layer);

    let mut file = document.to_photoshop_file().unwrap();
    file.layer_and_mask_info.layer_info.layer_records[0]
        .mask_data
        .as_mut()
        .unwrap()
        .pixel_mask
        .as_mut()
        .unwrap()
        .right = 30_001;
    let mut writer = BeWriter::new();
    file.write(&mut writer).unwrap();
    writer.into_inner()
}

#[test]
fn default_budget_is_documented_and_unlimited_is_explicit() {
    assert_eq!(
        ReadOptions::default().total_memory_limit,
        Some(DEFAULT_TOTAL_MEMORY_LIMIT)
    );
    assert_eq!(ReadOptions::unlimited().total_memory_limit, None);
    assert!(!ReadOptions::default().use_raw_data);
    assert!(!ReadOptions::unlimited().use_raw_data);

    let bytes = document_bytes(1);
    let decoded =
        LayeredFile::<u8>::from_bytes_with_options(&bytes, ReadOptions::unlimited()).unwrap();
    assert_eq!(decoded.layer_count(), 1);
}

#[test]
fn lazy_read_writes_untouched_compressed_channels_verbatim() {
    for version in [Version::Psd, Version::Psb] {
        let bytes = document_bytes_with_compressions(version);
        let original_payloads = channel_payloads(&bytes);
        let options = ReadOptions::default().with_raw_data(true);
        let document = LayeredFile::<u8>::from_bytes_with_options(&bytes, options).unwrap();

        for layer in document.layers() {
            let channels = &layer.image().unwrap().channels;
            assert!(channels.is_raw(ChannelKey::color(0)));
            assert!(channels.is_raw(ChannelKey::ALPHA));
            assert_eq!(channels.get(ChannelKey::color(0)), None);
        }

        let rewritten = document.to_bytes().unwrap();
        assert_eq!(channel_payloads(&rewritten), original_payloads);
    }
}

#[test]
fn decoding_one_channel_releases_only_that_raw_payload() {
    let bytes = document_bytes_with_compressions(Version::Psd);
    let original_payloads = channel_payloads(&bytes);
    let mut document = LayeredFile::<u8>::from_bytes_with_options(
        &bytes,
        ReadOptions::default().with_raw_data(true),
    )
    .unwrap();
    let id = document.find_layer("Layer 0").unwrap();

    assert!(document
        .decode_layer_channel(id, ChannelKey::color(0))
        .unwrap());
    let channels = &document.layer(id).unwrap().image().unwrap().channels;
    assert!(!channels.is_raw(ChannelKey::color(0)));
    assert!(channels.is_raw(ChannelKey::ALPHA));
    assert_eq!(
        channels.get(ChannelKey::color(0)),
        Some(
            (0..64)
                .map(|pixel| pixel as u8)
                .collect::<Vec<_>>()
                .as_slice()
        )
    );

    let partially_written = channel_payloads(&document.to_bytes().unwrap());
    assert_eq!(partially_written[0][0], original_payloads[0][0]);
    assert_eq!(&partially_written[1..], &original_payloads[1..]);

    document.decode_layer_pixels(id).unwrap();
    let channels = &document.layer(id).unwrap().image().unwrap().channels;
    assert!(!channels.is_raw(ChannelKey::ALPHA));
    assert_eq!(
        channels.get(ChannelKey::ALPHA),
        Some([255u8; 64].as_slice())
    );
    assert!(!channels.is_raw(ChannelKey::color(0)));
    let another_layer = document.find_layer("Layer 1").unwrap();
    assert!(document
        .layer(another_layer)
        .unwrap()
        .image()
        .unwrap()
        .channels
        .is_raw(ChannelKey::color(0)));
}

#[test]
fn failed_layer_decode_keeps_raw_payloads_and_does_not_spend_budget() {
    let bytes = document_bytes_with_compressions(Version::Psd);
    let mut document = LayeredFile::<u8>::from_bytes_with_options(
        &bytes,
        ReadOptions {
            total_memory_limit: Some(127),
            use_raw_data: true,
        },
    )
    .unwrap();
    let id = document.find_layer("Layer 0").unwrap();

    for _ in 0..2 {
        assert!(matches!(
            document.decode_layer_pixels(id),
            Err(PsdError::ExceededMemoryLimit {
                requested: 64,
                available: 63
            })
        ));
        let channels = &document.layer(id).unwrap().image().unwrap().channels;
        assert!(channels.is_raw(ChannelKey::color(0)));
        assert!(channels.is_raw(ChannelKey::ALPHA));
        assert_eq!(channels.get(ChannelKey::color(0)), None);
    }
}

#[test]
fn raw_channels_reject_incompatible_geometry_or_compression_edits() {
    let bytes = document_bytes_with_compressions(Version::Psd);
    let mut geometry = LayeredFile::<u8>::from_bytes_with_options(
        &bytes,
        ReadOptions::default().with_raw_data(true),
    )
    .unwrap();
    let id = geometry.find_layer("Layer 0").unwrap();
    geometry.layer_mut(id).unwrap().bounds.right += 1;
    assert!(matches!(
        geometry.to_bytes(),
        Err(PsdError::InvalidData { message, .. })
            if message == "decode raw channel data before changing its geometry"
    ));

    let mut compression = LayeredFile::<u8>::from_bytes_with_options(
        &bytes,
        ReadOptions::default().with_raw_data(true),
    )
    .unwrap();
    compression.compression = Some(Compression::Rle);
    assert!(matches!(
        compression.to_bytes(),
        Err(PsdError::InvalidData { message, .. })
            if message == "decode raw channels before changing their compression"
    ));
}

#[test]
fn raw_rle_channels_are_decoded_before_psd_psb_conversion() {
    let bytes = document_bytes_with_compressions(Version::Psd);
    let mut document = LayeredFile::<u8>::from_bytes_with_options(
        &bytes,
        ReadOptions::default().with_raw_data(true),
    )
    .unwrap();
    let id = document.find_layer("Layer 1").unwrap();
    document.version = Version::Psb;
    assert!(matches!(
        document.to_bytes(),
        Err(PsdError::InvalidData { message, .. })
            if message == "decode raw RLE channels before changing PSD/PSB version"
    ));

    document.decode_layer_pixels(id).unwrap();
    let converted = LayeredFile::<u8>::from_bytes(&document.to_bytes().unwrap()).unwrap();
    assert_eq!(converted.version, Version::Psb);
    assert_eq!(
        converted
            .layer_by_path("Layer 1")
            .unwrap()
            .image()
            .unwrap()
            .channels
            .get(ChannelKey::color(0)),
        Some(
            (0..64)
                .map(|pixel| pixel as u8)
                .collect::<Vec<_>>()
                .as_slice()
        )
    );
}

#[test]
fn memory_budget_is_cumulative_across_decoded_channels() {
    // The bottom layer covers the canvas, so it keeps its single color
    // channel; the layer above it gains the synthesized transparency channel.
    // The document therefore needs three 4-byte channels decoded.
    let bytes = document_bytes(2);
    let err = LayeredFile::<u8>::from_bytes_with_options(
        &bytes,
        ReadOptions {
            total_memory_limit: Some(7),
            ..ReadOptions::default()
        },
    )
    .unwrap_err();
    assert!(matches!(
        err,
        PsdError::ExceededMemoryLimit {
            requested: 4,
            available: 3
        }
    ));

    // The budget accumulates across channels: two fit in 11 bytes, the third
    // runs out.
    let err = LayeredFile::<u8>::from_bytes_with_options(
        &bytes,
        ReadOptions {
            total_memory_limit: Some(11),
            ..ReadOptions::default()
        },
    )
    .unwrap_err();
    assert!(matches!(
        err,
        PsdError::ExceededMemoryLimit {
            requested: 4,
            available: 3
        }
    ));

    LayeredFile::<u8>::from_bytes_with_options(
        &bytes,
        ReadOptions {
            total_memory_limit: Some(12),
            ..ReadOptions::default()
        },
    )
    .unwrap();
}

#[test]
fn oversized_layer_rect_is_rejected_before_channel_decode() {
    let bytes = bytes_with_large_layer(30_001, 2);
    let err = LayeredFile::<u8>::from_bytes(&bytes).unwrap_err();
    assert!(matches!(
        err,
        PsdError::InvalidImageBounds {
            kind: "layer",
            width: 30_001,
            height: 2
        }
    ));
}

#[test]
fn inverted_and_extreme_layer_rects_return_typed_errors() {
    let bytes = bytes_with_large_layer(2, -1);
    assert!(matches!(
        LayeredFile::<u8>::from_bytes(&bytes),
        Err(PsdError::InvalidImageBounds {
            kind: "layer",
            width: 2,
            height: -1
        })
    ));

    let bytes = bytes_with_layer_rect(i32::MIN, 0, i32::MAX, 2);
    assert!(matches!(
        LayeredFile::<u8>::from_bytes(&bytes),
        Err(PsdError::InvalidImageBounds {
            kind: "layer",
            width: 4_294_967_295,
            height: 2
        })
    ));
}

#[test]
fn memory_budget_rejects_large_declared_bitmap_before_allocation() {
    // The extent is valid for PSD but asks for a 900 MB channel plane. A tiny
    // configured budget proves rejection happens before the channel decoder runs.
    let bytes = bytes_with_large_layer(30_000, 30_000);
    let err = LayeredFile::<u8>::from_bytes_with_options(
        &bytes,
        ReadOptions {
            total_memory_limit: Some(64),
            ..ReadOptions::default()
        },
    )
    .unwrap_err();
    assert!(matches!(
        err,
        PsdError::ExceededMemoryLimit {
            requested: 900_000_000,
            available: 64
        }
    ));
}

#[test]
fn oversized_mask_rect_is_rejected_before_channel_decode() {
    let bytes = bytes_with_large_mask();
    let err = LayeredFile::<u8>::from_bytes(&bytes).unwrap_err();
    assert!(matches!(
        err,
        PsdError::InvalidImageBounds {
            kind: "mask",
            width: 30_001,
            height: 2
        }
    ));
}
