//! The shared style values (colours, contours, gradients, offsets, pattern references) against
//! every real effects descriptor in the corpora.
//!
//! For each value found inside a layer-effects block, three things are checked:
//!
//! 1. it models: `from_descriptor` accepts it;
//! 2. a no-op patch is byte-exact: applying the value it read back onto the descriptor it
//!    came from reproduces the bytes, so an edit that changes nothing changes nothing;
//! 3. a fresh build matches Photoshop: `to_descriptor` reproduces the original bytes, so a
//!    value authored from scratch is spelled the way Photoshop spells it (key encodings, item
//!    order, the terminating null on every name and string).
//!
//! `fixtures/` always runs. Set `PSD_EXTRA_CORPUS` to a list of directories (separated as
//! `PATH` is) to sweep more documents.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use psd::core::layer_effects::LayerEffectsData;
use psd::core::{
    BeWriter, Color, Contour, Descriptor, DescriptorValue, Gradient, LayerEffectsBlock, Offset,
    PatternRef,
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

#[derive(Default)]
struct Tally {
    seen: usize,
    modelled: usize,
    noop_exact: usize,
    fresh_exact: usize,
    /// The first few descriptors whose fresh build differed, for the failure message.
    fresh_examples: Vec<String>,
}

type Tallies = BTreeMap<&'static str, Tally>;

fn check<T>(
    tallies: &mut Tallies,
    kind: &'static str,
    descriptor: &Descriptor,
    read: impl Fn(&Descriptor) -> Option<T>,
    fresh: impl Fn(&T) -> Descriptor,
    patch: impl Fn(&T, &mut Descriptor),
) {
    let tally = tallies.entry(kind).or_default();
    tally.seen += 1;
    let Some(value) = read(descriptor) else {
        return;
    };
    tally.modelled += 1;

    let original = bytes(descriptor);
    let mut patched = descriptor.clone();
    patch(&value, &mut patched);
    if bytes(&patched) == original {
        tally.noop_exact += 1;
    }
    let rebuilt = fresh(&value);
    if bytes(&rebuilt) == original {
        tally.fresh_exact += 1;
    } else if tally.fresh_examples.len() < 2 {
        tally.fresh_examples.push(format!(
            "original {} bytes, fresh {} bytes",
            original.len(),
            bytes(&rebuilt).len()
        ));
    }
}

fn walk(descriptor: &Descriptor, tallies: &mut Tallies) {
    match descriptor.class_id.as_bytes() {
        b"RGBC" | b"CMYC" | b"Grsc" | b"HSBC" | b"LbCl" => check(
            tallies,
            "Color",
            descriptor,
            Color::from_descriptor,
            Color::to_descriptor,
            Color::apply_to,
        ),
        b"ShpC" => check(
            tallies,
            "Contour",
            descriptor,
            Contour::from_descriptor,
            Contour::to_descriptor,
            Contour::apply_to,
        ),
        b"Grdn" => check(
            tallies,
            "Gradient",
            descriptor,
            Gradient::from_descriptor,
            Gradient::to_descriptor,
            Gradient::apply_to,
        ),
        b"Ptrn" => check(
            tallies,
            "PatternRef",
            descriptor,
            PatternRef::from_descriptor,
            PatternRef::to_descriptor,
            PatternRef::apply_to,
        ),
        b"Pnt " => check(
            tallies,
            "Offset",
            descriptor,
            Offset::from_descriptor,
            Offset::to_descriptor,
            Offset::apply_to,
        ),
        _ => {}
    }
    for item in &descriptor.items {
        match &item.value {
            DescriptorValue::Descriptor(child) => walk(child, tallies),
            DescriptorValue::List(list) => {
                for value in list {
                    if let DescriptorValue::Descriptor(child) = value {
                        walk(child, tallies);
                    }
                }
            }
            _ => {}
        }
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
            if let LayerEffectsData::Modern(modern) = parsed.data {
                walk(&modern.descriptor, tallies);
            }
        }
    }
}

#[test]
fn shared_style_values_model_patch_and_rebuild_the_real_descriptors() {
    let mut tallies = Tallies::new();
    let documents = documents();
    assert!(!documents.is_empty(), "no documents found");
    for path in &documents {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let depth = u16::from_be_bytes([bytes[22], bytes[23]]);
        match depth {
            16 => scan::<u16>(&bytes, &mut tallies),
            32 => scan::<f32>(&bytes, &mut tallies),
            _ => scan::<u8>(&bytes, &mut tallies),
        }
    }

    let mut failures = Vec::new();
    for (kind, t) in &tallies {
        eprintln!(
            "{kind}: seen {}, modelled {}, no-op patch exact {}, fresh build exact {}",
            t.seen, t.modelled, t.noop_exact, t.fresh_exact
        );
        for example in &t.fresh_examples {
            eprintln!("    fresh differs: {example}");
        }
        if t.modelled != t.seen {
            failures.push(format!(
                "{kind}: {} of {} not modelled",
                t.seen - t.modelled,
                t.seen
            ));
        }
        if t.fresh_exact != t.modelled {
            failures.push(format!(
                "{kind}: {} of {} fresh builds differ from Photoshop's bytes",
                t.modelled - t.fresh_exact,
                t.modelled
            ));
        }
        if t.noop_exact != t.modelled {
            failures.push(format!(
                "{kind}: {} of {} no-op patches changed bytes",
                t.modelled - t.noop_exact,
                t.modelled
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
