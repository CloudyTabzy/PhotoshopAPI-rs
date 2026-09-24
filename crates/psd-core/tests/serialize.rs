//! `serde::Serialize` views (feature `serde`), upstream's `to_json`.
#![cfg(feature = "serde")]

use std::path::PathBuf;

use psd_core::{BeReader, PhotoshopFile, TaggedBlockKey, TypeToolTaggedBlock};
use serde_json::{json, Value};

fn fixture_tysh(name: &str) -> TypeToolTaggedBlock {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/documents/TextLayers")
        .join(name);
    let bytes = std::fs::read(path).unwrap();
    let file = PhotoshopFile::read(&mut BeReader::new(&bytes)).unwrap();
    let block = file
        .layer_and_mask_info
        .layer_info
        .layer_records
        .iter()
        .find_map(|record| {
            record
                .additional_layer_info
                .as_ref()?
                .get(TaggedBlockKey::new(*b"TySh"))
        })
        .unwrap();
    TypeToolTaggedBlock::read(&mut BeReader::new(&block.data)).unwrap()
}

#[test]
fn descriptors_serialize_in_source_order_tagged_by_ostype() {
    let tysh = fixture_tysh("TextLayers_Basic.psd");
    let text = serde_json::to_value(&tysh.text).unwrap();
    assert_eq!(text["class"], "TxLr");
    let keys: Vec<_> = text["items"].as_object().unwrap().keys().cloned().collect();
    assert_eq!(
        keys,
        [
            "Txt ",
            "textGridding",
            "Ornt",
            "AntA",
            "bounds",
            "boundingBox",
            "TextIndex",
            "EngineData"
        ]
    );
    assert_eq!(text["items"]["Txt "], json!({ "TEXT": "Hello 123" }));
    assert_eq!(
        text["items"]["Ornt"],
        json!({ "enum": { "type": "Ornt", "value": "Hrzn" } })
    );
    assert_eq!(text["items"]["TextIndex"], json!({ "long": 1 }));
    let bounds = &text["items"]["bounds"]["Objc"];
    assert_eq!(bounds["class"], "bounds");
    assert_eq!(
        bounds["items"]["Left"],
        json!({ "UntF": { "unit": "#Pnt", "value": 0.0 } })
    );
    assert!(text["items"]["EngineData"]["tdta"].is_array());

    let warp = serde_json::to_value(&tysh.warp).unwrap();
    assert_eq!(
        warp["items"]["warpStyle"],
        json!({ "enum": { "type": "warpStyle", "value": "warpNone" } })
    );
    assert_eq!(warp["items"]["warpValue"], json!({ "doub": 0.0 }));
}

#[test]
fn engine_data_serializes_as_natural_json() {
    let tysh = fixture_tysh("TextLayers_StyleRuns.psd");
    let engine = tysh.engine_data().unwrap().unwrap();
    let value: Value = serde_json::to_value(&engine).unwrap();
    // UTF-16BE literal strings decode to text, numbers keep their kind.
    assert_eq!(value["EngineDict"]["Editor"]["Text"], "Alpha Beta Gamma\r");
    assert_eq!(
        value["EngineDict"]["StyleRun"]["RunLengthArray"],
        json!([5, 1, 4, 1, 6])
    );
    let size =
        &value["EngineDict"]["StyleRun"]["RunArray"][2]["StyleSheet"]["StyleSheetData"]["FontSize"];
    assert_eq!(size.as_f64(), Some(44.0));
    assert!(value["ResourceDict"]["FontSet"].is_array());
    // Top-level keys keep their source order.
    let keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys.first().map(String::as_str), Some("EngineDict"));
}
