//! Layer-move semantics: what travels with a layer, and what must not.
//!
//! Photoshop's Move tool shifts the pixel bounds and carries the placement data
//! with the layer; a mask's stored rect is never rewritten, because its link
//! flag already says whether it follows. `LayeredFile::translate_layer` is the
//! document-level entry point that can do all of it; `Layer::translate` refuses
//! the parts that need the document rather than moving a layer halfway.

use std::path::Path;

use psd::core::vector::VectorData;
use psd::core::vector::{BezierKnot, PathPoint, PathRecord, VectorPath};
use psd::core::{TaggedBlockKey, VectorBlock, VectorMask};
use psd::{Layer, LayerKind, LayeredFile, Rect};

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/documents")
        .join(name)
}

fn knot(horizontal: i32, vertical: i32) -> PathRecord {
    let point = |horizontal: i32, vertical: i32| PathPoint {
        horizontal,
        vertical,
    };
    PathRecord::Knot(BezierKnot {
        closed: false,
        linked: true,
        preceding: point(horizontal, vertical),
        anchor: point(horizontal, vertical),
        leaving: point(horizontal, vertical),
    })
}

/// The 8.24 fraction a pixel delta becomes for a document of `extent` pixels.
fn fraction(pixels: i32, extent: u32) -> i32 {
    (f64::from(pixels) / f64::from(extent) * f64::from(1 << 24)).round() as i32
}

/// A mask's stored rect is never rewritten: a linked mask follows implicitly
/// (its rect is relative to the layer) and an unlinked one stays where it is.
#[test]
fn moving_a_layer_never_rewrites_a_mask_rect() {
    let document = LayeredFile::<u8>::read(fixture("Masks/Masks_8bit.psd")).unwrap();
    let id = document
        .flatten()
        .into_iter()
        .find(|&id| {
            let layer = document.layer(id).unwrap();
            layer.mask_record().is_some()
                && !layer.blocks.blocks.iter().any(|block| {
                    block.key.as_bytes() == *b"vmsk" || block.key.as_bytes() == *b"vsms"
                })
        })
        .expect("the fixture carries a pixel mask without vector geometry");

    for relative in [false, true] {
        let mut document = LayeredFile::<u8>::from_bytes(&document.to_bytes().unwrap()).unwrap();
        let layer = document.layer_mut(id).unwrap();
        if let Some(data) = layer.mask.as_mut() {
            for record in [data.pixel_mask.as_mut(), data.vector_mask.as_mut()]
                .into_iter()
                .flatten()
            {
                record.flags.set_position_relative_to_layer(relative);
            }
        }
        let before = document
            .layer(id)
            .unwrap()
            .mask_record()
            .map(|mask| (mask.top, mask.left, mask.bottom, mask.right));
        let bounds_before = document.layer(id).unwrap().bounds;

        document.layer_mut(id).unwrap().translate(7, 11).unwrap();

        let layer = document.layer(id).unwrap();
        assert_eq!(
            layer
                .mask_record()
                .map(|mask| (mask.top, mask.left, mask.bottom, mask.right)),
            before,
            "mask rect must not move (linked = {relative})"
        );
        assert_eq!(
            layer.bounds,
            bounds_before.translated(7, 11).unwrap(),
            "the pixel bounds do move"
        );
    }
}

/// A shape layer's path is document-relative geometry: the document-level move
/// translates it, and the layer-level move refuses instead of leaving it behind.
#[test]
fn moving_a_shape_layer_carries_its_path() {
    let mut document = LayeredFile::<u8>::new(psd::core::ColorMode::Rgb, 200, 100).unwrap();
    let mut layer = Layer::<u8>::new_shape("Shape", Rect::new(0, 0, 50, 50));
    let mask = VectorMask {
        version: 3,
        flags: 0,
        path: VectorPath {
            records: vec![knot(1 << 23, 1 << 23), knot(1 << 24, 1 << 24)],
            trailing_bytes: Vec::new(),
        },
    };
    layer
        .set_vector_block(
            &VectorBlock::new(TaggedBlockKey::new(*b"vmsk"), VectorData::Mask(mask)).unwrap(),
        )
        .expect("a vector mask needs a shape layer");
    let id = document.add_layer(layer);

    // The layer on its own cannot convert a pixel move into path units.
    assert!(
        document.layer_mut(id).unwrap().translate(10, 5).is_err(),
        "a layer with vector geometry refuses a document-less move"
    );

    let before: Vec<(i32, i32)> = path_points(&document, id);
    let bounds_before = document.layer(id).unwrap().bounds;
    document.translate_layer(id, 10, 5).unwrap();

    let after = path_points(&document, id);
    let dx = fraction(10, 200);
    let dy = fraction(5, 100);
    let expected: Vec<(i32, i32)> = before
        .iter()
        .map(|(horizontal, vertical)| (horizontal + dx, vertical + dy))
        .collect();
    assert_eq!(
        after, expected,
        "the path moves by the document-relative delta"
    );
    assert_eq!(
        document.layer(id).unwrap().bounds,
        bounds_before.translated(10, 5).unwrap()
    );
}

fn path_points(document: &LayeredFile<u8>, id: usize) -> Vec<(i32, i32)> {
    let mask = document
        .layer(id)
        .unwrap()
        .vector_mask()
        .unwrap()
        .expect("the layer keeps its vector mask");
    mask.path
        .records
        .iter()
        .filter_map(|record| match record {
            PathRecord::Knot(knot) => Some((knot.anchor.horizontal, knot.anchor.vertical)),
            _ => None,
        })
        .collect()
}

/// A group takes every layer inside it along.
#[test]
fn moving_a_group_carries_its_children() {
    let mut document = LayeredFile::<u8>::new(psd::core::ColorMode::Rgb, 64, 64).unwrap();
    let group = document.add_layer(Layer::<u8>::new_group("Group"));
    let child = document
        .add_layer_to_group(
            group,
            Layer::<u8>::new_image("Child", Rect::new(2, 3, 12, 13)),
        )
        .unwrap();
    let grandchild = document
        .add_layer_to_group(
            group,
            Layer::<u8>::new_image("Grandchild", Rect::new(4, 5, 9, 9)),
        )
        .unwrap();
    let child_before = document.layer(child).unwrap().bounds;
    let grandchild_before = document.layer(grandchild).unwrap().bounds;

    document.translate_layer(group, 6, 8).unwrap();

    assert_eq!(
        document.layer(child).unwrap().bounds,
        child_before.translated(6, 8).unwrap(),
        "a child moves with its group"
    );
    assert_eq!(
        document.layer(grandchild).unwrap().bounds,
        grandchild_before.translated(6, 8).unwrap(),
        "so does a nested child"
    );
    assert!(matches!(
        document.layer(group).unwrap().kind,
        LayerKind::Group(_)
    ));
}

/// A text layer moves its transform, not just its bounds, so Photoshop
/// re-renders the text at the new place.
#[test]
fn moving_a_text_layer_carries_its_transform() {
    let mut document = LayeredFile::<u8>::new(psd::core::ColorMode::Rgb, 128, 128).unwrap();
    let layer = psd::TextLayerBuilder::new("Text", "Hello")
        .font_size(12.0)
        .build::<u8>()
        .unwrap();
    let id = document.add_layer(layer);
    let before = document.layer(id).unwrap().text_position().unwrap();
    document.translate_layer(id, 9, 4).unwrap();
    let after = document.layer(id).unwrap().text_position().unwrap();
    assert_eq!((after.0 - before.0, after.1 - before.1), (9.0, 4.0));
}
