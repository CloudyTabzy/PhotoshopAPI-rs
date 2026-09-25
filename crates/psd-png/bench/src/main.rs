//! Benchmarks psd-png against the `png` + `fdeflate` stack it aims to replace.
//!
//! Usage: `cargo run --release -p psd-png-bench -- <mode>`
//!
//! Modes:
//!   `all` (default)  inflate, decode and unfilter
//!   `decode`         whole-PNG decode against the `png` crate
//!   `inflate`        zlib decompression against `fdeflate`
//!   `unfilter`       scanline reconstruction on its own
//!
//! Every mode runs on the synthetic set from `tools/gen_bench_images.py`. The encode-side
//! modes this harness once carried went with the encoder; the decode comparisons against
//! real files that produced the numbers in `docs/benchmarks.md` ran through the workspace's
//! own measurement scripts.

use std::time::{Duration, Instant};

mod cases;
use cases::{TestImage, load_images};

/// Runs `body` enough times to get a stable figure and returns the best wall time.
///
/// The minimum is the right statistic here: every source of noise on a shared machine adds
/// time, so the fastest observed run is the closest estimate of the work actually done.
fn measure(mut body: impl FnMut() -> usize) -> (Duration, usize) {
    // One warm-up pass to fault in pages and settle the branch predictors.
    let mut output = body();
    let mut best = Duration::MAX;

    let deadline = Instant::now() + Duration::from_millis(600);
    let mut runs = 0;
    while runs < 3 || (Instant::now() < deadline && runs < 200) {
        let start = Instant::now();
        output = body();
        best = best.min(start.elapsed());
        runs += 1;
    }
    (best, output)
}

fn throughput(bytes: usize, time: Duration) -> f64 {
    bytes as f64 / time.as_secs_f64() / 1e6
}

struct Row {
    label: String,
    ours: Duration,
    theirs: Duration,
    bytes: usize,
    our_size: Option<usize>,
    their_size: Option<usize>,
}

fn report(title: &str, rows: &[Row]) {
    println!("\n=== {title} ===");
    println!(
        "{:<22} {:>11} {:>11} {:>8}   {:>11} {:>11} {:>8}",
        "case", "psd-png", "reference", "speedup", "psd-png", "reference", "ratio"
    );
    let mut total_ours = Duration::ZERO;
    let mut total_theirs = Duration::ZERO;
    let (mut total_our_size, mut total_their_size) = (0usize, 0usize);

    for row in rows {
        let speedup = row.theirs.as_secs_f64() / row.ours.as_secs_f64();
        let sizes = match (row.our_size, row.their_size) {
            (Some(a), Some(b)) => format!("{:>11} {:>11} {:>7.3}x", a, b, a as f64 / b as f64),
            _ => format!("{:>11} {:>11} {:>8}", "-", "-", "-"),
        };
        println!(
            "{:<22} {:>8.1} MB/s {:>8.1} MB/s {:>7.2}x   {sizes}",
            row.label,
            throughput(row.bytes, row.ours),
            throughput(row.bytes, row.theirs),
            speedup,
        );
        total_ours += row.ours;
        total_theirs += row.theirs;
        total_our_size += row.our_size.unwrap_or(0);
        total_their_size += row.their_size.unwrap_or(0);
    }

    println!(
        "{:<22} {:>36.2}x total",
        "OVERALL",
        total_theirs.as_secs_f64() / total_ours.as_secs_f64()
    );
    if total_their_size > 0 {
        println!("{:<22} {:>47.3}x size", "", total_our_size as f64 / total_their_size as f64);
    }
}

fn bench_inflate(images: &[TestImage]) {
    let mut rows = Vec::new();
    for image in images {
        // Compare on the zlib stream the reference encoder produced for this image.
        let stream = &image.zlib_stream;
        let expected = image.raw_stream.len();

        let (ours, _) =
            measure(|| psd_png::inflate::decompress_zlib(stream, expected).unwrap().len());
        let (theirs, _) = measure(|| fdeflate::decompress_to_vec(stream).unwrap().len());

        // Split out the two halves of the work so a regression can be attributed.
        let mut buffer = vec![0u8; expected + psd_png::inflate::OUTPUT_SLACK];
        let mut plain = psd_png::inflate::Inflater::new();
        plain.verify_checksum(false);
        let (no_checksum, _) = measure(|| plain.zlib(stream, &mut buffer).unwrap());
        let (checksum_only, _) = measure(|| {
            std::hint::black_box(psd_png::adler32::adler32(std::hint::black_box(
                &buffer[..expected],
            ))) as usize
        });
        println!(
            "    {:<18} decode {:>8.1} MB/s   adler32 {:>8.1} MB/s",
            image.name,
            throughput(expected, no_checksum),
            throughput(expected, checksum_only),
        );

        rows.push(Row {
            label: image.name.clone(),
            ours,
            theirs,
            bytes: expected,
            our_size: None,
            their_size: None,
        });
    }
    report("inflate (fdeflate)", &rows);
}

fn bench_decode(images: &[TestImage]) {
    let mut rows = Vec::new();
    for image in images {
        let png = &image.png;
        let mut decoder = psd_png::decoder::Decoder::new();
        let (ours, size) = measure(|| decoder.decode(png).unwrap().data.len());

        let (theirs, other) = measure(|| {
            let decoder = png::Decoder::new(std::io::Cursor::new(png));
            let mut reader = decoder.read_info().unwrap();
            let mut buffer = vec![0; reader.output_buffer_size().unwrap()];
            let info = reader.next_frame(&mut buffer).unwrap();
            info.buffer_size()
        });
        assert_eq!(size, other, "{} decoded sizes differ", image.name);

        rows.push(Row {
            label: image.name.clone(),
            ours,
            theirs,
            bytes: size,
            our_size: None,
            their_size: None,
        });
    }
    report("decode (png crate)", &rows);
}

/// Times unfiltering on its own, so decode regressions can be attributed to the right stage.
fn bench_unfilter(images: &[TestImage]) {
    let mut rows = Vec::new();
    for image in images {
        let info = psd_png::common::Info::new(
            image.width,
            image.height,
            image.color_type,
            image.bit_depth,
        );
        let row_bytes = info.row_bytes();
        let height = image.height as usize;
        let stride = info.filter_stride();

        let mut scratch = vec![0u8; image.raw_stream.len() + 16];
        let (ours, _) = measure(|| {
            scratch[..image.raw_stream.len()].copy_from_slice(&image.raw_stream);
            psd_png::filter::unfilter_image(&mut scratch, row_bytes, height, stride).unwrap();
            scratch[0] as usize
        });
        // Subtract the cost of restoring the input, which is not part of unfiltering.
        let (copy_only, _) = measure(|| {
            scratch[..image.raw_stream.len()].copy_from_slice(&image.raw_stream);
            scratch[0] as usize
        });

        rows.push(Row {
            label: image.name.clone(),
            ours: ours.saturating_sub(copy_only).max(std::time::Duration::from_nanos(1)),
            theirs: copy_only,
            bytes: row_bytes * height,
            our_size: None,
            their_size: None,
        });
    }
    report("unfilter (vs a plain copy of the same data)", &rows);
}

fn main() {
    let what = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    let images = load_images();
    println!("{} images loaded", images.len());

    if what == "all" || what == "inflate" {
        bench_inflate(&images);
    }
    if what == "all" || what == "decode" {
        bench_decode(&images);
    }
    if what == "all" || what == "unfilter" {
        bench_unfilter(&images);
    }
}
