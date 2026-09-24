//! Descriptor round-trip tests against the binary fixtures.
//!
//! Mirrors upstream `TestPlacedLayerDescriptor` / `TestDescriptorStructure`:
//! every `binary_data/Descriptor/*.bin` is a raw descriptor blob that must
//! re-serialize byte-exactly.

use std::path::{Path, PathBuf};

use psd_core::{BeReader, BeWriter, Descriptor, DescriptorValue};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/documents/binary_data/Descriptor")
        .join(name)
}

const FIXTURES: &[&str] = &[
    "DistortWarp_PlacedLayerBlock.bin",
    "FXPerspectiveWarp_PlacedLayerBlock.bin",
    "FXPuppetWarp_PlacedLayerBlock.bin",
    "PerspectiveWarp_PlacedLayerBlock.bin",
    "placed_layer_taggedblock.bin",
    "QuiltWarp_PlacedLayerBlock.bin",
    "SkewWarp_PlacedLayerBlock.bin",
    "Warp_PlacedLayerBlock.bin",
];

#[test]
fn every_descriptor_fixture_round_trips_byte_exact() {
    for name in FIXTURES {
        let bytes = std::fs::read(fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let mut reader = BeReader::new(&bytes);
        let descriptor =
            Descriptor::read(&mut reader).unwrap_or_else(|e| panic!("{name}: parse: {e}"));

        // A fixture extracted from a tagged block may carry the block's
        // alignment padding; it must be zero and at most 4 bytes.
        let consumed = reader.position();
        let trailing = &bytes[consumed..];
        assert!(
            trailing.len() <= 4 && trailing.iter().all(|&b| b == 0),
            "{name}: unexpected trailing bytes {trailing:?}"
        );

        let mut writer = BeWriter::new();
        descriptor
            .write(&mut writer)
            .unwrap_or_else(|e| panic!("{name}: write: {e}"));
        assert_eq!(
            writer.as_slice(),
            &bytes[..consumed],
            "{name}: descriptor did not round-trip byte-exact"
        );
    }
}

#[test]
fn placed_layer_descriptor_has_expected_shape() {
    let bytes = std::fs::read(fixture("placed_layer_taggedblock.bin")).unwrap();
    let mut reader = BeReader::new(&bytes);
    let descriptor = Descriptor::read(&mut reader).unwrap();

    assert_eq!(descriptor.class_id.as_str(), "null");
    assert_eq!(descriptor.items.len(), 17);

    // The identifier item is a unicode string (a UUID-ish document id).
    let id = descriptor.get("Idnt").expect("Idnt missing");
    assert!(matches!(id, DescriptorValue::String(_)));
    assert!(id.as_str().unwrap().len() > 10);
    assert!(descriptor
        .get("placed")
        .expect("placed missing")
        .as_str()
        .is_some());
    assert!(descriptor
        .get("PgNm")
        .expect("PgNm missing")
        .as_integer()
        .is_some());

    // The transform is a list of eight doubles.
    let transform = descriptor
        .get("Trnf")
        .expect("Trnf missing")
        .as_list()
        .unwrap();
    assert_eq!(transform.len(), 8);
    assert!(transform.iter().all(|value| value.as_double().is_some()));
}

#[test]
fn warp_descriptor_contains_a_warp_subtree() {
    let bytes = std::fs::read(fixture("Warp_PlacedLayerBlock.bin")).unwrap();
    let mut reader = BeReader::new(&bytes);
    let descriptor = Descriptor::read(&mut reader).unwrap();

    let warp = descriptor
        .get("warp")
        .and_then(DescriptorValue::as_descriptor)
        .expect("warp subtree missing");
    assert_eq!(warp.class_id.as_str(), "warp");
    assert!(warp.items.len() >= 5);
    let (type_id, style) = warp
        .get("warpStyle")
        .expect("warpStyle missing")
        .as_enum()
        .unwrap();
    assert_eq!(type_id.as_str(), "warpStyle");
    assert!(!style.as_str().is_empty());
    assert!(warp.get("warpValue").unwrap().as_double().is_some());
    assert!(warp.get("bounds").unwrap().as_descriptor().is_some());
}
