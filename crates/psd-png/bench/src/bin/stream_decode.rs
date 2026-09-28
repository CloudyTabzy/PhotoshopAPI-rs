//! Interleaved decode() vs decode_to() vs decode_to_rgba8() vs decode_to_rgba16() timing
//! over a fixtures directory. Usage: `stream_decode <fixtures dir>` — best-of-N per arm,
//! per fixture.
//!
//! Four arms, all delivering the same pixels to the caller:
//!
//! - `decode`: the whole image into one buffer, in the file's native layout.
//! - `decode_to`: rows in the native layout, concatenated into a presized buffer.
//! - `decode_to_rgba8`: rows converted to 8-bit RGBA as they are reconstructed, which is
//!   one shape the PhotoshopAPI-rs port consumes.
//! - `decode_to_rgba16`: the same at 16 bits, which is the shape the port's fallback
//!   route for non-native sources actually calls.
//!
//! The converted arm is the one that answers whether paying for conversion during the
//! decode is cheaper than converting the native rows afterwards.

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

fn main() {
    let dir = std::env::args().nth(1).expect("usage: stream_decode <fixtures dir>");
    let files = pngs_under(std::path::Path::new(&dir));
    assert!(!files.is_empty(), "no PNGs under {dir}");

    println!(
        "{:<26} {:>9} {:>9} {:>9} {:>9} {:>8} {:>8}",
        "fixture", "decode ms", "stream ms", "rgba8 ms", "rgba16 ms", "vs strm", "vs strm"
    );
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let png = std::fs::read(&path).expect("read fixture");

        let whole = best_of(|| Decoder::new().decode(&png).map(|i| i.data.len()).unwrap_or(0));
        // The sink copies every row into a presized buffer, so the streaming arm delivers
        // the same pixels decode() does and the comparison is like-for-like. The output
        // buffers live outside the timed body and are cleared per run: allocating them
        // inside would pay page faults scaled to the output size, which is conversion
        // noise, not conversion cost.
        let info = Decoder::new().read_info(&png).unwrap();
        let output_size = info.output_size();
        let mut streamed_out = Vec::with_capacity(output_size);
        let streamed = best_of(|| {
            let mut decoder = Decoder::new();
            streamed_out.clear();
            decoder
                .decode_to(&png, |row: Row<'_>| -> Result<(), psd_png::Error> {
                    streamed_out.extend_from_slice(row.bytes);
                    Ok(())
                })
                .map(|_| streamed_out.len())
                .unwrap_or(0)
        });
        // Same again, with the conversions the port wants folded into the decode. The
        // buffers are sized from the image, not from the file's layout, so a source that
        // is not already the target shape does more writing work here than in the
        // native arm.
        let converted_size = info.width as usize * info.height as usize * 4;
        let mut converted_out = Vec::with_capacity(converted_size);
        let converted = best_of(|| {
            let mut decoder = Decoder::new();
            converted_out.clear();
            decoder
                .decode_to_rgba8(&png, |row: Row<'_>| -> Result<(), psd_png::Error> {
                    converted_out.extend_from_slice(row.bytes);
                    Ok(())
                })
                .map(|_| converted_out.len())
                .unwrap_or(0)
        });
        let mut converted16_out = Vec::with_capacity(converted_size * 2);
        let converted16 = best_of(|| {
            let mut decoder = Decoder::new();
            converted16_out.clear();
            decoder
                .decode_to_rgba16(&png, |row: Row<'_>| -> Result<(), psd_png::Error> {
                    converted16_out.extend_from_slice(row.bytes);
                    Ok(())
                })
                .map(|_| converted16_out.len())
                .unwrap_or(0)
        });
        let vs_stream =
            (converted.as_secs_f64() - streamed.as_secs_f64()) / streamed.as_secs_f64() * 100.0;
        let vs_stream16 =
            (converted16.as_secs_f64() - streamed.as_secs_f64()) / streamed.as_secs_f64() * 100.0;
        println!(
            "{name:<26} {:>9.2} {:>9.2} {:>9.2} {:>9.2} {:>+7.1}% {:>+7.1}%",
            whole.as_secs_f64() * 1e3,
            streamed.as_secs_f64() * 1e3,
            converted.as_secs_f64() * 1e3,
            converted16.as_secs_f64() * 1e3,
            vs_stream,
            vs_stream16
        );
    }
}
