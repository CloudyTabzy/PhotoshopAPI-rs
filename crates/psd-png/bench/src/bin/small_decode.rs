//! Small-file decode timing: best-of-N per fixture over a directory, for A/B runs where
//! the reconstruction frontier should never fire mid-inflation (streams under the 32 KiB
//! match window). Usage: `small_decode <fixtures dir> [rounds]`

use std::path::PathBuf;
use std::time::{Duration, Instant};

fn best_of(mut body: impl FnMut() -> usize) -> Duration {
    let _ = body();
    let mut best = Duration::MAX;
    for _ in 0..3000 {
        let start = Instant::now();
        let output = body();
        best = best.min(start.elapsed());
        std::hint::black_box(output);
    }
    best
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: small_decode <fixtures dir>");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fixtures dir")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            (path.extension().and_then(|e| e.to_str()) == Some("png")).then_some(path)
        })
        .collect();
    files.sort();
    files.truncate(12);

    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let png = std::fs::read(&path).expect("read fixture");
        let best = best_of(|| psd_png::decode(&png).map(|image| image.data.len()).unwrap_or(0));
        println!(
            "{name:<26} {:>8.1} KB {:>10.0} ns",
            png.len() as f64 / 1024.0,
            best.as_nanos() as f64
        );
    }
}
