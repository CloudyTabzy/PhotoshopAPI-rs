//! Interleaved decode() vs decode_to() timing over a fixtures directory.
//! Usage: `stream_decode <fixtures dir>` — best-of-N per arm, per fixture.

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
    let dir = std::env::args().nth(1).expect("usage: stream_decode <fixtures dir>");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fixtures dir")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            (path.extension().and_then(|e| e.to_str()) == Some("png")).then_some(path)
        })
        .collect();
    files.sort();

    println!("{:<26} {:>9} {:>9} {:>8}", "fixture", "decode ms", "stream ms", "delta");
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let png = std::fs::read(&path).expect("read fixture");

        let whole = best_of(|| Decoder::new().decode(&png).map(|i| i.data.len()).unwrap_or(0));
        // The sink copies every row into a presized buffer, so the streaming arm delivers
        // the same pixels decode() does and the comparison is like-for-like.
        let info = Decoder::new().read_info(&png).unwrap();
        let output_size = info.output_size();
        let streamed = best_of(|| {
            let mut decoder = Decoder::new();
            let mut out = Vec::with_capacity(output_size);
            decoder
                .decode_to(&png, |row: Row<'_>| -> Result<(), psd_png::Error> {
                    out.extend_from_slice(row.bytes);
                    Ok(())
                })
                .map(|_| out.len())
                .unwrap_or(0)
        });
        let delta = (streamed.as_secs_f64() - whole.as_secs_f64()) / whole.as_secs_f64() * 100.0;
        println!(
            "{name:<26} {:>9.2} {:>9.2} {:>+7.1}%",
            whole.as_secs_f64() * 1e3,
            streamed.as_secs_f64() * 1e3,
            delta
        );
    }
}
