//! Placed-layer (`PlLd` / `SoLd`) parsing tests against the smart-object
//! fixtures. The parsed structures are views over the preserved block data;
//! re-serializing them must reproduce the block bytes (minus alignment
//! padding).

use std::path::{Path, PathBuf};

use psd_core::{
    BeReader, BeWriter, PhotoshopFile, PlacedLayer, PlacedLayerData, PlacedLayerType,
    TaggedBlockKey,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/documents")
        .join(name)
}

#[test]
fn smart_object_fixtures_expose_placed_layer_blocks() {
    let mut placed_layers = 0;
    let mut placed_layer_data = 0;

    for name in [
        "SmartObjects/smart_objects_transformed.psd",
        "SmartObjects/smart_object_file_no_warp.psd",
    ] {
        let bytes = std::fs::read(fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let file = PhotoshopFile::read(&mut BeReader::new(&bytes))
            .unwrap_or_else(|e| panic!("{name}: {e}"));

        for record in &file.layer_and_mask_info.layer_info.layer_records {
            let Some(ali) = &record.additional_layer_info else {
                continue;
            };

            if let Some(block) = ali.get(TaggedBlockKey::new(*b"PlLd")) {
                let mut reader = BeReader::new(&block.data);
                let placed =
                    PlacedLayer::read(&mut reader).unwrap_or_else(|e| panic!("{name}: PlLd: {e}"));
                assert_eq!(placed.version, 3, "{name}");
                assert_eq!(placed.layer_type, PlacedLayerType::Raster, "{name}");
                // The warp class id depends on the warp type (`warp`,
                // `quiltWarp`, …) and always carries items.
                assert!(!placed.warp.class_id.as_str().is_empty(), "{name}");
                assert!(!placed.warp.items.is_empty(), "{name}");
                assert!(!placed.unique_id.value().is_empty(), "{name}");
                for point in [
                    placed.transform.top_left,
                    placed.transform.top_right,
                    placed.transform.bottom_right,
                    placed.transform.bottom_left,
                ] {
                    assert!(point.x.is_finite() && point.y.is_finite(), "{name}");
                }

                let mut writer = BeWriter::new();
                placed.write(&mut writer).unwrap();
                assert_eq!(
                    writer.as_slice(),
                    &block.data[..writer.position()],
                    "{name}: PlLd did not re-serialize byte-exact"
                );
                placed_layers += 1;
            }

            if let Some(block) = ali.get(TaggedBlockKey::new(*b"SoLd")) {
                let mut reader = BeReader::new(&block.data);
                let data = PlacedLayerData::read(&mut reader)
                    .unwrap_or_else(|e| panic!("{name}: SoLd: {e}"));
                assert_eq!(data.version, 4, "{name}");
                assert_eq!(data.descriptor_version, 16, "{name}");

                let mut writer = BeWriter::new();
                data.write(&mut writer).unwrap();
                assert_eq!(
                    writer.as_slice(),
                    &block.data[..writer.position()],
                    "{name}: SoLd did not re-serialize byte-exact"
                );
                placed_layer_data += 1;
            }
        }
    }

    assert!(placed_layers > 0, "no PlLd blocks found");
    assert!(placed_layer_data > 0, "no SoLd blocks found");
}

#[test]
fn placed_layer_edit_views_return_opaque_payload_suffixes() {
    let bytes = std::fs::read(fixture("SmartObjects/smart_objects_transformed.psd")).unwrap();
    let file = PhotoshopFile::read(&mut BeReader::new(&bytes)).unwrap();
    let record = file
        .layer_and_mask_info
        .layer_info
        .layer_records
        .iter()
        .find(|record| {
            record.additional_layer_info.as_ref().is_some_and(|ali| {
                ali.get(TaggedBlockKey::new(*b"PlLd")).is_some()
                    && ali.get(TaggedBlockKey::new(*b"SoLd")).is_some()
            })
        })
        .expect("smart-object record");
    let ali = record.additional_layer_info.as_ref().unwrap();
    let sentinel = [0xfa, 0xce, 0x11];

    let mut placed_bytes = ali.get(TaggedBlockKey::new(*b"PlLd")).unwrap().data.clone();
    placed_bytes.extend_from_slice(&sentinel);
    let (placed, placed_tail) =
        PlacedLayer::read_with_trailing(&mut BeReader::new(&placed_bytes)).unwrap();
    assert!(placed_tail.ends_with(&sentinel));
    let mut writer = BeWriter::new();
    placed.write(&mut writer).unwrap();
    writer.bytes(&placed_tail);
    assert_eq!(writer.as_slice(), placed_bytes);

    let mut data_bytes = ali.get(TaggedBlockKey::new(*b"SoLd")).unwrap().data.clone();
    data_bytes.extend_from_slice(&sentinel);
    let (data, data_tail) =
        PlacedLayerData::read_with_trailing(&mut BeReader::new(&data_bytes)).unwrap();
    assert!(data_tail.ends_with(&sentinel));
    let mut writer = BeWriter::new();
    data.write(&mut writer).unwrap();
    writer.bytes(&data_tail);
    assert_eq!(writer.as_slice(), data_bytes);
}

#[test]
fn placed_layer_transforms_are_plausible() {
    let bytes = std::fs::read(fixture("SmartObjects/smart_objects_transformed.psd")).unwrap();
    let file = PhotoshopFile::read(&mut BeReader::new(&bytes)).unwrap();

    for record in &file.layer_and_mask_info.layer_info.layer_records {
        let Some(block) = record
            .additional_layer_info
            .as_ref()
            .and_then(|ali| ali.get(TaggedBlockKey::new(*b"PlLd")))
        else {
            continue;
        };
        let placed = PlacedLayer::read(&mut BeReader::new(&block.data)).unwrap();
        let transform = placed.transform;
        // The corners must describe a non-degenerate quadrilateral.
        let width = (transform.top_right.x - transform.top_left.x).abs();
        let height = (transform.bottom_left.y - transform.top_left.y).abs();
        assert!(
            width > 0.0 && height > 0.0,
            "degenerate transform for layer {:?}",
            record.name.value()
        );
    }
}
