//! Section-level tests against the vendored fixture corpus.
//!
//! These mirror upstream's `TestDPIRead` / `TestICCProfileRead` /
//! `TestRoundtripping` cases at the raw-format layer: read a real Photoshop
//! file's header, ColorModeData and ImageResources, and assert both the
//! interpreted values and byte-exact re-serialization.

use std::path::PathBuf;

use psd_core::{
    BeReader, BeWriter, ColorModeData, DisplayUnit, FileHeader, ImageResources, ResolutionUnit,
    ResourceBlock,
};

fn fixture(name: &str) -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/documents")
        .join(name)
}

struct Sections {
    bytes: Vec<u8>,
    /// Bytes consumed by header + ColorModeData + ImageResources.
    prefix_len: usize,
    header: FileHeader,
    color_mode_data: ColorModeData,
    image_resources: ImageResources,
}

fn read_sections(name: &str) -> Sections {
    let bytes = std::fs::read(fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    let mut reader = BeReader::new(&bytes);
    let header = FileHeader::read(&mut reader).unwrap();
    let color_mode_data = ColorModeData::read(&mut reader).unwrap();
    let image_resources = ImageResources::read(&mut reader).unwrap();
    Sections {
        prefix_len: reader.position(),
        bytes,
        header,
        color_mode_data,
        image_resources,
    }
}

#[test]
fn dpi_values_match_upstream_tests() {
    for (name, expected) in [
        ("DPI/300dpi.psd", 300.0f32),
        ("DPI/300_point_5_dpi.psd", 300.5f32),
        ("DPI/700dpi.psd", 700.0f32),
    ] {
        let sections = read_sections(name);
        let info = sections
            .image_resources
            .resolution_info()
            .unwrap_or_else(|| panic!("{name}: no ResolutionInfo block"));
        assert_eq!(info.horizontal_resolution.to_f32(), expected, "{name}");
        assert_eq!(
            info.horizontal_resolution_unit,
            ResolutionUnit::PixelsPerInch
        );
        assert_eq!(info.width_unit, DisplayUnit::Cm);
    }
}

#[test]
fn icc_profiles_match_the_extracted_files() {
    for name in ["AdobeRGB1998", "AppleRGB", "CIERGB"] {
        let sections = read_sections(&format!("ICCProfiles/{name}.psb"));
        let icc = sections
            .image_resources
            .icc_profile()
            .unwrap_or_else(|| panic!("{name}: no ICC profile block"));
        let expected = std::fs::read(fixture(&format!("ICCProfiles/{name}.icc"))).unwrap();
        assert_eq!(icc.data(), expected, "{name}");
    }
}

#[test]
fn indexed_palette_reads_and_round_trips() {
    let sections = read_sections("Indexed/Indexed_8bit.psd");
    let palette = sections.color_mode_data.data();
    assert_eq!(palette.len(), 768); // 256-entry RGB palette
    assert_eq!(&palette[..4], &[0, 1, 2, 3]);
    assert_eq!(&palette[764..], &[0xFC, 0xFD, 0xFE, 0xFF]);

    // Re-serialize and compare against the exact on-disk section (4 + 768).
    let mut writer = BeWriter::new();
    sections
        .color_mode_data
        .write(&mut writer, &sections.header)
        .unwrap();
    let on_disk = &sections.bytes[26..26 + 4 + 768];
    assert_eq!(writer.as_slice(), on_disk);
}

#[test]
fn thirty_two_bit_blob_matches_photoshop_default() {
    for name in [
        "Compression/Compression_ZipPrediction_32bit.psd",
        "Compression/Compression_ZipPrediction_32bit.psb",
    ] {
        let sections = read_sections(name);
        assert_eq!(
            sections.color_mode_data.data(),
            ColorModeData::DEFAULT_32BIT,
            "{name}"
        );
    }
}

#[test]
fn unknown_resource_blocks_are_preserved() {
    // The XMP metadata block (id 1060, odd size) has no parser upstream and
    // would be dropped by it; the port keeps it and every other block.
    let sections = read_sections("DPI/300dpi.psd");
    let xmp = sections
        .image_resources
        .blocks()
        .iter()
        .find_map(|block| match block {
            ResourceBlock::Raw(raw) if raw.id == 1060 => Some(raw),
            _ => None,
        })
        .expect("XMP block missing");
    assert_eq!(xmp.data.len(), 14837);
    assert_eq!(sections.image_resources.blocks().len(), 26);
    assert_eq!(
        sections
            .image_resources
            .blocks()
            .iter()
            .filter(|b| matches!(b, ResourceBlock::IccProfile(_)))
            .count(),
        1
    );
}

#[test]
fn header_colormode_and_resources_round_trip_byte_exact() {
    for name in [
        "DPI/300dpi.psd",
        "DPI/300_point_5_dpi.psd",
        "ICCProfiles/AdobeRGB1998.psb",
        "Indexed/Indexed_8bit.psd",
        "Compression/Compression_ZipPrediction_32bit.psd",
    ] {
        let sections = read_sections(name);
        let mut writer = BeWriter::new();
        sections.header.write(&mut writer);
        sections
            .color_mode_data
            .write(&mut writer, &sections.header)
            .unwrap();
        sections.image_resources.write(&mut writer).unwrap();

        assert_eq!(
            writer.as_slice(),
            &sections.bytes[..sections.prefix_len],
            "{name} did not round-trip byte-exact"
        );
    }
}
