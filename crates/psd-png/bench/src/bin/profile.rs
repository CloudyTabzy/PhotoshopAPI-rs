//! Stage profile of psd-png's decode path over a directory of PNG fixtures.
//!
//! Reports best-of-N wall time for the whole decode and for the stages that dominate it:
//! chunk CRC verification, IDAT inflation, and scanline reconstruction. `other` is the
//! remainder (chunk walk, buffer allocation, Adam7 de-interleave, copies).
//!
//! Usage: `cargo run --release -p psd-png-bench --bin profile -- <fixtures dir>`

use std::path::PathBuf;
use std::time::{Duration, Instant};

use psd_png::common::{BitDepth, ColorType, Info, Interlacing};

/// Best-of-N wall time, with a warm-up and a wall-clock budget, matching the crate's own
/// benchmark methodology (minimum is the right statistic on a noisy machine).
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

struct Fixture {
    data: Vec<u8>,
    info: Info,
    idat: Vec<u8>,
}

fn parse(data: Vec<u8>) -> Option<Fixture> {
    if data.len() < 33 || &data[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let width = u32::from_be_bytes(data[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(data[20..24].try_into().ok()?);
    let bit_depth = BitDepth::from_byte(data[24])?;
    let color_type = ColorType::from_byte(data[25]).ok()?;
    let interlacing = match data[28] {
        0 => Interlacing::None,
        1 => Interlacing::Adam7,
        _ => return None,
    };
    let mut info = Info::new(width, height, color_type, bit_depth);
    info.interlacing = interlacing;
    info.validate().ok()?;

    let mut pos = 8;
    let mut idat = Vec::new();
    while pos + 8 <= data.len() {
        let len = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
        let kind = &data[pos + 4..pos + 8];
        let body = pos + 8;
        if body + len + 4 > data.len() {
            return None;
        }
        if kind == b"IDAT" {
            idat.extend_from_slice(&data[body..body + len]);
        }
        pos = body + len + 4;
    }
    Some(Fixture { data, info, idat })
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: profile <fixtures dir>");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fixtures dir")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_string_lossy().into_owned();
            (name.ends_with(".png")
                && !name.contains(".enc-")
                && !name.contains(".pngcrate")
                && !name.contains(".zune")
                && !name.contains(".pngspark"))
            .then_some(path)
        })
        .collect();
    files.sort();

    println!(
        "{:<26} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "fixture", "KB", "decode", "crc", "inflate", "unfilter", "parts", "other"
    );
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let Some(fixture) = parse(std::fs::read(&path).expect("read fixture")) else {
            println!("{name:<26} unparsed");
            continue;
        };
        let expected = fixture.info.decompressed_size();

        let decode =
            best_of(|| psd_png::decode(&fixture.data).map(|image| image.data.len()).unwrap_or(0));
        let crc = best_of(|| psd_png::crc32::crc32(&fixture.data) as usize);
        let inflate = best_of(|| {
            psd_png::inflate::decompress_zlib(&fixture.idat, expected)
                .map(|buffer| buffer.len())
                .unwrap_or(0)
        });

        // The unfilter timer must not include resetting the buffer, so the copy happens
        // outside the timed region.
        let unfilter = if fixture.info.interlacing == Interlacing::None {
            let row_bytes = fixture.info.row_bytes();
            let height = fixture.info.height as usize;
            let bpp = fixture.info.filter_stride();
            let inflated = psd_png::inflate::decompress_zlib(&fixture.idat, expected).unwrap();
            let mut buffer = inflated.clone();
            let mut best = Duration::MAX;
            for _ in 0..200 {
                buffer.copy_from_slice(&inflated);
                let start = Instant::now();
                psd_png::filter::unfilter_image(&mut buffer, row_bytes, height, bpp).unwrap();
                best = best.min(start.elapsed());
                std::hint::black_box(&buffer);
            }
            Some(best)
        } else {
            None
        };

        let parts = ms(crc) + ms(inflate) + unfilter.map_or(0.0, ms);
        println!(
            "{:<26} {:>8.1} {:>9.2} {:>9.2} {:>9.2} {:>9.2} {:>9.2} {:>9.2}",
            name,
            fixture.data.len() as f64 / 1024.0,
            ms(decode),
            ms(crc),
            ms(inflate),
            unfilter.map_or(f64::NAN, ms),
            parts,
            ms(decode) - parts
        );
    }
}
