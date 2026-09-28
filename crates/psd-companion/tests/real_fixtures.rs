//! Sweeps real `.abr`/`.csh`/`.ase` files from a directory, when one is named.
//!
//! Set `PSD_COMPANION_FIXTURES` to a directory to validate every companion
//! file under it, recursively. This is how the readers were checked against
//! real Photoshop output — a third-party corpus with Photoshop-written brush
//! files is what exposed the RLE framing and section-padding bugs — and the
//! hook stays so the same sweep can be repeated against any corpus:
//!
//! ```text
//! PSD_COMPANION_FIXTURES=/path/to/fixtures cargo test -p psd-companion --test real_fixtures
//! ```
//!
//! The test skips cleanly when the variable is unset, matching the port's
//! `PSD_EXTRA_CORPUS` convention. Failures name the file and the error; there
//! is no pinned-failure list here — a real file this reader cannot parse is a
//! bug, not a known limitation.

use std::path::{Path, PathBuf};

use psd_companion::{read_abr, read_ase, read_csh};

fn files_under(dir: &Path, extension: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(files_under(&path, extension));
        } else if path.extension().and_then(|e| e.to_str()) == Some(extension) {
            out.push(path);
        }
    }
    out.sort();
    out
}

#[test]
fn every_companion_fixture_decodes() {
    let Some(dir) = std::env::var_os("PSD_COMPANION_FIXTURES") else {
        eprintln!("PSD_COMPANION_FIXTURES unset; skipping");
        return;
    };
    let dir = PathBuf::from(dir);
    assert!(
        dir.is_dir(),
        "PSD_COMPANION_FIXTURES is not a directory: {dir:?}"
    );

    let mut checked = 0usize;
    for path in files_under(&dir, "abr") {
        let bytes = std::fs::read(&path).expect("read fixture");
        match read_abr(&bytes) {
            Ok(abr) => {
                eprintln!(
                    "{}: {} brushes, {} samples, {} patterns",
                    path.display(),
                    abr.brushes.len(),
                    abr.samples.len(),
                    abr.patterns.len()
                );
                // Every sampled preset must reference a sample this file
                // actually carries, by UUID: Photoshop does not keep the two
                // lists in a matching order.
                for brush in &abr.brushes {
                    if let psd_companion::BrushShape::Sampled { sampled_data, .. } = &brush.shape {
                        assert!(
                            abr.samples.iter().any(|sample| sample.id == *sampled_data),
                            "{}: brush {:?} references missing sample {sampled_data}",
                            path.display(),
                            brush.name
                        );
                    }
                }
                checked += 1;
            }
            Err(error) => panic!("{}: {error}", path.display()),
        }
    }
    for path in files_under(&dir, "csh") {
        let bytes = std::fs::read(&path).expect("read fixture");
        read_csh(&bytes).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        checked += 1;
    }
    for path in files_under(&dir, "ase") {
        let bytes = std::fs::read(&path).expect("read fixture");
        read_ase(&bytes).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        checked += 1;
    }
    eprintln!("checked {checked} companion fixture(s)");
}
