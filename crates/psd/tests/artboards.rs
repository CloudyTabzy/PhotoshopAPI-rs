//! Artboards: the generated corpus (`fixtures/generated/Artboards`, see its
//! README) and the Photoshop-saved group documents.

use std::path::{Path, PathBuf};

use psd::core::{Artboard, ArtboardBackground, ArtboardRect, Color, TaggedBlockKey, Version};
use psd::{ChannelKey, Layer, LayerId, LayerTree, LayeredFile, Rect};

fn fixture(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(path)
}

const GENERATED: [&str; 2] = [
    "generated/Artboards/artboards_8bit.psd",
    "generated/Artboards/artboards_8bit.psb",
];

fn id_of(file: &LayeredFile<u8>, name: &str) -> LayerId {
    file.layers_with_ids()
        .find(|(_, layer)| layer.name == name)
        .map(|(id, _)| id)
        .unwrap_or_else(|| panic!("missing layer {name}"))
}

fn child_names(file: &LayeredFile<u8>, id: LayerId) -> Vec<String> {
    file.layer(id)
        .unwrap()
        .group()
        .unwrap()
        .children
        .iter()
        .map(|child| file.layer(*child).unwrap().name.clone())
        .filter(|name| name != "</Layer group>")
        .collect()
}

fn check(file: &LayeredFile<u8>) {
    let names: Vec<_> = file
        .artboards()
        .into_iter()
        .map(|id| file.layer(id).unwrap().name.as_str())
        .collect();
    assert_eq!(names, ["Artboard Left", "Artboard Right"]);
    for name in ["Plain Group", "Inner Group", "Red Square", "Loose Pixels"] {
        let layer = file.layer(id_of(file, name)).unwrap();
        assert!(!layer.is_artboard(), "{name}");
        assert!(layer.artboard().unwrap().is_none(), "{name}");
    }

    let left = id_of(file, "Artboard Left");
    let artboard = file.layer(left).unwrap().artboard().unwrap().unwrap();
    assert_eq!(artboard.key, TaggedBlockKey::new(*b"artb"));
    let rect = artboard.rect().unwrap();
    assert_eq!(
        (rect.left, rect.top, rect.right, rect.bottom),
        (0.0, 0.0, 60.0, 50.0)
    );
    assert_eq!(artboard.preset_name(), Some("Icon 60"));
    assert_eq!(artboard.background(), Some(ArtboardBackground::White));
    assert_eq!(artboard.guide_indices(), Some(vec![]));
    assert_eq!(child_names(file, left), ["Red Square"]);

    let right = id_of(file, "Artboard Right");
    let artboard = file.layer(right).unwrap().artboard().unwrap().unwrap();
    let rect = artboard.rect().unwrap();
    assert_eq!((rect.width(), rect.height()), (60.0, 50.0));
    assert_eq!(artboard.preset_name(), Some(""));
    assert_eq!(artboard.background(), Some(ArtboardBackground::Custom));
    let blue = artboard.background_color().unwrap().get("Bl  ").unwrap();
    assert_eq!(blue.as_double(), Some(120.0));
    assert_eq!(artboard.guide_indices(), Some(vec![0]));
    assert_eq!(child_names(file, right), ["Inner Group"]);
    assert_eq!(child_names(file, id_of(file, "Inner Group")), ["Blue Bar"]);

    let settings = file.artboard_settings().unwrap().unwrap();
    assert_eq!(settings.count(), Some(2));
    assert_eq!(settings.origin(), Some((0.0, 0.0)));
    assert_eq!(settings.auto_expand_enabled(), Some(true));
    assert_eq!(settings.auto_position_enabled(), Some(false));
    assert_eq!(settings.shrinkwrap_on_save_enabled(), Some(true));
    assert_eq!(
        settings.default_background(),
        Some(ArtboardBackground::White)
    );
    assert!(settings.default_background_color().is_some());
}

#[test]
fn reads_generated_artboards() {
    for name in GENERATED {
        check(&LayeredFile::<u8>::read(fixture(name)).unwrap());
    }
}

#[test]
fn artboard_blocks_round_trip_byte_for_byte() {
    for name in GENERATED {
        let file = LayeredFile::<u8>::read(fixture(name)).unwrap();
        let back = LayeredFile::<u8>::from_bytes(&file.to_bytes().unwrap()).unwrap();
        for (before, after) in file.layers().zip(back.layers()) {
            assert_eq!(before.name, after.name);
            assert_eq!(
                before.blocks.get(TaggedBlockKey::new(*b"artb")),
                after.blocks.get(TaggedBlockKey::new(*b"artb")),
                "{}",
                before.name
            );
        }
        assert_eq!(back.document_blocks, file.document_blocks);
        check(&back);
    }
    // The PSB keeps `artd` under the `8B64` signature.
    let psb = LayeredFile::<u8>::read(fixture(GENERATED[1])).unwrap();
    let settings = psb
        .document_blocks
        .as_ref()
        .and_then(|blocks| blocks.get(TaggedBlockKey::new(*b"artd")))
        .unwrap();
    assert_eq!(&settings.signature, b"8B64");

    // PSB readers also accept `8BIM` for this key; a no-op must keep it.
    let mut alternate = psb;
    alternate
        .document_blocks
        .as_mut()
        .unwrap()
        .get_mut(TaggedBlockKey::new(*b"artd"))
        .unwrap()
        .signature = *b"8BIM";
    let settings = alternate.artboard_settings().unwrap().unwrap();
    let before = alternate.document_blocks.clone();
    alternate.set_artboard_settings(&settings).unwrap();
    assert_eq!(alternate.document_blocks, before);
}

#[test]
fn artboards_accept_group_edits() {
    // Artboards are group layers, so group editing applies to them.
    let mut file = LayeredFile::<u8>::read(fixture(GENERATED[0])).unwrap();
    let left = id_of(&file, "Artboard Left");
    let mut added = Layer::<u8>::new_image("Added", Rect::new(2, 2, 6, 6));
    added
        .image_mut()
        .unwrap()
        .set_channel(ChannelKey::color(0), vec![255; 16]);
    file.add_layer_to_group(left, added).unwrap();

    let back = LayeredFile::<u8>::from_bytes(&file.to_bytes().unwrap()).unwrap();
    let left = id_of(&back, "Artboard Left");
    assert!(back.layer(left).unwrap().is_artboard());
    assert_eq!(child_names(&back, left), ["Red Square", "Added"]);
}

#[test]
fn only_groups_are_artboards_and_malformed_data_fails_its_view() {
    let mut file = LayeredFile::<u8>::read(fixture(GENERATED[0])).unwrap();
    // An artboard block on a pixel layer does not make it an artboard.
    let pixels = id_of(&file, "Red Square");
    let artb = file
        .layer(id_of(&file, "Artboard Left"))
        .unwrap()
        .blocks
        .get(TaggedBlockKey::new(*b"artb"))
        .unwrap()
        .clone();
    file.layer_mut(pixels).unwrap().blocks.push(artb);
    assert!(!file.layer(pixels).unwrap().is_artboard());
    assert!(file.layer(pixels).unwrap().artboard().unwrap().is_none());

    let right = id_of(&file, "Artboard Right");
    let layer = file.layer_mut(right).unwrap();
    let block = layer.blocks.get_mut(TaggedBlockKey::new(*b"artb")).unwrap();
    block.data.truncate(10);
    let damaged = block.data.clone();
    assert!(layer.is_artboard());
    assert!(layer.artboard().is_err());

    let back = LayeredFile::<u8>::from_bytes(&file.to_bytes().unwrap()).unwrap();
    let layer = back.layer(id_of(&back, "Artboard Right")).unwrap();
    assert_eq!(
        layer
            .blocks
            .get(TaggedBlockKey::new(*b"artb"))
            .unwrap()
            .data,
        damaged
    );
}

#[test]
fn photoshop_groups_are_not_artboards() {
    for name in [
        "documents/Groups/Groups_8bit.psd",
        "documents/Groups/Groups_8bit.psb",
    ] {
        let file = LayeredFile::<u8>::read(fixture(name)).unwrap();
        assert!(file.layers().any(|layer| layer.group().is_some()));
        assert!(file.artboards().is_empty(), "{name}");
        assert!(file.artboard_settings().unwrap().is_none(), "{name}");
    }
}

#[test]
fn typed_artboard_authoring_round_trips_and_tracks_document_count() {
    let model = Artboard::new(
        ArtboardRect {
            top: 0.0,
            left: 0.0,
            bottom: 40.0,
            right: 50.0,
        },
        Some("Icon"),
        ArtboardBackground::Custom,
        Some(Color::Rgb {
            red: 18.0,
            green: 108.0,
            blue: 200.0,
        }),
        &[0],
    )
    .unwrap();

    for version in [Version::Psd, Version::Psb] {
        let mut file = LayeredFile::<u8>::new(psd::core::ColorMode::Rgb, 64, 64).unwrap();
        file.version = version;
        let id = file.add_artboard("Board", &model).unwrap();
        let mut child = Layer::<u8>::new_image("Pixels", Rect::new(2, 2, 8, 8));
        child
            .image_mut()
            .unwrap()
            .set_channel(ChannelKey::color(0), vec![42; 36]);
        file.add_layer_to_group(id, child).unwrap();

        assert_eq!(file.artboard_settings().unwrap().unwrap().count(), Some(1));
        let bytes = file.to_bytes().unwrap();
        let back = LayeredFile::<u8>::from_bytes(&bytes).unwrap();
        let board_id = id_of(&back, "Board");
        let board_layer = back.layer(board_id).unwrap();
        let parsed = board_layer.artboard().unwrap().unwrap();
        assert_eq!(parsed.key, TaggedBlockKey::new(*b"artb"));
        assert_eq!(parsed.preset_name(), Some("Icon"));
        assert_eq!(parsed.guide_indices(), Some(vec![0]));
        assert_eq!(child_names(&back, board_id), ["Pixels"]);
        let settings = back
            .document_blocks
            .as_ref()
            .and_then(|blocks| blocks.get(TaggedBlockKey::new(*b"artd")))
            .unwrap();
        assert_eq!(
            settings.signature,
            if version == Version::Psb {
                *b"8B64"
            } else {
                *b"8BIM"
            }
        );

        let removed = file.remove_layer(id).unwrap();
        assert!(removed.layer.is_artboard());
        assert_eq!(file.artboard_settings().unwrap().unwrap().count(), Some(0));
        assert!(file.artboards().is_empty());
    }
}

#[test]
fn artboards_cannot_be_nested_in_other_artboards() {
    let board = Artboard::new(
        ArtboardRect {
            top: 0.0,
            left: 0.0,
            bottom: 10.0,
            right: 10.0,
        },
        None,
        ArtboardBackground::Transparent,
        None,
        &[],
    )
    .unwrap();
    let mut file = LayeredFile::<u8>::new(psd::core::ColorMode::Rgb, 16, 16).unwrap();
    let parent = file.add_artboard("Parent", &board).unwrap();
    let mut child = Layer::<u8>::new_group("Child");
    child.set_artboard(&board).unwrap();
    assert!(file.add_layer_to_group(parent, child).is_err());

    let inner = file
        .add_layer_to_group(parent, Layer::new_group("Inner"))
        .unwrap();
    let child = file.add_artboard("Child", &board).unwrap();
    assert!(file.move_layer(child, Some(inner), None).is_err());
}

#[test]
fn artboard_tree_insertions_keep_the_document_count_in_sync() {
    let board = Artboard::new(
        ArtboardRect {
            top: 0.0,
            left: 0.0,
            bottom: 10.0,
            right: 10.0,
        },
        None,
        ArtboardBackground::Transparent,
        None,
        &[],
    )
    .unwrap();
    let mut file = LayeredFile::<u8>::new(psd::core::ColorMode::Rgb, 16, 16).unwrap();
    let group = file.add_layer(Layer::new_group("Group"));
    let mut detached = Layer::new_group("Board");
    detached.set_artboard(&board).unwrap();
    let id = file.add_layer_to_group(group, detached).unwrap();
    assert_eq!(file.artboard_settings().unwrap().unwrap().count(), Some(1));

    let detached_tree = file.remove_layer(id).unwrap();
    assert_eq!(file.artboard_settings().unwrap().unwrap().count(), Some(0));
    file.insert_layer_tree(None, None, detached_tree).unwrap();
    assert_eq!(file.artboard_settings().unwrap().unwrap().count(), Some(1));

    let mut outer = LayerTree::new(Layer::new_group("Container"));
    for name in ["Board A", "Board B"] {
        let mut layer = Layer::new_group(name);
        layer.set_artboard(&board).unwrap();
        outer.push_child(LayerTree::new(layer)).unwrap();
    }
    let container = file.insert_layer_tree(None, None, outer).unwrap();
    assert_eq!(file.artboards().len(), 3);
    assert_eq!(file.artboard_settings().unwrap().unwrap().count(), Some(3));
    let before = file.to_bytes().unwrap();
    assert!(file.set_artboard(container, &board).is_err());
    assert_eq!(file.to_bytes().unwrap(), before);
    assert!(!file.layer(container).unwrap().is_artboard());

    let mut nested = LayerTree::new(Layer::new_group("Outer Board"));
    nested.layer.set_artboard(&board).unwrap();
    let mut child = Layer::new_group("Nested Board");
    child.set_artboard(&board).unwrap();
    nested.push_child(LayerTree::new(child)).unwrap();
    assert!(file.insert_layer_tree(None, None, nested).is_err());
    assert_eq!(file.artboard_settings().unwrap().unwrap().count(), Some(3));
}
