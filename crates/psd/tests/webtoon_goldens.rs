//! Pixel goldens vendored from the Webtoon integration corpus
//! (`example/`): an authored 400x800
//! RGB document with 14 layer records — groups, CJK and emoji names, and
//! layers extending past the canvas. `example.layerN` is the raw decoded RGBA
//! of layer record N in bottom-to-top order (their `composite(false, false)`:
//! channels interleaved, alpha 255 when no `-1` channel exists) and
//! `example.imageData` is Photoshop's own merged composite.
//!
//! The layer goldens pin byte-exact channel decode; the merged golden is
//! compared against `composite_rgba8` within compositor noise.

use std::path::{Path, PathBuf};

use psd::layer::LayerKind;
use psd::{ChannelKey, LayeredFile};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/documents/Webtoon")
        .join(name)
}

fn layer_rgba(layer: &psd::layer::Layer<u8>, width: usize, height: usize) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(width * height * 4);
    let blank = vec![0u8; width * height];
    let opaque = vec![255u8; width * height];
    let get = |key: ChannelKey, fallback: &Vec<u8>| {
        layer
            .channels()
            .and_then(|store| store.get(key).map(<[u8]>::to_vec))
            .unwrap_or_else(|| fallback.clone())
    };
    let (r, g, b, a) = (
        get(ChannelKey::color(0), &blank),
        get(ChannelKey::color(1), &blank),
        get(ChannelKey::color(2), &blank),
        get(ChannelKey::ALPHA, &opaque),
    );
    for i in 0..width * height {
        rgba.extend_from_slice(&[r[i], g[i], b[i], a[i]]);
    }
    rgba
}

#[test]
fn example_layer_channels_match_the_stored_goldens() {
    let doc = LayeredFile::<u8>::read(fixture("example.psd")).unwrap();
    // Their `layers` array holds content records only — group frames and
    // `</Layer group>` dividers are not entries — ordered top-first.
    let mut layers: Vec<_> = doc
        .layers()
        .filter(|layer| {
            !matches!(
                layer.kind,
                LayerKind::Group(_) | LayerKind::SectionDivider(_)
            )
        })
        .collect();
    layers.reverse();
    assert_eq!(layers.len(), 14, "record count drifted from the goldens");
    for (index, layer) in layers.iter().enumerate() {
        let golden = std::fs::read(fixture(&format!("example.layer{index}"))).unwrap();
        let (width, height) = (
            usize::try_from(layer.bounds.width()).unwrap(),
            usize::try_from(layer.bounds.height()).unwrap(),
        );
        assert_eq!(
            golden.len(),
            width * height * 4,
            "layer{index} ({}): golden size does not match its record bounds",
            layer.name
        );
        let rgba = layer_rgba(layer, width, height);
        let diffs = rgba
            .iter()
            .zip(&golden)
            .filter(|(ours, theirs)| ours != theirs)
            .count();
        assert_eq!(
            diffs, 0,
            "layer{index} ({}): {diffs} differing bytes against the stored decode",
            layer.name
        );
    }
}

/// The stored merge is Photoshop's own compositing of the stack. This file's
/// merge and its per-layer previews disagree on text edges: over the flat
/// white background, the merge's implied coverage at glyph antialiasing is
/// ~10% lower than the preview alpha on 2.5k edge pixels — Photoshop rasterized
/// the text twice with different coverage. We composite the authored preview
/// channels (the golden-rule precedence), so the tolerance pins that authored
/// discrepancy: mean well under 1, max at thin-stroke edges.
fn assert_composite_within_golden(path: &Path, max_mean: f64, max_max: u32) {
    let doc = LayeredFile::<u8>::read(path).unwrap();
    let image = doc.composite_rgba8().unwrap();
    let golden = std::fs::read(fixture("example.imageData")).unwrap();
    assert_eq!(image.rgba.len(), golden.len());
    let (mut sum, mut max) = (0u64, 0u32);
    for (ours, theirs) in image.rgba.iter().zip(&golden) {
        let diff = ours.abs_diff(*theirs) as u32;
        sum += u64::from(diff);
        max = max.max(diff);
    }
    let mean = sum as f64 / golden.len() as f64;
    assert!(
        mean <= max_mean && max <= max_max,
        "{}: composite vs stored merge: mean {mean:.3} (limit {max_mean}), max {max} (limit {max_max})",
        path.file_name().unwrap().to_string_lossy()
    );
}

#[test]
fn example_composite_matches_the_stored_merge() {
    assert_composite_within_golden(&fixture("example.psd"), 0.3, 40);
}

/// The PSB twin carries the identical document under 64-bit lengths.
#[test]
fn example_psb_decodes_identically() {
    let doc = LayeredFile::<u8>::read(fixture("example.psb")).unwrap();
    let layers: Vec<_> = doc.layers().collect();
    assert_eq!(layers.len(), 16);
    assert_composite_within_golden(&fixture("example.psb"), 0.3, 40);
}
