//! One-off sanity check for the conversion fixtures: every file under `tmp/convert` and
//! `tmp/convertz` must decode whole and stream to the same native pixels its info
//! describes. Not a correctness gate for the crate — the generators are the fixture
//! source — but a guard so a split that runs on broken fixtures reports nothing.
//!
//! The directories are generator output (`tools/gen_convert_fixtures.py`); a checkout
//! without them skips rather than fails, the same convention as the corpus tests.

use std::path::PathBuf;

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

#[test]
fn conversion_fixtures_decode() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = pngs_under(&root.join("tmp/convert"));
    files.extend(pngs_under(&root.join("tmp/convertz")));
    if files.is_empty() {
        eprintln!("skipping: tmp/convert and tmp/convertz not generated");
        return;
    }
    assert_eq!(files.len(), 14, "expected the seven conversion fixtures, twice");
    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let png = std::fs::read(path).unwrap();
        let info = psd_png::read_info(&png).unwrap();
        let image = psd_png::decode(&png).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(image.data.len(), info.output_size(), "{name}");
        let converted =
            image.to_rgba8().unwrap_or_else(|error| panic!("{name}: converting: {error}"));
        assert_eq!(converted.len() % 4, 0, "{name}");
        std::hint::black_box(&converted);
    }
}
