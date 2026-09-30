//! EngineData corpus test: the text-layer (`TySh`) EngineData payloads of the
//! TextLayers fixtures parse and re-serialize losslessly.

use std::path::{Path, PathBuf};

use psd_core::engine_data;
use psd_core::{BeReader, Descriptor, DescriptorValue, PhotoshopFile, TaggedBlockKey};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/documents")
        .join(name)
}

/// Parse a `TySh` block payload far enough to reach the text descriptor,
/// mirroring upstream `TypeToolTaggedBlock`: version, 6×f64 transform matrix,
/// text version + descriptor version, then the `TxLr` descriptor.
fn text_descriptor(block_data: &[u8]) -> Option<(Descriptor, usize)> {
    let mut reader = BeReader::new(block_data);
    reader.u16().ok()?;
    for _ in 0..6 {
        reader.f64().ok()?;
    }
    reader.u16().ok()?;
    reader.u32().ok()?;
    let start = reader.position();
    let descriptor = Descriptor::read(&mut reader).ok()?;
    Some((descriptor, start))
}

#[test]
fn text_layer_engine_data_parses_from_corpus() {
    let mut parsed = 0;

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

            let (descriptor, _) = text_descriptor(&block.data)
                .unwrap_or_else(|| panic!("{name}: TySh header for {:?}", record.name.value()));
            let engine_data_bytes = match descriptor.get("EngineData") {
                Some(DescriptorValue::RawData { data, .. }) => data,
                other => panic!("{name}: EngineData missing or not raw data: {other:?}",),
            };

            let root = engine_data::parse(engine_data_bytes)
                .unwrap_or_else(|e| panic!("{name}: EngineData: {e}"));
            let engine_dict = root
                .get("EngineDict")
                .unwrap_or_else(|| panic!("{name}: EngineDict missing"));
            assert!(
                engine_dict.get("Editor").is_some(),
                "{name}: EngineDict/Editor missing"
            );
            assert!(
                root.get("ResourceDict").is_some(),
                "{name}: ResourceDict missing"
            );

            // Re-serialize → re-parse must be stable (round-trip fidelity;
            // compare bytes — parsed spans differ from serialized output).
            let serialized = engine_data::serialize(&root);
            let reparsed = engine_data::parse(&serialized).unwrap();
            assert_eq!(
                engine_data::serialize(&reparsed),
                serialized,
                "{name}: EngineData round trip"
            );
            parsed += 1;
        }
    }

    assert!(
        parsed > 0,
        "no TySh blocks found in the TextLayers fixtures"
    );
}

#[test]
fn splice_editing_updates_only_the_target_bytes() {
    // Demonstrates the byte-patch workflow: change a font size in the raw
    // payload via its recorded span, leaving everything else untouched.
    let bytes = std::fs::read(fixture("TextLayers/TextLayers_Basic.psd")).unwrap();
    let file = PhotoshopFile::read(&mut BeReader::new(&bytes)).unwrap();

    let block = file
        .layer_and_mask_info
        .layer_info
        .layer_records
        .iter()
        .find_map(|record| {
            record
                .additional_layer_info
                .as_ref()
                .and_then(|ali| ali.get(TaggedBlockKey::new(*b"TySh")))
        });
    let Some(block) = block else {
        panic!("no TySh block in TextLayers_Basic.psd");
    };
    let (descriptor, _) = text_descriptor(&block.data).unwrap();
    let DescriptorValue::RawData { data, .. } = descriptor.get("EngineData").unwrap() else {
        panic!("EngineData not raw");
    };

    let root = engine_data::parse(data).unwrap();
    let font_size = root
        .get("EngineDict")
        .and_then(|engine| engine.get("StyleRun"))
        .and_then(|run| run.get("RunArray"))
        .and_then(|array| array.as_array())
        .and_then(|runs| runs.first())
        .and_then(|run| run.get("StyleSheet"))
        .and_then(|sheet| sheet.get("StyleSheetData"))
        .and_then(|data| data.get("FontSize"))
        .expect("FontSize missing")
        .clone();
    assert!(font_size.as_double().unwrap() > 0.0);

    let font_size_path = |root: &psd_core::EngineValue| {
        root.get("EngineDict")
            .and_then(|engine| engine.get("StyleRun"))
            .and_then(|run| run.get("RunArray"))
            .and_then(|array| array.as_array())
            .and_then(|runs| runs.first())
            .and_then(|run| run.get("StyleSheet"))
            .and_then(|sheet| sheet.get("StyleSheetData"))
            .and_then(|data| data.get("FontSize"))
            .expect("FontSize missing after splice")
            .clone()
    };

    let mut payload = data.clone();
    engine_data::splice_payload(&mut payload, font_size.span.clone(), b"40.0");
    let updated = engine_data::parse(&payload).unwrap();
    assert_eq!(font_size_path(&updated).as_double(), Some(40.0));
    // The surrounding bytes are untouched.
    assert_eq!(
        &payload[..font_size.span.start],
        &data[..font_size.span.start]
    );
    let tail = payload[font_size.span.start + 4..].to_vec();
    assert_eq!(tail, data[font_size.span.end..]);
}

/// Upstream `TestEngineDataStructure.cpp` factory and mutation cases.
mod upstream_structure {
    use psd_core::engine_data::{self, EngineValue, EngineValueKind};

    #[test]
    fn make_number_creates_a_floating_point_value() {
        let value = EngineValue::number(2.5);
        let number = value.as_number().unwrap();
        assert_eq!(number.value, 2.5);
        assert_eq!(number.integer, None);
    }

    #[test]
    fn make_number_with_integer_valued_double_marks_is_integer() {
        let number = EngineValue::number(42.0).as_number().unwrap();
        assert_eq!(number.value, 42.0);
        assert_eq!(number.integer, Some(42));
    }

    #[test]
    fn make_float_never_marks_is_integer() {
        let value = EngineValue::float(42.0);
        assert_eq!(value.as_number().unwrap().integer, None);
        assert_eq!(engine_data::format_value_bytes(&value, 0), b"42.0");
    }

    #[test]
    fn make_integer_creates_integer_value() {
        let number = EngineValue::integer(7).as_number().unwrap();
        assert_eq!(number.value, 7.0);
        assert_eq!(number.integer, Some(7));
    }

    #[test]
    fn make_bool_creates_a_boolean_value() {
        assert_eq!(EngineValue::boolean(true).as_bool(), Some(true));
        assert_eq!(EngineValue::boolean(false).as_bool(), Some(false));
    }

    #[test]
    fn make_name_creates_a_name_value() {
        let value = EngineValue::name("WritingDirection");
        assert_eq!(value.as_name(), Some("WritingDirection"));
    }

    #[test]
    fn make_string_creates_a_literal_string_value() {
        let value = EngineValue::string("Hello World");
        assert_eq!(value.as_str(), Some("Hello World"));
    }

    #[test]
    fn make_dict_creates_an_empty_dictionary() {
        assert_eq!(EngineValue::dict().as_dictionary(), Some(&[][..]));
    }

    #[test]
    fn make_array_creates_an_empty_array() {
        assert_eq!(EngineValue::array().as_array(), Some(&[][..]));
    }

    #[test]
    fn set_bool_changes_value_and_rejects_wrong_type() {
        let mut value = EngineValue::boolean(false);
        assert!(value.set_bool(true));
        assert_eq!(value.as_bool(), Some(true));
        assert!(!EngineValue::number(1.0).set_bool(true));
    }

    #[test]
    fn set_name_changes_value_and_rejects_wrong_type() {
        let mut value = EngineValue::name("Old");
        assert!(value.set_name("New"));
        assert_eq!(value.as_name(), Some("New"));
        assert!(!EngineValue::number(1.0).set_name("Oops"));
    }

    #[test]
    fn set_string_changes_value_and_rejects_wrong_type() {
        let mut value = EngineValue::string("Old");
        assert!(value.set_string("New"));
        assert_eq!(value.as_str(), Some("New"));
        assert!(!EngineValue::number(1.0).set_string("Oops"));
    }

    #[test]
    fn insert_dict_value_inserts_a_new_key() {
        let mut dict = EngineValue::dict();
        assert!(dict.insert("FontSize", EngineValue::number(24.0)));
        let items = dict.as_dictionary().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].0, "FontSize");
        assert_eq!(items[0].1.as_double(), Some(24.0));
    }

    #[test]
    fn insert_dict_value_replaces_an_existing_key() {
        let mut dict = EngineValue::dict();
        dict.insert("FontSize", EngineValue::number(12.0));
        dict.insert("FontSize", EngineValue::number(36.0));
        let items = dict.as_dictionary().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].1.as_double(), Some(36.0));
    }

    #[test]
    fn insert_dict_value_rejects_non_dictionary() {
        assert!(!EngineValue::array().insert("Key", EngineValue::number(1.0)));
    }

    #[test]
    fn remove_dict_value_removes_an_existing_key() {
        let mut dict = EngineValue::dict();
        dict.insert("A", EngineValue::number(1.0));
        dict.insert("B", EngineValue::number(2.0));
        assert!(dict.remove("A"));
        let items = dict.as_dictionary().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].0, "B");
    }

    #[test]
    fn remove_dict_value_returns_false_for_missing_key() {
        assert!(!EngineValue::dict().remove("Missing"));
    }

    #[test]
    fn remove_dict_value_rejects_non_dictionary() {
        assert!(!EngineValue::number(42.0).remove("Key"));
    }

    #[test]
    fn append_array_item_appends_items_in_order() {
        let mut array = EngineValue::array();
        for value in [1.0, 2.0, 3.0] {
            assert!(array.push(EngineValue::number(value)));
        }
        let items = array.as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].as_double(), Some(1.0));
        assert_eq!(items[2].as_double(), Some(3.0));
    }

    #[test]
    fn append_array_item_rejects_non_array() {
        assert!(!EngineValue::dict().push(EngineValue::number(1.0)));
    }

    #[test]
    fn factory_built_tree_survives_serialize_parse_round_trip() {
        let mut inner = EngineValue::dict();
        inner.insert("Name", EngineValue::name("Hello"));
        inner.insert("Size", EngineValue::integer(42));
        inner.insert("Enabled", EngineValue::boolean(true));
        let mut items = EngineValue::array();
        for value in 1..=3 {
            items.push(EngineValue::integer(value));
        }
        inner.insert("Items", items);
        let mut root = EngineValue::dict();
        root.insert("Root", inner);

        let reparsed = engine_data::parse(&engine_data::serialize(&root)).unwrap();
        let inner = reparsed.get("Root").unwrap();
        assert!(matches!(inner.kind, EngineValueKind::Dictionary(_)));
        assert_eq!(inner.get("Name").unwrap().as_name(), Some("Hello"));
        assert_eq!(
            inner.get("Size").unwrap().as_number().unwrap().integer,
            Some(42)
        );
        assert_eq!(inner.get("Enabled").unwrap().as_bool(), Some(true));
        assert_eq!(
            inner.get("Items").unwrap().as_int32_vector(),
            Some(vec![1, 2, 3])
        );
    }
}

#[test]
fn dictionary_bodies_parse_and_byte_insertion_refuses_undelimited_containers() {
    let body = b" /98 << /0 13 >> /0 << /1 [ 1 2 ] >>\n";
    let root = engine_data::parse_dictionary_body(body).unwrap();
    assert_eq!(root.span, 0..body.len());
    assert_eq!(
        root.get_path(["0", "1"]).unwrap().as_int32_vector(),
        Some(vec![1, 2])
    );
    assert!(engine_data::parse(body).is_err());
    assert_eq!(
        engine_data::parse_dictionary_body(b"  ")
            .unwrap()
            .as_dictionary(),
        Some(&[][..])
    );
    assert!(engine_data::parse_dictionary_body(b"/Key").is_err());

    // Byte-level insertion needs the closing delimiter it inserts before.
    let mut payload = body.to_vec();
    let value = psd_core::EngineValue::integer(1);
    assert!(!engine_data::insert_dict_entry_bytes(
        &mut payload,
        &root,
        "New",
        &value
    ));
    assert_eq!(payload, body);
    let inner = root.get("0").unwrap().clone();
    assert!(engine_data::insert_dict_entry_bytes(
        &mut payload,
        &inner,
        "New",
        &value
    ));
    let reparsed = engine_data::parse_dictionary_body(&payload).unwrap();
    assert_eq!(
        reparsed.get_path(["0", "New"]).unwrap().as_double(),
        Some(1.0)
    );

    // A span that does not end on the container's closing byte is refused.
    let array = reparsed.get_path(["0", "1"]).unwrap().clone();
    let mut shifted = array.clone();
    shifted.span = array.span.start..array.span.end - 1;
    let mut untouched = payload.clone();
    assert!(!engine_data::insert_array_item_bytes(
        &mut untouched,
        &shifted,
        &value
    ));
    assert_eq!(untouched, payload);
}

/// Photoshop writes leading-dot floats (`.583`, `.8`) throughout its style
/// sheets; they must read as numbers, and non-numbers must keep their
/// identifier reading.
#[test]
fn leading_dot_tokens_are_numbers() {
    let payload =
        b"<< /SuperscriptSize .583 /WordSpacing [ .8 1.0 1.33 ] /Name P22-FLLW-Light /Bad inf >>";
    let root = engine_data::parse(payload).unwrap();
    assert_eq!(
        root.get("SuperscriptSize")
            .and_then(|value| value.as_number())
            .map(|number| number.value),
        Some(0.583)
    );
    assert_eq!(
        root.get("WordSpacing")
            .and_then(|value| value.as_array())
            .and_then(|items| items[1].as_number())
            .map(|number| number.value),
        Some(1.0)
    );
    assert!(root
        .get("Name")
        .is_some_and(|value| matches!(value.kind, engine_data::EngineValueKind::Identifier(_))));
    assert!(root
        .get("Bad")
        .is_some_and(|value| matches!(value.kind, engine_data::EngineValueKind::Identifier(_))));
}
