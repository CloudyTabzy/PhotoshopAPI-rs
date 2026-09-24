//! `Adjustments/`: one adjustment or fill layer per settings block over a
//! gradient background, in 8-bit PSD and PSB and 16-bit PSD.
//!
//! Every payload follows the layout Photoshop writes, including the `Lvls`
//! and `Crv ` extensions, the `CgEd` companion blocks, and four-byte payload
//! padding. The values are this generator's own and are checked by
//! `crates/psd/tests/adjustments.rs`.

use std::path::Path;

use psd::core::{
    BeWriter, ColorMode, DescriptorValue, LayerFlags, TaggedBlock, TaggedBlockKey, UnicodeString,
    Version,
};
use psd::{BitDepth, ChannelKey, Layer, LayeredFile, Rect};

use crate::builders::{
    descriptor, enumerated, object, padded, rgb, text, unit, versioned_descriptor,
};

const SIZE: u32 = 64;

/// A layer name and its settings blocks, as `(key, payload)` pairs.
type LayerSpec = (&'static str, Vec<(&'static [u8; 4], Vec<u8>)>);

pub fn generate(dir: &Path) -> psd::core::Result<()> {
    std::fs::create_dir_all(dir)?;
    document::<u8>(Version::Psd)?.write(dir.join("adjustment_layers_8bit.psd"))?;
    document::<u8>(Version::Psb)?.write(dir.join("adjustment_layers_8bit.psb"))?;
    document::<u16>(Version::Psd)?.write(dir.join("adjustment_layers_16bit.psd"))?;
    Ok(())
}

fn document<T: BitDepth>(version: Version) -> psd::core::Result<LayeredFile<T>> {
    let mut file = LayeredFile::<T>::new(ColorMode::Rgb, SIZE, SIZE)?;
    file.version = version;
    file.add_layer(background());

    let layers: Vec<LayerSpec> = vec![
        (
            "Brightness/Contrast",
            vec![
                (b"brit", brightness_contrast()),
                (b"CgEd", brightness_generator()),
            ],
        ),
        (
            "Levels",
            vec![
                (b"levl", levels()),
                (
                    b"CgEd",
                    preset("presetKind", "presetFileName", "Custom Levels"),
                ),
            ],
        ),
        (
            "Curves",
            vec![
                (b"curv", curves()),
                (
                    b"CgEd",
                    preset("curvesPresetKind", "curvesPresetFileName", "Custom Curves"),
                ),
            ],
        ),
        ("Exposure", vec![(b"expA", exposure())]),
        ("Vibrance", vec![(b"vibA", vibrance())]),
        ("Hue/Saturation", vec![(b"hue2", hue_saturation())]),
        ("Color Balance", vec![(b"blnc", color_balance())]),
        ("Black & White", vec![(b"blwh", black_and_white())]),
        ("Photo Filter", vec![(b"phfl", photo_filter())]),
        ("Channel Mixer", vec![(b"mixr", channel_mixer())]),
        ("Color Lookup", vec![(b"clrL", color_lookup())]),
        ("Invert", vec![(b"nvrt", Vec::new())]),
        ("Posterize", vec![(b"post", short_setting(6))]),
        ("Threshold", vec![(b"thrs", short_setting(100))]),
        ("Gradient Map", vec![(b"grdm", gradient_map())]),
        ("Selective Color", vec![(b"selc", selective_color())]),
        ("Color Fill", vec![(b"SoCo", solid_color_fill())]),
        ("Gradient Fill", vec![(b"GdFl", gradient_fill())]),
    ];
    for (name, blocks) in layers {
        let mut layer = Layer::<T>::new_image(name, Rect::default());
        // Photoshop marks adjustment and fill pixels as derived data.
        layer.flags =
            LayerFlags::from_bits(LayerFlags::BIT4_USEFUL | LayerFlags::PIXEL_DATA_IRRELEVANT);
        for (key, data) in blocks {
            layer
                .blocks
                .push(TaggedBlock::new(TaggedBlockKey::new(*key), data));
        }
        match name {
            // A clipped adjustment and a masked adjustment, both common in
            // Photoshop documents.
            "Hue/Saturation" => layer.clipping = 1,
            "Curves" => {
                let mask = (0..SIZE * SIZE)
                    .map(|index| T::from_f32((index % SIZE) as f32 / (SIZE - 1) as f32))
                    .collect();
                layer.set_mask(mask, Rect::new(0, 0, SIZE as i32, SIZE as i32))?;
            }
            _ => {}
        }
        file.add_layer(layer);
    }
    Ok(file)
}

fn background<T: BitDepth>() -> Layer<T> {
    let mut layer = Layer::<T>::new_image("Background", Rect::new(0, 0, SIZE as i32, SIZE as i32));
    let image = layer.image_mut().expect("image layer");
    let ramp = |f: fn(u32, u32) -> f32| -> Vec<T> {
        (0..SIZE * SIZE)
            .map(|index| T::from_f32(f(index % SIZE, index / SIZE)))
            .collect()
    };
    image.set_channel(ChannelKey::color(0), ramp(|x, _| x as f32 / 63.0));
    image.set_channel(ChannelKey::color(1), ramp(|_, y| y as f32 / 63.0));
    image.set_channel(ChannelKey::color(2), ramp(|_, _| 0.5));
    layer
}

fn brightness_contrast() -> Vec<u8> {
    // Photoshop CS3+ writes zeros here and keeps the values in `CgEd`.
    let mut writer = BeWriter::new();
    writer.bytes(&[0; 7]);
    padded(writer)
}

fn brightness_generator() -> Vec<u8> {
    let mut writer = BeWriter::new();
    versioned_descriptor(
        &mut writer,
        &descriptor(
            "null",
            vec![
                ("Vrsn", DescriptorValue::Integer(1)),
                ("Brgh", DescriptorValue::Integer(25)),
                ("Cntr", DescriptorValue::Integer(-12)),
                ("means", DescriptorValue::Integer(127)),
                ("Lab ", DescriptorValue::Boolean(false)),
                ("useLegacy", DescriptorValue::Boolean(false)),
                ("Auto", DescriptorValue::Boolean(false)),
            ],
        ),
    );
    padded(writer)
}

fn preset(kind_key: &str, name_key: &str, name: &str) -> Vec<u8> {
    let mut writer = BeWriter::new();
    versioned_descriptor(
        &mut writer,
        &descriptor(
            "null",
            vec![
                ("Vrsn", DescriptorValue::Integer(1)),
                (kind_key, DescriptorValue::Integer(5)),
                (name_key, text(name)),
            ],
        ),
    );
    padded(writer)
}

fn levels() -> Vec<u8> {
    const NEUTRAL: [u16; 5] = [0, 255, 0, 255, 100];
    let mut writer = BeWriter::new();
    writer.u16(2);
    let mut record = |values: [u16; 5]| values.into_iter().for_each(|value| writer.u16(value));
    record([8, 240, 4, 250, 120]); // composite
    record([0, 255, 10, 255, 100]); // red
    record([5, 250, 0, 255, 90]); // green
    for _ in 3..29 {
        record(NEUTRAL);
    }
    writer.bytes(b"Lvls");
    writer.u16(3);
    writer.u16(62);
    for _ in 29..62 {
        NEUTRAL.into_iter().for_each(|value| writer.u16(value));
    }
    padded(writer)
}

const COMPOSITE_CURVE: [(u16, u16); 4] = [(0, 0), (48, 64), (208, 192), (255, 255)];
const RED_CURVE: [(u16, u16); 2] = [(0, 0), (230, 255)];

fn curves() -> Vec<u8> {
    let mut writer = BeWriter::new();
    let points = |writer: &mut BeWriter, points: &[(u16, u16)]| {
        writer.u16(points.len() as u16);
        for &(output, input) in points {
            writer.u16(output);
            writer.u16(input);
        }
    };
    writer.u8(0); // control points, not freehand maps
    writer.u16(1);
    writer.u32(0b11); // composite and red
    points(&mut writer, &COMPOSITE_CURVE);
    points(&mut writer, &RED_CURVE);
    writer.bytes(b"Crv ");
    writer.u16(4);
    writer.u32(2);
    writer.u16(0);
    points(&mut writer, &COMPOSITE_CURVE);
    writer.u16(1);
    points(&mut writer, &RED_CURVE);
    padded(writer)
}

fn exposure() -> Vec<u8> {
    let mut writer = BeWriter::new();
    writer.u16(1);
    writer.f32(0.5);
    writer.f32(-0.031_25);
    writer.f32(1.25);
    padded(writer)
}

fn vibrance() -> Vec<u8> {
    let mut writer = BeWriter::new();
    versioned_descriptor(
        &mut writer,
        &descriptor(
            "null",
            vec![
                ("vibrance", DescriptorValue::Integer(30)),
                ("Strt", DescriptorValue::Integer(-10)),
            ],
        ),
    );
    padded(writer)
}

fn hue_saturation() -> Vec<u8> {
    let mut writer = BeWriter::new();
    writer.u16(2);
    writer.u8(0); // not colorized
    writer.u8(0);
    for value in [0i16, 25, 0, 10, -20, 5] {
        writer.i16(value); // colorization, then master
    }
    let ranges: [[i16; 4]; 6] = [
        [315, 345, 15, 45],
        [15, 45, 75, 105],
        [75, 105, 135, 165],
        [135, 165, 195, 225],
        [195, 225, 255, 285],
        [255, 285, 315, 345],
    ];
    for (index, range) in ranges.into_iter().enumerate() {
        range.into_iter().for_each(|value| writer.i16(value));
        let settings = if index == 0 { [0, 20, 0] } else { [0, 0, 0] };
        settings.into_iter().for_each(|value| writer.i16(value));
    }
    padded(writer)
}

fn color_balance() -> Vec<u8> {
    let mut writer = BeWriter::new();
    for value in [5i16, 0, -5, 10, -10, 20, 0, 0, 15] {
        writer.i16(value);
    }
    writer.u8(1); // preserve luminosity
    padded(writer)
}

fn black_and_white() -> Vec<u8> {
    let mut writer = BeWriter::new();
    versioned_descriptor(
        &mut writer,
        &descriptor(
            "null",
            vec![
                ("Rd  ", DescriptorValue::Integer(40)),
                ("Yllw", DescriptorValue::Integer(60)),
                ("Grn ", DescriptorValue::Integer(40)),
                ("Cyn ", DescriptorValue::Integer(60)),
                ("Bl  ", DescriptorValue::Integer(20)),
                ("Mgnt", DescriptorValue::Integer(80)),
                ("useTint", DescriptorValue::Boolean(true)),
                ("tintColor", rgb(225.0, 211.0, 179.0)),
                ("bwPresetKind", DescriptorValue::Integer(1)),
                ("blackAndWhitePresetFileName", text("")),
            ],
        ),
    );
    padded(writer)
}

fn photo_filter() -> Vec<u8> {
    let mut writer = BeWriter::new();
    writer.u16(2);
    writer.u16(7); // Lab color space: L × 100, signed a and b × 100
    writer.u16(6000);
    writer.i16(2000);
    writer.i16(-3000);
    writer.u16(0);
    writer.u32(40); // density, percent
    writer.u8(1); // preserve luminosity
    padded(writer)
}

fn channel_mixer() -> Vec<u8> {
    let mut writer = BeWriter::new();
    writer.u16(1);
    writer.u16(0); // not monochrome
    for mix in [
        [100i16, 0, 0, 0, 0],
        [0, 100, 0, 0, 0],
        [10, 20, 70, 0, 5],
        [40, 40, 20, 0, 0],
    ] {
        mix.into_iter().for_each(|value| writer.i16(value));
    }
    padded(writer)
}

/// A two-point identity cube LUT.
const IDENTITY_CUBE: &[u8] =
    b"LUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n";

fn color_lookup() -> Vec<u8> {
    let mut writer = BeWriter::new();
    writer.u16(1);
    versioned_descriptor(
        &mut writer,
        &descriptor(
            "null",
            vec![
                ("lookupType", enumerated("colorLookupType", "3DLUT")),
                ("Nm  ", text("Identity.cube")),
                ("Dthr", DescriptorValue::Boolean(true)),
                ("LUTFormat", enumerated("LUTFormatType", "LUTFormatCUBE")),
                ("dataOrder", enumerated("colorLookupOrder", "rgbOrder")),
                ("tableOrder", enumerated("colorLookupOrder", "bgrOrder")),
                (
                    "LUT3DFileData",
                    DescriptorValue::RawData {
                        os_key: *b"tdta",
                        data: IDENTITY_CUBE.to_vec(),
                    },
                ),
                ("LUT3DFileName", text("Identity.cube")),
            ],
        ),
    );
    padded(writer)
}

fn short_setting(value: u16) -> Vec<u8> {
    let mut writer = BeWriter::new();
    writer.u16(value);
    padded(writer)
}

fn gradient_map() -> Vec<u8> {
    let mut writer = BeWriter::new();
    writer.u16(1);
    writer.u8(0); // not reversed
    writer.u8(1); // dithered
    UnicodeString::new("Black, White", 1)
        .and_then(|name| name.write(&mut writer))
        .expect("short name");
    writer.u16(2);
    for (location, level) in [(0u32, 0u16), (4096, 0xffff)] {
        writer.u32(location);
        writer.u32(50);
        writer.u16(0); // RGB color space
        for component in [level, level, level, 0] {
            writer.u16(component);
        }
        writer.u16(0);
    }
    writer.u16(2);
    for location in [0u32, 4096] {
        writer.u32(location);
        writer.u32(50);
        writer.u16(255);
    }
    writer.u16(2); // expansion count
    writer.u16(4096); // interpolation (smoothness 100%)
    writer.u16(32); // noise settings length
    writer.u16(0); // solid gradient
    writer.u32(12_345_678);
    writer.u16(0);
    writer.u16(0);
    writer.u32(2048);
    writer.u16(3);
    for component in [0u16, 0, 0, 0, 0x8000, 0x8000, 0x8000, 0x8000] {
        writer.u16(component);
    }
    writer.u16(0); // unused
    padded(writer)
}

fn selective_color() -> Vec<u8> {
    let mut writer = BeWriter::new();
    writer.u16(1);
    writer.u16(0); // relative
    let plates: [[i16; 4]; 10] = [
        [0, 0, 0, 0],   // reserved
        [10, -5, 0, 0], // reds
        [0; 4],
        [0; 4],
        [0; 4],
        [0; 4],
        [0; 4],
        [0; 4],
        [0, 0, 0, 5],   // neutrals
        [0, 0, 0, -10], // blacks
    ];
    for plate in plates {
        plate.into_iter().for_each(|value| writer.i16(value));
    }
    padded(writer)
}

fn solid_color_fill() -> Vec<u8> {
    let mut writer = BeWriter::new();
    versioned_descriptor(
        &mut writer,
        &descriptor("null", vec![("Clr ", rgb(255.0, 128.0, 0.0))]),
    );
    padded(writer)
}

fn gradient_fill() -> Vec<u8> {
    let color_stop = |color: DescriptorValue, location: i32| {
        object(
            "Clrt",
            vec![
                ("Clr ", color),
                ("Type", enumerated("Clry", "UsrS")),
                ("Lctn", DescriptorValue::Integer(location)),
                ("Mdpn", DescriptorValue::Integer(50)),
            ],
        )
    };
    let transparency_stop = |location: i32| {
        object(
            "TrnS",
            vec![
                ("Opct", unit(b"#Prc", 100.0)),
                ("Lctn", DescriptorValue::Integer(location)),
                ("Mdpn", DescriptorValue::Integer(50)),
            ],
        )
    };
    let gradient = object(
        "Grdn",
        vec![
            ("Nm  ", text("Orange, Blue")),
            ("GrdF", enumerated("GrdF", "CstS")),
            ("Intr", DescriptorValue::Double(4096.0)),
            (
                "Clrs",
                DescriptorValue::List(vec![
                    color_stop(rgb(255.0, 128.0, 0.0), 0),
                    color_stop(rgb(0.0, 64.0, 255.0), 4096),
                ]),
            ),
            (
                "Trns",
                DescriptorValue::List(vec![transparency_stop(0), transparency_stop(4096)]),
            ),
        ],
    );
    let mut writer = BeWriter::new();
    versioned_descriptor(
        &mut writer,
        &descriptor(
            "null",
            vec![
                ("Angl", unit(b"#Ang", 45.0)),
                ("Type", enumerated("GrdT", "Lnr ")),
                ("Grad", gradient),
                ("Algn", DescriptorValue::Boolean(true)),
                ("Scl ", unit(b"#Prc", 100.0)),
                (
                    "Ofst",
                    object(
                        "Pnt ",
                        vec![("Hrzn", unit(b"#Prc", 0.0)), ("Vrtc", unit(b"#Prc", 0.0))],
                    ),
                ),
            ],
        ),
    );
    padded(writer)
}
