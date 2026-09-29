//! The generated legacy `lrFX` block against the ones Photoshop wrote.
//!
//! Every layer in the corpora that has both a descriptor effects block and an `lrFX` block is
//! a pair: Photoshop derived the second from the first. `legacy_effects_block_data` must
//! derive the same bytes from the typed model read out of the first.
//!
//! `fixtures/` always runs. Set `PSD_EXTRA_CORPUS` to a list of directories (separated as
//! `PATH` is) to sweep more documents.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use psd::core::{legacy_effects_block_data, LayerEffects};
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

#[derive(Default)]
struct Tally {
    pairs: usize,
    exact: usize,
    /// Blocks that differ only by rounding in a colour component or an opacity byte.
    rounding: usize,
    /// Blocks that also differ in a record listed in `PINNED`.
    pinned: usize,
    /// (record, byte offset within its payload) -> layers that differ there.
    differing: BTreeMap<(String, usize), usize>,
    examples: BTreeMap<(String, usize), Vec<String>>,
}

/// The model fields a record is derived from, for a mismatch report.
fn model_of(effects: &LayerEffects, key: &[u8; 4]) -> String {
    match key {
        b"dsdw" => format!("{:?}", effects.drop_shadows.first()),
        b"isdw" => format!("{:?}", effects.inner_shadows.first()),
        b"oglw" => format!("{:?}", effects.outer_glow),
        b"iglw" => format!("{:?}", effects.inner_glow),
        b"bevl" => format!("{:?}", effects.bevel),
        b"sofi" => format!("{:?}", effects.color_overlays.first()),
        _ => format!("{:?}", effects.master_switch),
    }
}

/// Records that a corpus document's Photoshop wrote differently from every other document, as
/// far as the `lfx2` beside them can tell: quirks of whichever version saved that file, not
/// mappings a writer could derive. `payload` is the real record's payload.
fn is_pinned(key: &[u8; 4], payload: &[u8], effects: &LayerEffects) -> bool {
    match key {
        // No drop shadow in `lfx2`, and the legacy record has distance 0 rather than the
        // default's 5 that an absent shadow gets elsewhere.
        b"dsdw" => effects.drop_shadows.is_empty() && payload[16..20] == [0; 4],
        // No inner glow in `lfx2`, and the legacy record is not "inverted" (elsewhere it is).
        b"iglw" => effects.inner_glow.is_none() && payload[32] == 0,
        // A 100 px bevel written to the legacy record as 50 px, where the same document saved
        // as a large-format file has 100.
        b"bevl" => {
            effects.bevel.as_ref().and_then(|b| b.size) == Some(100.0)
                && payload[12..16] == [0, 0x32, 0, 0]
        }
        _ => false,
    }
}

const RECORDS: [(&[u8; 4], usize); 7] = [
    (b"cmnS", 7),
    (b"dsdw", 51),
    (b"isdw", 51),
    (b"oglw", 42),
    (b"iglw", 43),
    (b"bevl", 78),
    (b"sofi", 34),
];

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn scan<D: BitDepth>(bytes: &[u8], name: &str, tally: &mut Tally) {
    let Ok(file) = LayeredFile::<D>::from_bytes(bytes) else {
        return;
    };
    for layer in file.layers() {
        let Some(real) = layer
            .blocks
            .blocks
            .iter()
            .find(|b| b.key.as_bytes() == *b"lrFX")
        else {
            continue;
        };
        let Ok(Some(effects)) = layer.layer_effects() else {
            continue;
        };
        tally.pairs += 1;
        let made = legacy_effects_block_data(&effects);
        if made == real.data {
            tally.exact += 1;
            continue;
        }
        if real.data.len() != made.len() || real.data[..4] != made[..4] {
            *tally.differing.entry(("layout".to_owned(), 0)).or_default() += 1;
            continue;
        }
        let mut at = 4;
        let mut only_rounding = true;
        let mut pinned_hit = false;
        for (key, len) in RECORDS {
            let payload = at + 12..at + 12 + len;
            let colours = colour_ranges(key);
            let opacities: &[usize] = match key {
                b"dsdw" | b"isdw" => &[40],
                b"oglw" | b"iglw" => &[31],
                b"bevl" => &[53, 54],
                b"sofi" => &[22],
                _ => &[],
            };
            for offset in 0..len {
                let real_byte = real.data[payload.start + offset];
                let made_byte = made[payload.start + offset];
                if real_byte == made_byte {
                    continue;
                }
                // Opacity bytes: an exact half percent truncates in current Photoshop and
                // rounds up in older versions, so one unit either way is accepted.
                if opacities.contains(&offset) && real_byte.abs_diff(made_byte) <= 1 {
                    continue;
                }
                if let Some(colour) = colours.iter().find(|c| c.contains(&offset)) {
                    // A colour component: up to two units off is Photoshop's own rounding, which
                    // depends on low float bits the model does not keep. The colour space
                    // (the first word) must match exactly.
                    let start = colour.start + (offset - colour.start) / 2 * 2;
                    let word = |bytes: &[u8]| {
                        i32::from(u16::from_be_bytes([
                            bytes[payload.start + start],
                            bytes[payload.start + start + 1],
                        ]))
                    };
                    if start != colour.start && (word(&real.data) - word(&made)).abs() <= 2 {
                        continue;
                    }
                }
                if is_pinned(key, &real.data[payload.clone()], &effects) {
                    pinned_hit = true;
                    continue;
                }
                only_rounding = false;
                {
                    let slot = (String::from_utf8_lossy(key).into_owned(), offset);
                    *tally.differing.entry(slot.clone()).or_default() += 1;
                    let examples = tally.examples.entry(slot).or_default();
                    if examples.len() < 4 {
                        examples.push(format!(
                            "{name} {:?}\n      real {}\n      made {}\n      model {}",
                            layer.name,
                            hex(&real.data[payload.clone()]),
                            hex(&made[payload.clone()]),
                            model_of(&effects, key)
                        ));
                    }
                }
            }
            at += 12 + len;
        }
        if only_rounding {
            if pinned_hit {
                tally.pinned += 1;
            } else {
                tally.rounding += 1;
            }
        }
    }
}

/// Payload byte ranges of each record's colours (a space word and four component words).
fn colour_ranges(key: &[u8; 4]) -> Vec<std::ops::Range<usize>> {
    match key {
        b"dsdw" | b"isdw" => vec![20..30, 41..51],
        b"oglw" => vec![12..22, 32..42],
        b"iglw" => vec![12..22, 33..43],
        b"bevl" => vec![32..42, 42..52, 58..68, 68..78],
        b"sofi" => vec![12..22, 24..34],
        _ => Vec::new(),
    }
}

#[test]
fn generated_lrfx_matches_what_photoshop_wrote() {
    let mut tally = Tally::default();
    for path in documents() {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if bytes.len() < 24 {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let depth = u16::from_be_bytes([bytes[22], bytes[23]]);
        match depth {
            16 => scan::<u16>(&bytes, &name, &mut tally),
            32 => scan::<f32>(&bytes, &name, &mut tally),
            _ => scan::<u8>(&bytes, &name, &mut tally),
        }
    }
    println!(
        "lrFX pairs: {}, exact: {}, rounding only: {}, with a pinned record: {}",
        tally.pairs, tally.exact, tally.rounding, tally.pinned
    );
    for (slot, count) in &tally.differing {
        println!("  {} +{}: {count}", slot.0, slot.1);
        let label = format!("{}+{}", slot.0, slot.1);
        if std::env::var("LRFX_FOCUS").is_ok_and(|focus| focus == label) {
            for example in tally.examples.get(slot).into_iter().flatten() {
                println!("    {example}");
            }
        }
    }
    if tally.pairs > 0 {
        assert_eq!(
            tally.exact + tally.rounding + tally.pinned,
            tally.pairs,
            "some generated blocks differ"
        );
    }
}
