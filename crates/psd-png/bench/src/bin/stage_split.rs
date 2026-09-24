//! How much of a decode is inflation, and how much is everything else.
//!
//! Usage: `stage_split <fixtures dir>`
//!
//! Times two arms per fixture on the same bytes:
//!
//! - `inflate`: `decompress_zlib` over the file's `IDAT`, which stops at the filtered
//!   stream. No reconstruction, no conversion, no allocation of an image.
//! - `decode`: the whole `Decoder::decode` path, with the fused frontier reconstructing
//!   rows in place behind the match window.
//!
//! The ratio is the part of the decode that is *not* inflation. In the fused path the two
//! stages overlap, so a ratio near 1.0 means reconstruction is hidden under inflation and
//! speeding it up cannot shorten the decode; a ratio well above 1.0 means it is exposed.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use psd_png::Decoder;

fn best_of(mut body: impl FnMut() -> usize) -> Duration {
    let _ = body();
    let mut best = Duration::MAX;
    let deadline = Instant::now() + Duration::from_millis(400);
    let mut runs = 0;
    while runs < 3 || (Instant::now() < deadline && runs < 2000) {
        let start = Instant::now();
        let output = body();
        best = best.min(start.elapsed());
        std::hint::black_box(output);
        runs += 1;
    }
    best
}

/// The concatenated `IDAT` payloads, which is what the inflate arm decompresses.
fn idat_bytes(png: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 8;
    while pos + 8 <= png.len() {
        let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
        if &png[pos + 4..pos + 8] == b"IDAT" {
            out.extend_from_slice(&png[pos + 8..pos + 8 + len]);
        }
        pos += 12 + len;
    }
    out
}

/// Every `.png` under `dir`, recursively, so a nested corpus directory works as an argument.
fn pngs_under(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(pngs_under(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("png") {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// How many rows use each filter type, and what the scanline filters cost on *this* image's
/// own filtered bytes — the two numbers that say whether reconstruction is exposed.
fn filter_mix(
    filtered: &[u8],
    row_bytes: usize,
    height: usize,
    pixel_stride: usize,
) -> (Vec<usize>, Duration) {
    let stride = 1 + row_bytes;
    let mut counts = vec![0usize; 5];
    for row in 0..height {
        counts[filtered[row * stride] as usize] += 1;
    }
    // Reversing the filters turns the filtered stream back into pixels, which is exactly the
    // work the fused frontier does — measured on the real data, not a stand-in. Each run
    // needs its own copy, because the pass consumes the buffer, and the copy is made outside
    // the clock so only the reversal is timed.
    let mut best = Duration::MAX;
    let deadline = Instant::now() + Duration::from_millis(400);
    let mut runs = 0;
    loop {
        let mut scratch = filtered.to_vec();
        let start = Instant::now();
        psd_png::filter::unfilter_image(&mut scratch, row_bytes, height, pixel_stride).unwrap();
        best = best.min(start.elapsed());
        std::hint::black_box(&scratch);
        runs += 1;
        if runs >= 3 && (Instant::now() >= deadline || runs >= 2000) {
            break;
        }
    }
    (counts, best)
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: stage_split <fixtures dir>");
    let files = pngs_under(std::path::Path::new(&dir));
    assert!(!files.is_empty(), "no PNGs under {dir}");

    println!(
        "{:<24} {:>6} {:>8} {:>8} {:>6} {:>8}  filter mix (none/sub/up/avg/paeth)",
        "fixture", "MiB", "inflate", "decode", "ratio", "unfilter",
    );
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let png = std::fs::read(&path).expect("read fixture");
        let info = psd_png::read_info(&png).expect("info");
        if info.interlacing != psd_png::Interlacing::None {
            println!("{name:<24} (interlaced, skipped)");
            continue;
        }
        let expected = info.decompressed_size();
        let idat = idat_bytes(&png);
        let mib = expected as f64 / (1024.0 * 1024.0);

        let filtered = psd_png::inflate::decompress_zlib(&idat, expected).unwrap();
        let (counts, unfiltered) =
            filter_mix(&filtered, info.row_bytes(), info.height as usize, info.filter_stride());
        let inflated =
            best_of(|| psd_png::inflate::decompress_zlib(&idat, expected).unwrap().len());
        let decoded = best_of(|| Decoder::new().decode(&png).map(|i| i.data.len()).unwrap_or(0));

        let ratio = decoded.as_secs_f64() / inflated.as_secs_f64();
        println!(
            "{name:<24} {mib:>6.2} {:>7.2}m {:>7.2}m {ratio:>5.2}x {:>7.2}m  {}",
            inflated.as_secs_f64() * 1e3,
            decoded.as_secs_f64() * 1e3,
            unfiltered.as_secs_f64() * 1e3,
            counts.iter().map(|c| format!("{c}")).collect::<Vec<_>>().join("/"),
        );
    }
}
