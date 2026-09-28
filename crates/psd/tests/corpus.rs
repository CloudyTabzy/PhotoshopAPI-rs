//! Corpus regression harness with a pinned known-failure list.
//!
//! Every `.psd`/`.psb` under `fixtures/` runs through the same named checks:
//!
//! - `read`: an eager read at the header's bit depth.
//! - `views`: every on-demand view parses (effects, adjustments, vector data,
//!   artboards, smart objects, linked layers, document paths, and artboard
//!   settings).
//! - `roundtrip`: write, read back, and compare the header fields, every
//!   layer's metadata and tagged blocks, and every decoded channel.
//! - `stable`: writing the re-read document reproduces the first write byte
//!   for byte.
//!
//! The failures found are compared with [`KNOWN_FAILURES`] rather than
//! required to be empty. A new failure and an unexpected pass both fail the
//! suite and are listed by name, so the pin cannot drift silently. When a
//! change fixes an entry, delete it from the list.
//!
//! Set `PSD_EXTRA_CORPUS` to a directory to sweep more documents with the same
//! checks. Its failures are printed and fail the test, but are never pinned.

use std::collections::BTreeSet;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use psd::core::AdditionalLayerInfo;
use psd::{BitDepth, ChannelStore, LayerKind, LayeredFile};

/// `relative/path.psd:check` entries that are known to fail today.
const KNOWN_FAILURES: &[&str] = &[];

const CHECKS: [&str; 4] = ["read", "views", "roundtrip", "stable"];

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("{}: {error}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect(&path, out);
        } else if matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("psd" | "psb")
        ) {
            out.push(path);
        }
    }
}

fn header_depth(bytes: &[u8]) -> Option<u16> {
    Some(u16::from_be_bytes([*bytes.get(22)?, *bytes.get(23)?]))
}

/// Run one check, turning an error or a panic into a message.
fn attempt<R>(check: impl FnOnce() -> Result<R, String>) -> Result<R, String> {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));
    let outcome = panic::catch_unwind(AssertUnwindSafe(check));
    panic::set_hook(previous);
    match outcome {
        Ok(result) => result,
        Err(payload) => Err(payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| {
                payload
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_owned())
            })
            .map_or_else(|| "panic".to_owned(), |text| format!("panic: {text}"))),
    }
}

fn views<T: BitDepth>(file: &LayeredFile<T>) -> Result<(), String> {
    for layer in file.layers() {
        let name = &layer.name;
        let fail = |view: &str, error: psd::core::PsdError| format!("{name}: {view}: {error}");
        layer.effects().map_err(|error| fail("effects", error))?;
        layer
            .adjustments()
            .map_err(|error| fail("adjustments", error))?;
        layer
            .vector_blocks()
            .map_err(|error| fail("vector", error))?;
        if let Some(mask) = layer
            .vector_mask()
            .map_err(|error| fail("vector mask", error))?
        {
            mask.path
                .subpaths()
                .map_err(|error| fail("subpaths", error))?;
        }
        layer.artboard().map_err(|error| fail("artboard", error))?;
        layer
            .smart_object_data()
            .map_err(|error| fail("smart object", error))?;
        layer
            .placed_layer()
            .map_err(|error| fail("placed layer", error))?;
    }
    for path in file
        .document_paths()
        .map_err(|error| format!("document paths: {error}"))?
    {
        path.path
            .subpaths()
            .map_err(|error| format!("document path {}: {error}", path.resource_id))?;
    }
    file.artboard_settings()
        .map_err(|error| format!("artboard settings: {error}"))?;
    file.linked_layer_views()
        .map_err(|error| format!("linked layers: {error}"))?;
    Ok(())
}

fn compare_channels<T: BitDepth>(
    layer: &str,
    before: &ChannelStore<T>,
    after: &ChannelStore<T>,
) -> Result<(), String> {
    let keys: Vec<_> = before.keys().collect();
    if keys != after.keys().collect::<Vec<_>>() {
        return Err(format!("{layer}: channel keys differ"));
    }
    for (key, samples) in before.iter() {
        if after.get(key) != Some(samples) {
            return Err(format!("{layer}: channel {key:?} pixels differ"));
        }
    }
    Ok(())
}

/// The keys of two block lists, marking the blocks whose bytes differ.
fn block_diff(before: Option<&AdditionalLayerInfo>, after: Option<&AdditionalLayerInfo>) -> String {
    let describe = |info: Option<&AdditionalLayerInfo>| -> Vec<String> {
        info.map(|info| {
            info.blocks
                .iter()
                .map(|block| format!("{}({})", block.key, block.data.len()))
                .collect()
        })
        .unwrap_or_default()
    };
    format!("{:?} -> {:?}", describe(before), describe(after))
}

fn compare<T: BitDepth>(before: &LayeredFile<T>, after: &LayeredFile<T>) -> Result<(), String> {
    let header = |file: &LayeredFile<T>| (file.width, file.height, file.color_mode, file.dpi);
    if header(before) != header(after) {
        return Err("header fields differ".to_owned());
    }
    if before.icc_profile != after.icc_profile {
        return Err("ICC profile differs".to_owned());
    }
    // The writer rebuilds `Lr16`/`Lr32` (the 16- and 32-bit layer records)
    // from the layer tree, which the per-layer comparison below covers.
    let preserved = |file: &LayeredFile<T>| {
        file.document_blocks
            .as_ref()
            .map(|info| AdditionalLayerInfo {
                blocks: info
                    .blocks
                    .iter()
                    .filter(|block| !matches!(&block.key.as_bytes(), b"Lr16" | b"Lr32"))
                    .cloned()
                    .collect(),
            })
    };
    let (document_before, document_after) = (preserved(before), preserved(after));
    if document_before != document_after {
        return Err(format!(
            "document tagged blocks differ: {}",
            block_diff(document_before.as_ref(), document_after.as_ref())
        ));
    }
    if before.layer_count() != after.layer_count() {
        return Err(format!(
            "layer count {} became {}",
            before.layer_count(),
            after.layer_count()
        ));
    }
    for (a, b) in before.layers().zip(after.layers()) {
        let name = &a.name;
        let metadata = |layer: &psd::Layer<T>| {
            (
                layer.name.clone(),
                layer.bounds,
                layer.opacity,
                layer.blend_mode,
                layer.flags,
                layer.clipping,
            )
        };
        if metadata(a) != metadata(b) {
            return Err(format!("{name}: layer metadata differs"));
        }
        if a.blocks != b.blocks {
            return Err(format!(
                "{name}: tagged blocks differ: {}",
                block_diff(Some(&a.blocks), Some(&b.blocks))
            ));
        }
        match (&a.kind, &b.kind) {
            (LayerKind::Image(x), LayerKind::Image(y)) => {
                compare_channels(name, &x.channels, &y.channels)?;
            }
            (LayerKind::Text(x), LayerKind::Text(y)) => {
                compare_channels(name, &x.channels, &y.channels)?;
            }
            (LayerKind::Group(x), LayerKind::Group(y)) => {
                if (x.open, &x.children) != (y.open, &y.children) {
                    return Err(format!("{name}: group state differs"));
                }
                compare_channels(name, &x.channels, &y.channels)?;
            }
            (LayerKind::SectionDivider(x), LayerKind::SectionDivider(y)) if x == y => {}
            _ => return Err(format!("{name}: layer kind differs")),
        }
    }
    Ok(())
}

/// Run every check on one document; returns the failed checks with messages.
fn check_document<T: BitDepth>(bytes: &[u8]) -> Vec<(&'static str, String)> {
    let mut failures = Vec::new();
    let original = match attempt(|| LayeredFile::<T>::from_bytes(bytes).map_err(|e| e.to_string()))
    {
        Ok(file) => file,
        Err(message) => {
            // Nothing else can run without the document.
            return CHECKS
                .iter()
                .map(|check| (*check, message.clone()))
                .collect();
        }
    };
    if let Err(message) = attempt(|| views(&original)) {
        failures.push(("views", message));
    }
    let written = attempt(|| original.to_bytes().map_err(|e| format!("write: {e}")));
    let reread = written.as_ref().map_err(Clone::clone).and_then(|bytes| {
        attempt(|| LayeredFile::<T>::from_bytes(bytes).map_err(|e| format!("reread: {e}")))
    });
    match &reread {
        Ok(back) => {
            if let Err(message) = attempt(|| compare(&original, back)) {
                failures.push(("roundtrip", message));
            }
        }
        Err(message) => failures.push(("roundtrip", message.clone())),
    }
    let stable = match (&written, &reread) {
        (Ok(first), Ok(back)) => attempt(|| {
            let second = back.to_bytes().map_err(|e| format!("second write: {e}"))?;
            if second == *first {
                Ok(())
            } else {
                Err(format!(
                    "second write differs ({} vs {} bytes)",
                    first.len(),
                    second.len()
                ))
            }
        }),
        _ => Err("no round-trip to compare".to_owned()),
    };
    if let Err(message) = stable {
        failures.push(("stable", message));
    }
    failures
}

/// Every failed `name:check` in `dir`, with its message.
fn sweep(dir: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    collect(dir, &mut files);
    let mut failures = Vec::new();
    for path in files {
        let name = path
            .strip_prefix(dir)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = std::fs::read(&path).unwrap();
        let found = match header_depth(&bytes) {
            // A 1-bit document is read as an 8-bit one: its packed pixels are
            // expanded during channel decode.
            Some(1) | Some(8) => check_document::<u8>(&bytes),
            Some(16) => check_document::<u16>(&bytes),
            Some(32) => check_document::<f32>(&bytes),
            other => CHECKS
                .iter()
                .map(|check| (*check, format!("unsupported bit depth {other:?}")))
                .collect(),
        };
        failures.extend(
            found
                .into_iter()
                .map(|(check, message)| (format!("{name}:{check}"), message)),
        );
    }
    failures
}

#[test]
fn corpus_failures_match_the_pinned_list() {
    let root = fixtures_root();
    let failures = sweep(&root);
    let found: BTreeSet<&str> = failures.iter().map(|(name, _)| name.as_str()).collect();
    let pinned: BTreeSet<&str> = KNOWN_FAILURES.iter().copied().collect();
    let new: Vec<_> = failures
        .iter()
        .filter(|(name, _)| !pinned.contains(name.as_str()))
        .map(|(name, message)| format!("  {name}: {message}"))
        .collect();
    let fixed: Vec<_> = pinned.difference(&found).collect();
    assert!(
        new.is_empty() && fixed.is_empty(),
        "corpus results changed.\nNew failures:\n{}\nPinned failures that now pass \
         (remove them from KNOWN_FAILURES):\n{fixed:#?}",
        new.join("\n")
    );

    let mut files = Vec::new();
    collect(&root, &mut files);
    assert!(files.len() >= 80, "corpus shrank to {} files", files.len());
}

#[test]
fn extra_corpus_passes_every_check() {
    let Some(dir) = std::env::var_os("PSD_EXTRA_CORPUS") else {
        return;
    };
    let failures = sweep(Path::new(&dir));
    let report: Vec<_> = failures
        .iter()
        .map(|(name, message)| format!("  {name}: {message}"))
        .collect();
    assert!(report.is_empty(), "failures:\n{}", report.join("\n"));
}
