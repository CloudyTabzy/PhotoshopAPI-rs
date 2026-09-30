//! Accuracy probe for `LayeredFile::composite_rgba8` against Photoshop's own flattens.
//!
//! Point `PSD_COMPOSITE_ORACLE` at a directory holding `<name>.psd` (or `.psb`)
//! documents next to `<name>.bmp`, a 24-bit Photoshop flatten over a white
//! background. The test does nothing when the variable is unset. Optional:
//!
//! - `PSD_COMPOSITE_FILTER`: only run files whose name contains this substring.
//! - `PSD_COMPOSITE_OUT`: write our flatten beside a diff image (BMP) per file.
//! - `PSD_COMPOSITE_STRICT`: fail when a file's max error exceeds this value.
//!
//! Run: `PSD_COMPOSITE_ORACLE=<dir> cargo test -p psd --test composite_oracle --release -- --nocapture`

use std::path::{Path, PathBuf};
use std::time::Instant;

use psd::LayeredFile;

struct Bmp {
    width: usize,
    height: usize,
    /// Top-down RGB.
    rgb: Vec<u8>,
}

fn read_bmp(path: &Path) -> Bmp {
    let data = std::fs::read(path).unwrap();
    assert_eq!(&data[..2], b"BM");
    let offset = u32::from_le_bytes(data[10..14].try_into().unwrap()) as usize;
    let width = i32::from_le_bytes(data[18..22].try_into().unwrap());
    let height = i32::from_le_bytes(data[22..26].try_into().unwrap());
    let bpp = u16::from_le_bytes(data[28..30].try_into().unwrap());
    assert_eq!(bpp, 24, "only 24-bit BMPs are supported");
    let (w, h) = (width as usize, height.unsigned_abs() as usize);
    let stride = (w * 3 + 3) & !3;
    let mut rgb = vec![0u8; w * h * 3];
    for y in 0..h {
        let src_row = if height > 0 { h - 1 - y } else { y };
        let row = &data[offset + src_row * stride..];
        for x in 0..w {
            rgb[(y * w + x) * 3] = row[x * 3 + 2];
            rgb[(y * w + x) * 3 + 1] = row[x * 3 + 1];
            rgb[(y * w + x) * 3 + 2] = row[x * 3];
        }
    }
    Bmp {
        width: w,
        height: h,
        rgb,
    }
}

fn write_bmp(path: &Path, width: usize, height: usize, rgb: &[u8]) {
    let stride = (width * 3 + 3) & !3;
    let size = 54 + stride * height;
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(size as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(width as i32).to_le_bytes());
    out.extend_from_slice(&(height as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    for y in (0..height).rev() {
        let start = out.len();
        for x in 0..width {
            let p = (y * width + x) * 3;
            out.extend_from_slice(&[rgb[p + 2], rgb[p + 1], rgb[p]]);
        }
        out.resize(start + stride, 0);
    }
    std::fs::write(path, out).unwrap();
}

fn source_for(dir: &Path, stem: &str) -> Option<PathBuf> {
    let mut candidates = vec![stem.to_string()];
    if let Some(base) = stem.strip_suffix("-psb") {
        candidates.push(base.to_string());
    }
    if let Some(base) = stem.strip_suffix("-render") {
        candidates.push(format!("{base}-roundtrip"));
    }
    for name in candidates {
        for ext in ["psd", "psb"] {
            let path = dir.join(format!("{name}.{ext}"));
            if path.exists() {
                return Some(path);
            }
        }
    }
    None
}

#[test]
fn composite_matches_photoshop_flattens() {
    let Some(dir) = std::env::var_os("PSD_COMPOSITE_ORACLE").map(PathBuf::from) else {
        return;
    };
    let filter = std::env::var("PSD_COMPOSITE_FILTER").unwrap_or_default();
    let out_dir = std::env::var_os("PSD_COMPOSITE_OUT").map(PathBuf::from);
    let strict: Option<u32> = std::env::var("PSD_COMPOSITE_STRICT")
        .ok()
        .and_then(|v| v.parse().ok());
    if let Some(out) = &out_dir {
        std::fs::create_dir_all(out).unwrap();
    }

    let mut stems: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            (p.extension()? == "bmp").then(|| p.file_stem().unwrap().to_string_lossy().into_owned())
        })
        .filter(|s| s.contains(&filter))
        .collect();
    stems.sort();

    println!(
        "{:<44} {:>6} {:>4} {:>7} {:>7}  note",
        "file", "mean", "max", ">2 %", "ms"
    );
    let mut failures = Vec::new();
    for stem in stems {
        let Some(source) = source_for(&dir, &stem) else {
            println!("{stem:<44} (no matching document)");
            continue;
        };
        let oracle = read_bmp(&dir.join(format!("{stem}.bmp")));
        let file = match LayeredFile::<u8>::read(&source) {
            Ok(f) => f,
            Err(e) => {
                println!("{stem:<44} read failed: {e}");
                continue;
            }
        };
        let started = Instant::now();
        let image = match file.composite_rgba8() {
            Ok(i) => i,
            Err(e) => {
                println!("{stem:<44} composite failed: {e}");
                continue;
            }
        };
        let ms = started.elapsed().as_millis();
        if image.width as usize != oracle.width || image.height as usize != oracle.height {
            println!(
                "{stem:<44} size mismatch: ours {}x{}, reference {}x{}",
                image.width, image.height, oracle.width, oracle.height
            );
            continue;
        }
        // The reference is a flatten over white; put ours over white too.
        let mut ours = vec![0u8; oracle.rgb.len()];
        for i in 0..(oracle.width * oracle.height) {
            let a = image.rgba[i * 4 + 3] as f32 / 255.0;
            for c in 0..3 {
                let v = image.rgba[i * 4 + c] as f32 * a + 255.0 * (1.0 - a);
                ours[i * 3 + c] = v.round().clamp(0.0, 255.0) as u8;
            }
        }
        let (mut sum, mut max, mut over) = (0u64, 0u32, 0usize);
        let mut diff = vec![0u8; ours.len()];
        for i in 0..(oracle.width * oracle.height) {
            let mut worst = 0u32;
            for c in 0..3 {
                let d = (ours[i * 3 + c] as i32 - oracle.rgb[i * 3 + c] as i32).unsigned_abs();
                sum += d as u64;
                worst = worst.max(d);
                diff[i * 3 + c] = (d * 8).min(255) as u8;
            }
            max = max.max(worst);
            if worst > 2 {
                over += 1;
            }
        }
        let pixels = oracle.width * oracle.height;
        let mean = sum as f64 / (pixels * 3) as f64;
        let over_pct = over as f64 * 100.0 / pixels as f64;
        println!("{stem:<44} {mean:>6.2} {max:>4} {over_pct:>7.2} {ms:>7}");
        if let Some(out) = &out_dir {
            write_bmp(
                &out.join(format!("{stem}.ours.bmp")),
                oracle.width,
                oracle.height,
                &ours,
            );
            write_bmp(
                &out.join(format!("{stem}.diff.bmp")),
                oracle.width,
                oracle.height,
                &diff,
            );
        }
        if strict.is_some_and(|limit| max > limit) {
            failures.push(stem);
        }
    }
    assert!(failures.is_empty(), "over the error limit: {failures:?}");
}
