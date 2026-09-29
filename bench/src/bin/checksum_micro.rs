//! CRC-32 and Adler-32 throughput against `crc32fast` and `simd-adler32`, the crates the
//! `png` + `fdeflate` stack uses.
//!
//! The default decode verifies every chunk's CRC-32; the zlib Adler-32 is only checked under
//! `Checks::Full`. Each is timed over buffers of several sizes, since a stage-sized call (a few
//! hundred KiB) is what the streaming decoder makes and a whole-image call is what the
//! one-shot path makes.
//!
//! Usage: `cargo run --release -p psd-png-bench --bin checksum_micro`

use std::hint::black_box;
use std::time::{Duration, Instant};

fn best_of(mut body: impl FnMut() -> u32) -> Duration {
    black_box(body());
    let mut best = Duration::MAX;
    let deadline = Instant::now() + Duration::from_millis(300);
    let mut runs = 0;
    while runs < 5 || (Instant::now() < deadline && runs < 100_000) {
        let start = Instant::now();
        black_box(body());
        best = best.min(start.elapsed());
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
                best_of(|| psd_png::crc32::crc32(black_box(&data))),
                best_of(|| crc32fast::hash(black_box(&data))),
            ),
            (
                "adler32",
                best_of(|| psd_png::adler32::adler32(black_box(&data))),
                best_of(|| simd_adler32::adler32(&black_box(data.as_slice()))),
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
