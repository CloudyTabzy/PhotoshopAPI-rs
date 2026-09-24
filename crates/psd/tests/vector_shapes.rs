//! Vector masks, shape layers, and document paths: the generated corpus
//! (`fixtures/generated/Vectors`, see its README) and the Photoshop-saved
//! `Masks` and `TextOnPath` documents.

use std::path::{Path, PathBuf};

use psd::core::vector::{BezierKnot, DocumentPath, Subpath, VectorContent, VectorStroke};
use psd::core::{AdjustmentKind, TaggedBlock, TaggedBlockKey, VectorData};
use psd::{Layer, LayeredFile};

fn fixture(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(path)
}

const GENERATED: [&str; 2] = [
    "generated/Vectors/vector_shapes_8bit.psd",
    "generated/Vectors/vector_shapes_8bit.psb",
];

fn layer<'a>(file: &'a LayeredFile<u8>, name: &str) -> &'a Layer<u8> {
    file.layers()
        .find(|layer| layer.name == name)
        .unwrap_or_else(|| panic!("missing layer {name}"))
}

fn anchors(file: &LayeredFile<u8>, subpath: &Subpath<'_>) -> Vec<(f64, f64)> {
    subpath
        .knots
        .iter()
        .map(|knot| {
            let (x, y) = knot.anchor.to_pixels(file.width, file.height);
            (x.round(), y.round())
        })
        .collect()
}

fn stroke(layer: &Layer<u8>) -> VectorStroke {
    layer
        .vector_blocks()
        .unwrap()
        .into_iter()
        .find_map(|block| match block.data {
            VectorData::Stroke(stroke) => Some(stroke),
            _ => None,
        })
        .expect("stroke block")
}

fn content(layer: &Layer<u8>) -> VectorContent {
    layer
        .vector_blocks()
        .unwrap()
        .into_iter()
        .find_map(|block| match block.data {
            VectorData::Content(content) => Some(content),
            _ => None,
        })
        .expect("vscg block")
}

fn origination(layer: &Layer<u8>) -> Vec<(Option<i32>, Option<i32>)> {
    let blocks = layer.vector_blocks().unwrap();
    let origination = blocks
        .iter()
        .find_map(|block| match &block.data {
            VectorData::Origination(origination) => Some(origination),
            _ => None,
        })
        .expect("vogk block");
    origination
        .keys()
        .iter()
        .map(|key| (key.origin_type(), key.index()))
        .collect()
}

fn check_generated(file: &LayeredFile<u8>) {
    for name in ["Legacy Rectangle", "Ellipse", "Frame", "Open Line"] {
        assert!(layer(file, name).is_shape_layer(), "{name}");
    }
    for name in ["Background", "Masked Pixels"] {
        assert!(!layer(file, name).is_shape_layer(), "{name}");
    }
    assert!(layer(file, "Background").vector_mask().unwrap().is_none());

    // Legacy shape: `SoCo` + `vmsk`.
    let rectangle = layer(file, "Legacy Rectangle");
    assert!(rectangle.is_adjustment_layer());
    assert_eq!(
        rectangle.adjustments().unwrap()[0].kind,
        AdjustmentKind::SolidColor
    );
    let mask = rectangle.vector_mask().unwrap().unwrap();
    assert_eq!(mask.flags, 0);
    let subpaths = mask.path.subpaths().unwrap();
    assert_eq!(subpaths.len(), 1);
    assert!(subpaths[0].record.closed);
    assert_eq!(subpaths[0].record.knot_count, 4);
    assert!(subpaths[0].knots.iter().all(|knot| !knot.linked));
    assert_eq!(
        anchors(file, &subpaths[0]),
        [(4.0, 4.0), (28.0, 4.0), (28.0, 20.0), (4.0, 20.0)]
    );
    assert_eq!(origination(rectangle), [(Some(1), Some(0))]);

    // CS6 shape: `vscg` + `vsms` + `vstk` + `vogk`.
    let ellipse = layer(file, "Ellipse");
    assert!(!ellipse.is_adjustment_layer());
    let blocks = ellipse.vector_blocks().unwrap();
    let keys: Vec<_> = blocks.iter().map(|block| block.key.to_string()).collect();
    assert_eq!(keys, ["vscg", "vsms", "vstk", "vogk"]);
    let subpaths_mask = ellipse.vector_mask().unwrap().unwrap();
    let subpaths = subpaths_mask.path.subpaths().unwrap();
    assert!(subpaths[0].knots.iter().all(|knot| knot.linked));
    assert_eq!(
        anchors(file, &subpaths[0]),
        [(46.0, 4.0), (58.0, 14.0), (46.0, 24.0), (34.0, 14.0)]
    );
    let top: &BezierKnot = subpaths[0].knots[0];
    assert!(top.preceding.x() < top.anchor.x() && top.anchor.x() < top.leaving.x());
    let paint = content(ellipse);
    assert_eq!(&paint.key, b"SoCo");
    assert_eq!(paint.kind(), Some(AdjustmentKind::SolidColor));
    let blue = paint.fill.color().unwrap().get("Bl  ").unwrap();
    assert_eq!(blue.as_double(), Some(220.0));
    let ellipse_stroke = stroke(ellipse);
    assert_eq!(ellipse_stroke.stroke_enabled(), Some(true));
    assert_eq!(ellipse_stroke.fill_enabled(), Some(true));
    assert_eq!(ellipse_stroke.line_width(), Some((*b"#Pxl", 2.0)));
    assert_eq!(
        ellipse_stroke.line_cap().unwrap().as_bytes(),
        b"strokeStyleRoundCap"
    );
    assert_eq!(
        ellipse_stroke.line_join().unwrap().as_bytes(),
        b"strokeStyleRoundJoin"
    );
    assert_eq!(ellipse_stroke.dash_set(), Some(vec![4.0, 2.0]));
    assert_eq!(ellipse_stroke.opacity_percent(), Some(100.0));
    assert_eq!(
        ellipse_stroke.content().unwrap().class_id.as_bytes(),
        b"solidColorLayer"
    );
    assert_eq!(origination(ellipse), [(Some(5), Some(0))]);

    // Compound shape: two subpaths linked to two origination entries.
    let frame = layer(file, "Frame");
    let mask = frame.vector_mask().unwrap().unwrap();
    let subpaths = mask.path.subpaths().unwrap();
    let operations: Vec<_> = subpaths
        .iter()
        .map(|subpath| (subpath.record.operation, subpath.record.origination_index()))
        .collect();
    assert_eq!(operations, [(1, 0), (2, 1)]);
    assert_eq!(
        anchors(file, &subpaths[1]),
        [(10.0, 36.0), (24.0, 36.0), (24.0, 54.0), (10.0, 54.0)]
    );
    assert_eq!(origination(frame), [(Some(2), Some(0)), (Some(1), Some(1))]);
    let blocks = frame.vector_blocks().unwrap();
    let keys = blocks
        .iter()
        .find_map(|block| match &block.data {
            VectorData::Origination(origination) => Some(origination.keys()),
            _ => None,
        })
        .unwrap();
    let radii = keys[0].corner_radii().unwrap();
    assert_eq!(radii.top_left, 3.0);
    assert_eq!(radii.bottom_right, 3.0);
    let bounds = keys[1].shape_bounds().unwrap();
    assert_eq!(
        (bounds.top, bounds.left, bounds.bottom, bounds.right),
        (36.0, 10.0, 54.0, 24.0)
    );
    assert_eq!(stroke(frame).stroke_enabled(), Some(false));

    // Open path with a stroke and no fill.
    let line = layer(file, "Open Line");
    let mask = line.vector_mask().unwrap().unwrap();
    let subpaths = mask.path.subpaths().unwrap();
    assert!(!subpaths[0].record.closed);
    assert_eq!(subpaths[0].record.operation, -1);
    assert!(subpaths[0].knots.iter().all(|knot| !knot.closed));
    assert_eq!(anchors(file, &subpaths[0]), [(36.0, 36.0), (60.0, 36.0)]);
    let line_stroke = stroke(line);
    assert_eq!(line_stroke.fill_enabled(), Some(false));
    assert_eq!(line_stroke.line_width(), Some((*b"#Pxl", 4.0)));

    // A pixel layer's vector mask with invert and unlink flags.
    let masked = layer(file, "Masked Pixels");
    let mask = masked.vector_mask().unwrap().unwrap();
    assert!(mask.inverted() && mask.not_linked() && !mask.disabled());
    assert_eq!(
        anchors(file, &mask.path.subpaths().unwrap()[0]),
        [(50.0, 42.0), (62.0, 62.0), (38.0, 62.0)]
    );

    // Document paths and their Unicode names.
    let paths = file.document_paths().unwrap();
    let summary: Vec<_> = paths
        .iter()
        .map(|path: &DocumentPath| {
            (
                path.resource_id,
                path.is_work_path(),
                path.name.as_str(),
                path.unicode_name.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (1025, true, "", None),
            (2000, false, "Outline", Some("Outline ✓"))
        ]
    );
    let outline = paths[1].path.subpaths().unwrap();
    assert_eq!(
        anchors(file, &outline[0]),
        [(32.0, 2.0), (62.0, 62.0), (2.0, 62.0)]
    );
}

fn vector_blocks(file: &LayeredFile<u8>) -> Vec<TaggedBlock> {
    let mut blocks: Vec<TaggedBlock> = file
        .layers()
        .flat_map(|layer| layer.blocks.blocks.iter())
        .filter(|block| {
            matches!(
                &block.key.as_bytes(),
                b"vmsk" | b"vsms" | b"vstk" | b"vscg" | b"vogk" | b"SoCo"
            )
        })
        .cloned()
        .collect();
    blocks.extend(
        file.document_blocks
            .iter()
            .flat_map(|blocks| blocks.blocks.iter())
            .cloned(),
    );
    blocks
}

#[test]
fn reads_generated_shapes_and_paths() {
    for name in GENERATED {
        check_generated(&LayeredFile::<u8>::read(fixture(name)).unwrap());
    }
}

#[test]
fn vector_data_round_trips_byte_for_byte() {
    for name in GENERATED {
        let file = LayeredFile::<u8>::read(fixture(name)).unwrap();
        let back = LayeredFile::<u8>::from_bytes(&file.to_bytes().unwrap()).unwrap();
        assert_eq!(vector_blocks(&back), vector_blocks(&file));
        assert_eq!(
            back.document_paths().unwrap(),
            file.document_paths().unwrap()
        );
        check_generated(&back);
    }
    // The PSB keeps `pths` under the `8B64` signature Photoshop uses there.
    let psb = LayeredFile::<u8>::read(fixture(GENERATED[1])).unwrap();
    let pths = psb
        .document_blocks
        .as_ref()
        .and_then(|blocks| blocks.get(TaggedBlockKey::new(*b"pths")))
        .unwrap();
    assert_eq!(&pths.signature, b"8B64");
}

#[test]
fn photoshop_vector_masks_without_subpaths() {
    for name in [
        "documents/Masks/Masks_8bit.psd",
        "documents/Masks/Masks_8bit.psb",
    ] {
        let file = LayeredFile::<u8>::read(fixture(name)).unwrap();
        let masked: Vec<_> = file
            .layers()
            .filter_map(|layer| {
                let mask = layer.vector_mask().unwrap()?;
                Some((layer.name.as_str(), mask))
            })
            .collect();
        assert_eq!(masked.len(), 4, "{name}");
        for (layer_name, mask) in &masked {
            assert!(mask.path.subpaths().unwrap().is_empty());
            assert_eq!(mask.path.initial_fill_rule(), Some(1));
            assert_eq!(mask.disabled(), *layer_name == "VectorMaskDisabled");
            assert!(!layer(&file, layer_name).is_shape_layer());
        }
    }
}

#[test]
fn photoshop_saved_path_and_unicode_name() {
    let file =
        LayeredFile::<u8>::read(fixture("documents/TextLayers/TextLayers_TextOnPath.psd")).unwrap();
    let paths = file.document_paths().unwrap();
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].resource_id, 2000);
    assert_eq!(paths[0].name, "TextPathLine");
    assert_eq!(paths[0].unicode_name.as_deref(), Some("TextPathLine"));
    let subpaths = paths[0].path.subpaths().unwrap();
    assert_eq!(subpaths.len(), 1);
    assert!(!subpaths[0].record.closed);
    assert_eq!(
        anchors(&file, &subpaths[0]),
        [(40.0, 112.0), (420.0, 112.0)]
    );
}

#[test]
fn a_malformed_vector_block_fails_only_its_view() {
    let mut file = LayeredFile::<u8>::read(fixture(GENERATED[0])).unwrap();
    let id = file
        .layers_with_ids()
        .find(|(_, layer)| layer.name == "Ellipse")
        .map(|(id, _)| id)
        .unwrap();
    let layer = file.layer_mut(id).unwrap();
    let key = TaggedBlockKey::new(*b"vstk");
    layer.blocks.get_mut(key).unwrap().data.truncate(12);
    assert!(layer.vector_blocks().is_err());
    assert!(layer.vector_mask().unwrap().is_some());
    assert!(layer.is_shape_layer());
    let damaged = layer.blocks.get(key).unwrap().data.clone();

    let back = LayeredFile::<u8>::from_bytes(&file.to_bytes().unwrap()).unwrap();
    let ellipse = back.layers().find(|layer| layer.name == "Ellipse").unwrap();
    assert_eq!(ellipse.blocks.get(key).unwrap().data, damaged);
}
