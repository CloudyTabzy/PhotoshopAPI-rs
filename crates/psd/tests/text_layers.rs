use std::path::{Path, PathBuf};

use psd::{
    core::{
        BeReader, BeWriter, Descriptor, DescriptorItem, DescriptorKey, DescriptorValue,
        EngineValue, TaggedBlock, TaggedBlockKey, TypeToolTaggedBlock, UnicodeString,
    },
    FontScript, FontType, LayerKind, LayeredFile, TextBoxBounds, TextShape, TextWarpRotation,
    TextWarpStyle, TextWritingDirection,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/documents")
        .join(name)
}

fn layer_with_text(file: &LayeredFile<u8>, text: &str) -> usize {
    file.layers()
        .position(|layer| layer.text().as_deref() == Some(text))
        .unwrap_or_else(|| panic!("no text layer contains {text:?}"))
}

fn untouched_tysh_regions(data: &[u8]) -> Vec<Vec<u8>> {
    let spans = TypeToolTaggedBlock::text_payload_spans(data).unwrap();
    let engine = spans.engine_data.unwrap();
    let mut touched = vec![spans.text.unwrap(), engine.start - 4..engine.end];
    touched.extend(
        TypeToolTaggedBlock::legacy_range_integer_spans(data)
            .unwrap()
            .into_iter()
            .map(|span| span.range),
    );
    touched.sort_by_key(|range| range.start);

    let mut untouched = Vec::new();
    let mut cursor = 0;
    for range in touched {
        assert!(cursor <= range.start && range.end <= data.len());
        untouched.push(data[cursor..range.start].to_vec());
        cursor = range.end;
    }
    untouched.push(data[cursor..].to_vec());
    untouched
}

fn collect_text_run_spans(
    node: &psd::core::EngineValue,
    engine_units: usize,
    spans: &mut Vec<std::ops::Range<usize>>,
) {
    if let Some(entries) = node.as_dictionary() {
        for (key, child) in entries {
            if key == "RunLengthArray" {
                if let Some(lengths) = child.as_int32_vector() {
                    let total = lengths.iter().try_fold(0usize, |sum, length| {
                        usize::try_from(*length)
                            .ok()
                            .and_then(|length| sum.checked_add(length))
                    });
                    if !lengths.is_empty() && total == Some(engine_units) {
                        spans.push(child.span.clone());
                    }
                }
            }
            collect_text_run_spans(child, engine_units, spans);
        }
    } else if let Some(items) = node.as_array() {
        for item in items {
            collect_text_run_spans(item, engine_units, spans);
        }
    }
}

fn untouched_engine_data_regions(data: &[u8]) -> Vec<Vec<u8>> {
    let root = psd::core::engine_data::parse(data).unwrap();
    let text = root.get_path(["EngineDict", "Editor", "Text"]).unwrap();
    let engine_units =
        psd::core::engine_data::decode_utf16be_literal_units(text.as_literal_bytes().unwrap())
            .unwrap()
            .0
            .len();
    let mut touched = vec![text.span.clone()];
    collect_text_run_spans(&root, engine_units, &mut touched);
    touched.sort_by_key(|range| range.start);

    let mut untouched = Vec::new();
    let mut cursor = 0;
    for range in touched {
        assert!(cursor <= range.start && range.end <= data.len());
        untouched.push(data[cursor..range.start].to_vec());
        cursor = range.end;
    }
    untouched.push(data[cursor..].to_vec());
    untouched
}

#[test]
fn text_layer_fixture_corpus_is_detected_and_round_trips_blocks() {
    let mut aggregate_text_layers = 0;
    for name in [
        "TextLayers/TextLayers_Basic.psd",
        "TextLayers/TextLayers_CharacterStyles.psd",
        "TextLayers/TextLayers_FontFallback.psd",
        "TextLayers/TextLayers_Paragraph.psd",
        "TextLayers/TextLayers_StyleRuns.psd",
        "TextLayers/TextLayers_TextOnPath.psd",
        "TextLayers/TextLayers_Transform.psd",
        "TextLayers/TextLayers_Vertical.psd",
        "TextLayers/TextLayers_VerticalBox.psd",
        "TextLayers/TextLayers_Warp.psd",
    ] {
        let file = LayeredFile::<u8>::read(fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let text_ids: Vec<_> = file
            .layers()
            .enumerate()
            .filter_map(|(id, layer)| layer.is_text_layer().then_some(id))
            .collect();
        assert!(!text_ids.is_empty(), "{name}");
        aggregate_text_layers += text_ids.len();

        for &id in &text_ids {
            let layer = file.layer(id).unwrap();
            assert!(
                matches!(layer.kind, LayerKind::Text(_)),
                "{name}: {}",
                layer.name
            );
            assert!(
                layer.blocks.get(TaggedBlockKey::new(*b"TySh")).is_some()
                    || layer.blocks.get(TaggedBlockKey::new(*b"Txt2")).is_some()
            );
        }

        let bytes = file.to_bytes().unwrap();
        let reread = LayeredFile::<u8>::from_bytes(&bytes).unwrap();
        for &id in &text_ids {
            let before = file.layer(id).unwrap();
            let after = reread
                .layers()
                .find(|candidate| candidate.name == before.name)
                .unwrap_or_else(|| panic!("{name}: lost layer {}", before.name));
            for key in [*b"TySh", *b"Txt2"] {
                assert_eq!(
                    before.blocks.get(TaggedBlockKey::new(key)),
                    after.blocks.get(TaggedBlockKey::new(key)),
                    "{name}: {} {:?}",
                    before.name,
                    key
                );
            }
        }
    }
    assert!(
        aggregate_text_layers >= 16,
        "found {aggregate_text_layers} text layers"
    );
}

#[test]
fn equal_length_and_variable_text_edits_round_trip_and_remap_utf16_runs() {
    let mut basic = LayeredFile::<u8>::read(fixture("TextLayers/TextLayers_Basic.psd")).unwrap();
    let basic_id = layer_with_text(&basic, "Hello 123");
    let unchanged = basic.layer(basic_id).unwrap().clone();
    assert!(basic
        .layer_mut(basic_id)
        .unwrap()
        .set_text_equal_length("different length")
        .is_err());
    assert_eq!(basic.layer(basic_id).unwrap(), &unchanged);
    let original_trailing = TypeToolTaggedBlock::read(&mut BeReader::new(
        &basic
            .layer(basic_id)
            .unwrap()
            .blocks
            .get(TaggedBlockKey::new(*b"TySh"))
            .unwrap()
            .data,
    ))
    .unwrap()
    .trailing;

    basic
        .layer_mut(basic_id)
        .unwrap()
        .set_text_equal_length("Hallo 123")
        .unwrap();
    assert_eq!(
        basic.layer(basic_id).unwrap().text().as_deref(),
        Some("Hallo 123")
    );
    let basic_after = LayeredFile::<u8>::from_bytes(&basic.to_bytes().unwrap()).unwrap();
    let reread_id = layer_with_text(&basic_after, "Hallo 123");
    let reread_tysh = TypeToolTaggedBlock::read(&mut BeReader::new(
        &basic_after
            .layer(reread_id)
            .unwrap()
            .blocks
            .get(TaggedBlockKey::new(*b"TySh"))
            .unwrap()
            .data,
    ))
    .unwrap();
    assert_eq!(reread_tysh.trailing, original_trailing);

    let mut styled =
        LayeredFile::<u8>::read(fixture("TextLayers/TextLayers_StyleRuns.psd")).unwrap();
    let styled_id = layer_with_text(&styled, "Alpha Beta Gamma");
    let original_tysh = styled
        .layer(styled_id)
        .unwrap()
        .blocks
        .get(TaggedBlockKey::new(*b"TySh"))
        .unwrap()
        .data
        .clone();
    assert_eq!(
        styled.layer(styled_id).unwrap().style_run_lengths(),
        Some(vec![5, 1, 4, 1, 6])
    );
    styled
        .layer_mut(styled_id)
        .unwrap()
        .replace_text("Beta", "Betaaaa")
        .unwrap();
    assert_eq!(
        styled.layer(styled_id).unwrap().text().as_deref(),
        Some("Alpha Betaaaa Gamma")
    );
    assert_eq!(
        styled.layer(styled_id).unwrap().style_run_lengths(),
        Some(vec![5, 1, 7, 1, 6])
    );
    let updated_tysh = styled
        .layer(styled_id)
        .unwrap()
        .blocks
        .get(TaggedBlockKey::new(*b"TySh"))
        .unwrap()
        .data
        .clone();
    assert_eq!(
        untouched_tysh_regions(&original_tysh),
        untouched_tysh_regions(&updated_tysh)
    );
    let original_engine = TypeToolTaggedBlock::read(&mut BeReader::new(&original_tysh))
        .unwrap()
        .engine_data_bytes()
        .unwrap()
        .to_vec();
    let updated_engine = TypeToolTaggedBlock::read(&mut BeReader::new(&updated_tysh))
        .unwrap()
        .engine_data_bytes()
        .unwrap()
        .to_vec();
    assert_eq!(
        untouched_engine_data_regions(&original_engine),
        untouched_engine_data_regions(&updated_engine)
    );

    let styled_after = LayeredFile::<u8>::from_bytes(&styled.to_bytes().unwrap()).unwrap();
    let reread_id = layer_with_text(&styled_after, "Alpha Betaaaa Gamma");
    assert_eq!(
        styled_after.layer(reread_id).unwrap().style_run_lengths(),
        Some(vec![5, 1, 7, 1, 6])
    );

    let mut shrinking =
        LayeredFile::<u8>::read(fixture("TextLayers/TextLayers_StyleRuns.psd")).unwrap();
    let shrink_id = layer_with_text(&shrinking, "Alpha Beta Gamma");
    shrinking
        .layer_mut(shrink_id)
        .unwrap()
        .replace_text("Alpha", "A")
        .unwrap();
    assert_eq!(
        shrinking.layer(shrink_id).unwrap().text().as_deref(),
        Some("A Beta Gamma")
    );
    assert_eq!(
        shrinking.layer(shrink_id).unwrap().style_run_lengths(),
        Some(vec![1, 1, 4, 1, 6])
    );
}

#[test]
fn text_editing_uses_utf16_units_and_preserves_engine_data_tail() {
    let mut file = LayeredFile::<u8>::read(fixture("TextLayers/TextLayers_Basic.psd")).unwrap();
    let id = layer_with_text(&file, "Hello 123");
    let layer = file.layer_mut(id).unwrap();
    layer.set_text("漢字🙂 e\u{301} عربي").unwrap();
    assert_eq!(layer.text().as_deref(), Some("漢字🙂 e\u{301} عربي"));
    assert!(layer
        .style_run_lengths()
        .unwrap()
        .iter()
        .all(|length| *length >= 0));
    let expected = layer.text().unwrap();

    let reread = LayeredFile::<u8>::from_bytes(&file.to_bytes().unwrap()).unwrap();
    let id = layer_with_text(&reread, "漢字🙂 e\u{301} عربي");
    assert_eq!(
        reread.layer(id).unwrap().text().as_deref(),
        Some(expected.as_str())
    );
}

#[test]
fn replace_text_defaults_to_all_and_can_select_first_match() {
    let mut all = psd::Layer::<u8>::new_text("All", "aaa").unwrap();
    all.replace_text("a", "b").unwrap();
    assert_eq!(all.text().as_deref(), Some("bbb"));
    all.replace_text("missing", "x").unwrap();
    assert_eq!(all.text().as_deref(), Some("bbb"));

    let mut first = psd::Layer::<u8>::new_text("First", "aaa").unwrap();
    first.replace_text_with_options("a", "b", false).unwrap();
    assert_eq!(first.text().as_deref(), Some("baa"));

    let mut non_overlapping = psd::Layer::<u8>::new_text("NonOverlapping", "aaa").unwrap();
    non_overlapping.replace_text("aa", "x").unwrap();
    assert_eq!(non_overlapping.text().as_deref(), Some("xa"));

    let mut strict = psd::Layer::<u8>::new_text("Strict", "same").unwrap();
    assert!(strict.replace_text_equal_length("same", "longer").is_err());
    assert_eq!(strict.text().as_deref(), Some("same"));
    assert!(strict.replace_text("", "not allowed").is_err());
    assert_eq!(strict.text().as_deref(), Some("same"));
    strict.set_text("").unwrap();
    assert_eq!(strict.text().as_deref(), Some(""));
}

#[test]
fn text_shape_reads_box_and_point_geometry() {
    let file = LayeredFile::<u8>::read(fixture("TextLayers/TextLayers_VerticalBox.psd")).unwrap();
    assert!(file.layers().any(|layer| {
        layer.text_shape() == Some(TextShape::Box)
            && layer.box_width().is_some_and(|width| width > 0.0)
            && layer.box_height().is_some_and(|height| height > 0.0)
    }));
    assert!(file.layers().any(|layer| {
        layer.text_shape() == Some(TextShape::Point) && layer.box_bounds().is_none()
    }));
}

#[test]
fn text_warp_descriptor_is_available_without_rebuilding_tysh() {
    let file = LayeredFile::<u8>::read(fixture("TextLayers/TextLayers_Warp.psd")).unwrap();
    let warp_layer = file
        .layers()
        .find(|layer| layer.text_warp_descriptor().is_some())
        .unwrap();
    let warp = warp_layer.text_warp_descriptor().unwrap();
    assert_eq!(warp.class_id.as_str(), "warp");
    assert!(warp
        .get("warpStyle")
        .and_then(DescriptorValue::as_enum)
        .is_some());
    assert!(warp_layer.text_transform().is_some());
}

#[test]
fn semantic_text_warp_readers_cover_warped_unwarped_and_round_trip_layers() {
    let file = LayeredFile::<u8>::read(fixture("TextLayers/TextLayers_Warp.psd")).unwrap();
    let warped = file.layers().find(|layer| layer.name == "WarpArc").unwrap();
    assert_eq!(warped.text_warp_style(), Some(TextWarpStyle::Arc));
    assert!(warped.has_text_warp());
    assert!((warped.text_warp_value().unwrap() - 28.0).abs() < 0.01);
    assert!(warped.text_warp_horizontal_distortion().unwrap().abs() < 0.01);
    assert!(warped.text_warp_vertical_distortion().unwrap().abs() < 0.01);
    assert_eq!(
        warped.text_warp_rotation(),
        Some(TextWarpRotation::Horizontal)
    );

    let unwarped = file
        .layers()
        .find(|layer| layer.name == "Secondary")
        .unwrap();
    assert_eq!(unwarped.text_warp_style(), Some(TextWarpStyle::NoWarp));
    assert!(!unwarped.has_text_warp());
    assert!(unwarped.text_warp_value().unwrap().abs() < 0.01);

    let reread = LayeredFile::<u8>::from_bytes(&file.to_bytes().unwrap()).unwrap();
    let warped_after = reread
        .layers()
        .find(|layer| layer.name == "WarpArc")
        .unwrap();
    assert_eq!(warped_after.text_warp_style(), Some(TextWarpStyle::Arc));
    assert!(warped_after.has_text_warp());
    assert!((warped_after.text_warp_value().unwrap() - 28.0).abs() < 0.01);
    assert_eq!(
        warped_after.text_warp_rotation(),
        Some(TextWarpRotation::Horizontal)
    );
}

#[test]
fn long_form_enum_ids_read_and_orientation_edits_write_char_ids() {
    // Newer Photoshop versions can write an enumerated value as its long-form
    // string ID (`horizontal`) instead of the char ID (`Hrzn`).
    let mut file = LayeredFile::<u8>::read(fixture("TextLayers/TextLayers_Warp.psd")).unwrap();
    let id = file
        .layers()
        .position(|layer| layer.name == "WarpArc")
        .unwrap();
    let layer = file.layer_mut(id).unwrap();
    let tysh = layer.blocks.get_mut(TaggedBlockKey::new(*b"TySh")).unwrap();
    let mut parsed = TypeToolTaggedBlock::read(&mut BeReader::new(&tysh.data)).unwrap();
    let long_form = |descriptor: &mut Descriptor, key: &str, id: &str| {
        let Some(DescriptorValue::Enumerated { value, .. }) = descriptor.get_mut(key) else {
            panic!("fixture {key} must be an enumerated value");
        };
        *value = DescriptorKey::new(id);
    };
    long_form(&mut parsed.warp, "warpRotate", "vertical");
    long_form(&mut parsed.text, "Ornt", "horizontal");
    long_form(&mut parsed.text, "AntA", "antiAliasSmooth");
    let mut writer = BeWriter::new();
    parsed.write(&mut writer).unwrap();
    tysh.data = writer.into_inner();

    assert_eq!(layer.text_warp_rotation(), Some(TextWarpRotation::Vertical));
    assert_eq!(layer.anti_alias(), Some(psd::AntiAliasMethod::Smooth));

    layer
        .set_orientation(TextWritingDirection::Vertical)
        .unwrap();
    let tysh = layer.blocks.get(TaggedBlockKey::new(*b"TySh")).unwrap();
    let edited = TypeToolTaggedBlock::read(&mut BeReader::new(&tysh.data)).unwrap();
    let (_, ornt) = edited.text.get("Ornt").unwrap().as_enum().unwrap();
    assert_eq!(ornt.as_bytes(), b"Vrtc");
    assert!(ornt.uses_implicit_length());
    let (_, rotate) = edited.warp.get("warpRotate").unwrap().as_enum().unwrap();
    assert_eq!(rotate.as_bytes(), b"vertical");

    let reread = LayeredFile::<u8>::from_bytes(&file.to_bytes().unwrap()).unwrap();
    let layer = reread.layer(id).unwrap();
    assert_eq!(layer.text_warp_rotation(), Some(TextWarpRotation::Vertical));
    assert_eq!(layer.orientation(), Some(TextWritingDirection::Vertical));
    assert_eq!(layer.anti_alias(), Some(psd::AntiAliasMethod::Smooth));
}

#[test]
fn unknown_text_warp_identifiers_are_exposed_without_rewriting_raw_tysh() {
    let mut file = LayeredFile::<u8>::read(fixture("TextLayers/TextLayers_Warp.psd")).unwrap();
    let id = file
        .layers()
        .position(|layer| layer.name == "WarpArc")
        .unwrap();
    let layer = file.layer_mut(id).unwrap();
    let tysh = layer.blocks.get_mut(TaggedBlockKey::new(*b"TySh")).unwrap();
    let mut parsed = TypeToolTaggedBlock::read(&mut BeReader::new(&tysh.data)).unwrap();
    let Some(DescriptorValue::Enumerated { value, .. }) = parsed.warp.get_mut("warpStyle") else {
        panic!("fixture warpStyle must be an enumerated value");
    };
    *value = value.replace_text_preserving_encoding("warpOdd").unwrap();
    let mut writer = BeWriter::new();
    parsed.write(&mut writer).unwrap();
    tysh.data = writer.into_inner();
    let expected_raw_tysh = tysh.data.clone();

    assert_eq!(
        layer.text_warp_style(),
        Some(TextWarpStyle::Other(b"warpOdd".to_vec()))
    );
    assert!(layer.has_text_warp());

    let reread = LayeredFile::<u8>::from_bytes(&file.to_bytes().unwrap()).unwrap();
    let reread_layer = &reread.layer(id).unwrap();
    assert_eq!(
        reread_layer.text_warp_style(),
        Some(TextWarpStyle::Other(b"warpOdd".to_vec()))
    );
    assert_eq!(
        reread_layer
            .blocks
            .get(TaggedBlockKey::new(*b"TySh"))
            .unwrap()
            .data,
        expected_raw_tysh
    );
}

#[test]
fn duplicate_tysh_blocks_are_all_edited_and_txt2_only_stays_opaque() {
    let mut file = LayeredFile::<u8>::new(psd::core::ColorMode::Rgb, 32, 32).unwrap();
    let mut first = psd::Layer::<u8>::new_text("Duplicates", "First").unwrap();
    let second = psd::Layer::<u8>::new_text("Duplicates", "Second").unwrap();
    first.blocks.push(
        second
            .blocks
            .get(TaggedBlockKey::new(*b"TySh"))
            .unwrap()
            .clone(),
    );
    first.blocks.push(TaggedBlock::new(
        TaggedBlockKey::new(*b"Txt2"),
        b"opaque".to_vec(),
    ));
    let id = file.add_layer(first);
    let mut file = LayeredFile::<u8>::from_bytes(&file.to_bytes().unwrap()).unwrap();
    assert_eq!(file.layer(id).unwrap().text().as_deref(), Some("First"));
    let opaque_before = file
        .layer(id)
        .unwrap()
        .blocks
        .get(TaggedBlockKey::new(*b"Txt2"))
        .unwrap()
        .data
        .clone();
    file.layer_mut(id).unwrap().set_text("Updated").unwrap();
    let layer = file.layer(id).unwrap();
    let tysh_count = layer
        .blocks
        .blocks
        .iter()
        .filter(|block| block.key == TaggedBlockKey::new(*b"TySh"))
        .count();
    assert_eq!(tysh_count, 2);
    for block in layer
        .blocks
        .blocks
        .iter()
        .filter(|block| block.key == TaggedBlockKey::new(*b"TySh"))
    {
        let parsed = TypeToolTaggedBlock::read(&mut BeReader::new(&block.data)).unwrap();
        let Some(DescriptorValue::String(text)) = parsed.text.get("Txt ") else {
            panic!("duplicate TySh lost its Txt payload");
        };
        assert_eq!(text.value(), "Updated");
    }
    assert_eq!(
        layer
            .blocks
            .get(TaggedBlockKey::new(*b"Txt2"))
            .unwrap()
            .data,
        opaque_before
    );

    let mut txt2_only = LayeredFile::<u8>::new(psd::core::ColorMode::Rgb, 32, 32).unwrap();
    let mut layer = psd::Layer::<u8>::new_image("Txt2 only", psd::Rect::default());
    layer.blocks.push(TaggedBlock::new(
        TaggedBlockKey::new(*b"Txt2"),
        vec![0xDE, 0xAD],
    ));
    let id = txt2_only.add_layer(layer);
    let mut txt2_only = LayeredFile::<u8>::from_bytes(&txt2_only.to_bytes().unwrap()).unwrap();
    assert!(txt2_only.layer(id).unwrap().is_text_layer());
    assert!(txt2_only.layer(id).unwrap().text().is_none());
    let before = txt2_only.layer(id).unwrap().clone();
    assert!(txt2_only
        .layer_mut(id)
        .unwrap()
        .set_text("Cannot edit")
        .is_err());
    assert_eq!(txt2_only.layer(id).unwrap(), &before);
}

#[test]
fn multi_tysh_edit_is_failure_atomic_when_a_later_engine_data_tree_is_invalid() {
    let mut file = LayeredFile::<u8>::new(psd::core::ColorMode::Rgb, 32, 32).unwrap();
    let mut first = psd::Layer::<u8>::new_text("Atomic", "First").unwrap();
    let mut second = psd::Layer::<u8>::new_text("Atomic", "Second").unwrap();
    let second_tysh = second
        .blocks
        .get_mut(TaggedBlockKey::new(*b"TySh"))
        .unwrap();
    let mut parsed = TypeToolTaggedBlock::read(&mut BeReader::new(&second_tysh.data)).unwrap();
    let Some(DescriptorValue::RawData { data, .. }) = parsed.text.get_mut("EngineData") else {
        panic!("builder did not produce an EngineData item");
    };
    *data = psd::core::engine_data::serialize(&EngineValue::dict());
    let mut writer = BeWriter::new();
    parsed.write(&mut writer).unwrap();
    second_tysh.data = writer.into_inner();
    first.blocks.push(second_tysh.clone());

    let id = file.add_layer(first);
    let layer = file.layer_mut(id).unwrap();
    let before = layer.clone();
    assert!(layer.set_text("Updated").is_err());
    assert_eq!(layer, &before);
}

#[test]
fn style_font_split_transform_and_box_mutations_round_trip() {
    let mut styled =
        LayeredFile::<u8>::read(fixture("TextLayers/TextLayers_StyleRuns.psd")).unwrap();
    let id = layer_with_text(&styled, "Alpha Beta Gamma");
    assert_eq!(styled.layer(id).unwrap().style_run_count(), 5);
    styled.layer_mut(id).unwrap().split_style_run(2, 2).unwrap();
    assert_eq!(styled.layer(id).unwrap().style_run_count(), 6);
    assert_eq!(
        styled.layer(id).unwrap().style_run_lengths(),
        Some(vec![5, 1, 2, 2, 1, 6])
    );

    let before_bold = styled
        .layer(id)
        .unwrap()
        .style_run(0)
        .and_then(|style| style.property("FauxBold").cloned())
        .unwrap()
        .as_bool()
        .unwrap();
    styled
        .layer_mut(id)
        .unwrap()
        .style_run_mut(0)
        .set_property("FauxBold", EngineValue::boolean(!before_bold))
        .map(|_| ())
        .unwrap();
    assert_eq!(
        styled
            .layer(id)
            .unwrap()
            .style_run(0)
            .and_then(|style| style.property("FauxBold").cloned())
            .unwrap()
            .as_bool(),
        Some(!before_bold)
    );
    styled
        .layer_mut(id)
        .unwrap()
        .set_font("Phase3-Uniform-Font")
        .unwrap();
    let uniform_font_index = styled
        .layer(id)
        .unwrap()
        .font_index("Phase3-Uniform-Font")
        .unwrap();
    assert_eq!(
        styled
            .layer(id)
            .unwrap()
            .font(uniform_font_index)
            .unwrap()
            .font_type,
        FontType::OpenType
    );
    styled
        .layer_mut(id)
        .unwrap()
        .style_normal_mut()
        .set_property("FontSize", EngineValue::number(22.0))
        .map(|_| ())
        .unwrap();
    assert_eq!(
        styled.layer(id).unwrap().primary_font_name().as_deref(),
        Some("Phase3-Uniform-Font")
    );
    let styled_after = LayeredFile::<u8>::from_bytes(&styled.to_bytes().unwrap()).unwrap();
    let reread_style_id = layer_with_text(&styled_after, "Alpha Beta Gamma");
    assert_eq!(
        styled_after
            .layer(reread_style_id)
            .unwrap()
            .style_run_count(),
        6
    );
    assert_eq!(
        styled_after
            .layer(reread_style_id)
            .unwrap()
            .style_normal()
            .and_then(|style| style.property("FontSize").cloned())
            .and_then(|value| value.as_double()),
        Some(22.0)
    );
    assert_eq!(
        styled_after
            .layer(reread_style_id)
            .unwrap()
            .primary_font_name()
            .as_deref(),
        Some("Phase3-Uniform-Font")
    );

    let mut fonts =
        LayeredFile::<u8>::read(fixture("TextLayers/TextLayers_FontFallback.psd")).unwrap();
    let font_layer = fonts
        .layers()
        .position(|layer| layer.font_count() > 0)
        .unwrap();
    let old_count = fonts.layer(font_layer).unwrap().font_count();
    fonts
        .layer_mut(font_layer)
        .unwrap()
        .rename_font(0, "Phase3-Renamed-Font")
        .unwrap();
    let new_index = fonts
        .layer_mut(font_layer)
        .unwrap()
        .add_font(
            "Phase3-Added-Font",
            FontType::TrueType,
            FontScript::Roman,
            0,
        )
        .unwrap();
    assert_eq!(new_index, old_count);
    assert_eq!(
        fonts
            .layer(font_layer)
            .unwrap()
            .font_index("Phase3-Added-Font"),
        Some(new_index)
    );
    let fonts_after = LayeredFile::<u8>::from_bytes(&fonts.to_bytes().unwrap()).unwrap();
    let reread_font_layer = fonts_after
        .layers()
        .position(|layer| layer.name == fonts.layer(font_layer).unwrap().name)
        .unwrap();
    assert_eq!(
        fonts_after
            .layer(reread_font_layer)
            .unwrap()
            .font_index("Phase3-Added-Font"),
        Some(new_index)
    );

    let mut paragraph =
        LayeredFile::<u8>::read(fixture("TextLayers/TextLayers_Paragraph.psd")).unwrap();
    let paragraph_layer = paragraph
        .layers()
        .position(|layer| layer.paragraph_run_count() > 0)
        .unwrap();
    let paragraph_lengths = paragraph
        .layer(paragraph_layer)
        .unwrap()
        .paragraph_run_lengths()
        .unwrap();
    if let Some((run, _)) = paragraph_lengths
        .iter()
        .enumerate()
        .find(|(_, length)| **length > 1)
    {
        paragraph
            .layer_mut(paragraph_layer)
            .unwrap()
            .split_paragraph_run(run, 1)
            .unwrap();
        assert_eq!(
            paragraph
                .layer(paragraph_layer)
                .unwrap()
                .paragraph_run_count(),
            paragraph_lengths.len() + 1
        );
    } else {
        panic!("paragraph fixture has no splittable paragraph run");
    }
    paragraph
        .layer_mut(paragraph_layer)
        .unwrap()
        .paragraph_normal_mut()
        .set_property("SpaceAfter", EngineValue::number(4.5))
        .map(|_| ())
        .unwrap();
    let paragraph_after = LayeredFile::<u8>::from_bytes(&paragraph.to_bytes().unwrap()).unwrap();
    assert_eq!(
        paragraph_after
            .layer(paragraph_layer)
            .unwrap()
            .paragraph_normal()
            .and_then(|style| style.property("SpaceAfter").cloned())
            .and_then(|value| value.as_double()),
        Some(4.5)
    );

    let mut geometry =
        LayeredFile::<u8>::read(fixture("TextLayers/TextLayers_VerticalBox.psd")).unwrap();
    let box_layer = geometry
        .layers()
        .position(|layer| layer.text_shape() == Some(TextShape::Box))
        .unwrap();
    geometry
        .layer_mut(box_layer)
        .unwrap()
        .set_box_bounds(TextBoxBounds {
            top: 10.0,
            left: 20.0,
            bottom: 110.0,
            right: 220.0,
        })
        .unwrap();
    let before = geometry.layer(box_layer).unwrap().text_transform().unwrap();
    geometry
        .layer_mut(box_layer)
        .unwrap()
        .set_text_transform_component(4, before[4] + 5.0)
        .unwrap();
    assert_eq!(geometry.layer(box_layer).unwrap().box_width(), Some(200.0));
    assert_eq!(
        geometry.layer(box_layer).unwrap().text_transform().unwrap()[4],
        before[4] + 5.0
    );

    let reread = LayeredFile::<u8>::from_bytes(&geometry.to_bytes().unwrap()).unwrap();
    let box_layer = reread
        .layers()
        .find(|layer| layer.text_shape() == Some(TextShape::Box))
        .unwrap();
    assert_eq!(
        box_layer.box_bounds(),
        Some(TextBoxBounds {
            top: 10.0,
            left: 20.0,
            bottom: 110.0,
            right: 220.0
        })
    );
}

#[test]
fn variable_text_edits_remap_legacy_descriptor_ranges() {
    let mut file = LayeredFile::<u8>::read(fixture("TextLayers/TextLayers_Basic.psd")).unwrap();
    let id = layer_with_text(&file, "Hello 123");
    let layer = file.layer_mut(id).unwrap();
    let tysh = layer.blocks.get_mut(TaggedBlockKey::new(*b"TySh")).unwrap();
    let mut parsed = TypeToolTaggedBlock::read(&mut BeReader::new(&tysh.data)).unwrap();
    parsed.text.items.push(DescriptorItem {
        key: DescriptorKey::new("From"),
        value: DescriptorValue::Integer(2),
    });
    parsed.text.items.push(DescriptorItem {
        key: DescriptorKey::new("T   "),
        value: DescriptorValue::Integer(8),
    });
    parsed.text.items.push(DescriptorItem {
        key: DescriptorKey::new("Nested"),
        value: DescriptorValue::Descriptor(Descriptor {
            name: UnicodeString::new("", 1).unwrap(),
            class_id: DescriptorKey::new("test"),
            items: vec![DescriptorItem {
                key: DescriptorKey::new("From"),
                value: DescriptorValue::Integer(8),
            }],
        }),
    });
    let mut writer = BeWriter::new();
    parsed.write(&mut writer).unwrap();
    tysh.data = writer.into_inner();

    layer.replace_text("Hello", "Greetings").unwrap();
    assert_eq!(layer.text().as_deref(), Some("Greetings 123"));
    let parsed = TypeToolTaggedBlock::read(&mut BeReader::new(
        &layer
            .blocks
            .get(TaggedBlockKey::new(*b"TySh"))
            .unwrap()
            .data,
    ))
    .unwrap();
    assert!(matches!(
        parsed.text.get("From"),
        Some(DescriptorValue::Integer(2))
    ));
    assert!(matches!(
        parsed.text.get("T   "),
        Some(DescriptorValue::Integer(12))
    ));
    let Some(DescriptorValue::Descriptor(nested)) = parsed.text.get("Nested") else {
        panic!("nested range descriptor was lost");
    };
    assert!(matches!(
        nested.get("From"),
        Some(DescriptorValue::Integer(12))
    ));
}

#[test]
fn minimal_text_builder_creates_editable_engine_data_and_round_trips() {
    let mut file = LayeredFile::<u8>::new(psd::core::ColorMode::Rgb, 64, 64).unwrap();
    let layer = psd::Layer::<u8>::new_text("New title", "Hello\nWorld").unwrap();
    let id = file.add_layer(layer);
    assert_eq!(
        file.layer(id).unwrap().text().as_deref(),
        Some("Hello\rWorld")
    );
    assert_eq!(file.layer(id).unwrap().style_run_count(), 1);
    assert_eq!(file.layer(id).unwrap().style_run_lengths(), Some(vec![12]));
    // Like Photoshop, the builder pairs the requested font with the
    // AdobeInvisFont sentinel.
    assert_eq!(file.layer(id).unwrap().font_count(), 2);
    assert_eq!(
        file.layer(id).unwrap().font(0).unwrap().font_type,
        FontType::TrueType
    );
    assert!(file.layer(id).unwrap().is_sentinel_font(1));
    assert_eq!(
        file.layer(id)
            .unwrap()
            .style_run(0)
            .and_then(|style| style.property("FillColor").cloned())
            .unwrap()
            .get("Values")
            .unwrap()
            .as_double_vector(),
        Some(vec![1.0, 0.0, 0.0, 0.0])
    );
    let before_invalid_key = file.layer(id).unwrap().clone();
    assert!(file
        .layer_mut(id)
        .unwrap()
        .style_run_mut(0)
        .set_property("Bad Key", EngineValue::boolean(true))
        .map(|_| ())
        .is_err());
    assert_eq!(file.layer(id).unwrap(), &before_invalid_key);
    file.layer_mut(id)
        .unwrap()
        .style_run_mut(0)
        .set_property("NoBreak", EngineValue::boolean(true))
        .map(|_| ())
        .unwrap();
    file.layer_mut(id)
        .unwrap()
        .style_normal_mut()
        .set_property("NoBreak", EngineValue::boolean(true))
        .map(|_| ())
        .unwrap();
    file.layer_mut(id)
        .unwrap()
        .paragraph_run_mut(0)
        .set_property("Hanging", EngineValue::boolean(true))
        .map(|_| ())
        .unwrap();
    file.layer_mut(id)
        .unwrap()
        .paragraph_normal_mut()
        .set_property("Burasagari", EngineValue::boolean(true))
        .map(|_| ())
        .unwrap();
    assert_eq!(
        file.layer(id).unwrap().orientation(),
        Some(TextWritingDirection::Horizontal)
    );
    file.layer_mut(id)
        .unwrap()
        .set_orientation(TextWritingDirection::Vertical)
        .unwrap();
    assert!(file.layer(id).unwrap().is_vertical());

    let reread = LayeredFile::<u8>::from_bytes(&file.to_bytes().unwrap()).unwrap();
    let id = reread.find_layer("New title").unwrap();
    assert_eq!(
        reread.layer(id).unwrap().text().as_deref(),
        Some("Hello\rWorld")
    );
    assert_eq!(
        reread.layer(id).unwrap().orientation(),
        Some(TextWritingDirection::Vertical)
    );
    assert_eq!(
        reread
            .layer(id)
            .unwrap()
            .style_run(0)
            .and_then(|style| style.property("NoBreak").cloned())
            .unwrap()
            .as_bool(),
        Some(true)
    );
    assert_eq!(
        reread
            .layer(id)
            .unwrap()
            .style_normal()
            .and_then(|style| style.property("NoBreak").cloned())
            .unwrap()
            .as_bool(),
        Some(true)
    );
    assert_eq!(
        reread
            .layer(id)
            .unwrap()
            .paragraph_run(0)
            .and_then(|style| style.property("Hanging").cloned())
            .unwrap()
            .as_bool(),
        Some(true)
    );
    assert_eq!(
        reread
            .layer(id)
            .unwrap()
            .paragraph_normal()
            .and_then(|style| style.property("Burasagari").cloned())
            .unwrap()
            .as_bool(),
        Some(true)
    );

    let mut sixteen = LayeredFile::<u16>::new(psd::core::ColorMode::Rgb, 8, 8).unwrap();
    let id = sixteen.add_layer(psd::Layer::<u16>::new_text("16-bit", "Sixteen").unwrap());
    sixteen
        .layer_mut(id)
        .unwrap()
        .set_text("Sixteen edited")
        .unwrap();
    let reread = LayeredFile::<u16>::from_bytes(&sixteen.to_bytes().unwrap()).unwrap();
    assert_eq!(
        reread.layer(id).unwrap().text().as_deref(),
        Some("Sixteen edited")
    );

    let mut thirty_two = LayeredFile::<f32>::new(psd::core::ColorMode::Rgb, 8, 8).unwrap();
    let id = thirty_two.add_layer(psd::Layer::<f32>::new_text("32-bit", "Thirty two").unwrap());
    thirty_two
        .layer_mut(id)
        .unwrap()
        .set_text("Thirty two edited")
        .unwrap();
    let reread = LayeredFile::<f32>::from_bytes(&thirty_two.to_bytes().unwrap()).unwrap();
    assert_eq!(
        reread.layer(id).unwrap().text().as_deref(),
        Some("Thirty two edited")
    );

    let mut shape_layer = psd::Layer::<u8>::new_text("Shape", "text").unwrap();
    assert_eq!(shape_layer.text_shape(), Some(TextShape::Box));
    shape_layer.convert_to_point_text().unwrap();
    assert_eq!(shape_layer.text_shape(), Some(TextShape::Point));
    shape_layer.convert_to_box_text(200.0, 100.0).unwrap();
    assert_eq!(shape_layer.text_shape(), Some(TextShape::Box));
    assert_eq!(
        shape_layer.box_bounds(),
        Some(TextBoxBounds {
            top: 0.0,
            left: 0.0,
            bottom: 100.0,
            right: 200.0
        })
    );
    shape_layer.convert_to_point_text().unwrap();
    assert_eq!(shape_layer.text_shape(), Some(TextShape::Point));
    assert_eq!(shape_layer.box_bounds(), None);
}

#[test]
fn run_splits_apply_to_every_tysh_block_atomically() {
    fn block_run_lengths(layer: &psd::Layer<u8>, path: [&str; 3]) -> Vec<Vec<i32>> {
        layer
            .blocks
            .blocks
            .iter()
            .filter(|block| block.key == TaggedBlockKey::new(*b"TySh"))
            .map(|block| {
                TypeToolTaggedBlock::read(&mut BeReader::new(&block.data))
                    .unwrap()
                    .engine_data()
                    .unwrap()
                    .unwrap()
                    .get_path(path)
                    .unwrap()
                    .as_int32_vector()
                    .unwrap()
            })
            .collect()
    }
    const STYLE: [&str; 3] = ["EngineDict", "StyleRun", "RunLengthArray"];
    const PARAGRAPH: [&str; 3] = ["EngineDict", "ParagraphRun", "RunLengthArray"];

    let mut layer = psd::Layer::<u8>::new_text("Duplicates", "Hello World").unwrap();
    let second = psd::Layer::<u8>::new_text("Duplicates", "Hi").unwrap();
    layer.blocks.push(
        second
            .blocks
            .get(TaggedBlockKey::new(*b"TySh"))
            .unwrap()
            .clone(),
    );
    assert_eq!(block_run_lengths(&layer, STYLE), vec![vec![12], vec![3]]);

    // Offset 5 is valid for the first block but not the second: nothing changes.
    let before = layer.clone();
    assert!(layer.split_style_run(0, 5).is_err());
    assert!(layer.split_paragraph_run(0, 5).is_err());
    assert_eq!(layer, before);

    layer.split_style_run(0, 2).unwrap();
    assert_eq!(
        block_run_lengths(&layer, STYLE),
        vec![vec![2, 10], vec![2, 1]]
    );
    layer.split_paragraph_run(0, 1).unwrap();
    assert_eq!(
        block_run_lengths(&layer, PARAGRAPH),
        vec![vec![1, 11], vec![1, 2]]
    );
}

/// A builder text layer whose `TxLr` descriptor gains a trailing item with an
/// OSType no parser knows (so its length is unknowable).
fn layer_with_unknown_descriptor_value() -> (psd::Layer<u8>, Vec<u8>) {
    let mut layer = psd::Layer::<u8>::new_text("Unknown", "Hello World").unwrap();
    let tysh = layer.blocks.get_mut(TaggedBlockKey::new(*b"TySh")).unwrap();
    let spans = TypeToolTaggedBlock::descriptor_spans(&tysh.data).unwrap();
    // Builder descriptors start with a one-unit NUL name (6 bytes) and the
    // implicit `TxLr` class id (8 bytes), then the item count.
    let count_at = spans.text.start + 14;
    let count = u32::from_be_bytes(tysh.data[count_at..count_at + 4].try_into().unwrap());
    tysh.data[count_at..count_at + 4].copy_from_slice(&(count + 1).to_be_bytes());
    let unknown = b"\0\0\0\0ZzzzQqqq\xDE\xAD\xBE\xEF".to_vec();
    tysh.data
        .splice(spans.text.end..spans.text.end, unknown.iter().copied());
    (layer, unknown)
}

#[test]
fn text_stays_editable_next_to_an_unknown_descriptor_ostype() {
    let (mut layer, unknown) = layer_with_unknown_descriptor_value();
    let tysh = |layer: &psd::Layer<u8>| {
        layer
            .blocks
            .get(TaggedBlockKey::new(*b"TySh"))
            .unwrap()
            .data
            .clone()
    };
    assert!(TypeToolTaggedBlock::read(&mut BeReader::new(&tysh(&layer))).is_err());

    // Text, EngineData, and the transform are still reachable.
    assert_eq!(layer.text().as_deref(), Some("Hello World"));
    assert_eq!(layer.style_run_lengths(), Some(vec![12]));
    assert_eq!(layer.text_position(), Some((20.0, 50.0)));

    layer.replace_text("World", "Rust").unwrap();
    layer.style_run_mut(0).set_faux_bold(true).unwrap();
    layer.split_style_run(0, 6).unwrap();
    layer.set_text_position(5.0, 6.0).unwrap();
    layer
        .set_orientation(TextWritingDirection::Vertical)
        .unwrap();
    assert_eq!(layer.text().as_deref(), Some("Hello Rust"));
    assert_eq!(layer.style_run_lengths(), Some(vec![6, 5]));
    assert_eq!(layer.orientation(), Some(TextWritingDirection::Vertical));

    // Descriptor-backed settings need the whole descriptor and fail closed.
    let before = layer.clone();
    assert!(layer.set_text_warp_value(1.0).is_err());
    assert!(layer.set_anti_alias(&psd::AntiAliasMethod::Sharp).is_err());
    assert_eq!(layer.text_warp_style(), None);
    assert_eq!(layer, before);

    // The unknown item survives every edit and a file roundtrip verbatim.
    let data = tysh(&layer);
    assert!(data.windows(unknown.len()).any(|window| window == unknown));
    let mut file = LayeredFile::<u8>::new(psd::core::ColorMode::Rgb, 16, 16).unwrap();
    let id = file.add_layer(layer);
    let reread = LayeredFile::<u8>::from_bytes(&file.to_bytes().unwrap()).unwrap();
    let layer = reread.layer(id).unwrap();
    assert_eq!(tysh(layer), data);
    assert_eq!(layer.text().as_deref(), Some("Hello Rust"));
}

#[test]
fn text_on_path_fixture_is_point_text_and_round_trips() {
    // The upstream "TextOnPath" fixture holds ordinary point text (its
    // generator could not script type-on-path); no upstream test reads it.
    let mut file =
        LayeredFile::<u8>::read(fixture("TextLayers/TextLayers_TextOnPath.psd")).unwrap();
    let names: Vec<_> = file
        .layers()
        .filter(|layer| layer.is_text_layer())
        .map(|layer| layer.name.clone())
        .collect();
    assert_eq!(names, ["TextOnPathMain", "PathControlText"]);
    let id = file
        .layers()
        .position(|layer| layer.name == "TextOnPathMain")
        .unwrap();
    let layer = file.layer(id).unwrap();
    assert_eq!(layer.text().as_deref(), Some("Text On Path Fixture"));
    assert_eq!(layer.text_shape(), Some(TextShape::Point));
    assert!(!layer.has_text_warp());
    assert_eq!(layer.orientation(), Some(TextWritingDirection::Horizontal));

    let raw = file.to_bytes().unwrap();
    let reread = LayeredFile::<u8>::from_bytes(&raw).unwrap();
    assert!(reread.layers().eq(file.layers()));

    file.layer_mut(id)
        .unwrap()
        .replace_text("Path", "Curve")
        .unwrap();
    let reread = LayeredFile::<u8>::from_bytes(&file.to_bytes().unwrap()).unwrap();
    assert_eq!(
        reread.layer(id).unwrap().text().as_deref(),
        Some("Text On Curve Fixture")
    );
}

#[test]
fn text_layers_round_trip_through_a_psb_container() {
    let mut file = LayeredFile::<u8>::read(fixture("TextLayers/TextLayers_StyleRuns.psd")).unwrap();
    file.version = psd::core::Version::Psb;
    let id = layer_with_text(&file, "Alpha Beta Gamma");
    file.layer_mut(id)
        .unwrap()
        .replace_text("Beta", "Betaaaa")
        .unwrap();
    let psb = file.to_bytes().unwrap();
    assert_eq!(&psb[..6], b"8BPS\0\x02");
    let reread = LayeredFile::<u8>::from_bytes(&psb).unwrap();
    assert_eq!(reread.version, psd::core::Version::Psb);
    assert!(reread.layers().eq(file.layers()));
    let layer = reread.layer(id).unwrap();
    assert_eq!(layer.text().as_deref(), Some("Alpha Betaaaa Gamma"));
    assert_eq!(layer.style_run_lengths(), Some(vec![5, 1, 7, 1, 6]));
}
