//! Editing a layer's effects through the typed set.
//!
//! The synthetic tests author effects on a new layer, edit them, and write and re-read the
//! document. The corpus test is the strong one: for every layer in every real document that has
//! effects, setting the layer's own effects back must change nothing, mirror blocks included.
//!
//! `fixtures/` always runs. Set `PSD_EXTRA_CORPUS` to a list of directories (separated as
//! `PATH` is) to sweep more documents.

use std::path::{Path, PathBuf};

use psd::core::{
    effects_block_data, Bevel, ColorMode, ColorOverlay, DescriptorValue, Glow, GlowKind,
    GradientOverlay, LayerEffects, LayerEffectsBlock, LayerEffectsData, Satin, Shadow, ShadowKind,
    Stroke, TaggedBlock, TaggedBlockKey, Version,
};
use psd::{BitDepth, Layer, LayeredFile, Rect};

fn keys<T: BitDepth>(layer: &Layer<T>) -> Vec<String> {
    layer.blocks.blocks.iter().map(|b| b.key.as_str()).collect()
}

fn full_set() -> LayerEffects {
    LayerEffects {
        drop_shadows: vec![Shadow::new(ShadowKind::Drop)],
        inner_shadows: vec![Shadow::new(ShadowKind::Inner)],
        outer_glow: Some(Glow::new(GlowKind::Outer)),
        inner_glow: Some(Glow::new(GlowKind::Inner)),
        bevel: Some(Bevel::default()),
        color_overlays: vec![ColorOverlay::default()],
        satin: Some(Satin::default()),
        gradient_overlays: vec![GradientOverlay::default()],
        strokes: vec![Stroke::default()],
        ..LayerEffects::default()
    }
}

/// A one-layer document, the layer carrying a name block like Photoshop's layers do.
fn document() -> LayeredFile<u8> {
    let mut file = LayeredFile::<u8>::new(ColorMode::Rgb, 8, 8).unwrap();
    let mut layer = Layer::<u8>::new_image("Styled", Rect::new(0, 0, 8, 8));
    layer
        .blocks
        .push(TaggedBlock::new(TaggedBlockKey::new(*b"luni"), vec![0; 4]));
    file.add_layer(layer);
    file
}

fn styled_layer(file: &LayeredFile<u8>) -> &Layer<u8> {
    file.layers().find(|l| l.name == "Styled").unwrap()
}

fn reread(file: &LayeredFile<u8>) -> LayeredFile<u8> {
    LayeredFile::<u8>::from_bytes(&file.to_bytes().unwrap()).unwrap()
}

fn edit(file: &mut LayeredFile<u8>, change: impl FnOnce(&mut Layer<u8>)) {
    let id = file.find_layer("Styled").unwrap();
    change(file.layer_mut(id).unwrap());
}

#[test]
fn a_layer_without_effects_has_no_set() {
    let file = document();
    assert_eq!(styled_layer(&file).layer_effects().unwrap(), None);
}

#[test]
fn a_new_set_is_stored_before_the_name_block_and_survives_a_save() {
    let mut file = document();
    let effects = full_set();
    edit(&mut file, |layer| {
        layer.set_layer_effects(&effects).unwrap()
    });

    let layer = styled_layer(&file);
    assert_eq!(
        keys(layer),
        ["lfx2", "lrFX", "luni"],
        "the effects block and its legacy mirror go before `luni`"
    );
    assert_eq!(layer.blocks.blocks[0].data.len() % 4, 0);

    let back = reread(&file);
    assert_eq!(styled_layer(&back).layer_effects().unwrap(), Some(effects));
}

#[test]
fn a_new_set_with_several_instances_is_stored_as_lmfx() {
    let mut file = document();
    let mut effects = full_set();
    effects.strokes.push(Stroke {
        size: Some(9.0),
        ..Stroke::default()
    });
    edit(&mut file, |layer| {
        layer.set_layer_effects(&effects).unwrap()
    });
    assert_eq!(keys(styled_layer(&file)), ["lmfx", "lrFX", "luni"]);
    assert_eq!(
        styled_layer(&reread(&file)).layer_effects().unwrap(),
        Some(effects)
    );
}

#[test]
fn an_existing_block_keeps_its_key_whatever_the_edit_does_to_the_instance_count() {
    // Photoshop-authored files hold several instances in `lfx2` as well as in `lmfx`, so an
    // edit does not move a layer from one to the other.
    let mut file = document();
    edit(&mut file, |layer| {
        layer.set_layer_effects(&full_set()).unwrap()
    });
    edit(&mut file, |layer| {
        let mut effects = layer.layer_effects().unwrap().unwrap();
        effects.strokes.push(Stroke::default());
        layer.set_layer_effects(&effects).unwrap();
    });
    assert_eq!(keys(styled_layer(&file)), ["lfx2", "lrFX", "luni"]);
    let effects = styled_layer(&reread(&file))
        .layer_effects()
        .unwrap()
        .unwrap();
    assert_eq!(effects.strokes.len(), 2);

    // And the same for `lmfx`, down to one instance.
    let mut file = document();
    let mut effects = full_set();
    effects.strokes.push(Stroke::default());
    edit(&mut file, |layer| {
        layer.set_layer_effects(&effects).unwrap()
    });
    effects.strokes.pop();
    edit(&mut file, |layer| {
        layer.set_layer_effects(&effects).unwrap()
    });
    assert_eq!(keys(styled_layer(&file)), ["lmfx", "lrFX", "luni"]);
    assert_eq!(styled_layer(&file).layer_effects().unwrap(), Some(effects));
}

#[test]
fn an_edit_changes_what_it_names_and_keeps_everything_else() {
    let mut file = document();
    edit(&mut file, |layer| {
        layer.set_layer_effects(&full_set()).unwrap()
    });

    edit(&mut file, |layer| {
        let mut effects = layer.layer_effects().unwrap().unwrap();
        effects.drop_shadows[0].distance = Some(40.0);
        effects.bevel = None;
        layer.set_layer_effects(&effects).unwrap();
    });

    let effects = styled_layer(&file).layer_effects().unwrap().unwrap();
    assert_eq!(effects.drop_shadows[0].distance, Some(40.0));
    assert_eq!(effects.drop_shadows[0].size, Some(5.0));
    assert!(effects.bevel.is_none());
    assert!(effects.satin.is_some() && effects.outer_glow.is_some());
    assert_eq!(effects.modifying_count(), 8);
}

#[test]
fn full_effects_payloads_keep_unknown_data_through_replacement_and_typed_edits() {
    let mut file = document();
    file.version = Version::Psb;
    let mut root = full_set().to_descriptor();
    root.set("futureEffectData", DescriptorValue::long(7));
    let mut payload = effects_block_data(&root).unwrap();
    payload.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    let mut raw = TaggedBlock::new(TaggedBlockKey::new(*b"lmfx"), payload);
    raw.signature = *b"8B64";
    edit(&mut file, |layer| {
        layer.set_layer_effects_block(&raw).unwrap()
    });
    assert_eq!(styled_layer(&file).blocks.blocks[0], raw);
    let LayerEffectsData::Modern(original) = LayerEffectsBlock::read(&raw).unwrap().unwrap().data
    else {
        panic!("modern effects expected")
    };
    assert_eq!(original.to_payload().unwrap(), raw.data);
    let before = styled_layer(&file).blocks.clone();
    edit(&mut file, |layer| {
        layer.set_layer_effects_block(&raw).unwrap()
    });
    assert_eq!(styled_layer(&file).blocks, before);

    edit(&mut file, |layer| {
        let mut effects = layer.layer_effects().unwrap().unwrap();
        effects.drop_shadows[0].distance = Some(40.0);
        layer.set_layer_effects(&effects).unwrap();
    });
    let back = reread(&file);
    let block = &styled_layer(&back).blocks.blocks[0];
    assert_eq!(block.key, raw.key);
    assert_eq!(block.signature, raw.signature);
    let LayerEffectsData::Modern(edited) = LayerEffectsBlock::read(block).unwrap().unwrap().data
    else {
        panic!("modern effects expected")
    };
    assert_eq!(
        edited
            .descriptor
            .get("futureEffectData")
            .unwrap()
            .as_integer(),
        Some(7)
    );
    assert_eq!(edited.trailing_bytes, original.trailing_bytes);
    assert_eq!(
        record_u32(&mirror(styled_layer(&back)), b"dsdw", 16),
        40 << 16
    );

    edit(&mut file, |layer| {
        let before = layer.blocks.clone();
        let malformed = TaggedBlock::new(raw.key, vec![0; 8]);
        assert!(layer.set_layer_effects_block(&malformed).is_err());
        assert_eq!(layer.blocks, before);
    });
}

#[test]
fn setting_the_same_effects_changes_nothing_and_clearing_removes_the_blocks() {
    let mut file = document();
    edit(&mut file, |layer| {
        layer.set_layer_effects(&full_set()).unwrap()
    });
    let before = styled_layer(&file).blocks.clone();
    edit(&mut file, |layer| {
        let same = layer.layer_effects().unwrap().unwrap();
        layer.set_layer_effects(&same).unwrap();
    });
    assert_eq!(styled_layer(&file).blocks, before);

    edit(&mut file, Layer::clear_layer_effects);
    assert_eq!(keys(styled_layer(&file)), ["luni"]);
    assert_eq!(styled_layer(&file).layer_effects().unwrap(), None);
}

/// The bytes of the layer's `lrFX` block.
fn mirror(layer: &Layer<u8>) -> Vec<u8> {
    let block = layer
        .blocks
        .blocks
        .iter()
        .find(|b| b.key.as_bytes() == *b"lrFX")
        .expect("the layer has a legacy mirror");
    block.data.clone()
}

/// The `u32` at `offset` of the record `key` in a legacy block's payload.
fn record_u32(block: &[u8], key: &[u8; 4], offset: usize) -> u32 {
    let mut at = 4;
    while at + 12 <= block.len() {
        let size = u32::from_be_bytes(block[at + 8..at + 12].try_into().unwrap()) as usize;
        if &block[at + 4..at + 8] == key {
            let start = at + 12 + offset;
            return u32::from_be_bytes(block[start..start + 4].try_into().unwrap());
        }
        at += 12 + size;
    }
    panic!("no {key:?} record");
}

#[test]
fn an_edit_regenerates_the_legacy_mirror_and_a_no_op_keeps_it() {
    let mut file = document();
    edit(&mut file, |layer| {
        layer.set_layer_effects(&full_set()).unwrap()
    });
    let first = mirror(styled_layer(&file));
    assert_eq!(first.len(), 396);
    // Drop shadow: size 5 px, distance 5 px, as 16.16 fixed point.
    assert_eq!(record_u32(&first, b"dsdw", 4), 5 << 16);
    assert_eq!(record_u32(&first, b"dsdw", 16), 5 << 16);

    edit(&mut file, |layer| {
        let same = layer.layer_effects().unwrap().unwrap();
        layer.set_layer_effects(&same).unwrap();
    });
    assert_eq!(
        mirror(styled_layer(&file)),
        first,
        "a no-op keeps the mirror"
    );

    edit(&mut file, |layer| {
        let mut effects = layer.layer_effects().unwrap().unwrap();
        effects.drop_shadows[0].distance = Some(40.0);
        layer.set_layer_effects(&effects).unwrap();
    });
    let after = mirror(styled_layer(&file));
    assert_eq!(record_u32(&after, b"dsdw", 16), 40 << 16);
    assert_eq!(
        keys(styled_layer(&file)),
        ["lfx2", "lrFX", "luni"],
        "the mirror follows the descriptor block"
    );

    // The mirror follows what the descriptor says, whatever the set leaves out: a set whose
    // shadow says nothing about its size still leaves the descriptor's 5 px in the mirror.
    edit(&mut file, |layer| {
        let mut effects = layer.layer_effects().unwrap().unwrap();
        effects.drop_shadows[0].size = None;
        effects.drop_shadows[0].distance = Some(12.0);
        layer.set_layer_effects(&effects).unwrap();
    });
    let last = mirror(styled_layer(&file));
    assert_eq!(record_u32(&last, b"dsdw", 4), 5 << 16);
    assert_eq!(record_u32(&last, b"dsdw", 16), 12 << 16);
}

#[test]
fn a_block_that_cannot_be_read_is_replaced() {
    let mut file = document();
    edit(&mut file, |layer| {
        layer.blocks.blocks.insert(
            0,
            TaggedBlock::new(TaggedBlockKey::new(*b"lfx2"), vec![0; 8]),
        );
        assert!(layer.layer_effects().is_err());
        layer.set_layer_effects(&full_set()).unwrap();
    });
    assert_eq!(
        styled_layer(&file).layer_effects().unwrap(),
        Some(full_set())
    );
    assert_eq!(keys(styled_layer(&file)), ["lfx2", "lrFX", "luni"]);
}

// ---------------------------------------------------------------------------------------
// Real documents
// ---------------------------------------------------------------------------------------

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = read.map(|entry| entry.unwrap().path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect(&path, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("psd" | "psb")
        ) {
            out.push(path);
        }
    }
}

fn documents() -> Vec<PathBuf> {
    let mut roots = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")];
    if let Some(extra) = std::env::var_os("PSD_EXTRA_CORPUS") {
        roots.extend(std::env::split_paths(&extra));
    }
    let mut out = Vec::new();
    for root in roots {
        collect(&root, &mut out);
    }
    out
}

#[derive(Default)]
struct Findings {
    layers: usize,
    noop_kept_blocks: usize,
    count_matches: usize,
    count_seen: usize,
    length_multiple_of_four: usize,
    problems: Vec<String>,
}

fn scan<T: BitDepth>(bytes: &[u8], name: &str, findings: &mut Findings) {
    let Ok(file) = LayeredFile::<T>::from_bytes(bytes) else {
        return;
    };
    let ids: Vec<_> = file.layers_with_ids().map(|(id, _)| id).collect();
    let mut file = file;
    for id in ids {
        let layer = file.layer_mut(id).unwrap();
        let Ok(Some(effects)) = layer.layer_effects() else {
            continue;
        };
        findings.layers += 1;

        let before = layer.blocks.clone();
        if layer.set_layer_effects(&effects).is_ok() && layer.blocks == before {
            findings.noop_kept_blocks += 1;
        } else {
            findings
                .problems
                .push(format!("{name}: {:?} changed on a no-op set", layer.name));
        }

        // `numModifyingFX`, wherever a file has it, is nearly always the count of enabled effects.
        let block = before
            .blocks
            .iter()
            .find(|b| matches!(&b.key.as_bytes(), b"lfx2" | b"lmfx" | b"lfxs"))
            .unwrap();
        let root = psd::core::LayerEffectsBlock::read(block).unwrap().unwrap();
        if let psd::core::layer_effects::LayerEffectsData::Modern(modern) = root.data {
            if let Some(stored) = modern
                .descriptor
                .get("numModifyingFX")
                .and_then(|v| v.as_integer())
            {
                findings.count_seen += 1;
                // A statistic, not a rule: Photoshop's tally is the enabled count except in
                // cases like a zero-width stroke, which the set deliberately does not chase.
                if usize::try_from(stored) == Ok(effects.modifying_count()) {
                    findings.count_matches += 1;
                }
            }
        }
        if block.data.len() % 4 == 0 {
            findings.length_multiple_of_four += 1;
        }
    }
}

#[test]
fn setting_a_real_layers_own_effects_back_changes_nothing() {
    let mut findings = Findings::default();
    let documents = documents();
    assert!(!documents.is_empty(), "no documents found");
    for path in &documents {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        match u16::from_be_bytes([bytes[22], bytes[23]]) {
            16 => scan::<u16>(&bytes, &name, &mut findings),
            32 => scan::<f32>(&bytes, &name, &mut findings),
            _ => scan::<u8>(&bytes, &name, &mut findings),
        }
    }
    eprintln!(
        "{} layers with effects: no-op set kept every block on {}; numModifyingFX matched on {} of \
         {}; block length a multiple of four on {}",
        findings.layers,
        findings.noop_kept_blocks,
        findings.count_matches,
        findings.count_seen,
        findings.length_multiple_of_four
    );
    assert!(findings.problems.is_empty(), "{:#?}", findings.problems);
    assert_eq!(findings.length_multiple_of_four, findings.layers);
}
