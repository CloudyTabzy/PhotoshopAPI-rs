//! Byte-exactness of the fused decode path against the legacy two-pass path on fixtures
//! large enough to force reconstruction mid-inflation.
//!
//! `Decoder::decode` reverses scanline filters as inflation's output cursor advances. The
//! generated fixtures exceed DEFLATE's 32 KiB match window in their filtered size and are
//! compressed by CPython's zlib, a full LZ77 match finder, so their streams contain matches
//! reaching back across reconstructed rows. psd-png's own encoder cannot produce such
//! streams (it emits zero-run, distance-1 matches only), which is why these exist.
//!
//! Run `python3 tools/gen_large_fixtures.py` to produce the fixtures. The test skips when
//! they are absent, matching the corpus tests.

use std::path::Path;

use psd_png::filter::unfilter_image;
use psd_png::inflate::decompress_zlib;
use psd_png::{Decoder, Interlacing};

/// The frontier's soundness condition: it never reconstructs a row whose bytes a later
/// match could still reach. Fixtures must exceed this to exercise the live path.
const MAX_MATCH_DISTANCE: usize = 32768;

/// Concatenates a PNG's IDAT payloads the way the decoder's `parse` does.
fn idat_bytes(png: &[u8]) -> Vec<u8> {
    let mut idat = Vec::new();
    let mut pos = 8;
    while pos + 8 <= png.len() {
        let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
        let kind = &png[pos + 4..pos + 8];
        let body = pos + 8;
        assert!(body + len + 4 <= png.len(), "truncated chunk");
        if kind == b"IDAT" {
            idat.extend_from_slice(&png[body..body + len]);
        }
        pos = body + len + 4;
    }
    idat
}

/// The legacy two-pass decode: inflate everything, then unfilter, in place.
fn legacy_decode(png: &[u8], info: &psd_png::Info) -> Vec<u8> {
    let idat = idat_bytes(png);
    let mut buffer = decompress_zlib(&idat, info.decompressed_size()).unwrap();
    unfilter_image(&mut buffer, info.row_bytes(), info.height as usize, info.filter_stride())
        .unwrap();
    buffer.truncate(info.output_size());
    buffer
}

#[test]
fn fused_decode_matches_the_legacy_two_pass_path() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp/large");
    if !dir.exists() {
        eprintln!("skipping: {} not generated", dir.display());
        return;
    }

    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("png"))
        .collect();
    paths.sort();

    let mut checked = 0;
    let mut largest = 0;
    for path in paths {
        let png = std::fs::read(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let image = Decoder::new().decode(&png).unwrap_or_else(|e| panic!("{name}: {e}"));

        // The legacy reference is only expressible through public APIs for the
        // non-interlaced path; Adam7 is covered by the reference corpus tests.
        if image.info.interlacing != Interlacing::None {
            continue;
        }

        largest = largest.max(image.info.decompressed_size());
        let legacy = legacy_decode(&png, &image.info);
        assert_eq!(image.data, legacy, "{name}: fused decode diverged from the legacy path");
        checked += 1;
    }

    assert!(checked > 0, "no non-interlaced images found in {}", dir.display());
    assert!(
        largest > MAX_MATCH_DISTANCE * 2,
        "fixture set is too small ({largest} bytes) to exercise reconstruction mid-inflation"
    );
    eprintln!("checked {checked} images, largest filtered stream {largest} bytes");
}
