//! Best-of-N time to turn a PNG into 8-bit RGBA pixels, the deliverable the PhotoshopAPI-rs
//! port consumes. Usage: `convert_bench <fixtures dir>`
//!
//! The streaming converting decode writes each row once into its scratch, and the sink
//! copies it into the caller's buffer, so this measures the whole pipeline a consumer sees
//! rather than the decoder in isolation.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use psd_png::{Decoder, Row};

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

fn main() {
    let dir = std::env::args().nth(1).expect("usage: convert_bench <fixtures dir>");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fixtures dir")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            (path.extension().and_then(|e| e.to_str()) == Some("png")).then_some(path)
        })
        .collect();
    files.sort();

    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let png = std::fs::read(&path).expect("read fixture");
        let info = psd_png::read_info(&png).expect("info");
        let capacity = info.width as usize * info.height as usize * 4;

        let elapsed = best_of(|| {
            let mut decoder = Decoder::new();
            let mut out = Vec::with_capacity(capacity);
            decoder
                .decode_to_rgba8(&png, |row: Row<'_>| -> Result<(), psd_png::Error> {
                    out.extend_from_slice(row.bytes);
                    Ok(())
                })
                .expect("decode_to_rgba8");
            out.len()
        });

        println!("{name} {:.2}", elapsed.as_secs_f64() * 1e3);
    }
}
