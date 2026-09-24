//! Smart-object views on the document API: `SoLd`/`PlLd` parsing on demand
//! and the document's linked-file records, cross-referenced by uuid.

use std::path::{Path, PathBuf};

use psd::core::{LinkedDataKind, PlacedLayerType};
use psd::LayeredFile;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/documents")
        .join(name)
}

#[test]
fn smart_object_layers_expose_placed_data() {
    for name in [
        "SmartObjects/smart_objects_transformed.psd",
        "SmartObjects/smart_object_file_no_warp.psd",
    ] {
        let document =
            LayeredFile::<u8>::read(fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let smart_objects: Vec<_> = document
            .layers()
            .filter(|layer| layer.is_smart_object())
            .collect();
        assert!(!smart_objects.is_empty(), "{name}: no smart-object layers");

        for layer in smart_objects {
            let data = layer
                .smart_object_data()
                .unwrap_or_else(|e| panic!("{name}: SoLd: {e}"))
                .unwrap_or_else(|| panic!("{name}: SoLd block missing"));
            assert_eq!(data.version, 4, "{name}");
            assert!(!data.descriptor.items.is_empty(), "{name}");

            let placed = layer
                .placed_layer()
                .unwrap_or_else(|e| panic!("{name}: PlLd: {e}"))
                .unwrap_or_else(|| panic!("{name}: PlLd block missing"));
            assert_eq!(placed.version, 3, "{name}");
            assert_eq!(placed.layer_type, PlacedLayerType::Raster, "{name}");
            assert!(!placed.warp.items.is_empty(), "{name}");
        }
    }
}

#[test]
fn placed_layer_uuids_reference_linked_records() {
    // Every smart-object layer's PlLd uuid must match a linked-layer record
    // in the document's lnk* blocks (the Photoshop cross-reference).
    let document =
        LayeredFile::<u8>::read(fixture("SmartObjects/smart_objects_transformed.psd")).unwrap();

    let linked = document.linked_layers().unwrap();
    assert!(!linked.is_empty());
    assert!(
        linked
            .iter()
            .any(|layer| layer.kind == LinkedDataKind::Data),
        "expected an embedded smart object"
    );

    for layer in document.layers().filter(|layer| layer.is_smart_object()) {
        let placed = layer.placed_layer().unwrap().expect("PlLd missing");
        let uuid = placed.unique_id.value();
        assert!(
            linked.iter().any(|record| record.unique_id.value() == uuid),
            "no linked-layer record for uuid {uuid:?} (layer {:?})",
            layer.name
        );
    }
}

#[test]
fn non_smart_object_layers_report_none() {
    let document = LayeredFile::<u8>::read(fixture("SingleLayer/SingleLayer_8bit.psd")).unwrap();
    let layer = &document.layer(0).unwrap();
    assert!(!layer.is_smart_object());
    assert!(layer.smart_object_data().unwrap().is_none());
    assert!(layer.placed_layer().unwrap().is_none());
    assert!(document.linked_layers().unwrap().is_empty());
}
