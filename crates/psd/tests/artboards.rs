//! Artboards: the generated corpus (`fixtures/generated/Artboards`, see its
//! README) and the Photoshop-saved group documents.

use std::path::{Path, PathBuf};

use psd::core::{ArtboardBackground, TaggedBlockKey};
use psd::{ChannelKey, Layer, LayerId, LayeredFile, Rect};

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
