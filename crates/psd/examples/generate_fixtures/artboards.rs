//! `Artboards/`: two artboards and a plain group on a 128×64 RGB canvas, in
//! 8-bit PSD and PSB.
//!
//! Artboard groups carry an `artb` descriptor on their group record, and the
//! document carries the `artd` tool settings, laid out as Photoshop writes
//! them. The values are this generator's own and are checked by
//! `crates/psd/tests/artboards.rs`.

use std::path::Path;

use psd::core::{
    AdditionalLayerInfo, BeWriter, ColorMode, DescriptorValue, TaggedBlock, TaggedBlockKey, Version,
};
use psd::{ChannelKey, Layer, LayeredFile, Rect};

use crate::builders::{descriptor, object, padded, rgb, text, versioned_descriptor};

pub fn generate(dir: &Path) -> psd::core::Result<()> {
    std::fs::create_dir_all(dir)?;
    document(Version::Psd)?.write(dir.join("artboards_8bit.psd"))?;
    document(Version::Psb)?.write(dir.join("artboards_8bit.psb"))?;
    Ok(())
}

fn solid(name: &str, bounds: Rect, color: [u8; 3]) -> Layer<u8> {
    let mut layer = Layer::<u8>::new_image(name, bounds);
    let samples = bounds.sample_count();
    let image = layer.image_mut().expect("image layer");
    for (channel, value) in color.into_iter().enumerate() {
        image.set_channel(ChannelKey::color(channel as u8), vec![value; samples]);
    }
    layer
}

fn point(x: f64, y: f64) -> DescriptorValue {
    object(
        "Pnt ",
        vec![
            ("Hrzn", DescriptorValue::Double(x)),
            ("Vrtc", DescriptorValue::Double(y)),
        ],
    )
}

/// An `artb` payload for an artboard spanning `(left, top)`–`(right, bottom)`.
fn artb(
    (left, top, right, bottom): (f64, f64, f64, f64),
    preset: &str,
    background: i32,
    color: (f64, f64, f64),
    guides: Vec<i32>,
) -> Vec<u8> {
    let rect = object(
        "classFloatRect",
        vec![
            ("Top ", DescriptorValue::Double(top)),
            ("Left", DescriptorValue::Double(left)),
            ("Btom", DescriptorValue::Double(bottom)),
            ("Rght", DescriptorValue::Double(right)),
        ],
    );
    let mut writer = BeWriter::new();
    versioned_descriptor(
        &mut writer,
        &descriptor(
            "artboard",
            vec![
                ("artboardRect", rect),
                (
                    "guideIndeces",
                    DescriptorValue::List(
                        guides.into_iter().map(DescriptorValue::Integer).collect(),
                    ),
                ),
                ("artboardPresetName", text(preset)),
                ("Clr ", rgb(color.0, color.1, color.2)),
                (
                    "artboardBackgroundType",
                    DescriptorValue::Integer(background),
                ),
            ],
        ),
    );
    padded(writer)
}

fn artd() -> Vec<u8> {
    let mut writer = BeWriter::new();
    versioned_descriptor(
        &mut writer,
        &descriptor(
            "null",
            vec![
                ("Cnt ", DescriptorValue::Integer(2)),
                ("autoExpandOffset", point(0.0, 0.0)),
                ("origin", point(0.0, 0.0)),
                ("autoExpandEnabled", DescriptorValue::Boolean(true)),
                ("autoNestEnabled", DescriptorValue::Boolean(true)),
                ("autoPositionEnabled", DescriptorValue::Boolean(false)),
                ("shrinkwrapOnSaveEnabled", DescriptorValue::Boolean(true)),
                (
                    "docDefaultNewArtboardBackgroundColor",
                    rgb(255.0, 255.0, 255.0),
                ),
                (
                    "docDefaultNewArtboardBackgroundType",
                    DescriptorValue::Integer(1),
                ),
            ],
        ),
    );
    padded(writer)
}

fn document(version: Version) -> psd::core::Result<LayeredFile<u8>> {
    let mut file = LayeredFile::<u8>::new(ColorMode::Rgb, 128, 64)?;
    file.version = version;

    // A plain group outside the artboards.
    let plain = file.add_layer(Layer::new_group("Plain Group"));
    file.add_layer_to_group(
        plain,
        solid("Loose Pixels", Rect::new(56, 0, 64, 128), [90, 90, 90]),
    )?;

    // A white artboard on the left with a preset name.
    let mut left = Layer::<u8>::new_group("Artboard Left");
    left.blocks.push(TaggedBlock::new(
        TaggedBlockKey::new(*b"artb"),
        artb(
            (0.0, 0.0, 60.0, 50.0),
            "Icon 60",
            1,
            (255.0, 255.0, 255.0),
            vec![],
        ),
    ));
    let left = file.add_layer(left);
    file.add_layer_to_group(
        left,
        solid("Red Square", Rect::new(10, 10, 40, 40), [220, 30, 30]),
    )?;

    // A custom-color artboard on the right with a nested plain group and a
    // guide.
    let mut right = Layer::<u8>::new_group("Artboard Right");
    right.blocks.push(TaggedBlock::new(
        TaggedBlockKey::new(*b"artb"),
        artb(
            (68.0, 0.0, 128.0, 50.0),
            "",
            4,
            (30.0, 60.0, 120.0),
            vec![0],
        ),
    ));
    let right = file.add_layer(right);
    let inner = file.add_layer_to_group(right, Layer::new_group("Inner Group"))?;
    file.add_layer_to_group(
        inner,
        solid("Blue Bar", Rect::new(20, 72, 30, 124), [40, 90, 230]),
    )?;

    let mut settings = TaggedBlock::new(TaggedBlockKey::new(*b"artd"), artd());
    if version == Version::Psb {
        // Photoshop marks PSB blocks whose length field is 8 bytes wide
        // with the `8B64` signature.
        settings.signature = *b"8B64";
    }
    file.document_blocks = Some(AdditionalLayerInfo {
        blocks: vec![settings],
    });
    Ok(file)
}
