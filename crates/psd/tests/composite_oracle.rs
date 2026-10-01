//! Accuracy probe for `LayeredFile::composite_rgba8` against Photoshop's own flattens.
//!
//! Point `PSD_COMPOSITE_ORACLE` at a directory holding `<name>.psd` (or `.psb`)
//! documents next to `<name>.bmp`, a 24-bit Photoshop flatten over a white
//! background. The test does nothing when the variable is unset. Optional:
//!
//! - `PSD_COMPOSITE_FILTER`: only run files whose name contains this substring.
//! - `PSD_COMPOSITE_OUT`: write our flatten beside a diff image (BMP) per file.
//! - `PSD_COMPOSITE_STRICT`: fail when a file's max error exceeds this value,
//!   a comparison cannot complete, or no comparable documents are found.
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

fn strict_limit() -> Option<u32> {
    std::env::var_os("PSD_COMPOSITE_STRICT").map(|value| {
        value
            .into_string()
            .expect("PSD_COMPOSITE_STRICT must be a u32")
            .parse()
            .expect("PSD_COMPOSITE_STRICT must be a u32")
    })
}

/// Diagnostic runs report skips; strict runs require complete comparisons.
struct AccuracyGate {
    limit: Option<u32>,
    compared: usize,
    failures: Vec<String>,
}

impl AccuracyGate {
    fn new(limit: Option<u32>) -> Self {
        Self {
            limit,
            compared: 0,
            failures: Vec::new(),
        }
    }

    fn incomplete(&mut self, name: &str, reason: &str) {
        if self.limit.is_some() {
            self.failures.push(format!("{name}: {reason}"));
        }
    }

    fn compared(&mut self, name: &str, max: u32) {
        self.compared += 1;
        if let Some(limit) = self.limit.filter(|limit| max > *limit) {
            self.failures
                .push(format!("{name}: maximum error {max} exceeds {limit}"));
        }
    }

    fn finish(self) -> Result<usize, String> {
        if !self.failures.is_empty() {
            return Err(format!("oracle comparisons failed: {:?}", self.failures));
        }
        if self.limit.is_some() && self.compared == 0 {
            return Err("strict oracle validation completed no comparisons".to_owned());
        }
        Ok(self.compared)
    }
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
    compare_photoshop_flattens(&dir, &filter, out_dir.as_deref(), strict_limit()).unwrap();
}

fn compare_photoshop_flattens(
    dir: &Path,
    filter: &str,
    out_dir: Option<&Path>,
    strict: Option<u32>,
) -> Result<usize, String> {
    if let Some(out) = out_dir {
        std::fs::create_dir_all(out).unwrap();
    }

    let mut stems: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            (p.extension()? == "bmp").then(|| p.file_stem().unwrap().to_string_lossy().into_owned())
        })
        .filter(|s| s.contains(filter))
        .collect();
    stems.sort();

    println!(
        "{:<44} {:>6} {:>4} {:>7} {:>7}  note",
        "file", "mean", "max", ">2 %", "ms"
    );
    let mut gate = AccuracyGate::new(strict);
    for stem in stems {
        let Some(source) = source_for(dir, &stem) else {
            println!("{stem:<44} (no matching document)");
            gate.incomplete(&stem, "no matching document");
            continue;
        };
        let oracle = read_bmp(&dir.join(format!("{stem}.bmp")));
        let file = match LayeredFile::<u8>::read(&source) {
            Ok(f) => f,
            Err(e) => {
                println!("{stem:<44} read failed: {e}");
                gate.incomplete(&stem, &format!("read failed: {e}"));
                continue;
            }
        };
        let started = Instant::now();
        let image = match file.composite_rgba8() {
            Ok(i) => i,
            Err(e) => {
                println!("{stem:<44} composite failed: {e}");
                gate.incomplete(&stem, &format!("composite failed: {e}"));
                continue;
            }
        };
        let ms = started.elapsed().as_millis();
        if image.width as usize != oracle.width || image.height as usize != oracle.height {
            println!(
                "{stem:<44} size mismatch: ours {}x{}, reference {}x{}",
                image.width, image.height, oracle.width, oracle.height
            );
            gate.incomplete(&stem, "size mismatch");
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
        gate.compared(&stem, max);
    }
    let compared = gate.finish()?;
    println!("{compared} completed oracle comparisons");
    Ok(compared)
}

struct TemporaryOracle(PathBuf);

impl TemporaryOracle {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "psd-composite-oracle-{}-{time}-{id}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn white_bmp(&self, width: usize) {
        write_bmp(&self.0.join("fixture.bmp"), width, 1, &vec![255; width * 3]);
    }

    fn empty_psd(&self) {
        let file = LayeredFile::<u8>::new(psd::core::ColorMode::Rgb, 1, 1).unwrap();
        std::fs::write(self.0.join("fixture.psd"), file.to_bytes().unwrap()).unwrap();
    }
}

impl Drop for TemporaryOracle {
    fn drop(&mut self) {
        // This unique directory was created by the test and contains only its fixtures.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn strict_oracle_requires_at_least_one_complete_comparison() {
    let oracle = TemporaryOracle::new();
    let error = compare_photoshop_flattens(&oracle.0, "", None, Some(0)).unwrap_err();
    assert!(error.contains("no comparisons"));
    oracle.white_bmp(1);
    oracle.empty_psd();
    assert_eq!(
        compare_photoshop_flattens(&oracle.0, "", None, Some(0)).unwrap(),
        1
    );
    assert!(compare_photoshop_flattens(&oracle.0, "not-present", None, Some(0)).is_err());
}

#[test]
fn strict_oracle_fails_for_missing_or_unreadable_documents() {
    let oracle = TemporaryOracle::new();
    oracle.white_bmp(1);
    let error = compare_photoshop_flattens(&oracle.0, "", None, Some(0)).unwrap_err();
    assert!(error.contains("no matching document"));
    std::fs::write(oracle.0.join("fixture.psd"), b"not a PSD").unwrap();
    let error = compare_photoshop_flattens(&oracle.0, "", None, Some(0)).unwrap_err();
    assert!(error.contains("read failed"));
    // A diagnostic sweep still reports and skips a bad source.
    assert_eq!(
        compare_photoshop_flattens(&oracle.0, "", None, None).unwrap(),
        0
    );
}

#[test]
fn strict_oracle_fails_for_size_mismatches_and_excess_error() {
    let oracle = TemporaryOracle::new();
    oracle.empty_psd();
    oracle.white_bmp(2);
    let error = compare_photoshop_flattens(&oracle.0, "", None, Some(0)).unwrap_err();
    assert!(error.contains("size mismatch"));
    write_bmp(&oracle.0.join("fixture.bmp"), 1, 1, &[0; 3]);
    let error = compare_photoshop_flattens(&oracle.0, "", None, Some(0)).unwrap_err();
    assert!(error.contains("maximum error 255 exceeds 0"));
    assert_eq!(
        compare_photoshop_flattens(&oracle.0, "", None, Some(255)).unwrap(),
        1
    );
}

#[test]
fn strict_oracle_reports_compositor_errors_even_when_another_document_matches() {
    use psd::core::{TaggedBlock, TaggedBlockKey};
    let oracle = TemporaryOracle::new();
    oracle.white_bmp(1);
    oracle.empty_psd();
    // The format reader preserves settings as raw blocks. Resolving this
    // truncated Levels payload fails only when the compositor requests it.
    let mut file = LayeredFile::<u8>::new(psd::core::ColorMode::Rgb, 1, 1).unwrap();
    let mut adjustment = psd::Layer::new_adjustment("invalid Levels", psd::Rect::default());
    adjustment
        .blocks
        .push(TaggedBlock::new(TaggedBlockKey::new(*b"levl"), vec![0]));
    file.add_layer(adjustment);
    std::fs::write(oracle.0.join("broken.psd"), file.to_bytes().unwrap()).unwrap();
    write_bmp(&oracle.0.join("broken.bmp"), 1, 1, &[255; 3]);
    let error = compare_photoshop_flattens(&oracle.0, "", None, Some(0)).unwrap_err();
    assert!(error.contains("broken: composite failed"));
    assert_eq!(
        compare_photoshop_flattens(&oracle.0, "", None, None).unwrap(),
        1
    );
}

/// Photoshop's stored merged image of each document in a directory of
/// `.merged` extracts (magic `PSDMERG1`, big-endian `u32` width, `u32` height, `u16`
/// channels, `u16` depth, then interleaved 8-bit samples) and a
/// `manifest.tsv` of `slug<TAB>status<TAB>detail` rows, where the slug is the
/// source path with `/` replaced by `__`.
///
/// Set `PSD_COMPOSITE_MERGED` to that directory and
/// `PSD_COMPOSITE_MERGED_SOURCES` to the directory the slugs are relative to.
/// A stored merge is often a flat placeholder, so documents whose merge has
/// fewer than 33 distinct colours are skipped. The worst files by mean error
/// are listed; `PSD_COMPOSITE_FILTER` narrows the run.
#[test]
fn composite_tracks_stored_merges() {
    let (Some(dir), Some(sources)) = (
        std::env::var_os("PSD_COMPOSITE_MERGED").map(PathBuf::from),
        std::env::var_os("PSD_COMPOSITE_MERGED_SOURCES").map(PathBuf::from),
    ) else {
        return;
    };
    let filter = std::env::var("PSD_COMPOSITE_FILTER").unwrap_or_default();
    let manifest = std::fs::read_to_string(dir.join("manifest.tsv")).unwrap();
    let mut rows = Vec::new();
    let mut gate = AccuracyGate::new(strict_limit());
    for line in manifest.lines() {
        let mut fields = line.split('\t');
        let (Some(slug), Some(status)) = (fields.next(), fields.next()) else {
            continue;
        };
        if status != "ok" || !slug.contains(&filter) {
            continue;
        }
        let source = sources.join(slug.replace("__", "/"));
        let Ok(data) = std::fs::read(dir.join(format!("{slug}.merged"))) else {
            gate.incomplete(slug, "missing or unreadable merged extract");
            continue;
        };
        if data.len() < 20 || &data[..8] != b"PSDMERG1" {
            gate.incomplete(slug, "invalid merged extract header");
            continue;
        }
        let width = u32::from_be_bytes(data[8..12].try_into().unwrap()) as usize;
        let height = u32::from_be_bytes(data[12..16].try_into().unwrap()) as usize;
        let channels = u16::from_be_bytes(data[16..18].try_into().unwrap()) as usize;
        let samples = &data[20..];
        if samples.len() < width * height * channels || !(3..=4).contains(&channels) {
            gate.incomplete(slug, "invalid merged extract samples");
            continue;
        }
        // Photoshop stores a merge that has transparency already matted
        // against white next to its alpha channel, so the colour channels are
        // the flatten over white as they are.
        // Some merges instead keep straight colour with black under a clear
        // alpha; those are composited over white here.
        let straight = channels == 4 && {
            let clear: Vec<usize> = (0..width * height)
                .filter(|index| samples[index * 4 + 3] == 0)
                .collect();
            !clear.is_empty()
                && clear
                    .iter()
                    .all(|index| samples[index * 4..index * 4 + 3] == [0, 0, 0])
        };
        let over_white = |index: usize| -> [u8; 3] {
            let color = [
                samples[index * channels],
                samples[index * channels + 1],
                samples[index * channels + 2],
            ];
            if !straight {
                return color;
            }
            let alpha = f32::from(samples[index * channels + 3]) / 255.0;
            color.map(|value| (f32::from(value) * alpha + 255.0 * (1.0 - alpha)).round() as u8)
        };
        let mut colors = std::collections::HashSet::new();
        for index in 0..width * height {
            colors.insert(over_white(index));
            if colors.len() > 32 {
                break;
            }
        }
        if colors.len() <= 32 {
            continue;
        }
        let file = match LayeredFile::<u8>::read(&source) {
            Ok(file) => file,
            Err(error) => {
                gate.incomplete(slug, &format!("read failed: {error}"));
                continue;
            }
        };
        if (file.width as usize, file.height as usize) != (width, height) {
            gate.incomplete(slug, "size mismatch");
            continue;
        }
        // The extracts read a merge's first three planes as RGB, which is
        // wrong for any mode that is not RGB or grayscale.
        if !matches!(
            file.color_mode,
            psd::core::ColorMode::Rgb | psd::core::ColorMode::Grayscale
        ) {
            continue;
        }
        let started = Instant::now();
        let image = match file.composite_rgba8() {
            Ok(image) => image,
            Err(error) => {
                gate.incomplete(slug, &format!("composite failed: {error}"));
                continue;
            }
        };
        let ms = started.elapsed().as_millis();
        let (mut sum, mut max, mut over) = (0u64, 0u32, 0usize);
        for index in 0..width * height {
            let reference = over_white(index);
            let alpha = f32::from(image.rgba[index * 4 + 3]) / 255.0;
            let mut worst = 0u32;
            for (channel, expected) in reference.iter().enumerate() {
                let value =
                    f32::from(image.rgba[index * 4 + channel]) * alpha + 255.0 * (1.0 - alpha);
                let diff = (value.round() as i32 - i32::from(*expected)).unsigned_abs();
                sum += u64::from(diff);
                worst = worst.max(diff);
            }
            max = max.max(worst);
            if worst > 2 {
                over += 1;
            }
        }
        let pixels = (width * height) as f64;
        gate.compared(slug, max);
        rows.push((
            sum as f64 / (pixels * 3.0),
            max,
            over as f64 * 100.0 / pixels,
            ms,
            slug.to_owned(),
        ));
    }
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    println!("{} documents with a real stored merge", rows.len());
    println!("{:>6} {:>4} {:>7} {:>6}  file", "mean", "max", ">2 %", "ms");
    let shown = std::env::var("PSD_COMPOSITE_TOP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);
    for (mean, max, over, ms, slug) in rows.iter().take(shown) {
        println!("{mean:>6.2} {max:>4} {over:>7.2} {ms:>6}  {slug}");
    }
    let median = rows.get(rows.len() / 2).map_or(0.0, |row| row.0);
    println!("median mean error {median:.3}");
    gate.finish().unwrap();
}
