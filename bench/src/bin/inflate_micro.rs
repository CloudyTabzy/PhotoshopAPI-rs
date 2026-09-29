//! Pure DEFLATE decode throughput against `fdeflate`, over the IDAT streams of a directory of
//! PNGs.
//!
//! Both sides decode into a buffer that is allocated once, outside the timed body, with the
//! Adler-32 off: what is left is the Huffman loop and the match copies, which is the part
//! the inflate stage of a decode pays for. (`bench -- inflate` times a whole-stream call
//! with the checksum on and the reference growing its output from 1 KiB, which measures
//! something else.) Interlaced files are skipped; their IDAT is one stream but its length is
//! not a function of the header alone.
//!
//! Two environment variables serve A/B runs of two builds of this crate: `INFLATE_MICRO_REF=0`
//! leaves the reference out, and `INFLATE_MICRO_MS=<n>` sets the per-stream time budget in
//! milliseconds (default 500). The last column is psd-png's best time in microseconds.
//!
//! Usage: `cargo run --release -p psd-png-bench --bin inflate_micro -- <dir> [<dir> ...]`

use std::path::PathBuf;
use std::time::{Duration, Instant};

use psd_png::inflate::{Inflater, OUTPUT_SLACK};

/// A PNG's zlib stream and the length it inflates to.
struct Stream {
    name: String,
    zlib: Vec<u8>,
    expected: usize,
}

fn read_stream(path: &PathBuf) -> Option<Stream> {
    let data = std::fs::read(path).ok()?;
    let (mut width, mut height, mut color, mut depth, mut interlace) = (0usize, 0, 0, 0, 0);
    let mut zlib = Vec::new();
    let mut at = 8;
    while at + 8 <= data.len() {
        let len = u32::from_be_bytes(data[at..at + 4].try_into().ok()?) as usize;
        let kind = &data[at + 4..at + 8];
        let body = &data[at + 8..(at + 8 + len).min(data.len())];
        match kind {
            b"IHDR" => {
                width = u32::from_be_bytes(body[0..4].try_into().ok()?) as usize;
                height = u32::from_be_bytes(body[4..8].try_into().ok()?) as usize;
                depth = usize::from(body[8]);
                color = usize::from(body[9]);
                interlace = body[12];
            }
            b"IDAT" => zlib.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
        at += 12 + len;
    }
    if interlace != 0 {
        return None;
    }
    let channels = match color {
        0 | 3 => 1,
        2 => 3,
        4 => 2,
        6 => 4,
        _ => return None,
    };
    let row_bytes = (width * channels * depth).div_ceil(8);
    let name = path.file_stem()?.to_string_lossy().into_owned();
    Some(Stream { name, zlib, expected: (row_bytes + 1) * height })
}

/// Best wall time of `body` over a budget, after one warm-up call.
fn best_of(budget: Duration, mut body: impl FnMut() -> usize) -> (Duration, usize) {
    let mut produced = body();
    let mut best = Duration::MAX;
    let deadline = Instant::now() + budget;
    let mut runs = 0;
    while runs < 5 || (Instant::now() < deadline && runs < 400) {
        let start = Instant::now();
        produced = body();
        best = best.min(start.elapsed());
        runs += 1;
    }
    (best, produced)
}

fn main() {
    let dirs: Vec<String> = std::env::args().skip(1).collect();
    assert!(!dirs.is_empty(), "usage: inflate_micro <dir> [<dir> ...]");
    let with_reference = std::env::var("INFLATE_MICRO_REF").as_deref() != Ok("0");
    let budget = Duration::from_millis(
        std::env::var("INFLATE_MICRO_MS").ok().and_then(|ms| ms.parse().ok()).unwrap_or(500),
    );

    let mut streams = Vec::new();
    for dir in &dirs {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
            .expect("fixtures dir")
            .filter_map(|entry| Some(entry.ok()?.path()))
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("png"))
            .collect();
        paths.sort();
        streams.extend(paths.iter().filter_map(read_stream));
    }

    let (mut total_ours, mut total_theirs) = (Duration::ZERO, Duration::ZERO);
    let mut inflater = Inflater::new();
    inflater.verify_checksum(false);
    println!(
        "{:<28} {:>10} {:>12} {:>12} {:>8} {:>10}",
        "stream", "MB out", "psd-png", "fdeflate", "ratio", "psd-png us"
    );

    for stream in &streams {
        let mut ours = vec![0u8; stream.expected + OUTPUT_SLACK];
        let (our_time, our_len) =
            best_of(budget, || inflater.zlib(&stream.zlib, &mut ours).unwrap());
        assert_eq!(our_len, stream.expected, "{}: length", stream.name);

        let mb = stream.expected as f64 / 1e6;
        let our_us = our_time.as_secs_f64() * 1e6;
        if !with_reference {
            println!(
                "{:<28} {:>10.2} {:>9.0} MB/s {:>12} {:>8} {:>10.1}",
                stream.name,
                mb,
                mb / our_time.as_secs_f64(),
                "-",
                "-",
                our_us
            );
            total_ours += our_time;
            continue;
        }

        // fdeflate's decompressor takes the zlib stream, header included, as `zlib` does.
        let mut theirs = vec![0u8; stream.expected + 1024];
        let (their_time, their_len) = best_of(budget, || {
            let mut decoder = fdeflate::Decompressor::new();
            decoder.ignore_adler32();
            let mut input = 0;
            let mut output = 0;
            while !decoder.is_done() {
                let (consumed, produced) =
                    decoder.read(&stream.zlib[input..], &mut theirs, output, true).unwrap();
                input += consumed;
                output += produced;
                assert!(consumed + produced > 0 || decoder.is_done(), "fdeflate stalled");
            }
            output
        });
        assert_eq!(their_len, stream.expected, "{}: fdeflate length", stream.name);
        assert_eq!(&ours[..stream.expected], &theirs[..stream.expected], "{}: bytes", stream.name);

        println!(
            "{:<28} {:>10.2} {:>9.0} MB/s {:>9.0} MB/s {:>7.2}x {:>10.1}",
            stream.name,
            mb,
            mb / our_time.as_secs_f64(),
            mb / their_time.as_secs_f64(),
            their_time.as_secs_f64() / our_time.as_secs_f64(),
            our_us,
        );
        total_ours += our_time;
        total_theirs += their_time;
    }
    if with_reference {
        println!(
            "{:<28} {:>10} {:>12.1} {:>12.1} {:>7.2}x  (ms, summed best times)",
            "TOTAL",
            "",
            total_ours.as_secs_f64() * 1e3,
            total_theirs.as_secs_f64() * 1e3,
            total_theirs.as_secs_f64() / total_ours.as_secs_f64(),
        );
    } else {
        println!(
            "{:<28} {:>10} {:>12.1}  (ms, summed best times)",
            "TOTAL",
            "",
            total_ours.as_secs_f64() * 1e3
        );
    }
}
