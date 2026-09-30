//! LayerAndMaskInformation tests against the vendored fixture corpus.
//!
//! The headline assertion is byte-exact re-serialization: reading
//! header + ColorModeData + ImageResources + LayerAndMaskInformation from a
//! real Photoshop file and writing them back must reproduce the file's first
//! `N` bytes exactly. That exercises layer records, masks, blending ranges,
//! `Lr16`/`Lr32` nesting, tagged-block passthrough and every padding rule at
//! once.

use std::path::PathBuf;

use psd_core::{
    BeReader, BeWriter, ColorModeData, FileHeader, ImageResources, LayerAndMaskInformation,
    SectionDivider, TaggedBlockKey,
};

fn fixture(name: &str) -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/documents")
        .join(name)
}

struct Sections {
    bytes: Vec<u8>,
    prefix_len: usize,
    header: FileHeader,
    color_mode_data: ColorModeData,
    image_resources: ImageResources,
    layer_and_mask_info: LayerAndMaskInformation<'static>,
}

fn read_sections(name: &str) -> Sections {
    let bytes = std::fs::read(fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    let mut reader = BeReader::new(&bytes);
    let header = FileHeader::read(&mut reader).unwrap();
    let color_mode_data = ColorModeData::read(&mut reader).unwrap();
    let image_resources = ImageResources::read(&mut reader).unwrap();
    let layer_and_mask_info = LayerAndMaskInformation::read(&mut reader, &header)
        .unwrap_or_else(|e| panic!("{name}: layer and mask info: {e}"));
    Sections {
        prefix_len: reader.position(),
        bytes,
        header,
        color_mode_data,
        image_resources,
        layer_and_mask_info,
    }
}

const ROUND_TRIP_FIXTURES: &[&str] = &[
    "SingleLayer/SingleLayer_8bit.psd",
    "SingleLayer/SingleLayer_8bit.psb",
    "SingleLayer/SingleLayer_16bit.psd",
    "SingleLayer/SingleLayer_16bit.psb",
    "SingleLayer/SingleLayer_32bit.psd",
    "SingleLayer/SingleLayer_32bit.psb",
    "Masks/Masks_8bit.psd",
    "Masks/Masks_8bit.psb",
    "Masks/SingleMask_White.psb",
    "Groups/Groups_8bit.psd",
    "Groups/Groups_8bit.psb",
    "Groups/Groups_16bit.psd",
    "Compression/Compression_RAW_8bit.psd",
    "Compression/Compression_RAW_8bit.psb",
    "Compression/Compression_ZipPrediction_16bit.psd",
    "Compression/Compression_ZipPrediction_32bit.psb",
    "CMYK/CMYK_8.psd",
    "CMYK/CMYK_16.psb",
    "Grayscale/Grayscale_8.psd",
    "Grayscale/Grayscale_32.psb",
];

#[test]
fn sections_round_trip_byte_exact_across_corpus() {
    for name in ROUND_TRIP_FIXTURES {
        let sections = read_sections(name);
        let mut writer = BeWriter::new();
        sections.header.write(&mut writer);
        sections
            .color_mode_data
            .write(&mut writer, &sections.header)
            .unwrap();
        sections.image_resources.write(&mut writer).unwrap();
        sections
            .layer_and_mask_info
            .write(&mut writer, &sections.header)
            .unwrap();

        assert_eq!(
            writer.as_slice(),
            &sections.bytes[..sections.prefix_len],
            "{name} did not round-trip byte-exact"
        );
    }
}

#[test]
fn layer_records_are_index_aligned_with_channel_data() {
    for name in ROUND_TRIP_FIXTURES {
        let sections = read_sections(name);
        let info = &sections.layer_and_mask_info.layer_info;
        assert_eq!(
            info.layer_records.len(),
            info.channel_image_data.len(),
            "{name}"
        );
        assert!(!info.layer_records.is_empty(), "{name}");
        for (record, channels) in info.layer_records.iter().zip(&info.channel_image_data) {
            assert_eq!(record.channels.len(), channels.channels.len(), "{name}");
        }
    }
}

#[test]
fn sixteen_and_thirty_two_bit_layers_come_from_tagged_blocks() {
    for (name, key) in [
        ("SingleLayer/SingleLayer_16bit.psd", TaggedBlockKey::LR16),
        ("SingleLayer/SingleLayer_32bit.psb", TaggedBlockKey::LR32),
    ] {
        let sections = read_sections(name);
        assert!(
            !sections
                .layer_and_mask_info
                .layer_info
                .layer_records
                .is_empty(),
            "{name}"
        );
        let ali = sections
            .layer_and_mask_info
            .additional_layer_info
            .as_ref()
            .unwrap_or_else(|| panic!("{name}: no document-level tagged blocks"));
        assert!(ali.get(key).is_some(), "{name}: missing {key}");
    }
}

#[test]
fn negative_layer_count_is_preserved() {
    // SingleMask_White.psb stores a negative count (merged image alpha).
    let sections = read_sections("Masks/SingleMask_White.psb");
    assert!(sections.layer_and_mask_info.layer_info.has_merged_alpha);

    // ...and the byte-exact round-trip above proves the sign is written back.
    let sections = read_sections("SingleLayer/SingleLayer_8bit.psd");
    assert!(!sections.layer_and_mask_info.layer_info.has_merged_alpha);
}

#[test]
fn masks_are_parsed_from_the_masks_fixture() {
    let sections = read_sections("Masks/Masks_8bit.psd");
    let records = &sections.layer_and_mask_info.layer_info.layer_records;

    // Layer 0 ("NoMask") has no mask; layer 1 ("Pixel(User)Mask") has a pixel
    // mask with default colour 255 (this fixture's mask region is empty).
    assert!(records[0].mask_data.is_none());
    let mask = records[1]
        .mask_data
        .as_ref()
        .and_then(|data| data.pixel_mask)
        .expect("pixel mask missing");
    assert_eq!((mask.top, mask.left, mask.bottom, mask.right), (0, 0, 0, 0));
    assert_eq!(mask.default_color, 255);
    assert!(mask.params.is_none());

    // The disabled variant carries the disabled flag.
    let disabled = records[2]
        .mask_data
        .as_ref()
        .and_then(|data| data.pixel_mask)
        .expect("disabled pixel mask missing");
    assert!(disabled.flags.disabled());

    // Mask channels are declared with just their 2-byte compression marker.
    let mask_channel = records[1]
        .channels
        .iter()
        .find(|c| c.index == -2)
        .expect("mask channel missing");
    assert_eq!(mask_channel.size, 2);
}

#[test]
fn real_mask_geometry_parses_from_the_white_mask_fixture() {
    // SingleMask_White.psb stores an actual mask region on a 32x32 layer:
    // top=16, left=0, bottom=32, right=32.
    let sections = read_sections("Masks/SingleMask_White.psb");
    let record = sections
        .layer_and_mask_info
        .layer_info
        .layer_records
        .iter()
        .find(|record| record.name.value() == "MaskLayer")
        .expect("MaskLayer missing");
    let mask = record
        .mask_data
        .as_ref()
        .and_then(|data| data.pixel_mask)
        .expect("pixel mask missing");
    assert_eq!(
        (mask.top, mask.left, mask.bottom, mask.right),
        (16, 0, 32, 32)
    );
}

#[test]
fn groups_expose_section_divider_blocks() {
    let sections = read_sections("Groups/Groups_8bit.psd");
    let records = &sections.layer_and_mask_info.layer_info.layer_records;

    let dividers: Vec<SectionDivider> = records
        .iter()
        .filter_map(|record| {
            record
                .additional_layer_info
                .as_ref()
                .and_then(|ali| ali.get(TaggedBlockKey::LSCT))
                .map(|block| {
                    let raw = u32::from_be_bytes(block.data[..4].try_into().unwrap());
                    SectionDivider::from_raw(raw)
                })
        })
        .collect();
    assert!(dividers.contains(&SectionDivider::OpenFolder));
    assert!(dividers.contains(&SectionDivider::BoundingSection));

    // Group records carry the pixel-data-irrelevant flag (bits 3+4).
    assert!(records[0].flags.pixel_data_irrelevant());
    assert_eq!(records[0].name.value(), "</Layer group>");
}

#[test]
fn unicode_names_and_unknown_blocks_survive() {
    // Every layer in the corpus has a 'luni' block and assorted blocks the
    // port has no parser for; all of them are preserved.
    let sections = read_sections("SingleLayer/SingleLayer_8bit.psd");
    let record = &sections.layer_and_mask_info.layer_info.layer_records[0];
    let ali = record
        .additional_layer_info
        .as_ref()
        .expect("layer tagged blocks missing");
    assert!(ali.get(TaggedBlockKey::LUNI).is_some());
    assert!(ali.get(TaggedBlockKey::new(*b"shmd")).is_some());
    assert!(ali.get(TaggedBlockKey::new(*b"fxrp")).is_some());
}
