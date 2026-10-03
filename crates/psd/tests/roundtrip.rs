//! Read → write → read round-trips through the document API, mirroring
//! upstream's `TestRoundtripping` cases, plus a corpus-wide parse gate.

use std::path::{Path, PathBuf};

use psd::{ChannelKey, LayerKind, LayeredFile, ReadOptions};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/documents")
        .join(name)
}

fn assert_documents_match<T: psd::BitDepth + std::fmt::Debug>(
    name: &str,
    original: &LayeredFile<T>,
    back: &LayeredFile<T>,
) {
    assert_eq!(back.width, original.width, "{name}: width");
    assert_eq!(back.height, original.height, "{name}: height");
    assert_eq!(back.color_mode, original.color_mode, "{name}: color mode");
    // num_channels is recomputed on save (as Photoshop and upstream do): a
    // file whose merged composite omits the alpha its layers carry gains one
    // on write — this pins that widening rather than a byte-equal header.
    // The port fixes upstream's `hasAlpha &=` bug.
    let expected_channels = original.num_channels.max(
        psd::color_channel_count(original.color_mode)
            + u16::from(original.has_alpha() || original.has_merged_alpha),
    );
    assert_eq!(back.num_channels, expected_channels, "{name}: channels");
    assert_eq!(back.layer_count(), original.layer_count(), "{name}: layers");
    assert_eq!(back.dpi, original.dpi, "{name}: dpi");
    assert_eq!(back.icc_profile, original.icc_profile, "{name}: icc");

    for (a, b) in original.layers().zip(back.layers()) {
        assert_eq!(a.name, b.name, "{name}: layer name");
        assert_eq!(a.bounds, b.bounds, "{name}: bounds of {}", a.name);
        assert_eq!(a.opacity, b.opacity, "{name}: opacity of {}", a.name);
        assert_eq!(a.blend_mode, b.blend_mode, "{name}: blend of {}", a.name);
        assert_eq!(a.flags, b.flags, "{name}: flags of {}", a.name);
        match (&a.kind, &b.kind) {
            (LayerKind::Image(ia), LayerKind::Image(ib)) => {
                assert_channel_stores_match(name, &a.name, &ia.channels, &ib.channels);
            }
            (LayerKind::Text(ta), LayerKind::Text(tb)) => {
                assert_channel_stores_match(name, &a.name, &ta.channels, &tb.channels);
            }
            (LayerKind::Group(ga), LayerKind::Group(gb)) => {
                assert_eq!(ga.open, gb.open, "{name}: group state of {}", a.name);
                assert_eq!(ga.children, gb.children, "{name}: children of {}", a.name);
            }
            (LayerKind::SectionDivider(da), LayerKind::SectionDivider(db)) => {
                assert_eq!(da, db, "{name}: divider kind");
            }
            _ => panic!("{name}: kind mismatch for layer {}", a.name),
        }
    }
}

fn assert_channel_stores_match<T: PartialEq + std::fmt::Debug>(
    document_name: &str,
    layer_name: &str,
    original: &psd::ChannelStore<T>,
    reread: &psd::ChannelStore<T>,
) {
    let original_keys: Vec<ChannelKey> = original.keys().collect();
    let reread_keys: Vec<ChannelKey> = reread.keys().collect();
    assert_eq!(
        original_keys, reread_keys,
        "{document_name}: channels of {layer_name}"
    );
    for (key, samples) in original.iter() {
        assert_eq!(
            reread.get(key),
            Some(samples),
            "{document_name}: pixels of {layer_name} channel {key:?}"
        );
    }
}

fn roundtrip<T: psd::BitDepth + std::fmt::Debug>(name: &str) {
    let document = LayeredFile::<T>::read(fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    let bytes = document
        .to_bytes()
        .unwrap_or_else(|e| panic!("{name}: write: {e}"));
    let back =
        LayeredFile::<T>::from_bytes(&bytes).unwrap_or_else(|e| panic!("{name}: re-read: {e}"));
    assert_documents_match(name, &document, &back);
}

#[test]
fn eight_bit_round_trips() {
    for name in [
        "SingleLayer/SingleLayer_8bit.psd",
        "SingleLayer/SingleLayer_8bit.psb",
        "Groups/Groups_8bit.psd",
        "Groups/Groups_8bit.psb",
        "Masks/Masks_8bit.psd",
        "Masks/Masks_8bit.psb",
        "Masks/SingleMask_White.psb",
        "CMYK/CMYK_8.psd",
        "Grayscale/Grayscale_8.psd",
        "Compression/Compression_RAW_8bit.psb",
        "Compression/Compression_RLE_8bit.psd",
        "Compression/Compression_Mixed_8bit.psd",
        "Compression/Compression_Zip_8bit.psd",
        // Photoshop 2026-authored: every BlnM enum in the file is long-form.
        "BlendModes/ps2026-blend-modes.psd",
        // Upstream "Roundtrip layer read-write multiple smart objects, no
        // warp information" plus the transformed-warp corpus.
        "SmartObjects/smart_object_file_no_warp.psd",
        "SmartObjects/smart_objects_transformed.psd",
    ] {
        roundtrip::<u8>(name);
    }
}

#[test]
fn sixteen_bit_round_trips() {
    for name in [
        "SingleLayer/SingleLayer_16bit.psd",
        "SingleLayer/SingleLayer_16bit.psb",
        "Groups/Groups_16bit.psd",
        "Grayscale/Grayscale_16.psb",
        "CMYK/CMYK_16.psd",
        "Compression/Compression_ZipPrediction_16bit.psd",
        "Compression/Compression_ZipPrediction_16bit.psb",
    ] {
        roundtrip::<u16>(name);
    }
}

#[test]
fn thirty_two_bit_round_trips() {
    for name in [
        "SingleLayer/SingleLayer_32bit.psd",
        "SingleLayer/SingleLayer_32bit.psb",
        "Grayscale/Grayscale_32.psd",
        "Compression/Compression_ZipPrediction_32bit.psd",
        "Compression/Compression_ZipPrediction_32bit.psb",
    ] {
        roundtrip::<f32>(name);
    }
}

#[test]
fn dpi_and_icc_survive_a_round_trip() {
    let document = LayeredFile::<u8>::read(fixture("DPI/300_point_5_dpi.psd")).unwrap();
    assert_eq!(document.dpi, 300.5);
    let back = LayeredFile::<u8>::from_bytes(&document.to_bytes().unwrap()).unwrap();
    assert_eq!(back.dpi, 300.5);

    let document = LayeredFile::<u8>::read(fixture("ICCProfiles/AdobeRGB1998.psb")).unwrap();
    let expected = std::fs::read(fixture("ICCProfiles/AdobeRGB1998.icc")).unwrap();
    assert_eq!(document.icc_profile, expected);
    let back = LayeredFile::<u8>::from_bytes(&document.to_bytes().unwrap()).unwrap();
    assert_eq!(back.icc_profile, expected);
}

#[test]
fn groups_expose_paths_and_open_state() {
    let document = LayeredFile::<u8>::read(fixture("Groups/Groups_8bit.psd")).unwrap();
    let grouped = document
        .layer_by_path("Group/GroupedLayer")
        .expect("Group/GroupedLayer missing");
    assert!(grouped.image().is_some());

    let group = document.layer_by_path("Group").expect("Group missing");
    assert!(group.group().is_some());

    // The group-end markers surface as section dividers.
    assert!(document
        .layers()
        .any(|layer| matches!(layer.kind, LayerKind::SectionDivider(_))));
}

#[test]
fn mask_geometry_and_pixels_survive() {
    let document = LayeredFile::<u8>::read(fixture("Masks/SingleMask_White.psb")).unwrap();
    let layer = document
        .layer_by_path("MaskGroup/MaskLayer")
        .or_else(|| document.layer_by_path("MaskLayer"))
        .expect("MaskLayer missing");
    let mask = layer
        .mask
        .as_ref()
        .and_then(|data| data.pixel_mask)
        .expect("pixel mask missing");
    assert_eq!(
        (mask.top, mask.left, mask.bottom, mask.right),
        (16, 0, 32, 32)
    );

    let image = layer.image().unwrap();
    let mask_pixels = image
        .channels
        .get(ChannelKey::USER_MASK)
        .expect("mask channel");
    assert_eq!(mask_pixels.len(), 16 * 32);

    let back = LayeredFile::<u8>::from_bytes(&document.to_bytes().unwrap()).unwrap();
    let layer = back
        .layer_by_path("MaskGroup/MaskLayer")
        .or_else(|| back.layer_by_path("MaskLayer"))
        .expect("MaskLayer missing after round trip");
    assert_eq!(
        layer.image().unwrap().channels.get(ChannelKey::USER_MASK),
        Some(mask_pixels)
    );
}

/// Read the bit depth straight from the header so every fixture can be opened
/// with the matching document type.
fn header_depth(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[22], bytes[23]])
}

#[test]
fn every_fixture_parses_as_a_layered_file() {
    fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect(&path, out);
            } else if matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("psd" | "psb")
            ) {
                out.push(path);
            }
        }
    }

    let mut files = Vec::new();
    collect(&fixture(""), &mut files);
    assert!(files.len() >= 70, "found {} fixtures", files.len());

    for path in &files {
        let bytes = std::fs::read(path).unwrap();
        let name = path.display();
        match header_depth(&bytes) {
            8 => {
                LayeredFile::<u8>::from_bytes(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            }
            16 => {
                LayeredFile::<u16>::from_bytes(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            }
            32 => {
                LayeredFile::<f32>::from_bytes(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            }
            depth => panic!("{name}: unexpected depth {depth}"),
        }
    }
}

#[test]
fn every_fixture_can_round_trip_with_lazy_channel_payloads() {
    fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect(&path, out);
            } else if matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("psd" | "psb")
            ) {
                out.push(path);
            }
        }
    }

    let mut files = Vec::new();
    collect(&fixture(""), &mut files);
    for path in &files {
        let bytes = std::fs::read(path).unwrap();
        let name = path.display().to_string();
        let options = ReadOptions::default().with_raw_data(true);
        match header_depth(&bytes) {
            8 => lazy_roundtrip::<u8>(&name, &bytes, options),
            16 => lazy_roundtrip::<u16>(&name, &bytes, options),
            32 => lazy_roundtrip::<f32>(&name, &bytes, options),
            depth => panic!("{name}: unexpected depth {depth}"),
        }
    }
}

fn lazy_roundtrip<T: psd::BitDepth + std::fmt::Debug>(
    name: &str,
    bytes: &[u8],
    options: ReadOptions,
) {
    let original = LayeredFile::<T>::from_bytes(bytes)
        .unwrap_or_else(|error| panic!("{name}: eager read: {error}"));
    let lazy = LayeredFile::<T>::from_bytes_with_options(bytes, options)
        .unwrap_or_else(|error| panic!("{name}: lazy read: {error}"));
    for layer in lazy.layers() {
        if let Some(channels) = layer.channels() {
            for key in channels.keys() {
                assert!(
                    channels.is_raw(key),
                    "{name}: channel {key:?} was decoded eagerly"
                );
            }
        }
    }

    let written = lazy
        .to_bytes()
        .unwrap_or_else(|error| panic!("{name}: lazy write: {error}"));
    let reread = LayeredFile::<T>::from_bytes(&written)
        .unwrap_or_else(|error| panic!("{name}: eager reread: {error}"));
    assert_documents_match(name, &original, &reread);
}
