//! The document-level `Txt2` text cache: upstream's explicit
//! `LayeredFile::invalidate_text_cache()` plus the port's automatic
//! invalidation of a stale cache on write.

use std::path::{Path, PathBuf};

use psd::core::{TaggedBlock, TaggedBlockKey};
use psd::{Layer, LayeredFile, TextWritingDirection};

const TXT2: TaggedBlockKey = TaggedBlockKey::new(*b"Txt2");

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/documents/TextLayers")
        .join(name)
}

fn open(name: &str) -> LayeredFile<u8> {
    LayeredFile::<u8>::read(fixture(name)).unwrap()
}

fn cache(file: &LayeredFile<u8>) -> Option<Vec<u8>> {
    file.document_blocks
        .as_ref()?
        .get(TXT2)
        .map(|block| block.data.clone())
}

fn written_cache(file: &LayeredFile<u8>) -> Option<Vec<u8>> {
    cache(&LayeredFile::<u8>::from_bytes(&file.to_bytes().unwrap()).unwrap())
}

fn text_layer(file: &LayeredFile<u8>, text: &str) -> usize {
    file.layers()
        .position(|layer| layer.text().as_deref() == Some(text))
        .unwrap()
}

#[test]
fn every_text_fixture_carries_one_document_level_txt2_cache() {
    for name in [
        "TextLayers_Basic.psd",
        "TextLayers_CharacterStyles.psd",
        "TextLayers_FontFallback.psd",
        "TextLayers_Paragraph.psd",
        "TextLayers_StyleRuns.psd",
        "TextLayers_TextOnPath.psd",
        "TextLayers_Transform.psd",
        "TextLayers_Vertical.psd",
        "TextLayers_VerticalBox.psd",
        "TextLayers_Warp.psd",
    ] {
        let file = open(name);
        assert!(cache(&file).is_some(), "{name}");
        assert!(!file.text_cache_is_stale(), "{name}");
    }
}

#[test]
fn untouched_documents_keep_the_cache_byte_for_byte() {
    let file = open("TextLayers_Basic.psd");
    assert_eq!(written_cache(&file), cache(&file));
}

#[test]
fn text_edits_make_the_cache_stale_and_writing_drops_it() {
    let mut file = open("TextLayers_Basic.psd");
    let id = text_layer(&file, "Hello 123");
    file.layer_mut(id).unwrap().set_text("Edited").unwrap();
    assert!(file.text_cache_is_stale());
    // Writing never mutates the document itself.
    assert!(cache(&file).is_some());
    assert_eq!(written_cache(&file), None);

    // The re-read document has no cache to go stale.
    let reread = LayeredFile::<u8>::from_bytes(&file.to_bytes().unwrap()).unwrap();
    assert!(!reread.text_cache_is_stale());
    assert_eq!(reread.layer(id).unwrap().text().as_deref(), Some("Edited"));
}

#[test]
fn style_orientation_and_transform_edits_all_invalidate() {
    let edits: [fn(&mut Layer<u8>); 4] = [
        |layer| {
            layer.style_run_mut(0).set_faux_bold(true).unwrap();
        },
        |layer| {
            layer
                .set_orientation(TextWritingDirection::Vertical)
                .unwrap()
        },
        |layer| layer.set_text_position(1.0, 2.0).unwrap(),
        |layer| layer.set_text_warp_value(10.0).unwrap(),
    ];
    for edit in edits {
        let mut file = open("TextLayers_Basic.psd");
        let id = text_layer(&file, "Hello 123");
        edit(file.layer_mut(id).unwrap());
        assert!(file.text_cache_is_stale());
        assert_eq!(written_cache(&file), None);
    }
}

#[test]
fn adding_a_text_layer_invalidates_but_other_layers_do_not() {
    let mut file = open("TextLayers_Basic.psd");
    file.add_layer(Layer::<u8>::new_image("Pixels", psd::Rect::default()));
    assert!(!file.text_cache_is_stale());
    assert_eq!(written_cache(&file), cache(&file));

    file.add_layer(Layer::<u8>::new_text("New text", "Fresh").unwrap());
    assert!(file.text_cache_is_stale());
    assert_eq!(written_cache(&file), None);
}

#[test]
fn non_text_edits_and_reverted_text_edits_keep_the_cache() {
    let mut file = open("TextLayers_Basic.psd");
    let id = text_layer(&file, "Hello 123");
    file.layer_mut(id).unwrap().set_visible(false);
    file.layer_mut(id).unwrap().name = "Renamed".to_owned();
    assert!(!file.text_cache_is_stale());

    file.layer_mut(id).unwrap().set_text("Changed").unwrap();
    assert!(file.text_cache_is_stale());
    file.layer_mut(id).unwrap().set_text("Hello 123").unwrap();
    assert!(!file.text_cache_is_stale(), "identical bytes are not stale");
    assert_eq!(written_cache(&file), cache(&file));
}

#[test]
fn invalidate_text_cache_removes_it_explicitly() {
    let mut file = open("TextLayers_Basic.psd");
    assert!(file.invalidate_text_cache());
    assert_eq!(cache(&file), None);
    assert!(!file.invalidate_text_cache());
    assert_eq!(written_cache(&file), None);
    // Other document-level blocks survive.
    let reread = LayeredFile::<u8>::from_bytes(&file.to_bytes().unwrap()).unwrap();
    let before = open("TextLayers_Basic.psd");
    let keys = |file: &LayeredFile<u8>| {
        file.document_blocks
            .as_ref()
            .map(|blocks| {
                blocks
                    .blocks
                    .iter()
                    .map(|block| block.key)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let mut expected = keys(&before);
    expected.retain(|&key| key != TXT2);
    assert_eq!(keys(&reread), expected);
}

#[test]
fn retain_text_cache_opts_out_of_automatic_invalidation() {
    let mut file = open("TextLayers_Basic.psd");
    let original = cache(&file);
    let id = text_layer(&file, "Hello 123");
    file.layer_mut(id).unwrap().set_text("Edited").unwrap();
    file.retain_text_cache();
    assert!(!file.text_cache_is_stale());
    assert_eq!(written_cache(&file), original);
}

#[test]
fn a_cache_replaced_by_the_caller_is_left_alone() {
    let mut file = open("TextLayers_Basic.psd");
    let id = text_layer(&file, "Hello 123");
    file.layer_mut(id).unwrap().set_text("Edited").unwrap();
    let blocks = file.document_blocks.as_mut().unwrap();
    blocks.get_mut(TXT2).unwrap().data = b"caller-managed".to_vec();
    assert!(!file.text_cache_is_stale());
    assert_eq!(written_cache(&file), Some(b"caller-managed".to_vec()));

    // A cache inserted into a document that had none is also kept.
    let mut created = LayeredFile::<u8>::new(psd::core::ColorMode::Rgb, 8, 8).unwrap();
    created.add_layer(Layer::<u8>::new_text("Text", "abc").unwrap());
    let mut blocks = psd::core::AdditionalLayerInfo::new();
    blocks.push(TaggedBlock::new(TXT2, b"inserted".to_vec()));
    created.document_blocks = Some(blocks);
    assert!(!created.text_cache_is_stale());
    assert_eq!(written_cache(&created), Some(b"inserted".to_vec()));
}

#[test]
fn the_cache_baseline_is_runtime_state_outside_document_equality() {
    let mut edited = open("TextLayers_Basic.psd");
    edited.retain_text_cache();
    assert_eq!(edited, open("TextLayers_Basic.psd"));
}
