//! Linked-layer (`lnk2` / `lnkD` / `lnkE` / `lnk3`) tests against real files.
//!
//! The blocks are views over preserved tagged-block data; re-serializing must
//! reproduce the block bytes (records are 4-aligned, so only sub-record
//! padding may remain).

use std::path::{Path, PathBuf};

use psd_core::{
    BeReader, BeWriter, LinkedDataKind, LinkedLayerTaggedBlock, PhotoshopFile, TaggedBlockKey,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/documents")
        .join(name)
}

const LINK_KEYS: &[[u8; 4]] = &[*b"lnk2", *b"lnkD", *b"lnkE", *b"lnk3"];

#[test]
fn linked_layer_blocks_round_trip_byte_exact() {
    let mut found = 0;

    for name in [
        "LayerColor/layers_with_display_color.psb",
        "SmartObjects/smart_objects_transformed.psd",
        "SmartObjects/smart_object_file_no_warp.psd",
    ] {
        let bytes = std::fs::read(fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let file = PhotoshopFile::read(&mut BeReader::new(&bytes))
            .unwrap_or_else(|e| panic!("{name}: {e}"));

        let mut blocks = Vec::new();
        if let Some(ali) = &file.layer_and_mask_info.additional_layer_info {
            blocks.extend(ali.blocks.iter());
        }
        for record in &file.layer_and_mask_info.layer_info.layer_records {
            if let Some(ali) = &record.additional_layer_info {
                blocks.extend(ali.blocks.iter());
            }
        }

        for block in blocks {
            if !LINK_KEYS.contains(&block.key.as_bytes()) {
                continue;
            }
            let mut reader = BeReader::new(&block.data);
            let parsed = LinkedLayerTaggedBlock::read(&mut reader)
                .unwrap_or_else(|e| panic!("{name}: {}: {e}", block.key));
            // Photoshop writes placeholder blocks with no records; only
            // non-empty blocks carry data to check.
            if parsed.layers.is_empty() {
                continue;
            }
            for layer in &parsed.layers {
                assert!(
                    matches!(
                        layer.kind,
                        LinkedDataKind::Data | LinkedDataKind::External | LinkedDataKind::Alias
                    ),
                    "{name}"
                );
                assert!(!layer.file_name.value().is_empty(), "{name}");
                assert!((1..=8).contains(&layer.version), "{name}: version");
            }

            let mut writer = BeWriter::new();
            parsed.write(&mut writer).unwrap();
            let consumed = writer.position();
            let trailing = &block.data[consumed..];
            assert!(
                trailing.len() < 8 && trailing.iter().all(|&b| b == 0),
                "{name}: {} unexpected trailing bytes {trailing:?}",
                block.key
            );
            if writer.as_slice() != &block.data[..consumed] {
                let mismatch = writer
                    .as_slice()
                    .iter()
                    .zip(block.data.iter())
                    .position(|(a, b)| a != b);
                panic!(
                    "{name}: {} mismatch at {mismatch:?} (wrote {consumed} bytes, original {} bytes)",
                    block.key,
                    block.data.len()
                );
            }
            found += 1;
        }
    }

    assert!(
        found > 0,
        "no non-empty linked-layer blocks found in the corpus"
    );
}

#[test]
fn embedded_linked_records_preserve_all_fields() {
    // layers_with_display_color.psb stores its smart-object links in `lnk2`
    // (the `lnkE` block next to it is an empty placeholder).
    let bytes = std::fs::read(fixture("LayerColor/layers_with_display_color.psb")).unwrap();
    let file = PhotoshopFile::read(&mut BeReader::new(&bytes)).unwrap();
    let ali = file
        .layer_and_mask_info
        .additional_layer_info
        .as_ref()
        .expect("document tagged blocks missing");

    let mut records = 0;
    for key in LINK_KEYS {
        if let Some(block) = ali.get(TaggedBlockKey::new(*key)) {
            let parsed = LinkedLayerTaggedBlock::read(&mut BeReader::new(&block.data)).unwrap();
            for layer in &parsed.layers {
                assert!(!layer.unique_id.value().is_empty());
                assert!(layer.file_open_descriptor.is_some());
                if layer.kind == LinkedDataKind::External {
                    assert!(layer.linked_file_descriptor.is_some());
                }
                records += 1;
            }
        }
    }
    assert!(records > 0, "no linked-layer records found");
}
