//! `TySh`/`Txt2` typed-block tests against the TextLayers corpus.

use std::path::PathBuf;

use psd_core::{BeReader, BeWriter, PhotoshopFile, TaggedBlockKey, TypeToolTaggedBlock};

fn fixture(name: &str) -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/documents")
        .join(name)
}

#[test]
fn tysh_blocks_round_trip_byte_exact() {
    let mut found = 0;

    for name in [
        "TextLayers/TextLayers_Basic.psd",
        "TextLayers/TextLayers_CharacterStyles.psd",
        "TextLayers/TextLayers_FontFallback.psd",
        "TextLayers/TextLayers_Paragraph.psd",
        "TextLayers/TextLayers_StyleRuns.psd",
        "TextLayers/TextLayers_TextOnPath.psd",
        "TextLayers/TextLayers_Transform.psd",
        "TextLayers/TextLayers_Vertical.psd",
        "TextLayers/TextLayers_VerticalBox.psd",
        "TextLayers/TextLayers_Warp.psd",
    ] {
        let bytes = std::fs::read(fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let file = PhotoshopFile::read(&mut BeReader::new(&bytes))
            .unwrap_or_else(|e| panic!("{name}: {e}"));

        for record in &file.layer_and_mask_info.layer_info.layer_records {
            let Some(block) = record
                .additional_layer_info
                .as_ref()
                .and_then(|ali| ali.get(TaggedBlockKey::new(*b"TySh")))
            else {
                continue;
            };

            let parsed = TypeToolTaggedBlock::read(&mut BeReader::new(&block.data))
                .unwrap_or_else(|e| panic!("{name}: TySh: {e}"));
            assert_eq!(parsed.tysh_version, 1, "{name}");
            assert_eq!(parsed.text_version, 50, "{name}");
            assert_eq!(parsed.text_descriptor_version, 16, "{name}");
            assert_eq!(parsed.warp_descriptor_version, 16, "{name}");
            assert_eq!(parsed.text.class_id.as_str(), "TxLr", "{name}");
            assert_eq!(parsed.warp.class_id.as_str(), "warp", "{name}");
            // Descriptor spans slice back to exactly the parsed descriptors
            // and re-serialize byte-exact in place.
            let descriptor_spans = TypeToolTaggedBlock::descriptor_spans(&block.data)
                .unwrap_or_else(|e| panic!("{name}: descriptor spans: {e}"));
            assert_eq!(descriptor_spans.text.start, 56, "{name}");
            assert_eq!(
                descriptor_spans.warp.start,
                descriptor_spans.text.end + 6,
                "{name}"
            );
            for (range, descriptor) in [
                (descriptor_spans.text.clone(), &parsed.text),
                (descriptor_spans.warp.clone(), &parsed.warp),
            ] {
                let mut writer = BeWriter::new();
                descriptor.write(&mut writer).unwrap();
                assert_eq!(writer.as_slice(), &block.data[range], "{name}");
            }
            let raw_engine_data = parsed.engine_data_bytes().expect("EngineData");
            let engine_span = TypeToolTaggedBlock::engine_data_payload_range(&block.data)
                .unwrap_or_else(|e| panic!("{name}: EngineData span: {e}"))
                .expect("EngineData raw-data span");
            assert_eq!(&block.data[engine_span], raw_engine_data, "{name}");
            let spans = TypeToolTaggedBlock::text_payload_spans(&block.data)
                .unwrap_or_else(|e| panic!("{name}: text payload spans: {e}"));
            let text_span = spans.text.expect("Txt UnicodeString span");
            let Some(psd_core::DescriptorValue::String(visible_text)) = parsed.text.get("Txt ")
            else {
                panic!("{name}: Txt string");
            };
            assert_eq!(
                u32::from_be_bytes(
                    block.data[text_span.start..text_span.start + 4]
                        .try_into()
                        .unwrap()
                ) as usize,
                visible_text.utf16().len(),
                "{name}"
            );
            let engine = parsed
                .engine_data()
                .unwrap_or_else(|e| panic!("{name}: EngineData: {e}"))
                .expect("{name}: EngineData missing");
            assert!(engine.get("EngineDict").is_some(), "{name}");

            let mut writer = BeWriter::new();
            parsed.write(&mut writer).unwrap();
            if writer.as_slice() != block.data {
                let mismatch = writer
                    .as_slice()
                    .iter()
                    .zip(block.data.iter())
                    .position(|(a, b)| a != b);
                panic!(
                    "{name}: TySh mismatch at {mismatch:?} (wrote {}, original {})",
                    writer.position(),
                    block.data.len()
                );
            }
            found += 1;
        }
    }

    assert!(found > 0, "no TySh blocks found in the TextLayers fixtures");
}

#[test]
fn document_level_txt2_is_a_parseable_engine_data_body() {
    for name in [
        "TextLayers/TextLayers_Basic.psd",
        "TextLayers/TextLayers_CharacterStyles.psd",
        "TextLayers/TextLayers_FontFallback.psd",
        "TextLayers/TextLayers_Paragraph.psd",
        "TextLayers/TextLayers_StyleRuns.psd",
        "TextLayers/TextLayers_TextOnPath.psd",
        "TextLayers/TextLayers_Transform.psd",
        "TextLayers/TextLayers_Vertical.psd",
        "TextLayers/TextLayers_VerticalBox.psd",
        "TextLayers/TextLayers_Warp.psd",
    ] {
        let bytes = std::fs::read(fixture(name)).unwrap();
        let file = PhotoshopFile::read(&mut BeReader::new(&bytes)).unwrap();

        // Photoshop CC stores Txt2 once, at document level, never per layer.
        let layer_level = file
            .layer_and_mask_info
            .layer_info
            .layer_records
            .iter()
            .filter_map(|record| record.additional_layer_info.as_ref())
            .filter(|ali| ali.get(TaggedBlockKey::new(*b"Txt2")).is_some())
            .count();
        assert_eq!(layer_level, 0, "{name}");
        let document = file
            .layer_and_mask_info
            .additional_layer_info
            .as_ref()
            .and_then(|ali| ali.get(TaggedBlockKey::new(*b"Txt2")))
            .unwrap_or_else(|| panic!("{name}: no document-level Txt2"));

        // A bare dictionary body with numeric keys, not `<< … >>`.
        assert!(
            psd_core::engine_data::parse(&document.data).is_err(),
            "{name}"
        );
        let root = psd_core::text_tool::parse_text_engine_data(&mut BeReader::new(&document.data))
            .unwrap_or_else(|e| panic!("{name}: Txt2: {e}"));
        let keys: Vec<_> = root
            .as_dictionary()
            .unwrap()
            .iter()
            .map(|(key, _)| key.as_str())
            .collect();
        assert_eq!(keys.first(), Some(&"98"), "{name}");
        assert!(
            keys.contains(&"0") && keys.contains(&"1"),
            "{name}: {keys:?}"
        );
        assert_eq!(root.span, 0..document.data.len(), "{name}");
        // Spans index the payload, so byte-level edits can target Txt2 too.
        let version = root.get_path(["98", "0"]).unwrap();
        assert_eq!(&document.data[version.span.clone()], b"13", "{name}");
    }
}
