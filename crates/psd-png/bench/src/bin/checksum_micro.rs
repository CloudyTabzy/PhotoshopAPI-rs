//! CRC-32 and Adler-32 throughput against `crc32fast` and `simd-adler32`, the crates the
//! `png` + `fdeflate` stack uses.
//!
//! The default decode verifies every chunk's CRC-32 over the compressed bytes; the zlib
//! Adler-32, over the decompressed ones, is only checked under `Checks::Full`. Each is timed
//! over buffers of several sizes, since a stage-sized call (a few hundred KiB) is what the
//! streaming decoder makes and a whole-image call is what the one-shot path makes. Small
//! buffers are timed in batches, because one call is shorter than the clock's resolution.
//!
//! Usage: `cargo run --release -p psd-png-bench --bin checksum_micro`

use std::hint::black_box;
use std::time::{Duration, Instant};

/// Best time of one call, from batches of enough calls to last about 20 microseconds.
fn best_of(size: usize, mut body: impl FnMut() -> u32) -> Duration {
    let batch = (65_536 / size).max(1) as u32;
    black_box(body());
    let mut best = Duration::MAX;
    let deadline = Instant::now() + Duration::from_millis(300);
    let mut runs = 0;
    while runs < 5 || (Instant::now() < deadline && runs < 200_000) {
        let start = Instant::now();
        for _ in 0..batch {
            black_box(body());
        }
        best = best.min(start.elapsed() / batch);
        runs += 1;
    }
    best
}

fn main() {
    println!(
        "{:<10} {:>10} {:>12} {:>12} {:>8}",
        "checksum", "bytes", "psd-png", "reference", "ratio"
    );
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    for size in [64usize, 1 << 10, 1 << 14, 1 << 18, 1 << 22] {
        let data: Vec<u8> = (0..size)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect();

        assert_eq!(psd_png::crc32::crc32(&data), crc32fast::hash(&data), "crc32, {size}");
        assert_eq!(
            psd_png::adler32::adler32(&data),
            simd_adler32::adler32(&data.as_slice()),
            "adler32, {size}"
        );

        for (name, ours, theirs) in [
            (
                "crc32",
                best_of(size, || psd_png::crc32::crc32(black_box(&data))),
                best_of(size, || crc32fast::hash(black_box(&data))),
            ),
            (
                "adler32",
                best_of(size, || psd_png::adler32::adler32(black_box(&data))),
                best_of(size, || simd_adler32::adler32(&black_box(data.as_slice()))),
            ),
        ] {
            let rate = |time: Duration| size as f64 / time.as_secs_f64() / 1e9;
            println!(
                "{name:<10} {size:>10} {:>9.2} GB/s {:>9.2} GB/s {:>7.2}x",
                rate(ours),
                rate(theirs),
                theirs.as_secs_f64() / ours.as_secs_f64(),
            );
        }
    }
}
