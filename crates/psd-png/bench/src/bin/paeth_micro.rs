//! Micro-benchmark for the Paeth kernel alone: one fixed image, many iterations.
//!
//! `unfilter_image` reconstructs a whole image; this builds one whose every row uses the
//! Paeth filter, so the number is the kernel's (or, with `PSD_PNG_FORCE_SCALAR=1`, the
//! scalar wavefront's) and nothing else's. Pass a PNG path to run on a real fixture's own
//! filtered bytes instead of the synthetic corpus.
//!
//! Usage: `cargo run --release -p psd-png-bench --bin paeth_micro -- [width] [height] [bpp] [png]`

use std::time::Instant;

use psd_png::filter::unfilter_image;

/// The IDAT payloads of a PNG, concatenated, and the header fields this needs.
fn read_png(path: &str) -> (Vec<u8>, usize, usize, usize) {
    let data = std::fs::read(path).expect("read the fixture");
    let (mut width, mut height, mut color, mut depth) = (0usize, 0usize, 0usize, 0usize);
    let mut idat = Vec::new();
    let mut at = 8;
    while at + 8 <= data.len() {
        let len = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        let kind = &data[at + 4..at + 8];
        let body = &data[at + 8..(at + 8 + len).min(data.len())];
        match kind {
            b"IHDR" => {
                width = u32::from_be_bytes(body[0..4].try_into().unwrap()) as usize;
                height = u32::from_be_bytes(body[4..8].try_into().unwrap()) as usize;
                depth = body[8] as usize;
                color = body[9] as usize;
            }
            b"IDAT" => idat.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
        at += 12 + len;
    }
    let channels = match color {
        0 => 1,
        2 => 3,
        4 => 2,
        6 => 4,
        other => panic!("colour type {other} not handled here"),
    };
    let bpp = channels * (depth / 8);
    (idat, width, height, bpp)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut width: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(2048);
    let mut height: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(512);
    let mut bpp: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(3);
    let path = args.next();

    let filtered = if let Some(path) = &path {
        let (idat, w, h, b) = read_png(path);
        (width, height, bpp) = (w, h, b);
        let expected = (w * b + 1) * h;
        psd_png::inflate::decompress_zlib(&idat, expected).expect("inflate the fixture")
    } else {
        let row_bytes = width * bpp;
        let stride = row_bytes + 1;
        let mut filtered = vec![0u8; stride * height];
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for row in filtered.chunks_exact_mut(stride) {
            row[0] = 4; // Paeth
            for byte in &mut row[1..] {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = (state >> 24) as u8;
            }
        }
        filtered
    };

    let row_bytes = width * bpp;
    let mut scratch = filtered.clone();
    unfilter_image(&mut scratch, row_bytes, height, bpp).unwrap();
    let checksum: u64 = scratch.iter().map(|&b| u64::from(b)).sum();

    let mut best = f64::MAX;
    for _ in 0..30 {
        scratch.copy_from_slice(&filtered);
        let start = Instant::now();
        unfilter_image(&mut scratch, row_bytes, height, bpp).unwrap();
        best = best.min(start.elapsed().as_secs_f64() * 1e3);
    }
    let mb = (row_bytes * height) as f64 / (1024.0 * 1024.0);
    println!(
        "width {width} height {height} bpp {bpp} {}: {best:.3} ms, {:.0} MB/s, checksum {checksum}",
        path.unwrap_or_else(|| "(synthetic)".into()),
        mb / (best / 1e3)
    );
}
