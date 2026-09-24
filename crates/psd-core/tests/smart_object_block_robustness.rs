//! Robustness of the smart-object block parsers (`PlLd`, `SoLd`/`SoLE`,
//! `lnk2`/`lnkD`/`lnkE`/`lnk3`) on malformed input.
//!
//! The payloads come from the smart-object fixtures. Every truncation must be
//! either rejected or parsed as a consistent prefix of the full block, and a
//! deterministic set of byte mutations over the structural fields must never
//! panic or read past the payload.

use std::path::{Path, PathBuf};

use psd_core::{BeReader, LinkedLayerTaggedBlock, PhotoshopFile, PlacedLayer, PlacedLayerData};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/documents")
        .join(name)
}

#[derive(Default)]
struct Payloads {
    placed: Vec<Vec<u8>>,
    placed_data: Vec<Vec<u8>>,
    linked: Vec<Vec<u8>>,
}

fn payloads() -> Payloads {
    let mut payloads = Payloads::default();
    for name in [
        "SmartObjects/smart_objects_transformed.psd",
        "SmartObjects/smart_object_file_no_warp.psd",
        "LayerColor/layers_with_display_color.psb",
    ] {
        let bytes = std::fs::read(fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let file = PhotoshopFile::read(&mut BeReader::new(&bytes))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let mut blocks = Vec::new();
        if let Some(info) = &file.layer_and_mask_info.additional_layer_info {
            blocks.extend(info.blocks.iter());
        }
        for record in &file.layer_and_mask_info.layer_info.layer_records {
            if let Some(info) = &record.additional_layer_info {
                blocks.extend(info.blocks.iter());
            }
        }
        for block in blocks {
            let data = block.data.to_vec();
            match &block.key.as_bytes() {
                b"PlLd" | b"plLd" => payloads.placed.push(data),
                b"SoLd" | b"SoLE" => payloads.placed_data.push(data),
                b"lnk2" | b"lnkD" | b"lnkE" | b"lnk3" if !data.is_empty() => {
                    payloads.linked.push(data)
                }
                _ => {}
            }
        }
    }
    assert!(!payloads.placed.is_empty(), "no PlLd fixtures");
    assert!(!payloads.placed_data.is_empty(), "no SoLd fixtures");
    assert!(!payloads.linked.is_empty(), "no linked-layer fixtures");
    payloads
}

/// Start offsets of the linked-layer records in a block (each record is an
/// 8-byte size, then content padded to 4).
fn record_starts(payload: &[u8]) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut offset = 0usize;
    while payload.len().saturating_sub(offset) >= 8 {
        starts.push(offset);
        let size = u64::from_be_bytes(payload[offset..offset + 8].try_into().unwrap()) as usize;
        offset += 8 + size.div_ceil(4) * 4;
    }
    starts
}

/// Offsets where a record's structure lives: its first 1 KiB and the last
/// 256 bytes before the next record (the post-payload version fields).
fn structural_regions(payload: &[u8], starts: &[usize]) -> Vec<std::ops::Range<usize>> {
    let mut regions = Vec::new();
    for (index, &start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(payload.len());
        regions.push(start..end.min(start + 1024));
        regions.push(end.saturating_sub(256).max(start)..end);
    }
    regions
}

/// Every cut inside the structural regions plus an even stride elsewhere.
fn cut_points(payload: &[u8], regions: &[std::ops::Range<usize>]) -> Vec<usize> {
    let mut cuts: Vec<usize> = regions.iter().flat_map(Clone::clone).collect();
    cuts.extend((0..payload.len()).step_by((payload.len() / 257).max(1)));
    cuts.sort_unstable();
    cuts.dedup();
    cuts
}

/// Small deterministic xorshift generator (no external dependency).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    fn nonzero_byte(&mut self) -> u8 {
        (self.next() % 255) as u8 + 1
    }
}

/// Flip 1–3 bytes at `offsets`, run `parse`, then restore the bytes.
fn mutate_and_parse(
    payload: &mut [u8],
    offsets: &[usize],
    rng: &mut Rng,
    rounds: usize,
    mut parse: impl FnMut(&[u8]),
) {
    for _ in 0..rounds {
        let flips: Vec<(usize, u8)> = (0..1 + rng.below(3))
            .map(|_| (offsets[rng.below(offsets.len())], rng.nonzero_byte()))
            .collect();
        for &(offset, mask) in &flips {
            payload[offset] ^= mask;
        }
        parse(payload);
        for &(offset, mask) in flips.iter().rev() {
            payload[offset] ^= mask;
        }
    }
}

#[test]
fn truncated_placed_layer_blocks_are_rejected_until_complete() {
    let payloads = payloads();
    for payload in &payloads.placed {
        let (full, trailing) =
            PlacedLayer::read_with_trailing(&mut BeReader::new(payload)).unwrap();
        let content_len = payload.len() - trailing.len();
        for cut in 0..payload.len() {
            match PlacedLayer::read(&mut BeReader::new(&payload[..cut])) {
                Ok(parsed) => {
                    assert!(cut >= content_len, "PlLd prefix {cut}/{content_len} parsed");
                    assert_eq!(parsed, full);
                }
                Err(_) => assert!(cut < content_len, "PlLd with its suffix cut at {cut}"),
            }
        }
    }
    for payload in &payloads.placed_data {
        let (full, trailing) =
            PlacedLayerData::read_with_trailing(&mut BeReader::new(payload)).unwrap();
        let content_len = payload.len() - trailing.len();
        for cut in 0..payload.len() {
            match PlacedLayerData::read(&mut BeReader::new(&payload[..cut])) {
                Ok(parsed) => {
                    assert!(cut >= content_len, "SoLd prefix {cut}/{content_len} parsed");
                    assert_eq!(parsed, full);
                }
                Err(_) => assert!(cut < content_len, "SoLd with its suffix cut at {cut}"),
            }
        }
    }
}

#[test]
fn truncated_linked_layer_blocks_parse_only_complete_records() {
    let payloads = payloads();
    for payload in &payloads.linked {
        let full = LinkedLayerTaggedBlock::read_views(payload).unwrap();
        let starts = record_starts(payload);
        let regions = structural_regions(payload, &starts);
        for cut in cut_points(payload, &regions) {
            if let Ok(views) = LinkedLayerTaggedBlock::read_views(&payload[..cut]) {
                // A cut can only drop whole trailing records (or leave a
                // sub-header tail that reads as padding).
                assert!(views.len() <= full.len(), "cut {cut}");
                assert!(views.iter().zip(&full).all(|(cut, full)| cut == full));
                let complete = starts
                    .iter()
                    .skip(1)
                    .chain(std::iter::once(&payload.len()))
                    .filter(|&&end| end <= cut)
                    .count();
                assert_eq!(views.len(), complete.min(full.len()), "cut {cut}");
            }
        }
    }
}

#[test]
fn mutated_smart_object_blocks_never_panic() {
    let payloads = payloads();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for mut payload in payloads.placed {
        let offsets: Vec<usize> = (0..payload.len()).collect();
        mutate_and_parse(&mut payload, &offsets, &mut rng, 2000, |bytes| {
            let _ = PlacedLayer::read_with_trailing(&mut BeReader::new(bytes));
        });
    }
    for mut payload in payloads.placed_data {
        let offsets: Vec<usize> = (0..payload.len()).collect();
        mutate_and_parse(&mut payload, &offsets, &mut rng, 2000, |bytes| {
            let _ = PlacedLayerData::read_with_trailing(&mut BeReader::new(bytes));
        });
    }
    for mut payload in payloads.linked {
        let starts = record_starts(&payload);
        let offsets: Vec<usize> = structural_regions(&payload, &starts)
            .into_iter()
            .flatten()
            .collect();
        let copy_parse = payload.len() <= 64 * 1024;
        mutate_and_parse(&mut payload, &offsets, &mut rng, 2000, |bytes| {
            let _ = LinkedLayerTaggedBlock::read_views(bytes);
            // The owning parser copies source files; only run it on small
            // blocks to keep the test fast.
            if copy_parse {
                let _ = LinkedLayerTaggedBlock::read(&mut BeReader::new(bytes));
            }
        });
    }
}
