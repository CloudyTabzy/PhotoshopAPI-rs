//! `Vectors/`: shape layers, a vector-masked pixel layer, and document paths
//! on a 64×64 RGB canvas, in 8-bit PSD and PSB.
//!
//! Path records use the specification's 26-byte layout. The mask and
//! subpath fields, the `vogk`/`vstk`/`vscg` descriptors, and four-byte payload
//! padding follow what Photoshop writes. The values are this generator's own
//! and are checked by `crates/psd/tests/vector_shapes.rs`.

use std::path::Path;

use psd::core::image_resources::{RawResourceBlock, ResourceBlock};
use psd::core::{
    AdditionalLayerInfo, BeWriter, ColorMode, DescriptorValue, LayerFlags, PascalString,
    TaggedBlock, TaggedBlockKey, Version,
};
use psd::{ChannelKey, Layer, LayeredFile, Rect};

use crate::builders::{
    descriptor, enumerated, object, padded, rgb, text, unit, versioned_descriptor,
};

const SIZE: u32 = 64;

pub fn generate(dir: &Path) -> psd::core::Result<()> {
    std::fs::create_dir_all(dir)?;
    document(Version::Psd)?.write(dir.join("vector_shapes_8bit.psd"))?;
    document(Version::Psb)?.write(dir.join("vector_shapes_8bit.psb"))?;
    Ok(())
}

/// One Bézier knot in document pixels: preceding control point, anchor,
/// leaving control point.
type Knot = [(f64, f64); 3];

/// A corner knot, whose control points sit on the anchor.
fn corner(x: f64, y: f64) -> Knot {
    [(x, y), (x, y), (x, y)]
}

struct Subpath {
    closed: bool,
    /// −1 none, 0 exclude, 1 combine, 2 subtract, 3 intersect.
    operation: i16,
    origination_index: u32,
    linked: bool,
    knots: Vec<Knot>,
}

fn point(writer: &mut BeWriter, (x, y): (f64, f64)) {
    let fixed =
        |value: f64, size: u32| (value / f64::from(size) * f64::from(1 << 24)).round() as i32;
    writer.i32(fixed(y, SIZE));
    writer.i32(fixed(x, SIZE));
}

/// Path records as Photoshop writes them: the path fill rule record, the
/// initial fill rule record, then each subpath's length record and knots.
fn path_records(writer: &mut BeWriter, initial_fill: u16, subpaths: &[Subpath]) {
    writer.u16(6);
    writer.bytes(&[0; 24]);
    writer.u16(8);
    writer.u16(initial_fill);
    writer.bytes(&[0; 22]);
    for subpath in subpaths {
        writer.u16(if subpath.closed { 0 } else { 3 });
        writer.u16(subpath.knots.len() as u16);
        writer.i16(subpath.operation);
        writer.u16(1);
        writer.u32(0);
        writer.u32(subpath.origination_index);
        writer.bytes(&[0; 10]);
        let selector = match (subpath.closed, subpath.linked) {
            (true, true) => 1,
            (true, false) => 2,
            (false, true) => 4,
            (false, false) => 5,
        };
        for knot in &subpath.knots {
            writer.u16(selector);
            knot.iter().for_each(|&xy| point(writer, xy));
        }
    }
}

fn vector_mask(flags: u32, subpaths: &[Subpath]) -> Vec<u8> {
    let mut writer = BeWriter::new();
    writer.u32(3);
    writer.u32(flags);
    path_records(&mut writer, 0, subpaths);
    padded(writer)
}

fn rectangle(left: f64, top: f64, right: f64, bottom: f64) -> Vec<Knot> {
    vec![
        corner(left, top),
        corner(right, top),
        corner(right, bottom),
        corner(left, bottom),
    ]
}

/// A four-knot ellipse approximation with linked control points.
fn ellipse(left: f64, top: f64, right: f64, bottom: f64) -> Vec<Knot> {
    const K: f64 = 0.552_284_75;
    let (cx, cy) = ((left + right) / 2.0, (top + bottom) / 2.0);
    let (rx, ry) = ((right - left) / 2.0, (bottom - top) / 2.0);
    vec![
        [(cx - rx * K, top), (cx, top), (cx + rx * K, top)],
        [(right, cy - ry * K), (right, cy), (right, cy + ry * K)],
        [(cx + rx * K, bottom), (cx, bottom), (cx - rx * K, bottom)],
        [(left, cy + ry * K), (left, cy), (left, cy - ry * K)],
    ]
}

fn solid_fill(red: f64, green: f64, blue: f64) -> psd::core::Descriptor {
    descriptor("null", vec![("Clr ", rgb(red, green, blue))])
}

/// `SoCo` payload (legacy shape fill).
fn soco(red: f64, green: f64, blue: f64) -> Vec<u8> {
    let mut writer = BeWriter::new();
    versioned_descriptor(&mut writer, &solid_fill(red, green, blue));
    padded(writer)
}

/// `vscg` payload: the content key, then the fill descriptor.
fn vscg(red: f64, green: f64, blue: f64) -> Vec<u8> {
    let mut writer = BeWriter::new();
    writer.bytes(b"SoCo");
    versioned_descriptor(&mut writer, &solid_fill(red, green, blue));
    padded(writer)
}

fn vstk(enabled: bool, fill: bool, width: f64, dashes: &[f64]) -> Vec<u8> {
    let content = object("solidColorLayer", vec![("Clr ", rgb(20.0, 20.0, 20.0))]);
    let mut writer = BeWriter::new();
    versioned_descriptor(
        &mut writer,
        &descriptor(
            "strokeStyle",
            vec![
                ("strokeStyleVersion", DescriptorValue::Integer(2)),
                ("strokeEnabled", DescriptorValue::Boolean(enabled)),
                ("fillEnabled", DescriptorValue::Boolean(fill)),
                ("strokeStyleLineWidth", unit(b"#Pxl", width)),
                ("strokeStyleLineDashOffset", unit(b"#Pnt", 0.0)),
                ("strokeStyleMiterLimit", DescriptorValue::Double(100.0)),
                (
                    "strokeStyleLineCapType",
                    enumerated("strokeStyleLineCapType", "strokeStyleRoundCap"),
                ),
                (
                    "strokeStyleLineJoinType",
                    enumerated("strokeStyleLineJoinType", "strokeStyleRoundJoin"),
                ),
                (
                    "strokeStyleLineAlignment",
                    enumerated("strokeStyleLineAlignment", "strokeStyleAlignCenter"),
                ),
                ("strokeStyleScaleLock", DescriptorValue::Boolean(false)),
                ("strokeStyleStrokeAdjust", DescriptorValue::Boolean(false)),
                (
                    "strokeStyleLineDashSet",
                    DescriptorValue::List(dashes.iter().map(|&dash| unit(b"#Nne", dash)).collect()),
                ),
                ("strokeStyleBlendMode", enumerated("BlnM", "Nrml")),
                ("strokeStyleOpacity", unit(b"#Prc", 100.0)),
                ("strokeStyleContent", content),
                ("strokeStyleResolution", DescriptorValue::Double(72.0)),
            ],
        ),
    );
    padded(writer)
}

/// One `keyDescriptorList` entry.
fn origin(
    kind: i32,
    index: i32,
    bounds: (f64, f64, f64, f64),
    radius: Option<f64>,
) -> DescriptorValue {
    let (left, top, right, bottom) = bounds;
    let mut items = vec![
        ("keyOriginType", DescriptorValue::Integer(kind)),
        ("keyOriginResolution", DescriptorValue::Double(72.0)),
    ];
    if let Some(radius) = radius {
        items.push((
            "keyOriginRRectRadii",
            object(
                "radii",
                vec![
                    ("unitValueQuadVersion", DescriptorValue::Integer(1)),
                    ("topRight", unit(b"#Pxl", radius)),
                    ("topLeft", unit(b"#Pxl", radius)),
                    ("bottomLeft", unit(b"#Pxl", radius)),
                    ("bottomRight", unit(b"#Pxl", radius)),
                ],
            ),
        ));
    }
    items.push((
        "keyOriginShapeBBox",
        object(
            "unitRect",
            vec![
                ("unitValueQuadVersion", DescriptorValue::Integer(1)),
                ("Top ", unit(b"#Pxl", top)),
                ("Left", unit(b"#Pxl", left)),
                ("Btom", unit(b"#Pxl", bottom)),
                ("Rght", unit(b"#Pxl", right)),
            ],
        ),
    ));
    items.push(("keyOriginIndex", DescriptorValue::Integer(index)));
    object("null", items)
}

fn vogk(entries: Vec<DescriptorValue>) -> Vec<u8> {
    let mut writer = BeWriter::new();
    writer.u32(1);
    versioned_descriptor(
        &mut writer,
        &descriptor(
            "null",
            vec![("keyDescriptorList", DescriptorValue::List(entries))],
        ),
    );
    padded(writer)
}

/// A layer whose pixels are `color` wherever `inside` holds, within `bounds`.
fn shape_layer(
    name: &str,
    bounds: Rect,
    color: [u8; 3],
    inside: impl Fn(f64, f64) -> bool,
    blocks: Vec<(&[u8; 4], Vec<u8>)>,
) -> Layer<u8> {
    let mut layer = Layer::<u8>::new_image(name, bounds);
    // Photoshop marks a shape's pixels as derived from its vector data.
    layer.flags =
        LayerFlags::from_bits(LayerFlags::BIT4_USEFUL | LayerFlags::PIXEL_DATA_IRRELEVANT);
    let width = (bounds.right - bounds.left) as usize;
    let height = (bounds.bottom - bounds.top) as usize;
    let alpha: Vec<u8> = (0..width * height)
        .map(|index| {
            let x = f64::from(bounds.left) + (index % width) as f64 + 0.5;
            let y = f64::from(bounds.top) + (index / width) as f64 + 0.5;
            if inside(x, y) {
                255
            } else {
                0
            }
        })
        .collect();
    let image = layer.image_mut().expect("image layer");
    for (channel, value) in color.into_iter().enumerate() {
        image.set_channel(
            ChannelKey::color(channel as u8),
            vec![value; width * height],
        );
    }
    image.set_channel(ChannelKey::ALPHA, alpha);
    for (key, data) in blocks {
        layer
            .blocks
            .push(TaggedBlock::new(TaggedBlockKey::new(*key), data));
    }
    layer
}

fn document(version: Version) -> psd::core::Result<LayeredFile<u8>> {
    let mut file = LayeredFile::<u8>::new(ColorMode::Rgb, SIZE, SIZE)?;
    file.version = version;

    let mut background = Layer::<u8>::new_image("Background", Rect::new(0, 0, 64, 64));
    let image = background.image_mut().expect("image layer");
    for channel in 0..3 {
        image.set_channel(ChannelKey::color(channel), vec![230; 64 * 64]);
    }
    file.add_layer(background);

    // A legacy shape: a `SoCo` fill with a `vmsk` rectangle.
    file.add_layer(shape_layer(
        "Legacy Rectangle",
        Rect::new(4, 4, 20, 28),
        [200, 40, 40],
        |_, _| true,
        vec![
            (b"SoCo", soco(200.0, 40.0, 40.0)),
            (
                b"vmsk",
                vector_mask(
                    0,
                    &[Subpath {
                        closed: true,
                        operation: 1,
                        origination_index: 0,
                        linked: false,
                        knots: rectangle(4.0, 4.0, 28.0, 20.0),
                    }],
                ),
            ),
            (
                b"vogk",
                vogk(vec![origin(1, 0, (4.0, 4.0, 28.0, 20.0), None)]),
            ),
        ],
    ));

    // A CS6-style ellipse with a dashed stroke.
    let (cx, cy, rx, ry) = (46.0, 14.0, 12.0, 10.0);
    file.add_layer(shape_layer(
        "Ellipse",
        Rect::new(4, 34, 24, 58),
        [40, 120, 220],
        move |x, y| ((x - cx) / rx).powi(2) + ((y - cy) / ry).powi(2) <= 1.0,
        vec![
            (b"vscg", vscg(40.0, 120.0, 220.0)),
            (
                b"vsms",
                vector_mask(
                    0,
                    &[Subpath {
                        closed: true,
                        operation: 1,
                        origination_index: 0,
                        linked: true,
                        knots: ellipse(34.0, 4.0, 58.0, 24.0),
                    }],
                ),
            ),
            (b"vstk", vstk(true, true, 2.0, &[4.0, 2.0])),
            (
                b"vogk",
                vogk(vec![origin(5, 0, (34.0, 4.0, 58.0, 24.0), None)]),
            ),
        ],
    ));

    // A compound shape: an outer rectangle minus an inner one, each with its
    // own origination entry.
    file.add_layer(shape_layer(
        "Frame",
        Rect::new(30, 4, 60, 30),
        [60, 160, 60],
        |x, y| !((10.0..24.0).contains(&x) && (36.0..54.0).contains(&y)),
        vec![
            (b"vscg", vscg(60.0, 160.0, 60.0)),
            (
                b"vsms",
                vector_mask(
                    0,
                    &[
                        Subpath {
                            closed: true,
                            operation: 1,
                            origination_index: 0,
                            linked: false,
                            knots: rectangle(4.0, 30.0, 30.0, 60.0),
                        },
                        Subpath {
                            closed: true,
                            operation: 2,
                            origination_index: 1,
                            linked: false,
                            knots: rectangle(10.0, 36.0, 24.0, 54.0),
                        },
                    ],
                ),
            ),
            (b"vstk", vstk(false, true, 1.0, &[])),
            (
                b"vogk",
                vogk(vec![
                    origin(2, 0, (4.0, 30.0, 30.0, 60.0), Some(3.0)),
                    origin(1, 1, (10.0, 36.0, 24.0, 54.0), None),
                ]),
            ),
        ],
    ));

    // An open path drawn with a stroke only.
    file.add_layer(shape_layer(
        "Open Line",
        Rect::new(34, 36, 38, 60),
        [20, 20, 20],
        |_, _| true,
        vec![
            (b"vscg", vscg(255.0, 255.0, 255.0)),
            (
                b"vsms",
                vector_mask(
                    0,
                    &[Subpath {
                        closed: false,
                        operation: -1,
                        origination_index: 0,
                        linked: false,
                        knots: vec![corner(36.0, 36.0), corner(60.0, 36.0)],
                    }],
                ),
            ),
            (b"vstk", vstk(true, false, 4.0, &[])),
        ],
    ));

    // A pixel layer with an inverted, unlinked vector mask: not a shape.
    let mut masked = Layer::<u8>::new_image("Masked Pixels", Rect::new(40, 36, 64, 64));
    let image = masked.image_mut().expect("image layer");
    for channel in 0..3 {
        image.set_channel(ChannelKey::color(channel), vec![90; 24 * 28]);
    }
    masked.blocks.push(TaggedBlock::new(
        TaggedBlockKey::new(*b"vmsk"),
        vector_mask(
            0b011,
            &[Subpath {
                closed: true,
                operation: 1,
                origination_index: 0,
                linked: false,
                knots: vec![corner(50.0, 42.0), corner(62.0, 62.0), corner(38.0, 62.0)],
            }],
        ),
    ));
    file.add_layer(masked);

    // Document paths: the work path and one saved path, plus `pths` with
    // the saved path's Unicode name.
    let path_resource = |id: u16, name: &str, knots: Vec<Knot>| {
        let mut writer = BeWriter::new();
        path_records(
            &mut writer,
            0,
            &[Subpath {
                closed: true,
                operation: -1,
                origination_index: 0,
                linked: false,
                knots,
            }],
        );
        ResourceBlock::Raw(RawResourceBlock {
            id,
            name: PascalString::new(name, 2),
            data: writer.into_inner(),
        })
    };
    file.image_resources
        .push(path_resource(1025, "", rectangle(2.0, 2.0, 62.0, 62.0)));
    file.image_resources.push(path_resource(
        2000,
        "Outline",
        vec![corner(32.0, 2.0), corner(62.0, 62.0), corner(2.0, 62.0)],
    ));
    let mut writer = BeWriter::new();
    versioned_descriptor(
        &mut writer,
        &descriptor(
            "pathsDataClass",
            vec![(
                "pathList",
                DescriptorValue::List(vec![object(
                    "pathInfoClass",
                    vec![("pathUnicodeName", text("Outline ✓"))],
                )]),
            )],
        ),
    );
    let mut pths = TaggedBlock::new(TaggedBlockKey::new(*b"pths"), padded(writer));
    if version == Version::Psb {
        // Photoshop writes `pths` in PSB files with the `8B64` signature and
        // an 8-byte length.
        pths.signature = *b"8B64";
    }
    file.document_blocks = Some(AdditionalLayerInfo { blocks: vec![pths] });
    Ok(file)
}
