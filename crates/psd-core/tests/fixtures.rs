//! Smoke test: the vendored upstream test corpus is present and reachable
//! without absolute paths.

use std::path::PathBuf;

/// `crates/psd-core` -> workspace root -> `fixtures/documents`.
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("fixtures/documents")
}

#[test]
fn corpus_is_vendored() {
    let dir = fixtures_dir();
    assert!(dir.is_dir(), "missing fixture corpus at {}", dir.display());

    let count = count_files(&dir);
    assert!(
        count >= 90,
        "fixture corpus looks truncated: only {count} files in {}",
        dir.display()
    );
}

fn count_files(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                count_files(&path)
            } else {
                1
            }
        })
        .sum()
}
