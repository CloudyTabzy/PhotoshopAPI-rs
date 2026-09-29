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
    Bevel, ColorMode, ColorOverlay, Glow, GlowKind, GradientOverlay, LayerEffects, Satin, Shadow,
    ShadowKind, Stroke, TaggedBlock, TaggedBlockKey,
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
        ["lfx2", "luni"],
        "the effects block goes before `luni`"
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
    assert_eq!(keys(styled_layer(&file)), ["lmfx", "luni"]);
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
    assert_eq!(keys(styled_layer(&file)), ["lfx2", "luni"]);
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
    assert_eq!(keys(styled_layer(&file)), ["lmfx", "luni"]);
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

#[test]
fn an_edit_drops_the_legacy_mirror_and_a_no_op_keeps_it() {
    let mut file = document();
    edit(&mut file, |layer| {
        layer.set_layer_effects(&full_set()).unwrap();
        // Photoshop writes `lrFX` right after the descriptor block.
        layer.blocks.blocks.insert(
            1,
            TaggedBlock::new(TaggedBlockKey::new(*b"lrFX"), vec![0; 4]),
        );
    });
    edit(&mut file, |layer| {
        let same = layer.layer_effects().unwrap().unwrap();
        layer.set_layer_effects(&same).unwrap();
    });
    assert_eq!(
        keys(styled_layer(&file)),
        ["lfx2", "lrFX", "luni"],
        "a no-op keeps the mirror"
    );

    edit(&mut file, |layer| {
        let mut effects = layer.layer_effects().unwrap().unwrap();
        effects.satin = None;
        layer.set_layer_effects(&effects).unwrap();
    });
    assert_eq!(
        keys(styled_layer(&file)),
        ["lfx2", "luni"],
        "a real edit drops it"
    );
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
    assert_eq!(keys(styled_layer(&file)), ["lfx2", "luni"]);
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
