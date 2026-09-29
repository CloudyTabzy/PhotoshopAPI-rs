//! Adjustment and fill layers from the generated corpus
//! (`fixtures/generated/Adjustments`, see its README).

use std::path::{Path, PathBuf};

use psd::core::{
    adjustments::{CurveData, CurvePoint, PhotoFilterColor},
    AdjustmentBlock, AdjustmentData, AdjustmentKind, AdjustmentPreset, TaggedBlock, TaggedBlockKey,
};
use psd::{BitDepth, Layer, LayerKind, LayeredFile, Rect, TextLayerBuilder};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/generated/Adjustments")
        .join(name)
}

fn layer<'a, T: BitDepth>(file: &'a LayeredFile<T>, name: &str) -> &'a Layer<T> {
    file.layers()
        .find(|layer| layer.name == name)
        .unwrap_or_else(|| panic!("missing layer {name}"))
}

/// The one settings block that marks the layer, plus any `CgEd` companion.
fn settings<T: BitDepth>(file: &LayeredFile<T>, name: &str) -> Vec<AdjustmentData> {
    layer(file, name)
        .adjustments()
        .unwrap()
        .into_iter()
        .map(|block| block.data)
        .collect()
}

fn adjustment_blocks<T: BitDepth>(file: &LayeredFile<T>) -> Vec<(String, Vec<TaggedBlock>)> {
    file.layers()
        .map(|layer| {
            let blocks = layer
                .blocks
                .blocks
                .iter()
                .filter(|block| AdjustmentKind::from_key(block.key).is_some())
                .cloned()
                .collect();
            (layer.name.clone(), blocks)
        })
        .collect()
}

fn check<T: BitDepth>(file: &LayeredFile<T>) {
    let expected_kinds = [
        ("Brightness/Contrast", AdjustmentKind::BrightnessContrast),
        ("Levels", AdjustmentKind::Levels),
        ("Curves", AdjustmentKind::Curves),
        ("Exposure", AdjustmentKind::Exposure),
        ("Vibrance", AdjustmentKind::Vibrance),
        ("Hue/Saturation", AdjustmentKind::HueSaturation),
        ("Color Balance", AdjustmentKind::ColorBalance),
        ("Black & White", AdjustmentKind::BlackAndWhite),
        ("Photo Filter", AdjustmentKind::PhotoFilter),
        ("Channel Mixer", AdjustmentKind::ChannelMixer),
        ("Color Lookup", AdjustmentKind::ColorLookup),
        ("Invert", AdjustmentKind::Invert),
        ("Posterize", AdjustmentKind::Posterize),
        ("Threshold", AdjustmentKind::Threshold),
        ("Gradient Map", AdjustmentKind::GradientMap),
        ("Selective Color", AdjustmentKind::SelectiveColor),
        ("Color Fill", AdjustmentKind::SolidColor),
        ("Gradient Fill", AdjustmentKind::GradientFill),
    ];
    assert_eq!(file.layer_count(), expected_kinds.len() + 1);
    assert!(!layer(file, "Background").is_adjustment_layer());
    assert!(layer(file, "Background").adjustments().unwrap().is_empty());
    for (name, kind) in expected_kinds {
        let layer = layer(file, name);
        assert!(layer.is_adjustment_layer(), "{name}");
        assert!(matches!(&layer.kind, LayerKind::Adjustment(_)), "{name}");
        let blocks = layer.adjustments().unwrap();
        assert_eq!(blocks[0].kind, kind, "{name}");
        assert_eq!(blocks[0].key, kind.key(), "{name}");
    }
    assert!(layer(file, "Hue/Saturation").is_clipping_mask());
    assert!(layer(file, "Curves").has_mask());

    let blocks = settings(file, "Brightness/Contrast");
    let [AdjustmentData::BrightnessContrast(legacy), AdjustmentData::ContentGenerator(current)] =
        &blocks[..]
    else {
        panic!("brightness/contrast blocks: {blocks:?}")
    };
    assert_eq!((legacy.brightness, legacy.contrast), (0, 0));
    assert_eq!(current.version(), Some(1.0));
    assert_eq!(current.brightness(), Some(25.0));
    assert_eq!(current.contrast(), Some(-12.0));
    assert_eq!(current.mean_value(), Some(127.0));
    assert_eq!(current.use_legacy(), Some(false));

    let blocks = settings(file, "Levels");
    let [AdjustmentData::Levels(levels), AdjustmentData::ContentGenerator(generator)] = &blocks[..]
    else {
        panic!("levels blocks: {blocks:?}")
    };
    assert_eq!(levels.records.len(), 62);
    assert_eq!(levels.extension_version, Some(3));
    let composite = levels.composite();
    assert_eq!((composite.input_floor, composite.input_ceiling), (8, 240));
    assert_eq!((composite.output_floor, composite.output_ceiling), (4, 250));
    assert_eq!(composite.gamma_value(), 1.2);
    assert_eq!(levels.channel(0).unwrap().output_floor, 10);
    assert_eq!(levels.channel(1).unwrap().gamma, 90);
    assert_eq!(levels.trailing_bytes, [0, 0]);
    assert_eq!(
        generator.preset(),
        Some(AdjustmentPreset {
            kind: Some(5.0),
            file_name: Some("Custom Levels")
        })
    );

    let blocks = settings(file, "Curves");
    let [AdjustmentData::Curves(curves), AdjustmentData::ContentGenerator(generator)] = &blocks[..]
    else {
        panic!("curves blocks: {blocks:?}")
    };
    assert!(!curves.is_map);
    let points = |pairs: &[(u16, u16)]| {
        CurveData::Points(
            pairs
                .iter()
                .map(|&(output, input)| CurvePoint { output, input })
                .collect(),
        )
    };
    let composite = points(&[(0, 0), (48, 64), (208, 192), (255, 255)]);
    let red = points(&[(0, 0), (230, 255)]);
    assert_eq!(curves.curves.len(), 2);
    assert_eq!(curves.curves[0].data, composite);
    assert_eq!(curves.curves[1].channel, 1);
    assert_eq!(curves.curves[1].data, red);
    assert_eq!(curves.effective_curves(), &curves.curves[..]);
    assert_eq!(generator.preset().unwrap().file_name, Some("Custom Curves"));

    let [AdjustmentData::Exposure(exposure)] = &settings(file, "Exposure")[..] else {
        panic!("exposure")
    };
    assert_eq!(
        (exposure.exposure, exposure.offset, exposure.gamma),
        (0.5, -0.031_25, 1.25)
    );

    let [AdjustmentData::Vibrance(vibrance)] = &settings(file, "Vibrance")[..] else {
        panic!("vibrance")
    };
    assert_eq!(
        (vibrance.vibrance(), vibrance.saturation()),
        (Some(30.0), Some(-10.0))
    );

    let [AdjustmentData::HueSaturation(hue)] = &settings(file, "Hue/Saturation")[..] else {
        panic!("hue/saturation")
    };
    assert!(!hue.colorize);
    assert_eq!(hue.colorization.saturation, 25);
    assert_eq!(
        (hue.master.hue, hue.master.saturation, hue.master.lightness),
        (10, -20, 5)
    );
    assert_eq!(hue.ranges[0].range, [315, 345, 15, 45]);
    assert_eq!(hue.ranges[0].values.saturation, 20);
    assert_eq!(hue.ranges[5].range, [255, 285, 315, 345]);

    let [AdjustmentData::ColorBalance(balance)] = &settings(file, "Color Balance")[..] else {
        panic!("color balance")
    };
    assert_eq!(balance.shadows.yellow_blue, -5);
    assert_eq!(balance.midtones.yellow_blue, 20);
    assert_eq!(balance.highlights.yellow_blue, 15);
    assert!(balance.preserve_luminosity);

    let [AdjustmentData::BlackAndWhite(black_and_white)] = &settings(file, "Black & White")[..]
    else {
        panic!("black and white")
    };
    assert_eq!(
        black_and_white.weights(),
        [40.0, 60.0, 40.0, 60.0, 20.0, 80.0].map(Some)
    );
    assert_eq!(black_and_white.use_tint(), Some(true));
    let tint = black_and_white.tint_color().unwrap();
    assert_eq!(tint.get("Grn ").unwrap().as_double(), Some(211.0));
    assert_eq!(black_and_white.preset().unwrap().file_name, Some(""));

    let [AdjustmentData::PhotoFilter(filter)] = &settings(file, "Photo Filter")[..] else {
        panic!("photo filter")
    };
    let PhotoFilterColor::Color(color) = filter.color else {
        panic!("version 2 color expected")
    };
    assert_eq!(color.color_space, 7);
    assert_eq!(color.components[0], 6000);
    assert_eq!(color.components[2] as i16, -3000);
    assert_eq!(filter.density, 40);

    let [AdjustmentData::ChannelMixer(mixer)] = &settings(file, "Channel Mixer")[..] else {
        panic!("channel mixer")
    };
    assert!(!mixer.monochrome);
    assert_eq!(mixer.mixes.len(), 4);
    assert_eq!(mixer.mixes[2].sources, [10, 20, 70, 0]);
    assert_eq!(mixer.mixes[2].constant, 5);

    let [AdjustmentData::ColorLookup(lookup)] = &settings(file, "Color Lookup")[..] else {
        panic!("color lookup")
    };
    assert_eq!(lookup.lookup_type().unwrap().as_bytes(), b"3DLUT");
    assert_eq!(lookup.lut_format().unwrap().as_bytes(), b"LUTFormatCUBE");
    assert_eq!(lookup.name(), Some("Identity.cube"));
    assert_eq!(lookup.lut_file_name(), Some("Identity.cube"));
    assert!(lookup
        .lut_file_data()
        .unwrap()
        .starts_with(b"LUT_3D_SIZE 2\n"));

    assert!(matches!(
        &settings(file, "Invert")[..],
        [AdjustmentData::Invert { trailing_bytes }] if trailing_bytes.is_empty()
    ));
    let [AdjustmentData::Posterize(posterize)] = &settings(file, "Posterize")[..] else {
        panic!("posterize")
    };
    assert_eq!(posterize.levels, 6);
    let [AdjustmentData::Threshold(threshold)] = &settings(file, "Threshold")[..] else {
        panic!("threshold")
    };
    assert_eq!(threshold.level, 100);

    let [AdjustmentData::GradientMap(map)] = &settings(file, "Gradient Map")[..] else {
        panic!("gradient map")
    };
    assert_eq!(map.version, 1);
    assert!(map.dithered && !map.reversed);
    assert_eq!(map.method, None);
    assert_eq!(map.name, "Black, White");
    assert_eq!(map.color_stops.len(), 2);
    assert_eq!(map.color_stops[1].location, 4096);
    assert_eq!(
        map.color_stops[1].color.components,
        [0xffff, 0xffff, 0xffff, 0]
    );
    assert_eq!(map.transparency_stops[1].opacity, 255);
    assert_eq!(map.smoothness(), 1.0);
    assert_eq!(map.random_seed, 12_345_678);
    assert_eq!(map.maximum_color, [0x8000; 4]);

    let [AdjustmentData::SelectiveColor(selective)] = &settings(file, "Selective Color")[..] else {
        panic!("selective color")
    };
    assert!(!selective.absolute);
    assert_eq!((selective.reds.cyan, selective.reds.magenta), (10, -5));
    assert_eq!(selective.neutrals.black, 5);
    assert_eq!(selective.blacks.black, -10);

    let [AdjustmentData::Fill(fill)] = &settings(file, "Color Fill")[..] else {
        panic!("color fill")
    };
    let color = fill.color().unwrap();
    assert_eq!(color.get("Grn ").unwrap().as_double(), Some(128.0));

    let [AdjustmentData::Fill(fill)] = &settings(file, "Gradient Fill")[..] else {
        panic!("gradient fill")
    };
    assert_eq!(fill.angle(), Some(45.0));
    assert_eq!(fill.gradient_type().unwrap().as_bytes(), b"Lnr ");
    assert_eq!(fill.scale(), Some(100.0));
    let gradient = fill.gradient().unwrap();
    assert_eq!(gradient.get("Clrs").unwrap().as_list().unwrap().len(), 2);
}

const EIGHT_BIT: [&str; 2] = ["adjustment_layers_8bit.psd", "adjustment_layers_8bit.psb"];

#[test]
fn reads_8bit_adjustment_layers() {
    for name in EIGHT_BIT {
        check(&LayeredFile::<u8>::read(fixture(name)).unwrap());
    }
}

#[test]
fn reads_16bit_adjustment_layers() {
    check(&LayeredFile::<u16>::read(fixture("adjustment_layers_16bit.psd")).unwrap());
}

#[test]
fn adjustment_blocks_round_trip_byte_for_byte() {
    for name in EIGHT_BIT {
        let file = LayeredFile::<u8>::read(fixture(name)).unwrap();
        let back = LayeredFile::<u8>::from_bytes(&file.to_bytes().unwrap()).unwrap();
        assert_eq!(adjustment_blocks(&back), adjustment_blocks(&file));
        check(&back);
    }
}

#[test]
fn adjustment_blocks_round_trip_at_16_bit() {
    let file = LayeredFile::<u16>::read(fixture("adjustment_layers_16bit.psd")).unwrap();
    let back = LayeredFile::<u16>::from_bytes(&file.to_bytes().unwrap()).unwrap();
    assert_eq!(adjustment_blocks(&back), adjustment_blocks(&file));
}

#[test]
fn typed_adjustment_payloads_rebuild_generated_blocks_byte_for_byte() {
    let eight_bit = LayeredFile::<u8>::read(fixture("adjustment_layers_8bit.psd")).unwrap();
    assert_typed_payloads_round_trip(&eight_bit);
    let eight_bit_large = LayeredFile::<u8>::read(fixture("adjustment_layers_8bit.psb")).unwrap();
    assert_typed_payloads_round_trip(&eight_bit_large);
    let sixteen_bit = LayeredFile::<u16>::read(fixture("adjustment_layers_16bit.psd")).unwrap();
    assert_typed_payloads_round_trip(&sixteen_bit);
}

fn assert_typed_payloads_round_trip<T: BitDepth>(file: &LayeredFile<T>) {
    for layer in file.layers() {
        for block in &layer.blocks.blocks {
            if AdjustmentKind::from_key(block.key).is_none() {
                continue;
            }
            let parsed = AdjustmentBlock::read(block)
                .unwrap()
                .expect("recognized adjustment block");
            assert_eq!(
                parsed.to_tagged_block().unwrap(),
                *block,
                "{} / {:?}",
                layer.name,
                block.key
            );
        }
    }
}

#[test]
fn new_adjustment_and_fill_layers_write_with_their_format_bounds() {
    let source = LayeredFile::<u8>::read(fixture("adjustment_layers_8bit.psd")).unwrap();
    let exposure = layer(&source, "Exposure").adjustments().unwrap()[0].clone();
    let fill = layer(&source, "Color Fill").adjustments().unwrap()[0].clone();

    let mut document = LayeredFile::<u8>::new(psd::core::ColorMode::Rgb, 64, 64).unwrap();
    let group_id = document.add_layer(Layer::new_group("Adjustments"));
    let exposure_id = document
        .add_adjustment_layer_to_group(group_id, "New Exposure", &exposure)
        .unwrap();
    let fill_id = document.add_adjustment_layer("New Fill", &fill).unwrap();
    let invert = AdjustmentBlock::new(
        AdjustmentKind::Invert,
        AdjustmentData::Invert {
            trailing_bytes: Vec::new(),
        },
    )
    .unwrap();
    let invert_id = document
        .add_adjustment_layer("New Invert", &invert)
        .unwrap();
    assert_eq!(document.layer(exposure_id).unwrap().bounds, Rect::default());
    assert_eq!(
        document.layer(fill_id).unwrap().bounds,
        Rect::new(0, 0, 64, 64)
    );
    assert!(matches!(
        &document.layer(exposure_id).unwrap().kind,
        LayerKind::Adjustment(_)
    ));
    assert!(document
        .layer(exposure_id)
        .unwrap()
        .flags
        .pixel_data_irrelevant());
    assert_eq!(document.layer(invert_id).unwrap().bounds, Rect::default());

    let bytes = document.to_bytes().unwrap();
    let reread = LayeredFile::<u8>::from_bytes(&bytes).unwrap();
    let exposure_back = layer(&reread, "New Exposure");
    let fill_back = layer(&reread, "New Fill");
    let invert_back = layer(&reread, "New Invert");
    assert!(matches!(&exposure_back.kind, LayerKind::Adjustment(_)));
    assert_eq!(exposure_back.bounds, Rect::default());
    assert_eq!(fill_back.bounds, Rect::new(0, 0, 64, 64));
    assert_eq!(
        invert_back.adjustment(AdjustmentKind::Invert).unwrap(),
        Some(invert)
    );
    assert_eq!(
        exposure_back.adjustment(AdjustmentKind::Exposure).unwrap(),
        Some(exposure)
    );
    assert_eq!(
        fill_back.adjustment(AdjustmentKind::SolidColor).unwrap(),
        Some(fill)
    );
}

#[test]
fn adjustment_edits_replace_in_place_and_preserve_companion_order() {
    let source = LayeredFile::<u8>::read(fixture("adjustment_layers_8bit.psd")).unwrap();
    let blocks = layer(&source, "Brightness/Contrast").adjustments().unwrap();
    let mut target = Layer::<u8>::new_adjustment("Editable", Rect::default());
    target.set_adjustment(&blocks[0]).unwrap();
    target.blocks.push(TaggedBlock::new(
        TaggedBlockKey::new(*b"zzzz"),
        vec![1, 2, 3],
    ));
    target.set_adjustment(&blocks[1]).unwrap();
    target.blocks.get_mut(blocks[0].key).unwrap().signature = *b"8B64";
    let before = target.blocks.clone();
    target.set_adjustment(&blocks[0]).unwrap();
    assert_eq!(target.blocks, before);

    assert_eq!(
        target
            .blocks
            .blocks
            .iter()
            .map(|block| block.key.as_bytes())
            .collect::<Vec<_>>(),
        [*b"brit", *b"CgEd", *b"zzzz"]
    );
    assert_eq!(
        target
            .adjustment(AdjustmentKind::BrightnessContrast)
            .unwrap(),
        Some(blocks[0].clone())
    );
    assert!(target.clear_adjustment(AdjustmentKind::ContentGenerator));
    assert_eq!(
        target
            .blocks
            .blocks
            .iter()
            .map(|block| block.key.as_bytes())
            .collect::<Vec<_>>(),
        [*b"brit", *b"zzzz"]
    );
}

#[test]
fn rejected_adjustment_edits_leave_inconsistent_text_layers_unchanged() {
    let source = LayeredFile::<u8>::read(fixture("adjustment_layers_8bit.psd")).unwrap();
    let fill = layer(&source, "Color Fill").adjustments().unwrap()[0].clone();
    let exposure = layer(&source, "Exposure").adjustments().unwrap()[0].clone();
    let mut text = TextLayerBuilder::new("Text", "Example")
        .build::<u8>()
        .unwrap();
    // Imported raw blocks can be inconsistent with the layer's declared kind.
    text.blocks.push(fill.to_tagged_block().unwrap());
    text.blocks
        .push(TaggedBlock::new(TaggedBlockKey::new(*b"vmsk"), vec![0; 8]));
    let before = text.blocks.clone();
    assert!(text.set_adjustment(&exposure).is_err());
    assert_eq!(text.blocks, before);
    assert!(matches!(text.kind, LayerKind::Text(_)));
}

#[test]
fn a_malformed_block_fails_only_its_view() {
    let mut file = LayeredFile::<u8>::read(fixture("adjustment_layers_8bit.psd")).unwrap();
    let id = file
        .layers_with_ids()
        .find(|(_, layer)| layer.name == "Levels")
        .map(|(id, _)| id)
        .unwrap();
    let layer = file.layer_mut(id).unwrap();
    let levels = layer
        .blocks
        .blocks
        .iter_mut()
        .find(|block| block.key == AdjustmentKind::Levels.key())
        .unwrap();
    levels.data.truncate(20);
    let truncated = levels.data.clone();
    assert!(layer.adjustments().is_err());
    assert!(layer.is_adjustment_layer());

    // The document still writes, and the damaged block is carried unchanged.
    let back = LayeredFile::<u8>::from_bytes(&file.to_bytes().unwrap()).unwrap();
    let block = layer_blocks(&back, "Levels");
    assert_eq!(block, truncated);
}

fn layer_blocks(file: &LayeredFile<u8>, name: &str) -> Vec<u8> {
    layer(file, name)
        .blocks
        .get(AdjustmentKind::Levels.key())
        .unwrap()
        .data
        .clone()
}
