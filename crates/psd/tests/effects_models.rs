//! The typed effect models against every real effect descriptor in the corpora.
//!
//! For each effect found in an `lfx2`, `lmfx` or `lfxs` block (a single effect or an entry of
//! a `*Multi` list), two things are checked, and both must hold for every one:
//!
//! 1. a no-op patch is byte-exact: the model read from the descriptor, applied back onto it,
//!    changes no bytes, whatever spelling the file used for a value;
//! 2. a fresh build matches Photoshop: `to_descriptor` on the model read from the file
//!    reproduces the original bytes, so a new effect has the item order, the key encodings and
//!    the optional items Photoshop-authored files have.
//!
//! `fixtures/` always runs. Set `PSD_EXTRA_CORPUS` to a list of directories (separated as
//! `PATH` is) to sweep more documents.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use psd::core::layer_effects::LayerEffectsData;
use psd::core::BlendMode;
use psd::core::{
    BeWriter, Bevel, ColorOverlay, Descriptor, DescriptorValue, Glow, GlowKind, GradientOverlay,
    LayerEffectsBlock, PatternOverlay, Satin, Shadow, ShadowKind, Stroke,
};
use psd::{BitDepth, LayeredFile};

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

fn bytes(descriptor: &Descriptor) -> Vec<u8> {
    let mut writer = BeWriter::new();
    descriptor.write(&mut writer).unwrap();
    writer.into_inner()
}

fn key_sequence(descriptor: &Descriptor) -> String {
    descriptor
        .items
        .iter()
        .map(|item| item.key.as_str().trim_end().to_owned())
        .collect::<Vec<_>>()
        .join(",")
}

/// The descriptor with every blend-mode value spelled the historical way.
fn canonical(descriptor: &Descriptor) -> Descriptor {
    let mut copy = descriptor.clone();
    canonicalise(&mut copy);
    copy
}

fn canonicalise(descriptor: &mut Descriptor) {
    for item in &mut descriptor.items {
        match &mut item.value {
            DescriptorValue::Enumerated { type_id, value } if type_id.as_bytes() == b"BlnM" => {
                if let Some(mode) = BlendMode::from_descriptor_enum(value.as_bytes()) {
                    if let Some(historical) = mode.to_descriptor_enum() {
                        *value = historical;
                    }
                }
            }
            DescriptorValue::Descriptor(child) => canonicalise(child),
            DescriptorValue::List(list) => {
                for entry in list {
                    if let DescriptorValue::Descriptor(child) = entry {
                        canonicalise(child);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Item orders that only pre-CS6 files use. A fresh build writes the modern order, which every
/// Photoshop version reads, so these are the one accepted difference, pinned by layout: a
/// colour overlay with its colour last, and an inner glow that writes `ShdN` before `Nose`
/// and `glwS` before its contour. Any other mismatch fails the test.
const LEGACY_LAYOUTS: [&str; 2] = [
    "enab,Md,Opct,Clr",
    "enab,Md,Clr,Opct,GlwT,Ckmt,blur,ShdN,Nose,AntA,glwS,TrnS,Inpr",
];

#[derive(Default)]
struct Tally {
    seen: usize,
    noop_exact: usize,
    fresh_exact: usize,
    /// Fresh builds that differ only because the file used a pre-CS6 item order.
    legacy_layout: usize,
    /// `original layout -> rebuilt layout` for the fresh builds that differed.
    differences: BTreeMap<String, usize>,
}

type Tallies = BTreeMap<&'static str, Tally>;

fn check<M>(
    tallies: &mut Tallies,
    kind: &'static str,
    descriptor: &Descriptor,
    read: impl Fn(&Descriptor) -> M,
    apply: impl Fn(&M, &mut Descriptor),
    fresh: impl Fn(&M) -> Descriptor,
) {
    let tally = tallies.entry(kind).or_default();
    tally.seen += 1;
    let model = read(descriptor);
    let original = bytes(descriptor);

    let mut patched = descriptor.clone();
    apply(&model, &mut patched);
    if bytes(&patched) == original {
        tally.noop_exact += 1;
    }

    // A fresh build is compared modulo the blend-mode spelling, which is the one thing a
    // model does not keep: newer files write `multiply`, older ones `Mltp`, and both read in
    // every Photoshop version. Everything else must match exactly.
    let rebuilt = fresh(&model);
    let expected = bytes(&canonical(descriptor));
    if bytes(&canonical(&rebuilt)) == expected {
        tally.fresh_exact += 1;
    } else if LEGACY_LAYOUTS.contains(&key_sequence(descriptor).as_str()) {
        tally.legacy_layout += 1;
    } else {
        let original = expected;
        let fresh_bytes = bytes(&canonical(&rebuilt));
        let at = original
            .iter()
            .zip(&fresh_bytes)
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        let window = |b: &[u8]| {
            let from = at.saturating_sub(12);
            String::from_utf8_lossy(&b[from..(at + 16).min(b.len())]).replace('\0', ".")
        };
        let label = format!(
            "{} => {}: first diff at {at} of {}/{}: orig[{}] fresh[{}]",
            key_sequence(descriptor),
            key_sequence(&rebuilt),
            original.len(),
            fresh_bytes.len(),
            window(&original),
            window(&fresh_bytes)
        );
        *tally.differences.entry(label).or_default() += 1;
    }
}

fn effect(tallies: &mut Tallies, root_key: &str, descriptor: &Descriptor) {
    match root_key {
        "DrSh" | "dropShadowMulti" => check(
            tallies,
            "drop shadow",
            descriptor,
            Shadow::from_descriptor,
            |m, d| m.apply_to(d, ShadowKind::Drop),
            |m| m.to_descriptor(ShadowKind::Drop),
        ),
        "IrSh" | "innerShadowMulti" => check(
            tallies,
            "inner shadow",
            descriptor,
            Shadow::from_descriptor,
            |m, d| m.apply_to(d, ShadowKind::Inner),
            |m| m.to_descriptor(ShadowKind::Inner),
        ),
        "OrGl" => check(
            tallies,
            "outer glow",
            descriptor,
            Glow::from_descriptor,
            |m, d| m.apply_to(d, GlowKind::Outer),
            |m| m.to_descriptor(GlowKind::Outer),
        ),
        "IrGl" => check(
            tallies,
            "inner glow",
            descriptor,
            Glow::from_descriptor,
            |m, d| m.apply_to(d, GlowKind::Inner),
            |m| m.to_descriptor(GlowKind::Inner),
        ),
        "ebbl" => check(
            tallies,
            "bevel",
            descriptor,
            Bevel::from_descriptor,
            Bevel::apply_to,
            Bevel::to_descriptor,
        ),
        "SoFi" | "solidFillMulti" => check(
            tallies,
            "color overlay",
            descriptor,
            ColorOverlay::from_descriptor,
            ColorOverlay::apply_to,
            ColorOverlay::to_descriptor,
        ),
        "ChFX" => check(
            tallies,
            "satin",
            descriptor,
            Satin::from_descriptor,
            Satin::apply_to,
            Satin::to_descriptor,
        ),
        "GrFl" | "gradientFillMulti" => check(
            tallies,
            "gradient overlay",
            descriptor,
            GradientOverlay::from_descriptor,
            GradientOverlay::apply_to,
            GradientOverlay::to_descriptor,
        ),
        "patternFill" => check(
            tallies,
            "pattern overlay",
            descriptor,
            PatternOverlay::from_descriptor,
            PatternOverlay::apply_to,
            PatternOverlay::to_descriptor,
        ),
        "FrFX" | "frameFXMulti" => check(
            tallies,
            "stroke",
            descriptor,
            Stroke::from_descriptor,
            Stroke::apply_to,
            Stroke::to_descriptor,
        ),
        _ => {}
    }
}

fn scan<T: BitDepth>(bytes: &[u8], tallies: &mut Tallies) {
    let Ok(file) = LayeredFile::<T>::from_bytes(bytes) else {
        return;
    };
    for layer in file.layers() {
        for block in &layer.blocks.blocks {
            let Ok(Some(parsed)) = LayerEffectsBlock::read(block) else {
                continue;
            };
            let LayerEffectsData::Modern(modern) = parsed.data else {
                continue;
            };
            for item in &modern.descriptor.items {
                let key = item.key.as_str();
                match &item.value {
                    DescriptorValue::Descriptor(d) => effect(tallies, &key, d),
                    DescriptorValue::List(list) => {
                        for value in list {
                            if let DescriptorValue::Descriptor(d) = value {
                                effect(tallies, &key, d);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

#[test]
fn effect_models_patch_and_rebuild_every_real_effect_exactly() {
    let mut tallies = Tallies::new();
    let documents = documents();
    assert!(!documents.is_empty(), "no documents found");
    for path in &documents {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        match u16::from_be_bytes([bytes[22], bytes[23]]) {
            16 => scan::<u16>(&bytes, &mut tallies),
            32 => scan::<f32>(&bytes, &mut tallies),
            _ => scan::<u8>(&bytes, &mut tallies),
        }
    }

    let mut failures = Vec::new();
    for (kind, t) in &tallies {
        eprintln!(
            "{kind}: {} seen, no-op patch exact {}, fresh build exact {} (+{} legacy order)",
            t.seen, t.noop_exact, t.fresh_exact, t.legacy_layout
        );
        let mut differences: Vec<_> = t.differences.iter().collect();
        differences.sort_by(|a, b| b.1.cmp(a.1));
        for (label, count) in differences.into_iter().take(4) {
            eprintln!("    {count}x differs: {label}");
        }
        if t.noop_exact != t.seen {
            failures.push(format!(
                "{kind}: {} no-op patches changed bytes",
                t.seen - t.noop_exact
            ));
        }
        if t.fresh_exact + t.legacy_layout != t.seen {
            failures.push(format!(
                "{kind}: {} fresh builds differ",
                t.seen - t.fresh_exact - t.legacy_layout
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
