//! One-for-one port of upstream `PhotoshopTest/src/TestTextLayers/*.cpp`.
//!
//! Each module mirrors one upstream file and each test one `TEST_CASE`
//! (named after it). Upstream exposes flat `style_run_*` getters *and* proxy
//! structs; the Rust API has one typed surface (snapshots + mutable handles),
//! so upstream "proxy agrees with flat getter" cases check the typed views
//! against the raw EngineData values instead. Where the Rust type system makes
//! an upstream runtime check unrepresentable (`[f64; 6]` transforms, `usize`
//! indices, `[f64; 4]` colors), the test says so and checks what remains.
//! Roundtrips go through `to_bytes`/`from_bytes` instead of temp files.

use std::path::{Path, PathBuf};

use psd::core::EngineValue;
use psd::{
    AntiAliasMethod, BaselineDirection, BitDepth, CharacterDirection, DiacriticPosition,
    FontBaseline, FontCaps, FontScript, FontType, Justification, KinsokuOrder, Layer, LayeredFile,
    LeadingType, Occurrence, TextBoxBounds, TextLayerBuilder, TextShape, TextWarpRotation,
    TextWarpStyle, TextWritingDirection,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/documents/TextLayers")
        .join(name)
}

fn open(name: &str) -> LayeredFile<u8> {
    LayeredFile::<u8>::read(fixture(name)).unwrap_or_else(|error| panic!("{name}: {error}"))
}

fn roundtrip<T: BitDepth>(file: &LayeredFile<T>) -> LayeredFile<T> {
    LayeredFile::<T>::from_bytes(&file.to_bytes().unwrap()).unwrap()
}

fn with_text<T: BitDepth>(file: &LayeredFile<T>, text: &str) -> usize {
    try_with_text(file, text).unwrap_or_else(|| panic!("no text layer reads {text:?}"))
}

fn try_with_text<T: BitDepth>(file: &LayeredFile<T>, text: &str) -> Option<usize> {
    file.layers()
        .position(|layer| layer.text().as_deref() == Some(text))
}

fn containing<T: BitDepth>(file: &LayeredFile<T>, needle: &str) -> usize {
    file.layers()
        .position(|layer| layer.text().is_some_and(|text| text.contains(needle)))
        .unwrap_or_else(|| panic!("no text layer contains {needle:?}"))
}

fn named<T: BitDepth>(file: &LayeredFile<T>, name: &str) -> usize {
    file.layers()
        .position(|layer| layer.is_text_layer() && layer.name == name)
        .unwrap_or_else(|| panic!("no text layer is named {name:?}"))
}

fn text_layer_count<T: BitDepth>(file: &LayeredFile<T>) -> usize {
    file.layers().filter(|layer| layer.is_text_layer()).count()
}

/// doctest `Approx(expected).epsilon(eps)`.
#[track_caller]
fn assert_close(actual: f64, expected: f64, epsilon: f64) {
    let scale = 1.0 + actual.abs().max(expected.abs());
    assert!(
        (actual - expected).abs() <= epsilon * scale,
        "{actual} is not within {epsilon} of {expected}"
    );
}

#[track_caller]
fn assert_all_close(actual: &[f64], expected: &[f64], epsilon: f64) {
    assert_eq!(actual.len(), expected.len(), "{actual:?} vs {expected:?}");
    for (&a, &e) in actual.iter().zip(expected) {
        assert_close(a, e, epsilon);
    }
}

/// Every `RunLengthArray` in the layer's EngineData (upstream
/// `extract_engine_run_length_arrays`).
fn run_length_arrays<T: BitDepth>(layer: &Layer<T>) -> Vec<Vec<i32>> {
    fn collect(value: &EngineValue, out: &mut Vec<Vec<i32>>) {
        if let Some(items) = value.as_dictionary() {
            for (key, child) in items {
                if key == "RunLengthArray" {
                    if let Some(lengths) = child.as_int32_vector() {
                        out.push(lengths);
                    }
                }
                collect(child, out);
            }
        } else if let Some(items) = value.as_array() {
            for item in items {
                collect(item, out);
            }
        }
    }
    let mut arrays = Vec::new();
    collect(&layer.engine_data().unwrap(), &mut arrays);
    arrays
}

fn procession_values<T: BitDepth>(layer: &Layer<T>) -> Vec<f64> {
    layer
        .engine_data()
        .unwrap()
        .get_path(["EngineDict", "Rendered", "Shapes", "Children"])
        .and_then(EngineValue::as_array)
        .unwrap()
        .iter()
        .filter_map(|child| child.get("Procession")?.as_double())
        .collect()
}

// ===========================================================================
// TestEngineDataStructure.cpp (the factory/mutation cases live in
// crates/psd-core/tests/engine_data.rs)
// ===========================================================================
mod engine_data_structure {
    use super::*;

    #[test]
    fn engine_data_parser_can_parse_and_reserialize_style_run_payload() {
        let file = open("TextLayers_StyleRuns.psd");
        let layer = file.layer(with_text(&file, "Alpha Beta Gamma")).unwrap();
        let root = layer.engine_data().unwrap();
        let runs = root
            .get_path(["EngineDict", "StyleRun", "RunArray"])
            .and_then(EngineValue::as_array)
            .unwrap();
        assert_eq!(runs.len(), 5);
        let lengths = root
            .get_path(["EngineDict", "StyleRun", "RunLengthArray"])
            .unwrap()
            .as_int32_vector()
            .unwrap();
        assert_eq!(lengths, vec![5, 1, 4, 1, 6]);

        let reparsed =
            psd::core::engine_data::parse(&psd::core::engine_data::serialize(&root)).unwrap();
        assert_eq!(
            reparsed
                .get_path(["EngineDict", "StyleRun", "RunLengthArray"])
                .unwrap()
                .as_int32_vector(),
            Some(lengths)
        );
    }
}

// ===========================================================================
// TestTextLayerBuilder.cpp
// ===========================================================================
mod builder {
    use super::*;

    fn document<T: BitDepth>(layers: impl IntoIterator<Item = Layer<T>>) -> LayeredFile<T> {
        let mut file = LayeredFile::<T>::new(psd::core::ColorMode::Rgb, 800, 600).unwrap();
        for layer in layers {
            file.add_layer(layer);
        }
        file
    }

    #[test]
    fn create_produces_a_layer_with_correct_text() {
        let layer = Layer::<u8>::new_text("TestLayer", "Hello World").unwrap();
        assert_eq!(layer.name, "TestLayer");
        assert!(layer.is_text_layer());
        // The builder appends the trailing CR to EngineData only.
        assert_eq!(layer.text().as_deref(), Some("Hello World"));
    }

    #[test]
    fn create_with_default_parameters_produces_valid_single_run() {
        let layer = Layer::<u8>::new_text("Defaults", "ABCDEF").unwrap();
        assert_eq!(layer.style_run_lengths(), Some(vec![7]));
        assert_eq!(layer.style_run_count(), 1);
        let run = layer.style_run(0).unwrap();
        assert_close(run.font_size().unwrap(), 24.0, 0.01);
        assert_all_close(&run.fill_color().unwrap(), &[1.0, 0.0, 0.0, 0.0], 0.001);
    }

    #[test]
    fn create_with_custom_font_size_and_fill_color() {
        let layer = TextLayerBuilder::new("Custom", "Test")
            .font("ArialMT")
            .font_size(36.0)
            .fill_color([1.0, 0.5, 0.3, 0.8])
            .build::<u8>()
            .unwrap();
        let run = layer.style_run(0).unwrap();
        assert_close(run.font_size().unwrap(), 36.0, 0.01);
        assert_all_close(&run.fill_color().unwrap(), &[1.0, 0.5, 0.3, 0.8], 0.001);
    }

    #[test]
    fn create_converts_newlines_to_carriage_returns() {
        let layer = Layer::<u8>::new_text("Multiline", "Line1\nLine2\nLine3").unwrap();
        assert_eq!(layer.text().as_deref(), Some("Line1\rLine2\rLine3"));
    }

    #[test]
    fn create_roundtrips_through_file_write_and_read() {
        let layer = TextLayerBuilder::new("RoundTrip", "Hello World")
            .font_size(28.0)
            .build::<u8>()
            .unwrap();
        let reread = roundtrip(&document([layer]));
        let layer = reread.layer(named(&reread, "RoundTrip")).unwrap();
        assert_eq!(layer.text().as_deref(), Some("Hello World"));
        assert_close(layer.style_run(0).unwrap().font_size().unwrap(), 28.0, 0.01);
    }

    #[test]
    fn create_multiline_roundtrips_correctly() {
        let layer = TextLayerBuilder::new("MultiRT", "First\nSecond\nThird")
            .font_size(20.0)
            .build::<u8>()
            .unwrap();
        let reread = roundtrip(&document([layer]));
        let layer = reread.layer(named(&reread, "MultiRT")).unwrap();
        assert_eq!(layer.text().as_deref(), Some("First\rSecond\rThird"));
    }

    #[test]
    fn create_then_split_style_run_produces_correct_run_lengths() {
        let mut layer = Layer::<u8>::new_text("Split", "Hello Bold World").unwrap();
        assert_eq!(layer.style_run_lengths(), Some(vec![17]));
        layer.split_style_run(0, 6).unwrap();
        assert_eq!(layer.style_run_lengths(), Some(vec![6, 11]));
        layer.split_style_run(1, 4).unwrap();
        assert_eq!(layer.style_run_lengths(), Some(vec![6, 4, 7]));
    }

    #[test]
    fn split_style_run_with_invalid_run_index_returns_false() {
        let mut layer = Layer::<u8>::new_text("Invalid", "Short").unwrap();
        let before = layer.clone();
        assert!(layer.split_style_run(5, 2).is_err());
        assert_eq!(layer, before);
    }

    #[test]
    fn split_style_run_with_zero_offset_returns_false() {
        let mut layer = Layer::<u8>::new_text("ZeroOff", "Hello").unwrap();
        assert!(layer.split_style_run(0, 0).is_err());
    }

    #[test]
    fn style_properties_can_be_set_on_split_runs_and_roundtrip() {
        let mut layer = TextLayerBuilder::new("StyleMut", "Hello Bold World")
            .font_size(28.0)
            .build::<u8>()
            .unwrap();
        layer.split_style_run(0, 6).unwrap();
        layer.split_style_run(1, 4).unwrap();
        layer
            .style_run_mut(1)
            .set_faux_bold(true)
            .unwrap()
            .set_font_size(32.0)
            .unwrap()
            .set_fill_color([1.0, 1.0, 0.0, 0.0])
            .unwrap();
        layer
            .style_run_mut(2)
            .set_faux_italic(true)
            .unwrap()
            .set_underline(true)
            .unwrap();
        assert_eq!(layer.style_run(1).unwrap().faux_bold(), Some(true));
        assert_eq!(layer.style_run(2).unwrap().faux_italic(), Some(true));
        assert_eq!(layer.style_run(2).unwrap().underline(), Some(true));

        let reread = roundtrip(&document([layer]));
        let layer = reread.layer(named(&reread, "StyleMut")).unwrap();
        assert_eq!(layer.style_run_lengths(), Some(vec![6, 4, 7]));
        let bold = layer.style_run(1).unwrap();
        assert_eq!(bold.faux_bold(), Some(true));
        assert_close(bold.font_size().unwrap(), 32.0, 0.01);
        assert_close(bold.fill_color().unwrap()[1], 1.0, 0.001);
        let tail = layer.style_run(2).unwrap();
        assert_eq!(tail.faux_italic(), Some(true));
        assert_eq!(tail.underline(), Some(true));
        assert_eq!(layer.style_run(0).unwrap().faux_bold(), Some(false));
    }

    #[test]
    fn stroke_flag_color_and_outline_width_roundtrip_on_created_layer() {
        let mut layer = TextLayerBuilder::new("Stroke", "Outlined Text")
            .font_size(28.0)
            .build::<u8>()
            .unwrap();
        layer
            .style_run_mut(0)
            .set_stroke_flag(true)
            .unwrap()
            .set_stroke_color([1.0, 0.0, 0.0, 1.0])
            .unwrap()
            .set_outline_width(3.0)
            .unwrap();
        assert_eq!(layer.style_run(0).unwrap().stroke_flag(), Some(true));

        let reread = roundtrip(&document([layer]));
        let run = reread
            .layer(named(&reread, "Stroke"))
            .unwrap()
            .style_run(0)
            .unwrap();
        assert_eq!(run.stroke_flag(), Some(true));
        assert_all_close(&run.stroke_color().unwrap(), &[1.0, 0.0, 0.0, 1.0], 0.001);
        assert_close(run.outline_width().unwrap(), 3.0, 0.01);
    }

    #[test]
    fn fill_first_defaults_to_false_on_created_layers() {
        let layer = Layer::<u8>::new_text("FillFirstCheck", "Test").unwrap();
        assert_eq!(layer.style_run(0).unwrap().fill_first(), Some(false));
    }

    #[test]
    fn font_size_remains_float_after_set_style_run_font_size_with_whole_number() {
        let mut layer = TextLayerBuilder::new("FontFloat", "Test")
            .font_size(28.0)
            .build::<u8>()
            .unwrap();
        layer.style_run_mut(0).set_font_size(20.0).unwrap();
        assert_close(layer.style_run(0).unwrap().font_size().unwrap(), 20.0, 0.01);
        // The float spelling survives: 20.0 is not rewritten as the integer 20.
        let raw = layer
            .style_run(0)
            .and_then(|style| style.property("FontSize").cloned())
            .unwrap();
        assert_eq!(raw.as_number().unwrap().integer, None);

        let reread = roundtrip(&document([layer]));
        let layer = reread.layer(named(&reread, "FontFloat")).unwrap();
        assert_close(layer.style_run(0).unwrap().font_size().unwrap(), 20.0, 0.01);
        assert_eq!(
            layer
                .style_run(0)
                .and_then(|style| style.property("FontSize").cloned())
                .unwrap()
                .as_number()
                .unwrap()
                .integer,
            None
        );
    }

    #[test]
    fn multiple_created_text_layers_coexist_in_a_single_document() {
        let layers = [
            ("Layer1", "First", 24.0),
            ("Layer2", "Second", 32.0),
            ("Layer3", "Third", 16.0),
        ]
        .map(|(name, text, size)| {
            TextLayerBuilder::new(name, text)
                .font_size(size)
                .build::<u8>()
                .unwrap()
        });
        let reread = roundtrip(&document(layers));
        for (name, text, size) in [
            ("Layer1", "First", 24.0),
            ("Layer2", "Second", 32.0),
            ("Layer3", "Third", 16.0),
        ] {
            let layer = reread.layer(named(&reread, name)).unwrap();
            assert_eq!(layer.text().as_deref(), Some(text));
            assert_close(layer.style_run(0).unwrap().font_size().unwrap(), size, 0.01);
        }
    }

    #[test]
    fn complex_multi_run_styled_layer_roundtrips_all_properties() {
        let mut layer = TextLayerBuilder::new("Complex", "Hello Bold World\nUnderline here")
            .font_size(28.0)
            .fill_color([1.0, 0.0, 0.0, 0.0])
            .build::<u8>()
            .unwrap();
        layer.split_style_run(0, 6).unwrap();
        layer.split_style_run(1, 4).unwrap();
        layer.split_style_run(2, 7).unwrap();
        layer.split_style_run(3, 9).unwrap();
        assert_eq!(layer.style_run_lengths(), Some(vec![6, 4, 7, 9, 6]));

        layer.style_run_mut(1).set_faux_bold(true).unwrap();
        layer
            .style_run_mut(2)
            .set_fill_color([1.0, 1.0, 0.0, 0.0])
            .unwrap()
            .set_stroke_flag(true)
            .unwrap()
            .set_stroke_color([1.0, 0.0, 0.0, 1.0])
            .unwrap()
            .set_outline_width(2.0)
            .unwrap();
        layer.style_run_mut(3).set_underline(true).unwrap();
        layer
            .style_run_mut(4)
            .set_faux_italic(true)
            .unwrap()
            .set_font_size(20.0)
            .unwrap();

        let reread = roundtrip(&document([layer]));
        let layer = reread.layer(named(&reread, "Complex")).unwrap();
        assert_eq!(
            layer.text().as_deref(),
            Some("Hello Bold World\rUnderline here")
        );
        assert_eq!(layer.style_run_lengths(), Some(vec![6, 4, 7, 9, 6]));
        let runs = layer.style_runs();
        assert_eq!(runs.len(), 5);
        assert_eq!(runs[0].faux_bold(), Some(false));
        assert_eq!(runs[0].faux_italic(), Some(false));
        assert_eq!(runs[1].faux_bold(), Some(true));
        assert_close(runs[2].fill_color().unwrap()[1], 1.0, 0.001);
        assert_eq!(runs[2].stroke_flag(), Some(true));
        assert_close(runs[2].stroke_color().unwrap()[3], 1.0, 0.001);
        assert_close(runs[2].outline_width().unwrap(), 2.0, 0.01);
        assert_eq!(runs[3].underline(), Some(true));
        assert_eq!(runs[4].faux_italic(), Some(true));
        assert_close(runs[4].font_size().unwrap(), 20.0, 0.01);
    }

    #[test]
    fn create_with_single_character() {
        let layer = Layer::<u8>::new_text("Single", "X").unwrap();
        assert_eq!(layer.text().as_deref(), Some("X"));
        assert_eq!(layer.style_run_lengths(), Some(vec![2]));
    }

    #[test]
    fn create_with_box_width_and_box_height_roundtrips() {
        let layer = TextLayerBuilder::new("BoxSize", "Box text here")
            .position(20.0, 50.0)
            .box_size(500.0, 200.0)
            .build::<u8>()
            .unwrap();
        let reread = roundtrip(&document([layer]));
        let layer = reread.layer(named(&reread, "BoxSize")).unwrap();
        assert_eq!(layer.text().as_deref(), Some("Box text here"));
        assert!(layer.is_box_text());
        assert_eq!(layer.box_width(), Some(500.0));
        assert_eq!(layer.box_height(), Some(200.0));
        assert_eq!(layer.text_position(), Some((20.0, 50.0)));
    }

    #[test]
    fn create_16bit_variant_roundtrips() {
        let mut file = LayeredFile::<u16>::new(psd::core::ColorMode::Rgb, 400, 300).unwrap();
        file.add_layer(Layer::<u16>::new_text("Layer16", "Sixteen Bit").unwrap());
        let reread = roundtrip(&file);
        assert_eq!(
            reread
                .layer(named(&reread, "Layer16"))
                .unwrap()
                .text()
                .as_deref(),
            Some("Sixteen Bit")
        );
    }

    #[test]
    fn create_32bit_variant_roundtrips() {
        let mut file = LayeredFile::<f32>::new(psd::core::ColorMode::Rgb, 400, 300).unwrap();
        file.add_layer(Layer::<f32>::new_text("Layer32", "ThirtyTwo Bit").unwrap());
        let reread = roundtrip(&file);
        assert_eq!(
            reread
                .layer(named(&reread, "Layer32"))
                .unwrap()
                .text()
                .as_deref(),
            Some("ThirtyTwo Bit")
        );
    }

    #[test]
    fn builder_rejects_invalid_parameters() {
        assert!(TextLayerBuilder::new("Bad", "x")
            .font_size(0.0)
            .build::<u8>()
            .is_err());
        assert!(TextLayerBuilder::new("Bad", "x")
            .font_size(f64::NAN)
            .build::<u8>()
            .is_err());
        assert!(TextLayerBuilder::new("Bad", "x")
            .fill_color([1.0, f64::INFINITY, 0.0, 0.0])
            .build::<u8>()
            .is_err());
        assert!(TextLayerBuilder::new("Bad", "x")
            .box_size(-1.0, 10.0)
            .build::<u8>()
            .is_err());
    }
}

// ===========================================================================
// TestTextLayerDetection.cpp
// ===========================================================================
mod detection {
    use super::*;
    use psd::core::{TaggedBlock, TaggedBlockKey};

    const TXT2: TaggedBlockKey = TaggedBlockKey::new(*b"Txt2");

    fn txt2_only_document() -> LayeredFile<u8> {
        let mut layer = Layer::<u8>::new_image("TextLayer", psd::Rect::default());
        layer.blocks.push(TaggedBlock::new(TXT2, Vec::new()));
        let mut file = LayeredFile::<u8>::new(psd::core::ColorMode::Rgb, 1, 1).unwrap();
        file.add_layer(layer);
        file
    }

    #[test]
    fn text_layer_detection_from_txt2_tagged_block() {
        let reread = roundtrip(&txt2_only_document());
        let layer = reread
            .layer(reread.find_layer("TextLayer").unwrap())
            .unwrap();
        assert!(layer.is_text_layer());
        assert!(layer.text_layer().is_some());
    }

    #[test]
    fn text_layer_preserves_txt2_tagged_block_on_write() {
        let reread = roundtrip(&txt2_only_document());
        let layer = reread
            .layer(reread.find_layer("TextLayer").unwrap())
            .unwrap();
        assert!(layer.blocks.get(TXT2).is_some());
        // Txt2 alone is preserved but is not an editable text source.
        assert_eq!(layer.text(), None);
    }
}

// ===========================================================================
// TestTextLayerFixtures.cpp
// ===========================================================================
mod fixtures {
    use super::*;
    use psd::core::TaggedBlockKey;

    const SPECS: [(&str, usize); 9] = [
        ("TextLayers_Basic.psd", 2),
        ("TextLayers_Warp.psd", 2),
        ("TextLayers_Paragraph.psd", 2),
        ("TextLayers_CharacterStyles.psd", 2),
        ("TextLayers_StyleRuns.psd", 2),
        ("TextLayers_Vertical.psd", 2),
        ("TextLayers_Transform.psd", 2),
        ("TextLayers_VerticalBox.psd", 3),
        ("TextLayers_FontFallback.psd", 1),
    ];

    #[test]
    fn text_layer_fixtures_parse_as_text_layer() {
        for (name, minimum) in SPECS {
            assert!(text_layer_count(&open(name)) >= minimum, "{name}");
        }
    }

    #[test]
    fn text_layer_fixtures_preserve_tysh_or_txt2_tagged_blocks_on_write() {
        let mut checked = 0;
        for (name, minimum) in SPECS {
            let reread = roundtrip(&open(name));
            let mut in_file = 0;
            for layer in reread.layers().filter(|layer| layer.is_text_layer()) {
                assert!(
                    layer.blocks.get(TaggedBlockKey::new(*b"TySh")).is_some()
                        || layer.blocks.get(TaggedBlockKey::new(*b"Txt2")).is_some()
                );
                in_file += 1;
            }
            assert!(in_file >= minimum, "{name}");
            checked += in_file;
        }
        assert!(checked >= 16);
    }

    #[test]
    fn text_style_fixtures_preserve_underline_and_non_path_semantics() {
        let file = open("TextLayers_CharacterStyles.psd");
        let layer = file.layer(named(&file, "CharacterStylePrimary")).unwrap();
        assert!(!layer.is_vertical());
        assert!(!layer.has_text_warp());
        assert_eq!(layer.style_run(0).unwrap().underline(), Some(true));

        let file = open("TextLayers_StyleRuns.psd");
        let layer = file.layer(named(&file, "MixedRuns")).unwrap();
        assert!(!layer.is_vertical());
        assert!(!layer.has_text_warp());
        assert!(layer.style_run_count() >= 5);
        assert_eq!(layer.style_run(2).unwrap().underline(), Some(true));
        assert_eq!(layer.style_run(2).unwrap().faux_bold(), Some(true));
        assert_eq!(layer.style_run(4).unwrap().faux_italic(), Some(true));
    }
}

// ===========================================================================
// TestTextLayerMutationCore.cpp
// ===========================================================================
mod mutation_core {
    use super::*;

    #[test]
    fn text_layer_can_read_text_payload_from_fixture_descriptor() {
        let file = open("TextLayers_Basic.psd");
        let layer = file.layer(with_text(&file, "Hello 123")).unwrap();
        assert_eq!(layer.text().as_deref(), Some("Hello 123"));
    }

    #[test]
    fn equal_length_replacement_survives_write_read_roundtrip() {
        let mut file = open("TextLayers_Basic.psd");
        let id = with_text(&file, "Hello 123");
        file.layer_mut(id)
            .unwrap()
            .replace_text_equal_length("Hello", "Hallo")
            .unwrap();
        assert_eq!(file.layer(id).unwrap().text().as_deref(), Some("Hallo 123"));
        assert!(try_with_text(&roundtrip(&file), "Hallo 123").is_some());
    }

    #[test]
    fn rejects_non_equal_length_replacements() {
        let mut file = open("TextLayers_Basic.psd");
        let layer = file.layer_mut(with_text(&file, "Hello 123")).unwrap();
        assert!(layer
            .replace_text_equal_length("Hello", "Greetings")
            .is_err());
        assert!(layer.set_text_equal_length("Longer than nine").is_err());
        assert_eq!(layer.text().as_deref(), Some("Hello 123"));
    }

    #[test]
    fn variable_length_replacement_survives_write_read_roundtrip() {
        let mut file = open("TextLayers_Basic.psd");
        let id = with_text(&file, "Hello 123");
        file.layer_mut(id)
            .unwrap()
            .replace_text("Hello", "Greetings")
            .unwrap();
        assert_eq!(
            file.layer(id).unwrap().text().as_deref(),
            Some("Greetings 123")
        );
        assert!(try_with_text(&roundtrip(&file), "Greetings 123").is_some());
    }

    #[test]
    fn remaps_engine_data_run_lengths_for_style_runs() {
        let mut file = open("TextLayers_StyleRuns.psd");
        let id = with_text(&file, "Alpha Beta Gamma");
        file.layer_mut(id)
            .unwrap()
            .replace_text("Beta", "Betaaaa")
            .unwrap();
        let reread = roundtrip(&file);
        let layer = reread
            .layer(with_text(&reread, "Alpha Betaaaa Gamma"))
            .unwrap();
        let arrays = run_length_arrays(layer);
        assert!(arrays.contains(&vec![5, 1, 7, 1, 6]), "{arrays:?}");
        assert!(arrays.contains(&vec![20]), "{arrays:?}");
    }

    #[test]
    fn legacy_from_to_remap_variable_length_mutation_on_multi_style_file_roundtrips() {
        let mut file = open("TextLayers_StyleRuns.psd");
        let id = with_text(&file, "Alpha Beta Gamma");
        file.layer_mut(id)
            .unwrap()
            .replace_text("Beta", "BetaBetaBeta")
            .unwrap();
        let reread = roundtrip(&file);
        let layer = reread
            .layer(with_text(&reread, "Alpha BetaBetaBeta Gamma"))
            .unwrap();
        assert!(run_length_arrays(layer).contains(&vec![5, 1, 12, 1, 6]));
        assert_eq!(layer.style_run_count(), 5);
    }

    #[test]
    fn legacy_from_to_remap_shrinking_mutation_on_multi_style_file_roundtrips() {
        let mut file = open("TextLayers_StyleRuns.psd");
        let id = with_text(&file, "Alpha Beta Gamma");
        file.layer_mut(id)
            .unwrap()
            .replace_text("Alpha", "A")
            .unwrap();
        let reread = roundtrip(&file);
        let layer = reread.layer(with_text(&reread, "A Beta Gamma")).unwrap();
        assert!(run_length_arrays(layer).contains(&vec![1, 1, 4, 1, 6]));
        assert_eq!(layer.style_run_count(), 5);
    }

    #[test]
    fn legacy_from_to_remap_no_op_on_same_length_mutation_preserves_tysh_descriptor() {
        let mut file = open("TextLayers_StyleRuns.psd");
        let id = with_text(&file, "Alpha Beta Gamma");
        file.layer_mut(id)
            .unwrap()
            .replace_text("Beta", "XXXX")
            .unwrap();
        let reread = roundtrip(&file);
        let layer = reread
            .layer(with_text(&reread, "Alpha XXXX Gamma"))
            .unwrap();
        assert!(run_length_arrays(layer).contains(&vec![5, 1, 4, 1, 6]));
        assert_eq!(layer.style_run_count(), 5);
    }
}

// ===========================================================================
// TestTextLayerMutationFontOrientation.cpp
// ===========================================================================
mod font_orientation {
    use super::*;

    fn style_runs() -> (LayeredFile<u8>, usize) {
        let file = open("TextLayers_StyleRuns.psd");
        let id = with_text(&file, "Alpha Beta Gamma");
        (file, id)
    }

    fn basic() -> (LayeredFile<u8>, usize) {
        let file = open("TextLayers_Basic.psd");
        let id = with_text(&file, "Hello 123");
        (file, id)
    }

    #[test]
    fn font_count_returns_the_number_of_fonts_in_font_set() {
        let (file, id) = style_runs();
        assert!(file.layer(id).unwrap().font_count() >= 3);
    }

    #[test]
    fn font_postscript_name_returns_correct_postscript_names() {
        let (file, id) = style_runs();
        let layer = file.layer(id).unwrap();
        let fonts = layer.fonts();
        assert_eq!(fonts.len(), layer.font_count());
        assert!(fonts.iter().all(|font| !font.postscript_name.is_empty()));
        assert!(fonts
            .iter()
            .any(|font| font.postscript_name == "AdobeInvisFont"));
    }

    #[test]
    fn font_postscript_name_returns_nullopt_for_out_of_range_index() {
        let (file, id) = style_runs();
        assert!(file.layer(id).unwrap().font(100).is_none());
    }

    #[test]
    fn font_script_returns_script_codes_for_each_font() {
        let (file, id) = style_runs();
        assert_eq!(
            file.layer(id).unwrap().font(0).unwrap().script,
            FontScript::Roman
        );
    }

    #[test]
    fn font_type_returns_font_type_codes_for_each_font() {
        let (file, id) = style_runs();
        for font in file.layer(id).unwrap().fonts() {
            assert!(matches!(
                font.font_type,
                FontType::OpenType | FontType::TrueType
            ));
        }
    }

    #[test]
    fn font_synthetic_returns_synthetic_flag_for_each_font() {
        let (file, id) = style_runs();
        assert!(file.layer(id).unwrap().font(0).unwrap().synthetic >= 0);
    }

    #[test]
    fn font_index_from_style_run_font_maps_to_font_postscript_name() {
        let (file, id) = style_runs();
        let layer = file.layer(id).unwrap();
        let index = layer.style_run(0).unwrap().font_index().unwrap();
        let name = layer.font(index).unwrap().postscript_name;
        assert!(matches!(
            name.as_str(),
            "ArialMT" | "MyriadPro-Regular" | "AdobeInvisFont"
        ));
    }

    #[test]
    fn orientation_returns_0_for_horizontal_text() {
        let (file, id) = basic();
        let layer = file.layer(id).unwrap();
        assert_eq!(layer.orientation(), Some(TextWritingDirection::Horizontal));
        assert!(!layer.is_vertical());
    }

    #[test]
    fn orientation_returns_2_for_vertical_text() {
        let file = open("TextLayers_Vertical.psd");
        let layer = file.layer(with_text(&file, "VERTICAL")).unwrap();
        assert_eq!(layer.orientation(), Some(TextWritingDirection::Vertical));
        assert_eq!(TextWritingDirection::Vertical.raw(), 2);
        assert!(layer.is_vertical());
    }

    #[test]
    fn set_orientation_changes_horizontal_to_vertical() {
        let (mut file, id) = basic();
        let layer = file.layer_mut(id).unwrap();
        layer
            .set_orientation(TextWritingDirection::Vertical)
            .unwrap();
        assert_eq!(layer.orientation(), Some(TextWritingDirection::Vertical));
        assert!(layer.is_vertical());
    }

    #[test]
    fn set_orientation_writes_procession_1_for_vertical_text() {
        let (mut file, id) = basic();
        let layer = file.layer_mut(id).unwrap();
        layer
            .set_orientation(TextWritingDirection::Vertical)
            .unwrap();
        assert_eq!(procession_values(layer).first(), Some(&1.0));
    }

    #[test]
    fn set_orientation_changes_vertical_to_horizontal() {
        let mut file = open("TextLayers_Vertical.psd");
        let id = with_text(&file, "VERTICAL");
        let layer = file.layer_mut(id).unwrap();
        layer
            .set_orientation(TextWritingDirection::Horizontal)
            .unwrap();
        assert_eq!(layer.orientation(), Some(TextWritingDirection::Horizontal));
        assert!(!layer.is_vertical());
    }

    #[test]
    fn set_orientation_writes_procession_0_for_horizontal_text() {
        let mut file = open("TextLayers_Vertical.psd");
        let id = with_text(&file, "VERTICAL");
        let layer = file.layer_mut(id).unwrap();
        layer
            .set_orientation(TextWritingDirection::Horizontal)
            .unwrap();
        assert_eq!(procession_values(layer).first(), Some(&0.0));
    }

    #[test]
    fn is_vertical_false_for_control_layer_in_vertical_fixture() {
        let file = open("TextLayers_Vertical.psd");
        let layer = file.layer(with_text(&file, "Control")).unwrap();
        assert_eq!(layer.orientation(), Some(TextWritingDirection::Horizontal));
        assert!(!layer.is_vertical());
    }

    #[test]
    fn used_font_indices_returns_sorted_unique_indices_from_style_runs() {
        let (file, id) = style_runs();
        let layer = file.layer(id).unwrap();
        let indices = layer.used_font_indices();
        assert!(!indices.is_empty());
        assert!(indices.windows(2).all(|pair| pair[1] > pair[0]));
        assert!(indices.iter().all(|&index| index < layer.font_count()));
    }

    #[test]
    fn used_font_names_returns_real_font_names_excluding_adobe_invis_font() {
        let (file, id) = style_runs();
        let names = file.layer(id).unwrap().used_font_names();
        assert!(!names.is_empty());
        assert!(names
            .iter()
            .all(|name| name != "AdobeInvisFont" && !name.is_empty()));
    }

    #[test]
    fn is_sentinel_font_detects_adobe_invis_font() {
        let file = open("TextLayers_FontFallback.psd");
        let layer = file.layer(with_text(&file, "Fallback Probe")).unwrap();
        assert_eq!(layer.font_count(), 2);
        assert!(!layer.is_sentinel_font(0));
        assert!(layer.is_sentinel_font(1));
        assert!(layer.font(1).unwrap().is_sentinel());
    }

    #[test]
    fn used_font_names_from_font_fallback_contains_the_patched_font_name() {
        let file = open("TextLayers_FontFallback.psd");
        let names = file
            .layer(with_text(&file, "Fallback Probe"))
            .unwrap()
            .used_font_names();
        assert!(
            names.iter().any(|name| name == "ZZZMissingFontTok"),
            "{names:?}"
        );
    }

    #[test]
    fn used_font_indices_from_basic_fixture_references_valid_fonts() {
        let (file, id) = basic();
        let layer = file.layer(id).unwrap();
        let indices = layer.used_font_indices();
        assert!(!indices.is_empty());
        for index in indices {
            assert!(!layer.font(index).unwrap().postscript_name.is_empty());
        }
    }

    #[test]
    fn add_font_appends_a_new_font_entry_and_increments_font_count() {
        let (mut file, id) = basic();
        let layer = file.layer_mut(id).unwrap();
        let before = layer.font_count();
        let index = layer
            .add_font("TestNewFont-Bold", FontType::TrueType, FontScript::Roman, 0)
            .unwrap();
        assert_eq!(index, before);
        assert_eq!(layer.font_count(), before + 1);
        let font = layer.font(index).unwrap();
        assert_eq!(font.postscript_name, "TestNewFont-Bold");
        assert_eq!(font.font_type, FontType::TrueType);
        assert_eq!(font.script, FontScript::Roman);
        assert_eq!(font.synthetic, 0);
    }

    #[test]
    fn set_font_postscript_name_renames_an_existing_font() {
        let (mut file, id) = basic();
        let layer = file.layer_mut(id).unwrap();
        let count = layer.font_count();
        layer.rename_font(0, "ReplacedFont-Regular").unwrap();
        assert_eq!(
            layer.font(0).unwrap().postscript_name,
            "ReplacedFont-Regular"
        );
        assert_eq!(layer.font_count(), count);
    }

    #[test]
    fn set_font_postscript_name_out_of_range_returns_false() {
        let (mut file, id) = basic();
        assert!(file
            .layer_mut(id)
            .unwrap()
            .rename_font(9999, "NoSuchFont")
            .is_err());
    }

    #[test]
    fn add_font_then_use_the_new_font_in_a_style_run() {
        let (mut file, id) = basic();
        let layer = file.layer_mut(id).unwrap();
        let index = layer
            .add_font(
                "CustomFont-Italic",
                FontType::OpenType,
                FontScript::Roman,
                0,
            )
            .unwrap();
        layer.style_run_mut(0).set_font_index(index).unwrap();
        assert_eq!(layer.style_run(0).unwrap().font_index(), Some(index));
        assert!(layer
            .used_font_names()
            .contains(&"CustomFont-Italic".to_owned()));
    }

    #[test]
    fn add_font_with_synthetic_parameter() {
        let (mut file, id) = basic();
        let layer = file.layer_mut(id).unwrap();
        let index = layer
            .add_font("SynthFont-Bold", FontType::TrueType, FontScript::Roman, 1)
            .unwrap();
        assert_eq!(layer.font(index).unwrap().synthetic, 1);
    }

    #[test]
    fn find_font_index_returns_correct_index_for_existing_font() {
        let (file, id) = basic();
        let layer = file.layer(id).unwrap();
        let name = layer.font(0).unwrap().postscript_name;
        assert_eq!(layer.font_index(&name), Some(0));
        assert_eq!(layer.font_index("NoSuchFont-12345"), None);
    }

    #[test]
    fn set_style_run_font_by_name_with_existing_font() {
        let (mut file, id) = style_runs();
        let layer = file.layer_mut(id).unwrap();
        let name = layer.font(0).unwrap().postscript_name;
        let count = layer.font_count();
        layer.style_run_mut(0).set_font(&name).map(|_| ()).unwrap();
        assert_eq!(layer.font_count(), count);
        assert_eq!(layer.style_run(0).unwrap().font_index(), Some(0));
    }

    #[test]
    fn set_style_run_font_by_name_adds_new_font_when_not_found() {
        let (mut file, id) = basic();
        let layer = file.layer_mut(id).unwrap();
        let count = layer.font_count();
        layer
            .style_run_mut(0)
            .set_font("BrandNewFont-Regular")
            .map(|_| ())
            .unwrap();
        assert_eq!(layer.font_count(), count + 1);
        let index = layer.style_run(0).unwrap().font_index().unwrap();
        assert_eq!(index, count);
        assert_eq!(
            layer.font(index).unwrap().postscript_name,
            "BrandNewFont-Regular"
        );
    }

    #[test]
    fn set_style_normal_font_by_name_sets_the_normal_sheet_font() {
        let (mut file, id) = basic();
        let layer = file.layer_mut(id).unwrap();
        let count = layer.font_count();
        layer
            .style_normal_mut()
            .set_font("NormalNewFont-Light")
            .map(|_| ())
            .unwrap();
        assert_eq!(layer.font_count(), count + 1);
        let index = layer.style_normal().unwrap().font_index().unwrap();
        assert_eq!(index, count);
        assert_eq!(
            layer.font(index).unwrap().postscript_name,
            "NormalNewFont-Light"
        );
    }

    #[test]
    fn set_style_normal_font_by_name_reuses_existing_font() {
        let (mut file, id) = basic();
        let layer = file.layer_mut(id).unwrap();
        let name = layer.font(0).unwrap().postscript_name;
        let count = layer.font_count();
        layer
            .style_normal_mut()
            .set_font(&name)
            .map(|_| ())
            .unwrap();
        assert_eq!(layer.font_count(), count);
        assert_eq!(layer.style_normal().unwrap().font_index(), Some(0));
    }

    #[test]
    fn add_font_roundtrip_through_file_write_and_read() {
        let (mut file, id) = basic();
        let layer = file.layer_mut(id).unwrap();
        let index = layer
            .add_font(
                "RoundTripFont-Medium",
                FontType::TrueType,
                FontScript::Roman,
                0,
            )
            .unwrap();
        layer.style_run_mut(0).set_font_index(index).unwrap();
        let count = layer.font_count();

        let reread = roundtrip(&file);
        let layer = reread.layer(with_text(&reread, "Hello 123")).unwrap();
        assert_eq!(layer.font_count(), count);
        let font = layer.font(index).unwrap();
        assert_eq!(font.postscript_name, "RoundTripFont-Medium");
        assert_eq!(font.font_type, FontType::TrueType);
        assert_eq!(layer.style_run(0).unwrap().font_index(), Some(index));
    }
}

// ===========================================================================
// TestTextLayerMutationParagraph.cpp
// ===========================================================================
mod paragraph {
    use super::*;

    const NEEDLE: &str = "paragraph text for fixture coverage";

    fn flip<T: PartialEq + Copy>(value: T, a: T, b: T) -> T {
        if value == a {
            b
        } else {
            a
        }
    }

    fn plus(values: &[f64], delta: f64) -> Vec<f64> {
        values.iter().map(|value| value + delta).collect()
    }

    #[test]
    fn mutates_paragraph_run_properties_with_roundtrip() {
        let mut file = open("TextLayers_Paragraph.psd");
        let id = containing(&file, NEEDLE);
        let layer = file.layer_mut(id).unwrap();
        assert!(layer.paragraph_run_count() >= 1);
        let before = layer.paragraph_run(0).unwrap();
        let word = before.word_spacing().unwrap();
        let letter = before.letter_spacing().unwrap();
        let glyph = before.glyph_spacing().unwrap();
        assert_eq!((word.len(), letter.len(), glyph.len()), (3, 3, 3));

        let justification = flip(
            before.justification().unwrap(),
            Justification::Left,
            Justification::Right,
        );
        let first_line = before.first_line_indent().unwrap() + 12.5;
        let start = before.start_indent().unwrap() + 4.25;
        let end = before.end_indent().unwrap() + 2.75;
        let space_before = before.space_before().unwrap() + 3.0;
        let space_after = before.space_after().unwrap() + 6.0;
        let auto_hyphenate = !before.auto_hyphenate().unwrap();
        let word_size = before.hyphenated_word_size().unwrap() + 1;
        let pre = before.pre_hyphen().unwrap() + 1;
        let post = before.post_hyphen().unwrap() + 1;
        let consecutive = before.consecutive_hyphens().unwrap() + 1;
        let zone = before.zone().unwrap() + 5.5;
        let word_after = plus(&word, 0.05);
        let letter_after = plus(&letter, 1.0);
        let glyph_after = plus(&glyph, 0.1);
        let auto_leading = before.auto_leading().unwrap() + 0.2;
        let leading_type = flip(
            before.leading_type().unwrap(),
            LeadingType::BottomToBottom,
            LeadingType::TopToTop,
        );
        let hanging = !before.hanging().unwrap();
        let burasagari = !before.burasagari().unwrap();
        let kinsoku = flip(
            before.kinsoku_order().unwrap(),
            KinsokuOrder::PushInFirst,
            KinsokuOrder::PushOutFirst,
        );
        let every_line = !before.every_line_composer().unwrap();

        layer
            .paragraph_run_mut(0)
            .set_justification(justification)
            .unwrap()
            .set_first_line_indent(first_line)
            .unwrap()
            .set_start_indent(start)
            .unwrap()
            .set_end_indent(end)
            .unwrap()
            .set_space_before(space_before)
            .unwrap()
            .set_space_after(space_after)
            .unwrap()
            .set_auto_hyphenate(auto_hyphenate)
            .unwrap()
            .set_hyphenated_word_size(word_size)
            .unwrap()
            .set_pre_hyphen(pre)
            .unwrap()
            .set_post_hyphen(post)
            .unwrap()
            .set_consecutive_hyphens(consecutive)
            .unwrap()
            .set_zone(zone)
            .unwrap()
            .set_word_spacing(&word_after)
            .unwrap()
            .set_letter_spacing(&letter_after)
            .unwrap()
            .set_glyph_spacing(&glyph_after)
            .unwrap()
            .set_auto_leading(auto_leading)
            .unwrap()
            .set_leading_type(leading_type)
            .unwrap()
            .set_hanging(hanging)
            .unwrap()
            .set_burasagari(burasagari)
            .unwrap()
            .set_kinsoku_order(kinsoku)
            .unwrap()
            .set_every_line_composer(every_line)
            .unwrap();

        let unchanged = layer.clone();
        let mut out_of_range = layer.paragraph_run_mut(200);
        assert!(out_of_range.set_justification(justification).is_err());
        assert!(out_of_range.set_first_line_indent(first_line).is_err());
        assert!(out_of_range.set_start_indent(start).is_err());
        assert!(out_of_range.set_end_indent(end).is_err());
        assert!(out_of_range.set_space_before(space_before).is_err());
        assert!(out_of_range.set_space_after(space_after).is_err());
        assert!(out_of_range.set_auto_hyphenate(auto_hyphenate).is_err());
        assert!(out_of_range.set_hyphenated_word_size(word_size).is_err());
        assert!(out_of_range.set_pre_hyphen(pre).is_err());
        assert!(out_of_range.set_post_hyphen(post).is_err());
        assert!(out_of_range.set_consecutive_hyphens(consecutive).is_err());
        assert!(out_of_range.set_zone(zone).is_err());
        assert!(out_of_range.set_word_spacing(&word_after).is_err());
        assert!(out_of_range.set_letter_spacing(&letter_after).is_err());
        assert!(out_of_range.set_glyph_spacing(&glyph_after).is_err());
        assert!(out_of_range.set_auto_leading(auto_leading).is_err());
        assert!(out_of_range.set_leading_type(leading_type).is_err());
        assert!(out_of_range.set_hanging(hanging).is_err());
        assert!(out_of_range.set_burasagari(burasagari).is_err());
        assert!(out_of_range.set_kinsoku_order(kinsoku).is_err());
        assert!(out_of_range.set_every_line_composer(every_line).is_err());
        let mut first = layer.paragraph_run_mut(0);
        assert!(first.set_space_before(f64::INFINITY).is_err());
        assert!(first.set_word_spacing(&[]).is_err());
        assert!(first.set_word_spacing(&[1.0, f64::INFINITY, 2.0]).is_err());
        assert_eq!(
            layer, &unchanged,
            "rejected paragraph edits must not mutate"
        );

        let reread = roundtrip(&file);
        let after = reread
            .layer(containing(&reread, NEEDLE))
            .unwrap()
            .paragraph_run(0)
            .unwrap();
        assert_eq!(after.justification(), Some(justification));
        assert_close(after.first_line_indent().unwrap(), first_line, 1e-4);
        assert_close(after.start_indent().unwrap(), start, 1e-4);
        assert_close(after.end_indent().unwrap(), end, 1e-4);
        assert_close(after.space_before().unwrap(), space_before, 1e-4);
        assert_close(after.space_after().unwrap(), space_after, 1e-4);
        assert_eq!(after.auto_hyphenate(), Some(auto_hyphenate));
        assert_eq!(after.hyphenated_word_size(), Some(word_size));
        assert_eq!(after.pre_hyphen(), Some(pre));
        assert_eq!(after.post_hyphen(), Some(post));
        assert_eq!(after.consecutive_hyphens(), Some(consecutive));
        assert_close(after.zone().unwrap(), zone, 1e-4);
        assert_all_close(&after.word_spacing().unwrap(), &word_after, 1e-4);
        assert_all_close(&after.letter_spacing().unwrap(), &letter_after, 1e-4);
        assert_all_close(&after.glyph_spacing().unwrap(), &glyph_after, 1e-4);
        assert_close(after.auto_leading().unwrap(), auto_leading, 1e-4);
        assert_eq!(after.leading_type(), Some(leading_type));
        assert_eq!(after.hanging(), Some(hanging));
        assert_eq!(after.burasagari(), Some(burasagari));
        assert_eq!(after.kinsoku_order(), Some(kinsoku));
        assert_eq!(after.every_line_composer(), Some(every_line));
    }

    #[test]
    fn mutates_normal_paragraph_sheet_properties_with_roundtrip() {
        let mut file = open("TextLayers_Paragraph.psd");
        let id = containing(&file, NEEDLE);
        let layer = file.layer_mut(id).unwrap();
        assert!(layer.paragraph_sheet_count() >= 1);
        let sheet_index = layer.paragraph_normal_sheet_index().unwrap();
        let run_before = layer.paragraph_run(0).unwrap();
        let before = layer.paragraph_normal().unwrap();

        let justification = flip(
            before.justification().unwrap(),
            Justification::Left,
            Justification::Center,
        );
        let first_line = before.first_line_indent().unwrap() + 7.5;
        let space_before = before.space_before().unwrap() + 2.0;
        let word_after = plus(&before.word_spacing().unwrap(), 0.05);
        let letter_after = plus(&before.letter_spacing().unwrap(), 1.0);
        let glyph_after = plus(&before.glyph_spacing().unwrap(), 0.1);
        let auto_leading = before.auto_leading().unwrap() + 0.1;
        let leading_type = flip(
            before.leading_type().unwrap(),
            LeadingType::BottomToBottom,
            LeadingType::TopToTop,
        );
        let hanging = !before.hanging().unwrap();
        let every_line = !before.every_line_composer().unwrap();

        layer.set_paragraph_normal_sheet_index(sheet_index).unwrap();
        layer
            .paragraph_normal_mut()
            .set_justification(justification)
            .unwrap()
            .set_first_line_indent(first_line)
            .unwrap()
            .set_space_before(space_before)
            .unwrap()
            .set_word_spacing(&word_after)
            .unwrap()
            .set_letter_spacing(&letter_after)
            .unwrap()
            .set_glyph_spacing(&glyph_after)
            .unwrap()
            .set_auto_leading(auto_leading)
            .unwrap()
            .set_leading_type(leading_type)
            .unwrap()
            .set_hanging(hanging)
            .unwrap()
            .set_every_line_composer(every_line)
            .unwrap();

        // `usize` makes upstream's negative sheet index unrepresentable.
        assert!(layer.set_paragraph_normal_sheet_index(200).is_err());
        let mut normal = layer.paragraph_normal_mut();
        assert!(normal.set_space_before(f64::INFINITY).is_err());
        assert!(normal.set_word_spacing(&[]).is_err());
        assert!(normal.set_word_spacing(&[1.0, f64::INFINITY, 2.0]).is_err());
        assert!(normal.set_letter_spacing(&[]).is_err());
        assert!(normal.set_glyph_spacing(&[]).is_err());

        let reread = roundtrip(&file);
        let layer = reread.layer(containing(&reread, NEEDLE)).unwrap();
        assert_eq!(layer.paragraph_normal_sheet_index(), Some(sheet_index));
        let after = layer.paragraph_normal().unwrap();
        assert_eq!(after.justification(), Some(justification));
        assert_close(after.first_line_indent().unwrap(), first_line, 1e-4);
        assert_close(after.space_before().unwrap(), space_before, 1e-4);
        assert_all_close(&after.word_spacing().unwrap(), &word_after, 1e-4);
        assert_all_close(&after.letter_spacing().unwrap(), &letter_after, 1e-4);
        assert_all_close(&after.glyph_spacing().unwrap(), &glyph_after, 1e-4);
        assert_close(after.auto_leading().unwrap(), auto_leading, 1e-4);
        assert_eq!(after.leading_type(), Some(leading_type));
        assert_eq!(after.hanging(), Some(hanging));
        assert_eq!(after.every_line_composer(), Some(every_line));
        // Normal-sheet edits must not rewrite explicit run values.
        assert_eq!(layer.paragraph_run(0).unwrap(), run_before);
    }
}

// ===========================================================================
// TestTextLayerMutationStyle.cpp
// ===========================================================================
mod style {
    use super::*;

    fn nudge(color: &[f64], delta: f64) -> [f64; 4] {
        [
            color[0],
            (color[1] + delta).clamp(0.0, 1.0),
            (color[2] + delta).clamp(0.0, 1.0),
            (color[3] + delta).clamp(0.0, 1.0),
        ]
    }

    #[test]
    fn mutates_style_run_font_size_and_fill_color_with_roundtrip() {
        let mut file = open("TextLayers_StyleRuns.psd");
        let id = with_text(&file, "Alpha Beta Gamma");
        let layer = file.layer_mut(id).unwrap();
        assert_eq!(layer.style_run_count(), 5);
        let run = layer.style_run(2).unwrap();
        assert_close(run.font_size().unwrap(), 44.0, 1e-4);
        assert_eq!(run.fill_color().unwrap().len(), 4);

        layer
            .style_run_mut(2)
            .set_font_size(42.5)
            .unwrap()
            .set_fill_color([1.0, 0.2, 0.3, 0.4])
            .unwrap();
        assert!(layer.style_run_mut(20).set_font_size(55.0).is_err());
        // A three-component color is a compile error with `[f64; 4]`; the
        // remaining runtime check is finiteness.
        assert!(layer
            .style_run_mut(2)
            .set_fill_color([1.0, f64::NAN, 0.3, 0.4])
            .is_err());

        let reread = roundtrip(&file);
        let run = reread
            .layer(with_text(&reread, "Alpha Beta Gamma"))
            .unwrap()
            .style_run(2)
            .unwrap();
        assert_close(run.font_size().unwrap(), 42.5, 1e-4);
        assert_all_close(&run.fill_color().unwrap(), &[1.0, 0.2, 0.3, 0.4], 1e-4);
    }

    #[test]
    fn mutates_style_run_character_properties_with_roundtrip() {
        let mut file = open("TextLayers_CharacterStyles.psd");
        let id = named(&file, "CharacterStylePrimary");
        let layer = file.layer_mut(id).unwrap();
        assert!(layer.style_run_count() >= 1);
        let before = layer.style_run(0).unwrap();
        let normal_font_size = layer.style_normal().unwrap().font_size().unwrap();

        let font = if before.font_index().unwrap() == 0 {
            1
        } else {
            0
        };
        let font_size = before.font_size().unwrap() + 2.0;
        let faux_bold = !before.faux_bold().unwrap();
        let faux_italic = !before.faux_italic().unwrap();
        let horizontal_scale = before.horizontal_scale().unwrap() + 0.05;
        let vertical_scale = before.vertical_scale().unwrap() + 0.05;
        let tracking = before.tracking().unwrap() + 8;
        let auto_kerning = !before.auto_kerning().unwrap();
        let baseline_shift = before.baseline_shift().unwrap() + 1.0;
        let leading = before.leading().unwrap() + 1.25;
        let auto_leading = !before.auto_leading().unwrap();
        let kerning = before.kerning().unwrap() + 12;
        let font_caps = if before.font_caps() == Some(FontCaps::Normal) {
            FontCaps::AllCaps
        } else {
            FontCaps::Normal
        };
        let no_break = !before.no_break().unwrap();
        let font_baseline = if before.font_baseline() == Some(FontBaseline::Normal) {
            FontBaseline::Superscript
        } else {
            FontBaseline::Normal
        };
        let language = before.language().unwrap() + 1;
        // Absent optional properties are inserted by the style setters.
        let character_direction = match before.character_direction() {
            Some(CharacterDirection::Default) | None => CharacterDirection::LeftToRight,
            Some(_) => CharacterDirection::Default,
        };
        let baseline_direction = if before.baseline_direction() == Some(BaselineDirection::Default)
        {
            BaselineDirection::Vertical
        } else {
            BaselineDirection::Default
        };
        let tsume = before.tsume().unwrap() + 5.0;
        let kashida = before.kashida().unwrap() + 1;
        let diacritic = match before.diacritic_position() {
            Some(DiacriticPosition::OpenType) | None => DiacriticPosition::Loose,
            Some(_) => DiacriticPosition::OpenType,
        };
        let ligatures = !before.ligatures().unwrap();
        let discretionary = !before.discretionary_ligatures().unwrap();
        let underline = !before.underline().unwrap();
        let strikethrough = !before.strikethrough().unwrap();
        // Absent flags are inserted: stroke/fill flags as true, FillFirst as false.
        let stroke_flag = !before.stroke_flag().unwrap_or(false);
        let fill_flag = !before.fill_flag().unwrap_or(false);
        let fill_first = !before.fill_first().unwrap_or(true);
        let outline_width = before.outline_width().map_or(2.0, |value| value + 1.0);
        let fill_color = nudge(&before.fill_color().unwrap(), 0.05);
        let stroke_color = before.stroke_color().map(|color| nudge(&color, 0.05));

        {
            let mut run = layer.style_run_mut(0);
            run.set_font_index(font).unwrap();
            run.set_font_size(font_size).unwrap();
            run.set_faux_bold(faux_bold).unwrap();
            run.set_faux_italic(faux_italic).unwrap();
            run.set_horizontal_scale(horizontal_scale).unwrap();
            run.set_vertical_scale(vertical_scale).unwrap();
            run.set_tracking(tracking).unwrap();
            run.set_auto_kerning(auto_kerning).unwrap();
            run.set_baseline_shift(baseline_shift).unwrap();
            run.set_leading(leading).unwrap();
            run.set_auto_leading(auto_leading).unwrap();
            run.set_kerning(kerning).unwrap();
            run.set_font_caps(font_caps).unwrap();
            run.set_no_break(no_break).unwrap();
            run.set_font_baseline(font_baseline).unwrap();
            run.set_language(language).unwrap();
            run.set_character_direction(character_direction).unwrap();
            run.set_baseline_direction(baseline_direction).unwrap();
            run.set_tsume(tsume).unwrap();
            run.set_kashida(kashida).unwrap();
            run.set_diacritic_position(diacritic).unwrap();
            run.set_ligatures(ligatures).unwrap();
            run.set_discretionary_ligatures(discretionary).unwrap();
            run.set_underline(underline).unwrap();
            run.set_strikethrough(strikethrough).unwrap();
            run.set_stroke_flag(stroke_flag).unwrap();
            run.set_fill_flag(fill_flag).unwrap();
            run.set_fill_first(fill_first).unwrap();
            run.set_outline_width(outline_width).unwrap();
            run.set_fill_color(fill_color).unwrap();
            match stroke_color {
                Some(color) => {
                    run.set_stroke_color(color).unwrap();
                }
                // Colors are never inserted: a missing StrokeColor is an error.
                None => assert!(run.set_stroke_color([1.0, 0.1, 0.2, 0.3]).is_err()),
            }
        }

        let unchanged = layer.clone();
        let mut missing = layer.style_run_mut(200);
        assert!(missing.set_font_index(font).is_err());
        assert!(missing.set_leading(leading).is_err());
        assert!(missing.set_auto_leading(auto_leading).is_err());
        assert!(missing.set_kerning(kerning).is_err());
        assert!(missing.set_font_baseline(font_baseline).is_err());
        assert!(missing.set_language(language).is_err());
        assert!(missing.set_baseline_direction(baseline_direction).is_err());
        assert!(missing.set_tsume(tsume).is_err());
        assert!(missing.set_kashida(kashida).is_err());
        assert!(missing.set_stroke_flag(true).is_err());
        assert!(missing.set_fill_flag(true).is_err());
        assert!(missing.set_fill_first(true).is_err());
        assert!(missing.set_outline_width(1.0).is_err());
        let mut first = layer.style_run_mut(0);
        assert!(first.set_font_size(f64::INFINITY).is_err());
        assert!(first.set_font_size(0.0).is_err());
        assert!(first
            .set_fill_color([1.0, 0.0, f64::INFINITY, 0.0])
            .is_err());
        assert_eq!(layer, &unchanged, "rejected style edits must not mutate");

        let reread = roundtrip(&file);
        let layer = reread
            .layer(named(&reread, "CharacterStylePrimary"))
            .unwrap();
        let after = layer.style_run(0).unwrap();
        assert_eq!(after.font_index(), Some(font));
        assert_close(after.font_size().unwrap(), font_size, 1e-4);
        assert_eq!(after.faux_bold(), Some(faux_bold));
        assert_eq!(after.faux_italic(), Some(faux_italic));
        assert_close(after.horizontal_scale().unwrap(), horizontal_scale, 1e-4);
        assert_close(after.vertical_scale().unwrap(), vertical_scale, 1e-4);
        assert_eq!(after.tracking(), Some(tracking));
        assert_eq!(after.auto_kerning(), Some(auto_kerning));
        assert_close(after.baseline_shift().unwrap(), baseline_shift, 1e-4);
        assert_close(after.leading().unwrap(), leading, 1e-4);
        assert_eq!(after.auto_leading(), Some(auto_leading));
        assert_eq!(after.kerning(), Some(kerning));
        assert_eq!(after.font_caps(), Some(font_caps));
        assert_eq!(after.no_break(), Some(no_break));
        assert_eq!(after.font_baseline(), Some(font_baseline));
        assert_eq!(after.language(), Some(language));
        assert_eq!(after.character_direction(), Some(character_direction));
        assert_eq!(after.baseline_direction(), Some(baseline_direction));
        assert_close(after.tsume().unwrap(), tsume, 1e-4);
        assert_eq!(after.kashida(), Some(kashida));
        assert_eq!(after.diacritic_position(), Some(diacritic));
        assert_eq!(after.ligatures(), Some(ligatures));
        assert_eq!(after.discretionary_ligatures(), Some(discretionary));
        assert_eq!(after.underline(), Some(underline));
        assert_eq!(after.strikethrough(), Some(strikethrough));
        assert_eq!(after.stroke_flag(), Some(stroke_flag));
        assert_eq!(after.fill_flag(), Some(fill_flag));
        assert_eq!(after.fill_first(), Some(fill_first));
        assert_close(after.outline_width().unwrap(), outline_width, 1e-4);
        assert_all_close(&after.fill_color().unwrap(), &fill_color, 1e-4);
        match stroke_color {
            Some(color) => assert_all_close(&after.stroke_color().unwrap(), &color, 1e-4),
            None => assert_eq!(after.stroke_color(), None),
        }
        // Style-run edits must not rewrite normal style-sheet defaults.
        assert_close(
            layer.style_normal().unwrap().font_size().unwrap(),
            normal_font_size,
            1e-4,
        );
    }

    #[test]
    fn mutates_normal_style_sheet_properties_with_roundtrip() {
        let mut file = open("TextLayers_CharacterStyles.psd");
        let id = named(&file, "CharacterStylePrimary");
        let layer = file.layer_mut(id).unwrap();
        assert!(layer.style_sheet_count() >= 1);
        let sheet_index = layer.style_normal_sheet_index().unwrap();
        let run_font_size = layer.style_run(0).unwrap().font_size().unwrap();
        let before = layer.style_normal().unwrap();

        let font = if before.font_index().unwrap() == 0 {
            1
        } else {
            0
        };
        let font_size = before.font_size().unwrap() + 3.0;
        let leading = before.leading().unwrap() + 2.0;
        let auto_leading = !before.auto_leading().unwrap();
        let kerning = before.kerning().unwrap() + 12;
        let faux_bold = !before.faux_bold().unwrap();
        let faux_italic = !before.faux_italic().unwrap();
        let horizontal_scale = before.horizontal_scale().unwrap() + 0.05;
        let vertical_scale = before.vertical_scale().unwrap() + 0.05;
        let tracking = before.tracking().unwrap() + 12;
        let auto_kerning = !before.auto_kerning().unwrap();
        let baseline_shift = before.baseline_shift().unwrap() + 2.5;
        let font_caps = if before.font_caps() == Some(FontCaps::Normal) {
            FontCaps::AllCaps
        } else {
            FontCaps::Normal
        };
        let font_baseline = if before.font_baseline() == Some(FontBaseline::Normal) {
            FontBaseline::Superscript
        } else {
            FontBaseline::Normal
        };
        let no_break = !before.no_break().unwrap();
        let language = before.language().unwrap() + 1;
        let character_direction =
            if before.character_direction() == Some(CharacterDirection::Default) {
                CharacterDirection::LeftToRight
            } else {
                CharacterDirection::Default
            };
        let baseline_direction = if before.baseline_direction() == Some(BaselineDirection::Default)
        {
            BaselineDirection::Vertical
        } else {
            BaselineDirection::Default
        };
        let tsume = before.tsume().unwrap() + 5.0;
        let kashida = before.kashida().unwrap() + 1;
        let diacritic = if before.diacritic_position() == Some(DiacriticPosition::OpenType) {
            DiacriticPosition::Loose
        } else {
            DiacriticPosition::OpenType
        };
        let ligatures = !before.ligatures().unwrap();
        let discretionary = !before.discretionary_ligatures().unwrap();
        let underline = !before.underline().unwrap();
        let strikethrough = !before.strikethrough().unwrap();
        let stroke_flag = !before.stroke_flag().unwrap();
        let fill_flag = !before.fill_flag().unwrap();
        let fill_first = !before.fill_first().unwrap();
        let outline_width = before.outline_width().unwrap() + 1.0;
        let fill_color = nudge(&before.fill_color().unwrap(), 0.1);
        let stroke_color = nudge(&before.stroke_color().unwrap(), 0.1);

        layer.set_style_normal_sheet_index(sheet_index).unwrap();
        {
            let mut normal = layer.style_normal_mut();
            normal.set_font_index(font).unwrap();
            normal.set_font_size(font_size).unwrap();
            normal.set_leading(leading).unwrap();
            normal.set_auto_leading(auto_leading).unwrap();
            normal.set_kerning(kerning).unwrap();
            normal.set_faux_bold(faux_bold).unwrap();
            normal.set_faux_italic(faux_italic).unwrap();
            normal.set_horizontal_scale(horizontal_scale).unwrap();
            normal.set_vertical_scale(vertical_scale).unwrap();
            normal.set_tracking(tracking).unwrap();
            normal.set_auto_kerning(auto_kerning).unwrap();
            normal.set_baseline_shift(baseline_shift).unwrap();
            normal.set_font_caps(font_caps).unwrap();
            normal.set_font_baseline(font_baseline).unwrap();
            normal.set_no_break(no_break).unwrap();
            normal.set_language(language).unwrap();
            normal.set_character_direction(character_direction).unwrap();
            normal.set_baseline_direction(baseline_direction).unwrap();
            normal.set_tsume(tsume).unwrap();
            normal.set_kashida(kashida).unwrap();
            normal.set_diacritic_position(diacritic).unwrap();
            normal.set_ligatures(ligatures).unwrap();
            normal.set_discretionary_ligatures(discretionary).unwrap();
            normal.set_underline(underline).unwrap();
            normal.set_strikethrough(strikethrough).unwrap();
            normal.set_stroke_flag(stroke_flag).unwrap();
            normal.set_fill_flag(fill_flag).unwrap();
            normal.set_fill_first(fill_first).unwrap();
            normal.set_outline_width(outline_width).unwrap();
            normal.set_fill_color(fill_color).unwrap();
            normal.set_stroke_color(stroke_color).unwrap();
        }
        assert!(layer.set_style_normal_sheet_index(200).is_err());
        let mut normal = layer.style_normal_mut();
        assert!(normal.set_font_size(f64::INFINITY).is_err());
        assert!(normal
            .set_fill_color([1.0, 0.0, f64::INFINITY, 0.0])
            .is_err());
        assert!(normal
            .set_stroke_color([1.0, 0.0, f64::INFINITY, 0.0])
            .is_err());

        let reread = roundtrip(&file);
        let layer = reread
            .layer(named(&reread, "CharacterStylePrimary"))
            .unwrap();
        assert_eq!(layer.style_normal_sheet_index(), Some(sheet_index));
        let after = layer.style_normal().unwrap();
        assert_eq!(after.font_index(), Some(font));
        assert_close(after.font_size().unwrap(), font_size, 1e-4);
        assert_close(after.leading().unwrap(), leading, 1e-4);
        assert_eq!(after.auto_leading(), Some(auto_leading));
        assert_eq!(after.kerning(), Some(kerning));
        assert_eq!(after.faux_bold(), Some(faux_bold));
        assert_eq!(after.faux_italic(), Some(faux_italic));
        assert_close(after.horizontal_scale().unwrap(), horizontal_scale, 1e-4);
        assert_close(after.vertical_scale().unwrap(), vertical_scale, 1e-4);
        assert_eq!(after.tracking(), Some(tracking));
        assert_eq!(after.auto_kerning(), Some(auto_kerning));
        assert_close(after.baseline_shift().unwrap(), baseline_shift, 1e-4);
        assert_eq!(after.font_caps(), Some(font_caps));
        assert_eq!(after.font_baseline(), Some(font_baseline));
        assert_eq!(after.no_break(), Some(no_break));
        assert_eq!(after.language(), Some(language));
        assert_eq!(after.character_direction(), Some(character_direction));
        assert_eq!(after.baseline_direction(), Some(baseline_direction));
        assert_close(after.tsume().unwrap(), tsume, 1e-4);
        assert_eq!(after.kashida(), Some(kashida));
        assert_eq!(after.diacritic_position(), Some(diacritic));
        assert_eq!(after.ligatures(), Some(ligatures));
        assert_eq!(after.discretionary_ligatures(), Some(discretionary));
        assert_eq!(after.underline(), Some(underline));
        assert_eq!(after.strikethrough(), Some(strikethrough));
        assert_eq!(after.stroke_flag(), Some(stroke_flag));
        assert_eq!(after.fill_flag(), Some(fill_flag));
        assert_eq!(after.fill_first(), Some(fill_first));
        assert_close(after.outline_width().unwrap(), outline_width, 1e-4);
        assert_all_close(&after.fill_color().unwrap(), &fill_color, 1e-4);
        assert_all_close(&after.stroke_color().unwrap(), &stroke_color, 1e-4);
        // Normal-sheet edits must not rewrite explicit run values.
        assert_close(
            layer.style_run(0).unwrap().font_size().unwrap(),
            run_font_size,
            1e-4,
        );
    }
}

// ===========================================================================
// TestTextLayerProxies.cpp — typed views vs raw EngineData
// ===========================================================================
mod proxies {
    use super::*;

    const PARAGRAPH: &str = "paragraph text for fixture coverage";

    fn raw_number(value: Option<EngineValue>) -> Option<f64> {
        value?.as_double()
    }

    fn raw_bool(value: Option<EngineValue>) -> Option<bool> {
        value?.as_bool()
    }

    #[test]
    fn style_run_proxy_getters_agree_with_flat_mixin_getters() {
        let file = open("TextLayers_CharacterStyles.psd");
        let layer = file.layer(named(&file, "CharacterStylePrimary")).unwrap();
        let run = layer.style_run(0).unwrap();
        let raw = |key| {
            layer
                .style_run(0)
                .and_then(|style| style.property(key).cloned())
        };
        assert_eq!(run.font_size(), raw_number(raw("FontSize")));
        assert_eq!(run.leading(), raw_number(raw("Leading")));
        assert_eq!(run.auto_leading(), raw_bool(raw("AutoLeading")));
        assert_eq!(run.kerning().map(f64::from), raw_number(raw("Kerning")));
        assert_eq!(
            run.font_index().map(|index| index as f64),
            raw_number(raw("Font"))
        );
        assert_eq!(run.faux_bold(), raw_bool(raw("FauxBold")));
        assert_eq!(run.faux_italic(), raw_bool(raw("FauxItalic")));
        assert_eq!(run.horizontal_scale(), raw_number(raw("HorizontalScale")));
        assert_eq!(run.vertical_scale(), raw_number(raw("VerticalScale")));
        assert_eq!(run.tracking().map(f64::from), raw_number(raw("Tracking")));
        assert_eq!(run.auto_kerning(), raw_bool(raw("AutoKerning")));
        assert_eq!(run.baseline_shift(), raw_number(raw("BaselineShift")));
        assert_eq!(
            run.font_caps().map(|value| f64::from(value.raw())),
            raw_number(raw("FontCaps"))
        );
        assert_eq!(run.no_break(), raw_bool(raw("NoBreak")));
        assert_eq!(run.tsume(), raw_number(raw("Tsume")));
        assert_eq!(run.underline(), raw_bool(raw("Underline")));
        assert_eq!(run.strikethrough(), raw_bool(raw("Strikethrough")));
        assert_eq!(run.ligatures(), raw_bool(raw("Ligatures")));
        assert_eq!(run.discretionary_ligatures(), raw_bool(raw("DLigatures")));
        assert_eq!(run.stroke_flag(), raw_bool(raw("StrokeFlag")));
        assert_eq!(run.fill_flag(), raw_bool(raw("FillFlag")));
        assert_eq!(run.fill_first(), raw_bool(raw("FillFirst")));
        assert_eq!(run.outline_width(), raw_number(raw("OutlineWidth")));
        assert_eq!(
            run.fill_color(),
            raw("FillColor").and_then(|color| color.get("Values")?.as_double_vector())
        );
        assert_eq!(run.property("FontSize").cloned(), raw("FontSize"));
        assert_eq!(layer.style_runs()[0], run);
    }

    #[test]
    fn style_run_proxy_setter_mutates_via_proxy_and_roundtrips() {
        let mut file = open("TextLayers_CharacterStyles.psd");
        let id = named(&file, "CharacterStylePrimary");
        let layer = file.layer_mut(id).unwrap();
        let size = layer.style_run(0).unwrap().font_size().unwrap() + 10.0;
        layer.style_run_mut(0).set_font_size(size).unwrap();
        assert_close(layer.style_run(0).unwrap().font_size().unwrap(), size, 1e-9);
        let reread = roundtrip(&file);
        let layer = reread
            .layer(named(&reread, "CharacterStylePrimary"))
            .unwrap();
        assert_close(layer.style_run(0).unwrap().font_size().unwrap(), size, 1e-9);
    }

    #[test]
    fn style_normal_proxy_getters_agree_with_flat_mixin_getters() {
        let file = open("TextLayers_CharacterStyles.psd");
        let layer = file.layer(named(&file, "CharacterStylePrimary")).unwrap();
        let normal = layer.style_normal().unwrap();
        let raw = |key| {
            layer
                .style_normal()
                .and_then(|style| style.property(key).cloned())
        };
        assert_eq!(
            normal.font_index().map(|index| index as f64),
            raw_number(raw("Font"))
        );
        assert_eq!(normal.font_size(), raw_number(raw("FontSize")));
        assert_eq!(normal.leading(), raw_number(raw("Leading")));
        assert_eq!(normal.auto_leading(), raw_bool(raw("AutoLeading")));
        assert_eq!(normal.faux_bold(), raw_bool(raw("FauxBold")));
        assert_eq!(
            normal.font_baseline().map(|value| f64::from(value.raw())),
            raw_number(raw("FontBaseline"))
        );
        assert_eq!(
            normal.stroke_color(),
            raw("StrokeColor").and_then(|color| color.get("Values")?.as_double_vector())
        );
    }

    #[test]
    fn style_normal_proxy_setter_mutates_via_proxy_and_roundtrips() {
        let mut file = open("TextLayers_CharacterStyles.psd");
        let id = named(&file, "CharacterStylePrimary");
        let layer = file.layer_mut(id).unwrap();
        let size = layer.style_normal().unwrap().font_size().unwrap() + 5.0;
        layer.style_normal_mut().set_font_size(size).unwrap();
        let reread = roundtrip(&file);
        let layer = reread
            .layer(named(&reread, "CharacterStylePrimary"))
            .unwrap();
        assert_close(
            layer.style_normal().unwrap().font_size().unwrap(),
            size,
            1e-9,
        );
    }

    #[test]
    fn paragraph_run_proxy_getters_agree_with_flat_mixin_getters() {
        let file = open("TextLayers_Paragraph.psd");
        let layer = file.layer(containing(&file, PARAGRAPH)).unwrap();
        let run = layer.paragraph_run(0).unwrap();
        let raw = |key| {
            layer
                .paragraph_run(0)
                .and_then(|style| style.property(key).cloned())
        };
        assert_eq!(
            run.justification().map(|value| f64::from(value.raw())),
            raw_number(raw("Justification"))
        );
        assert_eq!(run.space_before(), raw_number(raw("SpaceBefore")));
        assert_eq!(run.space_after(), raw_number(raw("SpaceAfter")));
        assert_eq!(run.auto_hyphenate(), raw_bool(raw("AutoHyphenate")));
        assert_eq!(
            run.word_spacing(),
            raw("WordSpacing").and_then(|value| value.as_double_vector())
        );
        assert_eq!(layer.paragraph_runs().len(), layer.paragraph_run_count());
    }

    #[test]
    fn paragraph_run_proxy_setter_mutates_via_proxy_and_roundtrips() {
        let mut file = open("TextLayers_Paragraph.psd");
        let id = containing(&file, PARAGRAPH);
        let layer = file.layer_mut(id).unwrap();
        let justification =
            if layer.paragraph_run(0).unwrap().justification() == Some(Justification::Center) {
                Justification::Right
            } else {
                Justification::Center
            };
        layer
            .paragraph_run_mut(0)
            .set_justification(justification)
            .unwrap();
        let reread = roundtrip(&file);
        let layer = reread.layer(containing(&reread, PARAGRAPH)).unwrap();
        assert_eq!(
            layer.paragraph_run(0).unwrap().justification(),
            Some(justification)
        );
    }

    #[test]
    fn paragraph_normal_proxy_getters_agree_with_flat_mixin_getters() {
        let file = open("TextLayers_Paragraph.psd");
        let layer = file.layer(containing(&file, PARAGRAPH)).unwrap();
        let normal = layer.paragraph_normal().unwrap();
        let raw = |key| {
            layer
                .paragraph_normal()
                .and_then(|style| style.property(key).cloned())
        };
        assert_eq!(
            normal.justification().map(|value| f64::from(value.raw())),
            raw_number(raw("Justification"))
        );
        assert_eq!(normal.space_before(), raw_number(raw("SpaceBefore")));
        assert_eq!(normal.zone(), raw_number(raw("Zone")));
        assert_eq!(normal.property("Zone").cloned(), raw("Zone"));
    }

    #[test]
    fn paragraph_normal_proxy_setter_mutates_via_proxy_and_roundtrips() {
        let mut file = open("TextLayers_Paragraph.psd");
        let id = containing(&file, PARAGRAPH);
        let layer = file.layer_mut(id).unwrap();
        let space = layer.paragraph_normal().unwrap().space_before().unwrap() + 4.0;
        layer
            .paragraph_normal_mut()
            .set_space_before(space)
            .unwrap();
        let reread = roundtrip(&file);
        let layer = reread.layer(containing(&reread, PARAGRAPH)).unwrap();
        assert_close(
            layer.paragraph_normal().unwrap().space_before().unwrap(),
            space,
            1e-9,
        );
    }

    fn first_text_layer(file: &LayeredFile<u8>) -> usize {
        file.layers().position(Layer::is_text_layer).unwrap()
    }

    #[test]
    fn font_proxy_getters_agree_with_flat_mixin_getters() {
        let file = open("TextLayers_Basic.psd");
        let layer = file.layer(first_text_layer(&file)).unwrap();
        assert!(layer.font_count() > 0);
        let font = layer.font(0).unwrap();
        assert_eq!(font, layer.fonts()[0]);
        assert_eq!(font.is_sentinel(), layer.is_sentinel_font(0));
    }

    #[test]
    fn font_proxy_setter_roundtrips_postscript_name() {
        let mut file = open("TextLayers_Basic.psd");
        let id = first_text_layer(&file);
        let name = file.layer(id).unwrap().name.clone();
        file.layer_mut(id)
            .unwrap()
            .rename_font(0, "Helvetica-Bold")
            .unwrap();
        assert_eq!(
            file.layer(id).unwrap().font(0).unwrap().postscript_name,
            "Helvetica-Bold"
        );
        let reread = roundtrip(&file);
        assert_eq!(
            reread
                .layer(named(&reread, &name))
                .unwrap()
                .font(0)
                .unwrap()
                .postscript_name,
            "Helvetica-Bold"
        );
    }

    #[test]
    fn font_set_proxy_getters_agree_with_flat_mixin_getters() {
        let file = open("TextLayers_Basic.psd");
        let layer = file.layer(first_text_layer(&file)).unwrap();
        let fonts = layer.fonts();
        assert_eq!(fonts.len(), layer.font_count());
        let used: Vec<_> = layer
            .used_font_indices()
            .into_iter()
            .map(|index| fonts[index].postscript_name.clone())
            .filter(|name| name != "AdobeInvisFont")
            .collect();
        assert_eq!(used, layer.used_font_names());
    }

    #[test]
    fn font_set_proxy_add_creates_a_new_font_entry() {
        let mut file = open("TextLayers_Basic.psd");
        let id = first_text_layer(&file);
        let layer = file.layer_mut(id).unwrap();
        let before = layer.font_count();
        let index = layer
            .add_font("TestFont-Regular", FontType::OpenType, FontScript::Roman, 0)
            .unwrap();
        assert_eq!(layer.font_count(), before + 1);
        assert_eq!(layer.font_index("TestFont-Regular"), Some(index));
        assert_eq!(
            layer.font(index).unwrap().postscript_name,
            "TestFont-Regular"
        );
    }

    #[test]
    fn font_set_proxy_find_index_returns_minus_1_for_unknown_fonts() {
        let file = open("TextLayers_Basic.psd");
        let layer = file.layer(first_text_layer(&file)).unwrap();
        assert_eq!(layer.font_index("NonExistentFont-XYZ123"), None);
    }
}

// ===========================================================================
// TestTextLayerShape.cpp
// ===========================================================================
mod shape {
    use super::*;

    const FIXTURE: &str = "TextLayers_VerticalBox.psd";

    fn layer(name: &str) -> (LayeredFile<u8>, usize) {
        let file = open(FIXTURE);
        let id = named(&file, name);
        (file, id)
    }

    #[test]
    fn box_text_layer_reports_shape_type_box_text() {
        let (file, id) = layer("HorizontalBoxControl");
        let layer = file.layer(id).unwrap();
        assert_eq!(layer.text_shape(), Some(TextShape::Box));
        assert!(layer.is_box_text());
        assert!(!layer.is_point_text());
    }

    #[test]
    fn point_text_layer_reports_shape_type_point_text() {
        let (file, id) = layer("VerticalPointControl");
        let layer = file.layer(id).unwrap();
        assert_eq!(layer.text_shape(), Some(TextShape::Point));
        assert!(layer.is_point_text());
        assert!(!layer.is_box_text());
    }

    #[test]
    fn box_bounds_returns_valid_bounds_for_box_text() {
        let (file, id) = layer("HorizontalBoxControl");
        let bounds = file.layer(id).unwrap().box_bounds().unwrap();
        assert!(bounds.width() > 0.0);
        assert!(bounds.height() > 0.0);
    }

    #[test]
    fn box_width_and_box_height_agree_with_box_bounds() {
        let (file, id) = layer("HorizontalBoxControl");
        let layer = file.layer(id).unwrap();
        let bounds = layer.box_bounds().unwrap();
        assert_eq!(layer.box_width(), Some(bounds.right - bounds.left));
        assert_eq!(layer.box_height(), Some(bounds.bottom - bounds.top));
    }

    #[test]
    fn box_bounds_returns_nullopt_for_point_text() {
        let (file, id) = layer("VerticalPointControl");
        let layer = file.layer(id).unwrap();
        assert_eq!(layer.box_bounds(), None);
        assert_eq!(layer.box_width(), None);
        assert_eq!(layer.box_height(), None);
    }

    #[test]
    fn set_box_bounds_changes_bounds_and_survives_roundtrip() {
        let (mut file, id) = layer("HorizontalBoxControl");
        let bounds = TextBoxBounds {
            top: 10.0,
            left: 20.0,
            bottom: 310.0,
            right: 420.0,
        };
        file.layer_mut(id).unwrap().set_box_bounds(bounds).unwrap();
        assert_eq!(file.layer(id).unwrap().box_bounds(), Some(bounds));
        let reread = roundtrip(&file);
        assert_eq!(
            reread
                .layer(named(&reread, "HorizontalBoxControl"))
                .unwrap()
                .box_bounds(),
            Some(bounds)
        );
    }

    #[test]
    fn set_box_size_keeps_top_left_and_sets_new_width_height() {
        let (mut file, id) = layer("HorizontalBoxControl");
        let layer = file.layer_mut(id).unwrap();
        let original = layer.box_bounds().unwrap();
        layer.set_box_size(400.0, 200.0).unwrap();
        let after = layer.box_bounds().unwrap();
        assert_eq!((after.top, after.left), (original.top, original.left));
        assert_close(layer.box_width().unwrap(), 400.0, 1e-9);
        assert_close(layer.box_height().unwrap(), 200.0, 1e-9);
    }

    #[test]
    fn set_box_width_changes_only_width() {
        let (mut file, id) = layer("HorizontalBoxControl");
        let layer = file.layer_mut(id).unwrap();
        let height = layer.box_height().unwrap();
        layer.set_box_width(999.0).unwrap();
        assert_close(layer.box_width().unwrap(), 999.0, 1e-9);
        assert_close(layer.box_height().unwrap(), height, 1e-9);
    }

    #[test]
    fn set_box_height_changes_only_height() {
        let (mut file, id) = layer("HorizontalBoxControl");
        let layer = file.layer_mut(id).unwrap();
        let width = layer.box_width().unwrap();
        layer.set_box_height(777.0).unwrap();
        assert_close(layer.box_height().unwrap(), 777.0, 1e-9);
        assert_close(layer.box_width().unwrap(), width, 1e-9);
    }

    #[test]
    fn set_box_bounds_rejects_non_finite_values() {
        let (mut file, id) = layer("HorizontalBoxControl");
        let layer = file.layer_mut(id).unwrap();
        let before = layer.clone();
        let bounds = |top, left| TextBoxBounds {
            top,
            left,
            bottom: 100.0,
            right: 100.0,
        };
        assert!(layer.set_box_bounds(bounds(f64::INFINITY, 0.0)).is_err());
        assert!(layer.set_box_bounds(bounds(0.0, f64::NAN)).is_err());
        assert!(layer.set_box_size(f64::INFINITY, 100.0).is_err());
        assert!(layer.set_box_width(f64::NAN).is_err());
        assert!(layer.set_box_height(-1.0).is_err());
        assert_eq!(layer, &before);
    }

    #[test]
    fn convert_to_box_text_turns_point_text_into_box_text() {
        let (mut file, id) = layer("VerticalPointControl");
        let layer = file.layer_mut(id).unwrap();
        layer.convert_to_box_text(300.0, 150.0).unwrap();
        assert!(layer.is_box_text());
        assert!(!layer.is_point_text());
        assert_close(layer.box_width().unwrap(), 300.0, 1e-9);
        assert_close(layer.box_height().unwrap(), 150.0, 1e-9);
    }

    #[test]
    fn convert_to_box_text_roundtrips_through_file_write_read() {
        let (mut file, id) = layer("VerticalPointControl");
        file.layer_mut(id)
            .unwrap()
            .convert_to_box_text(250.0, 120.0)
            .unwrap();
        let reread = roundtrip(&file);
        let layer = reread
            .layer(named(&reread, "VerticalPointControl"))
            .unwrap();
        assert!(layer.is_box_text());
        assert_close(layer.box_width().unwrap(), 250.0, 1e-9);
        assert_close(layer.box_height().unwrap(), 120.0, 1e-9);
    }

    #[test]
    fn convert_to_point_text_turns_box_text_into_point_text() {
        let (mut file, id) = layer("HorizontalBoxControl");
        let layer = file.layer_mut(id).unwrap();
        layer.convert_to_point_text().unwrap();
        assert!(layer.is_point_text());
        assert!(!layer.is_box_text());
        assert_eq!(layer.box_bounds(), None);
    }

    #[test]
    fn convert_to_point_text_roundtrips_through_file_write_read() {
        let (mut file, id) = layer("HorizontalBoxControl");
        file.layer_mut(id).unwrap().convert_to_point_text().unwrap();
        let reread = roundtrip(&file);
        let layer = reread
            .layer(named(&reread, "HorizontalBoxControl"))
            .unwrap();
        assert!(layer.is_point_text());
        assert_eq!(layer.box_bounds(), None);
    }

    #[test]
    fn convert_to_box_text_fails_when_already_box_text() {
        let (mut file, id) = layer("HorizontalBoxControl");
        assert!(file
            .layer_mut(id)
            .unwrap()
            .convert_to_box_text(100.0, 100.0)
            .is_err());
    }

    #[test]
    fn convert_to_point_text_fails_when_already_point_text() {
        let (mut file, id) = layer("VerticalPointControl");
        assert!(file.layer_mut(id).unwrap().convert_to_point_text().is_err());
    }

    #[test]
    fn vertical_box_text_has_correct_shape_and_orientation() {
        let (file, id) = layer("VerticalBoxText");
        let layer = file.layer(id).unwrap();
        assert!(layer.is_box_text());
        assert!(layer.is_vertical());
        assert_eq!(layer.orientation(), Some(TextWritingDirection::Vertical));
        assert!(layer.box_width().unwrap() > 0.0);
        assert!(layer.box_height().unwrap() > 0.0);
    }
}

// ===========================================================================
// TestTextLayerTransform.cpp
// ===========================================================================
mod transform {
    use super::*;

    fn rotated() -> (LayeredFile<u8>, usize) {
        let file = open("TextLayers_Transform.psd");
        let id = named(&file, "RotatedText");
        (file, id)
    }

    fn simple() -> (LayeredFile<u8>, usize) {
        let file = open("TextLayers_Basic.psd");
        let id = named(&file, "SimpleASCII");
        (file, id)
    }

    fn rotation(degrees: f64, tx: f64, ty: f64) -> [f64; 6] {
        let (sin, cos) = degrees.to_radians().sin_cos();
        [cos, sin, -sin, cos, tx, ty]
    }

    #[test]
    fn read_returns_6_element_vector_for_rotated_text() {
        let (file, id) = rotated();
        let [xx, xy, yx, yy, ..] = file.layer(id).unwrap().text_transform().unwrap();
        assert!(
            !((xx - 1.0).abs() < 1e-6
                && (yy - 1.0).abs() < 1e-6
                && xy.abs() < 1e-6
                && yx.abs() < 1e-6)
        );
    }

    #[test]
    fn control_layer_has_identity_like_rotation_submatrix() {
        let file = open("TextLayers_Transform.psd");
        let transform = file
            .layer(named(&file, "TransformControl"))
            .unwrap()
            .text_transform()
            .unwrap();
        assert_all_close(&transform[..4], &[1.0, 0.0, 0.0, 1.0], 0.01);
    }

    #[test]
    fn individual_component_accessors_agree_with_vector() {
        let (file, id) = rotated();
        let layer = file.layer(id).unwrap();
        let transform = layer.text_transform().unwrap();
        for (index, value) in transform.into_iter().enumerate() {
            assert_eq!(layer.text_transform_component(index), Some(value));
        }
        assert_eq!(layer.text_transform_component(6), None);
    }

    #[test]
    fn rotated_text_has_expected_rotation_direction() {
        let (file, id) = rotated();
        let [xx, xy, ..] = file.layer(id).unwrap().text_transform().unwrap();
        assert!(xy.abs() > 0.001);
        assert!(xx > 0.0 && xx < 1.0);
    }

    #[test]
    fn translation_components_are_non_zero_for_positioned_text() {
        let (file, id) = rotated();
        let (tx, ty) = file.layer(id).unwrap().text_position().unwrap();
        assert!(tx.abs() > 1.0);
        assert!(ty.abs() > 1.0);
    }

    #[test]
    fn set_transform_writes_and_reads_back_correctly() {
        let (mut file, id) = simple();
        let layer = file.layer_mut(id).unwrap();
        let custom = rotation(30.0, 42.5, 99.0);
        layer.set_text_transform(custom).unwrap();
        assert_all_close(&layer.text_transform().unwrap(), &custom, 1e-10);
    }

    #[test]
    fn set_transform_xx_yy_individual_writers_work() {
        let (mut file, id) = simple();
        let layer = file.layer_mut(id).unwrap();
        for (index, value) in [2.5, 0.1, -0.2, 3.0, 100.0, 200.0].into_iter().enumerate() {
            layer.set_text_transform_component(index, value).unwrap();
        }
        assert_eq!(
            layer.text_transform(),
            Some([2.5, 0.1, -0.2, 3.0, 100.0, 200.0])
        );
    }

    #[test]
    fn set_transform_rejects_wrong_size_vector() {
        // `[f64; 6]` makes a wrong-sized transform a compile error; the
        // remaining runtime check rejects non-finite components atomically.
        let (mut file, id) = simple();
        let layer = file.layer_mut(id).unwrap();
        let before = layer.clone();
        assert!(layer
            .set_text_transform([1.0, 0.0, 0.0, f64::NAN, 0.0, 0.0])
            .is_err());
        assert_eq!(layer, &before);
    }

    #[test]
    fn set_transform_component_rejects_out_of_range_index() {
        let (mut file, id) = simple();
        let layer = file.layer_mut(id).unwrap();
        assert!(layer.set_text_transform_component(6, 1.0).is_err());
        assert!(layer.set_text_transform_component(100, 1.0).is_err());
    }

    #[test]
    fn roundtrip_through_file_write_preserves_transform() {
        let (mut file, id) = simple();
        let custom = rotation(-12.0, 55.5, 77.7);
        file.layer_mut(id)
            .unwrap()
            .set_text_transform(custom)
            .unwrap();
        let reread = roundtrip(&file);
        assert_all_close(
            &reread
                .layer(named(&reread, "SimpleASCII"))
                .unwrap()
                .text_transform()
                .unwrap(),
            &custom,
            1e-10,
        );
    }

    #[test]
    fn basic_fixture_simple_ascii_has_identity_rotation_submatrix() {
        let (file, id) = simple();
        let transform = file.layer(id).unwrap().text_transform().unwrap();
        assert_all_close(&transform[..4], &[1.0, 0.0, 0.0, 1.0], 0.01);
    }

    #[test]
    fn rotation_angle_returns_0_for_un_rotated_text() {
        let (file, id) = simple();
        assert_close(
            file.layer(id).unwrap().text_rotation_angle().unwrap(),
            0.0,
            0.1,
        );
    }

    #[test]
    fn rotation_angle_returns_non_zero_for_rotated_fixture() {
        let (file, id) = rotated();
        assert!(file.layer(id).unwrap().text_rotation_angle().unwrap().abs() > 1.0);
    }

    #[test]
    fn scale_x_and_scale_y_return_1_for_un_scaled_text() {
        let (file, id) = simple();
        let layer = file.layer(id).unwrap();
        assert_close(layer.text_scale_x().unwrap(), 1.0, 0.01);
        assert_close(layer.text_scale_y().unwrap(), 1.0, 0.01);
    }

    #[test]
    fn scale_x_and_scale_y_reflect_fixture_scaling() {
        let (file, id) = rotated();
        let layer = file.layer(id).unwrap();
        assert_close(layer.text_scale_x().unwrap(), 0.86, 0.02);
        assert_close(layer.text_scale_y().unwrap(), 1.18, 0.02);
    }

    #[test]
    fn set_rotation_angle_sets_angle_and_preserves_scale_translation() {
        let (mut file, id) = simple();
        let layer = file.layer_mut(id).unwrap();
        let (tx, ty) = layer.text_position().unwrap();
        layer.set_text_rotation_angle(45.0).unwrap();
        assert_close(layer.text_rotation_angle().unwrap(), 45.0, 0.01);
        assert_close(layer.text_scale_x().unwrap(), 1.0, 0.01);
        assert_close(layer.text_scale_y().unwrap(), 1.0, 0.01);
        assert_eq!(layer.text_position(), Some((tx, ty)));
    }

    #[test]
    fn set_rotation_angle_negative_angle() {
        let (mut file, id) = simple();
        let layer = file.layer_mut(id).unwrap();
        layer.set_text_rotation_angle(-30.0).unwrap();
        assert_close(layer.text_rotation_angle().unwrap(), -30.0, 0.01);
    }

    #[test]
    fn set_scale_x_changes_horizontal_scale_preserving_angle() {
        let (mut file, id) = simple();
        let layer = file.layer_mut(id).unwrap();
        layer.set_text_rotation_angle(20.0).unwrap();
        layer.set_text_scale_x(1.5).unwrap();
        assert_close(layer.text_scale_x().unwrap(), 1.5, 0.01);
        assert_close(layer.text_scale_y().unwrap(), 1.0, 0.01);
        assert_close(layer.text_rotation_angle().unwrap(), 20.0, 0.01);
    }

    #[test]
    fn set_scale_y_changes_vertical_scale_preserving_angle() {
        let (mut file, id) = simple();
        let layer = file.layer_mut(id).unwrap();
        layer.set_text_rotation_angle(20.0).unwrap();
        layer.set_text_scale_y(0.75).unwrap();
        assert_close(layer.text_scale_y().unwrap(), 0.75, 0.01);
        assert_close(layer.text_scale_x().unwrap(), 1.0, 0.01);
        assert_close(layer.text_rotation_angle().unwrap(), 20.0, 0.01);
    }

    #[test]
    fn set_scale_sets_both_factors_at_once() {
        let (mut file, id) = simple();
        let layer = file.layer_mut(id).unwrap();
        layer.set_text_rotation_angle(15.0).unwrap();
        layer.set_text_scale(2.0, 0.5).unwrap();
        assert_close(layer.text_scale_x().unwrap(), 2.0, 0.01);
        assert_close(layer.text_scale_y().unwrap(), 0.5, 0.01);
        assert_close(layer.text_rotation_angle().unwrap(), 15.0, 0.01);
    }

    #[test]
    fn rotation_plus_scale_roundtrip_through_file_write() {
        let (mut file, id) = simple();
        let layer = file.layer_mut(id).unwrap();
        layer.set_text_rotation_angle(60.0).unwrap();
        layer.set_text_scale(1.25, 0.8).unwrap();
        let reread = roundtrip(&file);
        let layer = reread.layer(named(&reread, "SimpleASCII")).unwrap();
        assert_close(layer.text_rotation_angle().unwrap(), 60.0, 0.01);
        assert_close(layer.text_scale_x().unwrap(), 1.25, 0.01);
        assert_close(layer.text_scale_y().unwrap(), 0.8, 0.01);
    }

    #[test]
    fn position_returns_tx_ty_pair() {
        let (file, id) = rotated();
        let layer = file.layer(id).unwrap();
        assert_eq!(
            layer.text_position(),
            Some((
                layer.text_transform_component(4).unwrap(),
                layer.text_transform_component(5).unwrap()
            ))
        );
    }

    #[test]
    fn set_position_changes_tx_ty_and_preserves_rotation() {
        let (mut file, id) = rotated();
        let layer = file.layer_mut(id).unwrap();
        let angle = layer.text_rotation_angle().unwrap();
        let scale = layer.text_scale_x().unwrap();
        layer.set_text_position(123.0, 456.0).unwrap();
        assert_eq!(layer.text_position(), Some((123.0, 456.0)));
        assert_close(layer.text_rotation_angle().unwrap(), angle, 1e-9);
        assert_close(layer.text_scale_x().unwrap(), scale, 1e-9);
    }

    #[test]
    fn reset_transform_produces_identity_with_preserved_position() {
        let (mut file, id) = rotated();
        let layer = file.layer_mut(id).unwrap();
        let (tx, ty) = layer.text_position().unwrap();
        layer.reset_text_transform().unwrap();
        assert_eq!(layer.text_transform(), Some([1.0, 0.0, 0.0, 1.0, tx, ty]));
    }

    #[test]
    fn primary_font_name_returns_a_non_empty_string() {
        let (file, id) = simple();
        assert!(!file
            .layer(id)
            .unwrap()
            .primary_font_name()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn set_font_applies_to_all_style_runs_and_normal_sheet() {
        let mut file = open("TextLayers_StyleRuns.psd");
        let id = with_text(&file, "Alpha Beta Gamma");
        let layer = file.layer_mut(id).unwrap();
        layer.set_font("CourierNewPSMT").unwrap();
        let index = layer.font_index("CourierNewPSMT").unwrap();
        assert!(layer
            .style_runs()
            .iter()
            .all(|run| run.font_index() == Some(index)));
        assert_eq!(layer.style_normal().unwrap().font_index(), Some(index));
        assert_eq!(layer.primary_font_name().as_deref(), Some("CourierNewPSMT"));
    }

    #[test]
    fn set_font_roundtrips_through_file_write() {
        let (mut file, id) = simple();
        file.layer_mut(id)
            .unwrap()
            .set_font("TimesNewRomanPSMT")
            .unwrap();
        let reread = roundtrip(&file);
        assert_eq!(
            reread
                .layer(named(&reread, "SimpleASCII"))
                .unwrap()
                .primary_font_name()
                .as_deref(),
            Some("TimesNewRomanPSMT")
        );
    }

    #[test]
    fn position_roundtrip_through_file_write() {
        let (mut file, id) = simple();
        file.layer_mut(id)
            .unwrap()
            .set_text_position(321.5, 654.25)
            .unwrap();
        let reread = roundtrip(&file);
        assert_eq!(
            reread
                .layer(named(&reread, "SimpleASCII"))
                .unwrap()
                .text_position(),
            Some((321.5, 654.25))
        );
    }
}

// ===========================================================================
// TestTextLayerUnicode.cpp
// ===========================================================================
mod unicode {
    use super::*;

    /// `set_text` on the Basic fixture, then check in memory and after a
    /// roundtrip (upstream's repeated find-by-contents pattern).
    #[track_caller]
    fn set_and_roundtrip(text: &str) {
        let mut file = open("TextLayers_Basic.psd");
        let id = with_text(&file, "Hello 123");
        file.layer_mut(id).unwrap().set_text(text).unwrap();
        assert_eq!(file.layer(id).unwrap().text().as_deref(), Some(text));
        assert!(try_with_text(&roundtrip(&file), text).is_some());
    }

    fn edit_and_roundtrip(
        fixture_name: &str,
        original: &str,
        edit: impl FnOnce(&mut Layer<u8>),
        expected: &str,
    ) -> LayeredFile<u8> {
        let mut file = open(fixture_name);
        let id = with_text(&file, original);
        edit(file.layer_mut(id).unwrap());
        assert_eq!(file.layer(id).unwrap().text().as_deref(), Some(expected));
        let reread = roundtrip(&file);
        assert!(try_with_text(&reread, expected).is_some());
        reread
    }

    #[test]
    fn cjk_text_set_text_roundtrip() {
        set_and_roundtrip("\u{4F60}\u{597D}\u{4E16}\u{754C}");
    }

    #[test]
    fn cjk_replace_text_partial_replacement_roundtrip() {
        edit_and_roundtrip(
            "TextLayers_Basic.psd",
            "Hello 123",
            |layer| {
                layer.set_text("AB\u{4E2D}\u{6587}CD").unwrap();
                layer
                    .replace_text(
                        "\u{4E2D}\u{6587}",
                        "\u{65E5}\u{672C}\u{8A9E}\u{6587}\u{5B57}",
                    )
                    .unwrap();
            },
            "AB\u{65E5}\u{672C}\u{8A9E}\u{6587}\u{5B57}CD",
        );
    }

    #[test]
    fn cjk_fullwidth_characters_roundtrip() {
        set_and_roundtrip("\u{FF21}\u{FF22}\u{FF23}");
    }

    #[test]
    fn arabic_text_set_text_roundtrip() {
        set_and_roundtrip("\u{0645}\u{0631}\u{062D}\u{0628}\u{0627}");
    }

    #[test]
    fn hebrew_text_replace_text_roundtrip() {
        let hebrew = "\u{05E9}\u{05DC}\u{05D5}\u{05DD}";
        let longer = "\u{05E9}\u{05DC}\u{05D5}\u{05DD} \u{05E2}\u{05D5}\u{05DC}\u{05DD}";
        edit_and_roundtrip(
            "TextLayers_Basic.psd",
            "Hello 123",
            |layer| {
                layer.set_text(hebrew).unwrap();
                layer.replace_text(hebrew, longer).unwrap();
            },
            longer,
        );
    }

    #[test]
    fn mixed_ltr_rtl_text_roundtrip() {
        set_and_roundtrip("Hello \u{0645}\u{0631}\u{062D}\u{0628}\u{0627} World");
    }

    #[test]
    fn emoji_surrogate_pair_text_roundtrip() {
        set_and_roundtrip("\u{1F600}\u{1F389}\u{1F30D}");
    }

    #[test]
    fn mixed_ascii_and_emoji_replacement_roundtrip() {
        edit_and_roundtrip(
            "TextLayers_Basic.psd",
            "Hello 123",
            |layer| {
                layer.set_text("Hi \u{1F600} there").unwrap();
                layer.replace_text("Hi", "Hey").unwrap();
            },
            "Hey \u{1F600} there",
        );
    }

    #[test]
    fn replace_text_adjacent_to_surrogate_pair_does_not_corrupt_pair() {
        edit_and_roundtrip(
            "TextLayers_Basic.psd",
            "Hello 123",
            |layer| {
                layer.set_text("A\u{1F600}B").unwrap();
                layer.replace_text("B", "XY").unwrap();
                assert_eq!(layer.text().as_deref(), Some("A\u{1F600}XY"));
                layer.replace_text("A", "PQR").unwrap();
            },
            "PQR\u{1F600}XY",
        );
    }

    #[test]
    fn multiple_emoji_with_supplementary_plane_characters() {
        set_and_roundtrip("\u{1D11E}\u{1D122}\u{1F3B5}");
    }

    #[test]
    fn precomposed_vs_decomposed_accent_roundtrip() {
        set_and_roundtrip("caf\u{00E9}");
        set_and_roundtrip("cafe\u{0301}");
    }

    #[test]
    fn combining_marks_multiple_diacritics_roundtrip() {
        set_and_roundtrip("o\u{0303}\u{0301}");
    }

    #[test]
    fn replace_within_text_containing_combining_marks() {
        edit_and_roundtrip(
            "TextLayers_Basic.psd",
            "Hello 123",
            |layer| {
                layer.set_text("caf\u{00E9} 123").unwrap();
                layer.replace_text("123", "XYZ").unwrap();
            },
            "caf\u{00E9} XYZ",
        );
    }

    #[test]
    fn mixed_script_multi_run_replace_preserves_style_runs() {
        let expected = "Alpha \u{4E16}\u{754C} Gamma";
        let reread = edit_and_roundtrip(
            "TextLayers_StyleRuns.psd",
            "Alpha Beta Gamma",
            |layer| layer.replace_text("Beta", "\u{4E16}\u{754C}").unwrap(),
            expected,
        );
        assert_eq!(
            reread
                .layer(with_text(&reread, expected))
                .unwrap()
                .style_run_count(),
            5
        );
    }

    #[test]
    fn replace_with_emoji_in_multi_run_text_preserves_style_runs() {
        let expected = "Alpha \u{1F600}\u{1F389} Gamma";
        let reread = edit_and_roundtrip(
            "TextLayers_StyleRuns.psd",
            "Alpha Beta Gamma",
            |layer| layer.replace_text("Beta", "\u{1F600}\u{1F389}").unwrap(),
            expected,
        );
        let layer = reread.layer(with_text(&reread, expected)).unwrap();
        assert_eq!(layer.style_run_count(), 5);
        // Same UTF-16 length: the run boundaries do not move.
        assert_eq!(layer.style_run_lengths(), Some(vec![5, 1, 4, 1, 6]));
    }

    #[test]
    fn replace_with_arabic_in_multi_run_text_preserves_style_runs() {
        let arabic = "\u{0645}\u{0631}\u{062D}\u{0628}\u{0627} \u{0628}\u{0643}\u{0645}";
        let expected = format!("{arabic} Beta Gamma");
        edit_and_roundtrip(
            "TextLayers_StyleRuns.psd",
            "Alpha Beta Gamma",
            |layer| layer.replace_text("Alpha", arabic).unwrap(),
            &expected,
        );
    }

    #[test]
    fn utf8_to_utf16_code_unit_count_is_correct_for_various_scripts() {
        // Rust strings encode to UTF-16 natively; this pins the code-unit
        // counts that text matching and run remapping rely on.
        let units = |text: &str| text.encode_utf16().collect::<Vec<_>>();
        assert_eq!(units("A").len(), 1);
        assert_eq!(units("\u{00E9}").len(), 1);
        assert_eq!(units("e\u{0301}").len(), 2);
        assert_eq!(units("\u{4F60}").len(), 1);
        assert_eq!(units("\u{0645}").len(), 1);
        assert_eq!(units("\u{1F600}"), vec![0xD83D, 0xDE00]);
        assert_eq!(units("\u{1D11E}").len(), 2);
    }

    #[test]
    fn utf16_roundtrip_preserves_surrogate_pair_integrity() {
        let original = "A\u{1F600}B\u{4E2D}\u{0301}C";
        let units: Vec<u16> = original.encode_utf16().collect();
        assert_eq!(units.len(), 7);
        assert_eq!(String::from_utf16(&units).unwrap(), original);
        // And through the text layer: run lengths count code units.
        let layer = Layer::<u8>::new_text("Units", original).unwrap();
        assert_eq!(layer.style_run_lengths(), Some(vec![8]));
    }

    #[test]
    fn empty_string_replacement_roundtrip() {
        set_and_roundtrip("\u{200B}Hi");
    }

    #[test]
    fn korean_hangul_jamo_roundtrip() {
        set_and_roundtrip("\u{D55C}\u{AE00}");
    }

    #[test]
    fn thai_script_with_tone_marks_roundtrip() {
        set_and_roundtrip("\u{0E2A}\u{0E27}\u{0E31}\u{0E2A}\u{0E14}\u{0E35}");
    }

    #[test]
    fn long_mixed_script_text_with_all_edge_cases() {
        set_and_roundtrip(
            "Hi \u{4F60}\u{597D} \u{0645}\u{0631}\u{062D}\u{0628}\u{0627} \u{1F600} caf\u{00E9}",
        );
    }
}

// ===========================================================================
// TestTextLayerWarp.cpp
// ===========================================================================
mod warp {
    use super::*;

    fn layer(name: &str) -> (LayeredFile<u8>, usize) {
        let file = open("TextLayers_Warp.psd");
        let id = named(&file, name);
        (file, id)
    }

    #[test]
    fn has_warp_returns_true_for_warped_layer() {
        let (file, id) = layer("WarpArc");
        assert!(file.layer(id).unwrap().has_text_warp());
    }

    #[test]
    fn has_warp_returns_false_for_non_warped_layer() {
        let (file, id) = layer("Secondary");
        assert!(!file.layer(id).unwrap().has_text_warp());
    }

    #[test]
    fn warp_style_returns_correct_value_for_arc_warp() {
        let (file, id) = layer("WarpArc");
        assert_eq!(
            file.layer(id).unwrap().text_warp_style(),
            Some(TextWarpStyle::Arc)
        );
    }

    #[test]
    fn warp_style_returns_warp_none_for_non_warped_layer() {
        let (file, id) = layer("Secondary");
        assert_eq!(
            file.layer(id).unwrap().text_warp_style(),
            Some(TextWarpStyle::NoWarp)
        );
    }

    #[test]
    fn warp_value_returns_bend_amount() {
        let (file, id) = layer("WarpArc");
        assert_close(
            file.layer(id).unwrap().text_warp_value().unwrap(),
            28.0,
            0.01,
        );
    }

    #[test]
    fn warp_value_returns_0_for_non_warped_layer() {
        let (file, id) = layer("Secondary");
        assert!(file.layer(id).unwrap().text_warp_value().unwrap().abs() < 0.01);
    }

    #[test]
    fn warp_horizontal_distortion_returns_0_for_arc_fixture() {
        let (file, id) = layer("WarpArc");
        assert!(
            file.layer(id)
                .unwrap()
                .text_warp_horizontal_distortion()
                .unwrap()
                .abs()
                < 0.01
        );
    }

    #[test]
    fn warp_vertical_distortion_returns_0_for_arc_fixture() {
        let (file, id) = layer("WarpArc");
        assert!(
            file.layer(id)
                .unwrap()
                .text_warp_vertical_distortion()
                .unwrap()
                .abs()
                < 0.01
        );
    }

    #[test]
    fn warp_rotation_returns_0_horizontal_for_arc_fixture() {
        let (file, id) = layer("WarpArc");
        assert_eq!(
            file.layer(id).unwrap().text_warp_rotation(),
            Some(TextWarpRotation::Horizontal)
        );
    }

    #[test]
    fn warp_apis_still_work_after_text_roundtrip() {
        let (file, id) = layer("WarpArc");
        assert_eq!(
            file.layer(id).unwrap().text().as_deref(),
            Some("Warped Text")
        );
        let reread = roundtrip(&file);
        let layer = reread.layer(named(&reread, "WarpArc")).unwrap();
        assert_eq!(layer.text_warp_style(), Some(TextWarpStyle::Arc));
        assert_close(layer.text_warp_value().unwrap(), 28.0, 0.01);
        assert!(layer.has_text_warp());
    }

    #[test]
    fn warp_set_apis_mutate_and_survive_roundtrip() {
        let (mut file, id) = layer("WarpArc");
        let layer = file.layer_mut(id).unwrap();
        layer.set_text_warp_style(&TextWarpStyle::Wave).unwrap();
        layer.set_text_warp_value(-22.5).unwrap();
        layer.set_text_warp_horizontal_distortion(15.0).unwrap();
        layer.set_text_warp_vertical_distortion(-8.25).unwrap();
        layer
            .set_text_warp_rotation(&TextWarpRotation::Vertical)
            .unwrap();

        let check = |layer: &Layer<u8>| {
            assert_eq!(layer.text_warp_style(), Some(TextWarpStyle::Wave));
            assert_close(layer.text_warp_value().unwrap(), -22.5, 0.001);
            assert_close(
                layer.text_warp_horizontal_distortion().unwrap(),
                15.0,
                0.001,
            );
            assert_close(layer.text_warp_vertical_distortion().unwrap(), -8.25, 0.001);
            assert_eq!(layer.text_warp_rotation(), Some(TextWarpRotation::Vertical));
            assert!(layer.has_text_warp());
            // Editing the warp never touches the text or its EngineData.
            assert_eq!(layer.text().as_deref(), Some("Warped Text"));
        };
        check(layer);
        let reread = roundtrip(&file);
        check(reread.layer(named(&reread, "WarpArc")).unwrap());
    }

    #[test]
    fn basic_fixture_layers_have_warp_none() {
        let file = open("TextLayers_Basic.psd");
        for layer in file.layers().filter(|layer| layer.is_text_layer()) {
            assert!(!layer.has_text_warp());
            if let Some(style) = layer.text_warp_style() {
                assert_eq!(style, TextWarpStyle::NoWarp);
            }
        }
    }
}

// ===========================================================================
// Rust-side coverage of upstream API surface without dedicated upstream
// test cases: anti-aliasing and range styling.
// ===========================================================================
mod anti_alias {
    use super::*;
    use psd::core::TaggedBlockKey;

    const TYSH: TaggedBlockKey = TaggedBlockKey::new(*b"TySh");

    #[test]
    fn fixture_anti_alias_reads_crisp_and_same_value_write_is_byte_exact() {
        let mut file = open("TextLayers_Basic.psd");
        let id = with_text(&file, "Hello 123");
        let layer = file.layer_mut(id).unwrap();
        assert_eq!(layer.anti_alias(), Some(AntiAliasMethod::Crisp));
        let before = layer.blocks.get(TYSH).unwrap().data.clone();
        layer.set_anti_alias(&AntiAliasMethod::Crisp).unwrap();
        assert_eq!(layer.blocks.get(TYSH).unwrap().data, before);
    }

    #[test]
    fn every_anti_alias_method_roundtrips() {
        for method in [
            AntiAliasMethod::NoAntiAlias,
            AntiAliasMethod::Sharp,
            AntiAliasMethod::Strong,
            AntiAliasMethod::Smooth,
            AntiAliasMethod::Crisp,
        ] {
            let mut file = open("TextLayers_Basic.psd");
            let id = with_text(&file, "Hello 123");
            let layer = file.layer_mut(id).unwrap();
            let engine = layer.engine_data();
            layer.set_anti_alias(&method).unwrap();
            assert_eq!(layer.anti_alias(), Some(method.clone()));
            assert_eq!(layer.engine_data(), engine, "EngineData must not change");
            let reread = roundtrip(&file);
            let layer = reread.layer(with_text(&reread, "Hello 123")).unwrap();
            assert_eq!(layer.anti_alias(), Some(method));
        }
    }

    #[test]
    fn warp_and_anti_alias_edits_preserve_bytes_outside_their_descriptor() {
        let mut file = open("TextLayers_Warp.psd");
        let id = named(&file, "WarpArc");
        let layer = file.layer_mut(id).unwrap();
        let original = layer.blocks.get(TYSH).unwrap().data.clone();
        let spans = psd::core::TypeToolTaggedBlock::descriptor_spans(&original).unwrap();

        layer.set_text_warp_value(12.0).unwrap();
        let edited = layer.blocks.get(TYSH).unwrap().data.clone();
        // Same-size numeric edit: only the warp descriptor bytes may differ.
        assert_eq!(edited.len(), original.len());
        assert_eq!(edited[..spans.warp.start], original[..spans.warp.start]);
        assert_eq!(edited[spans.warp.end..], original[spans.warp.end..]);

        layer.set_anti_alias(&AntiAliasMethod::Sharp).unwrap();
        let sharp = layer.blocks.get(TYSH).unwrap().data.clone();
        // `antiAliasSharp` is longer than `AnCr`; the transform prefix and
        // everything after the text descriptor must be unchanged.
        assert_eq!(sharp[..spans.text.start], edited[..spans.text.start]);
        let growth = sharp.len() - edited.len();
        assert_eq!(sharp[spans.text.end + growth..], edited[spans.text.end..]);
    }

    #[test]
    fn warp_setters_reject_missing_items_without_mutation() {
        let mut layer = Layer::<u8>::new_image("NotText", psd::Rect::default());
        assert!(layer.set_text_warp_value(1.0).is_err());
        let mut text = Layer::<u8>::new_text("Text", "abc").unwrap();
        let before = text.clone();
        assert!(text.set_text_warp_value(f64::NAN).is_err());
        assert_eq!(text, before);
        assert_eq!(text.anti_alias(), Some(AntiAliasMethod::Crisp));
    }
}

mod ranges {
    use super::*;

    #[test]
    fn style_range_splits_runs_at_boundaries_and_styles_only_the_span() {
        let mut layer = Layer::<u8>::new_text("Range", "Hello Bold World").unwrap();
        layer
            .style_range(6..10)
            .set_faux_bold(true)
            .unwrap()
            .set_font_size(32.0)
            .unwrap();
        assert_eq!(layer.style_run_lengths(), Some(vec![6, 4, 7]));
        let runs = layer.style_runs();
        assert_eq!(runs[0].faux_bold(), Some(false));
        assert_eq!(runs[1].faux_bold(), Some(true));
        assert_eq!(runs[1].font_size(), Some(32.0));
        assert_eq!(runs[2].faux_bold(), Some(false));
        assert_eq!(runs[2].font_size(), Some(24.0));
    }

    #[test]
    fn style_text_targets_all_or_one_occurrence() {
        let mut layer = Layer::<u8>::new_text("Needles", "the cat and the hat").unwrap();
        let mut all = layer.style_text("the", Occurrence::All);
        assert_eq!(all.spans(), &[0..3, 12..15]);
        all.set_underline(true).unwrap();
        assert_eq!(layer.style_run_lengths(), Some(vec![3, 9, 3, 5]));
        let underlined: Vec<_> = layer
            .style_runs()
            .iter()
            .map(|run| run.underline().unwrap())
            .collect();
        assert_eq!(underlined, vec![true, false, true, false]);

        let mut layer = Layer::<u8>::new_text("Needles", "the cat and the hat").unwrap();
        layer
            .style_text("the", Occurrence::Nth(1))
            .set_faux_italic(true)
            .unwrap();
        assert_eq!(layer.style_run_lengths(), Some(vec![12, 3, 5]));
        assert_eq!(layer.style_run(1).unwrap().faux_italic(), Some(true));

        let mut layer = Layer::<u8>::new_text("Needles", "the cat").unwrap();
        let before = layer.clone();
        let mut none = layer.style_text("dog", Occurrence::All);
        assert!(none.is_empty());
        none.set_faux_bold(true).unwrap();
        assert!(layer.style_text("the", Occurrence::Nth(5)).is_empty());
        assert_eq!(layer, before);
    }

    #[test]
    fn style_all_and_range_font_by_name_roundtrip() {
        let mut layer = Layer::<u8>::new_text("Fonts", "Mixed fonts").unwrap();
        layer.style_all().set_tracking(50).unwrap();
        layer.style_range(0..5).set_font("Georgia").unwrap();
        let georgia = layer.font_index("Georgia").unwrap();
        let mut file = LayeredFile::<u8>::new(psd::core::ColorMode::Rgb, 64, 64).unwrap();
        file.add_layer(layer);
        let reread = roundtrip(&file);
        let layer = reread.layer(named(&reread, "Fonts")).unwrap();
        assert_eq!(layer.style_run_lengths(), Some(vec![5, 7]));
        assert!(layer
            .style_runs()
            .iter()
            .all(|run| run.tracking() == Some(50)));
        assert_eq!(layer.style_run(0).unwrap().font_index(), Some(georgia));
        assert_eq!(layer.style_run(1).unwrap().font_index(), Some(0));
    }

    #[test]
    fn range_styling_is_atomic_when_a_setter_fails() {
        let mut layer = Layer::<u8>::new_text("Atomic", "abcdef").unwrap();
        let before = layer.clone();
        // The color is validated after the span boundaries were split, so
        // the failure must also roll the splits back.
        assert!(layer
            .style_range(2..4)
            .set_fill_color([1.0, f64::NAN, 0.0, 0.0])
            .is_err());
        assert_eq!(layer, before);
        assert_eq!(layer.style_run_lengths(), Some(vec![7]));
        layer.style_range(10..20).set_underline(true).unwrap();
        assert_eq!(layer, before, "a span past the end is a no-op");
    }

    #[test]
    fn paragraph_ranges_cover_whole_paragraphs() {
        // The builder writes one paragraph run per paragraph, like Photoshop.
        let mut layer = Layer::<u8>::new_text("Paragraphs", "One\nTwo\nThree").unwrap();
        assert_eq!(layer.paragraph_run_lengths(), Some(vec![4, 4, 6]));
        assert_eq!(layer.style_run_lengths(), Some(vec![14]));
        layer
            .paragraph_text("Two", Occurrence::All)
            .set_justification(Justification::Center)
            .unwrap();
        // "Two" widens to its whole paragraph, so no run is split.
        assert_eq!(layer.paragraph_run_lengths(), Some(vec![4, 4, 6]));
        let justification: Vec<_> = layer
            .paragraph_runs()
            .iter()
            .map(|run| run.justification().unwrap())
            .collect();
        assert_eq!(
            justification,
            [
                Justification::Left,
                Justification::Center,
                Justification::Left
            ]
        );
        layer
            .paragraph_all()
            .set_space_after(6.0)
            .unwrap()
            .set_word_spacing(&[0.9, 1.0, 1.2])
            .unwrap();
        assert!(layer
            .paragraph_runs()
            .iter()
            .all(|run| run.space_after() == Some(6.0)));
        // A range inside "One" styles all of "One" and nothing else.
        layer
            .paragraph_range(1..2)
            .set_leading_type(LeadingType::TopToTop)
            .unwrap();
        let leading: Vec<_> = layer
            .paragraph_runs()
            .iter()
            .map(|run| run.leading_type().unwrap())
            .collect();
        assert_eq!(
            leading,
            [
                LeadingType::TopToTop,
                LeadingType::BottomToBottom,
                LeadingType::BottomToBottom
            ]
        );
    }

    #[test]
    fn paragraph_ranges_split_unaligned_runs_only_at_paragraph_boundaries() {
        let mut layer = Layer::<u8>::new_text("Split", "One\nTwo").unwrap();
        // A deliberate mid-paragraph split (as some files carry).
        layer.split_paragraph_run(0, 2).unwrap();
        assert_eq!(layer.paragraph_run_lengths(), Some(vec![2, 2, 4]));
        layer
            .paragraph_range(5..6)
            .set_justification(Justification::Right)
            .unwrap();
        assert_eq!(layer.paragraph_run_lengths(), Some(vec![2, 2, 4]));
        assert_eq!(
            layer.paragraph_run(2).unwrap().justification(),
            Some(Justification::Right)
        );
        // Styling "One" covers both of its runs.
        layer
            .paragraph_text("One", Occurrence::All)
            .set_space_before(3.0)
            .unwrap();
        let space: Vec<_> = layer
            .paragraph_runs()
            .iter()
            .map(|run| run.space_before().unwrap())
            .collect();
        assert_eq!(space, [3.0, 3.0, 0.0]);
    }

    #[test]
    fn text_edits_keep_paragraph_runs_on_paragraph_boundaries() {
        let mut layer = Layer::<u8>::new_text("Edit", "One\nTwo\nThree").unwrap();
        layer
            .paragraph_run_mut(1)
            .set_justification(Justification::Center)
            .unwrap();
        layer
            .paragraph_run_mut(2)
            .set_justification(Justification::Right)
            .unwrap();

        // Splitting "Two" into two paragraphs: both halves keep its style.
        layer.replace_text("Two", "Tw\ro").unwrap();
        assert_eq!(layer.text().as_deref(), Some("One\rTw\ro\rThree"));
        assert_eq!(layer.paragraph_run_lengths(), Some(vec![4, 3, 2, 6]));
        let justification = |layer: &Layer<u8>| -> Vec<Justification> {
            layer
                .paragraph_runs()
                .iter()
                .map(|run| run.justification().unwrap())
                .collect()
        };
        assert_eq!(
            justification(&layer),
            [
                Justification::Left,
                Justification::Center,
                Justification::Center,
                Justification::Right
            ]
        );
        // Style runs are untouched by paragraph alignment (14 units + "\r").
        assert_eq!(layer.style_run_lengths(), Some(vec![15]));

        // Joining paragraphs keeps the first paragraph's attributes.
        layer.replace_text("o\rThree", "o Three").unwrap();
        assert_eq!(layer.paragraph_run_lengths(), Some(vec![4, 3, 8]));
        assert_eq!(
            justification(&layer),
            [
                Justification::Left,
                Justification::Center,
                Justification::Center
            ]
        );
        layer.set_text("Single").unwrap();
        assert_eq!(layer.paragraph_run_lengths(), Some(vec![7]));
        assert_eq!(justification(&layer), [Justification::Left]);

        // A deliberately unaligned layout is left alone by text edits.
        let mut layer = Layer::<u8>::new_text("Unaligned", "One\nTwo").unwrap();
        layer.split_paragraph_run(0, 2).unwrap();
        layer.replace_text("Two", "Four").unwrap();
        assert_eq!(layer.paragraph_run_lengths(), Some(vec![2, 2, 5]));
    }
}

// ===========================================================================
// python/psapi-test/text_layer/*.py — contracts beyond the C++ suite
// ===========================================================================
mod python_contracts {
    use super::*;

    #[test]
    fn add_font_unicode_name_round_trips() {
        // test_textlayer_fonts.py::test_add_font_unicode_name
        let name = "\u{65E5}\u{672C}\u{8A9E}\u{30D5}\u{30A9}\u{30F3}\u{30C8}";
        let mut file = open("TextLayers_Basic.psd");
        let id = with_text(&file, "Hello 123");
        let index = file
            .layer_mut(id)
            .unwrap()
            .add_font(name, FontType::OpenType, FontScript::Roman, 0)
            .unwrap();
        assert_eq!(
            file.layer(id).unwrap().font(index).unwrap().postscript_name,
            name
        );
        assert_eq!(file.layer(id).unwrap().font_index(name), Some(index));
        let reread = roundtrip(&file);
        assert_eq!(
            reread
                .layer(id)
                .unwrap()
                .font(index)
                .unwrap()
                .postscript_name,
            name
        );
    }

    #[test]
    fn uniform_scale_sets_both_axes_and_preserves_rotation() {
        // test_textlayer_transform.py::TestUniformScale (`set_scale(factor)`)
        let mut file = open("TextLayers_Basic.psd");
        let id = named(&file, "SimpleASCII");
        let layer = file.layer_mut(id).unwrap();
        layer.set_text_scale(1.5, 1.5).unwrap();
        assert_close(layer.text_scale_x().unwrap(), 1.5, 1e-10);
        assert_close(layer.text_scale_y().unwrap(), 1.5, 1e-10);
        layer.set_text_rotation_angle(35.0).unwrap();
        layer.set_text_scale(2.0, 2.0).unwrap();
        assert_close(layer.text_rotation_angle().unwrap(), 35.0, 1e-6);
        assert_close(layer.text_scale_x().unwrap(), 2.0, 1e-10);
        assert_close(layer.text_scale_y().unwrap(), 2.0, 1e-10);
    }

    #[test]
    fn non_warped_layer_warp_style_is_no_warp() {
        // test_textlayer_transform.py::test_non_warped_layer_warp_style_is_none_like
        let file = open("TextLayers_Warp.psd");
        let layer = file.layer(named(&file, "Secondary")).unwrap();
        assert!(!layer.has_text_warp());
        assert!(matches!(
            layer.text_warp_style(),
            None | Some(TextWarpStyle::NoWarp)
        ));
    }

    #[test]
    fn style_run_mutation_rejects_bad_input_atomically() {
        // test_textlayer.py::test_style_run_mutation_roundtrip
        let mut file = open("TextLayers_StyleRuns.psd");
        let id = with_text(&file, "Alpha Beta Gamma");
        let layer = file.layer_mut(id).unwrap();
        layer
            .style_run_mut(2)
            .set_font_size(48.0)
            .unwrap()
            .set_fill_color([1.0, 0.1, 0.25, 0.75])
            .unwrap();
        let before = layer.clone();
        assert!(layer.style_run_mut(200).set_font_size(48.0).is_err());
        assert_eq!(layer, &before);
        let reread = roundtrip(&file);
        let run = reread.layer(id).unwrap().style_run(2).unwrap();
        assert_close(run.font_size().unwrap(), 48.0, 1e-3);
        assert_all_close(&run.fill_color().unwrap(), &[1.0, 0.1, 0.25, 0.75], 1e-3);
    }
}
