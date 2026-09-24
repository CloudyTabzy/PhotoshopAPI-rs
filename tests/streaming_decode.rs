//! `Decoder::decode_to`: rows stream out of the decoder as they are reconstructed,
//! in file order, in the file's native layout — byte-identical to `decode().data` split
//! into scanlines, at O(match window + rows) memory instead of O(width x height).
//!
//! Run `python3 tools/gen_large_fixtures.py` (and `gen_bomb_fixture.py` for the
//! size-ceiling test) to produce the fixtures. Tests skip when they are absent, matching
//! the corpus tests.

use std::path::Path;

use png_spark::{Decoder, Row};

/// Collect every row through the sink and require the concatenation to equal `decode()`s
/// pixel buffer, row for row.
fn assert_rows_match_decode(png: &[u8], name: &str) {
    let image = Decoder::new().decode(png).unwrap_or_else(|e| panic!("{name}: decode: {e}"));
    let info = image.info.clone();
    let row_bytes = info.row_bytes();
    let height = info.height as usize;

    let mut rows: Vec<Vec<u8>> = Vec::new();
    let mut decoder = Decoder::new();
    decoder
        .decode_to(png, |row: Row<'_>| -> Result<(), png_spark::Error> {
            rows.push(row.bytes.to_vec());
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{name}: decode_to: {e}"));

    assert_eq!(rows.len(), height, "{name}: row count");
    for (index, bytes) in rows.iter().enumerate() {
        assert_eq!(bytes.len(), row_bytes, "{name}: row {index} length");
        assert_eq!(
            bytes[..],
            image.data[index * row_bytes..(index + 1) * row_bytes],
            "{name}: row {index} diverges from decode()"
        );
    }
}

#[test]
fn streaming_rows_match_decode_on_large_fixtures() {
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
    for path in paths {
        let png = std::fs::read(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        // Interlaced files take the buffered fallback; parity must hold there too.
        assert_rows_match_decode(&png, &name);
        checked += 1;
    }
    assert!(checked > 0, "no images found in {}", dir.display());
    eprintln!("checked {checked} images");
}

#[test]
fn streaming_rows_match_decode_on_small_and_interlaced() {
    // Small corpus files exercise the below-window path; the interlaced ones the
    // buffered Adam7 fallback. Both must stay row-exact against decode().
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp/png");
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
    paths.truncate(24);

    for path in paths {
        let png = std::fs::read(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert_rows_match_decode(&png, &name);
    }
}

#[derive(Debug)]
enum SinkError {
    Decode(#[allow(dead_code)] png_spark::Error),
    Boom,
}

impl From<png_spark::Error> for SinkError {
    fn from(error: png_spark::Error) -> Self {
        SinkError::Decode(error)
    }
}

#[test]
fn a_failing_sink_aborts_with_its_error() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp/large");
    let png = std::fs::read(dir.join("rgba8_gradient_1024.png")).unwrap();

    #[derive(Debug)]
    struct Seen(std::cell::Cell<usize>);

    let seen = Seen(std::cell::Cell::new(0));
    let mut decoder = Decoder::new();
    let result: Result<(), SinkError> =
        decoder.decode_to(&png, |row: Row<'_>| -> Result<(), SinkError> {
            let n = seen.0.get();
            seen.0.set(n + 1);
            if n == 5 {
                return Err(SinkError::Boom);
            }
            let _ = row;
            Ok(())
        });
    assert!(matches!(result, Err(SinkError::Boom)));
}

#[test]
fn streaming_ignores_the_decompressed_size_ceiling() {
    // A 16384x16384 RGBA8 image is ~1.07 GB filtered: decode() refuses it under the
    // default 512 MiB ceiling, decode_to() must stream it in a bounded stage anyway.
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp/bomb/zeros_16384.png");
    if !path.exists() {
        eprintln!("skipping: {} not generated", path.display());
        return;
    }
    let png = std::fs::read(&path).unwrap();

    let mut decoder = Decoder::new();
    assert!(decoder.decode(&png).is_err(), "decode must refuse the bomb");

    let mut rows = 0usize;
    let mut bytes = 0usize;
    let mut decoder = Decoder::new();
    decoder
        .decode_to(&png, |row: Row<'_>| -> Result<(), png_spark::Error> {
            rows += 1;
            bytes += row.bytes.len();
            Ok(())
        })
        .expect("decode_to must stream past the ceiling");

    assert_eq!(rows, 16384);
    assert_eq!(bytes, 16384 * 16384 * 4);
}

#[test]
fn corrupt_streams_agree_between_paths() {
    // A mid-stream bit flip either breaks inflation (both paths error) or produces a
    // different-but-valid stream (both paths decode it identically). The invariant under
    // corruption is that the paths never disagree.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp/large");
    let path = dir.join("rgba8_gradient_1024.png");
    if !path.exists() {
        eprintln!("skipping: {} not generated", dir.display());
        return;
    }
    let mut png = std::fs::read(&path).unwrap();

    let mut pos = 8;
    let idat_start = loop {
        let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
        if &png[pos + 4..pos + 8] == b"IDAT" {
            break pos + 8;
        }
        pos += 8 + len + 4;
    };
    png[idat_start + 40] ^= 0xFF;

    let decoded = Decoder::new().decode(&png);
    let mut rows: Vec<Vec<u8>> = Vec::new();
    let mut decoder = Decoder::new();
    let streamed: Result<(), png_spark::Error> =
        decoder.decode_to(&png, |row: Row<'_>| -> Result<(), png_spark::Error> {
            rows.push(row.bytes.to_vec());
            Ok(())
        });

    match (decoded, streamed) {
        (Ok(image), Ok(())) => {
            let row_bytes = image.info.row_bytes();
            assert_eq!(rows.len(), image.info.height as usize);
            for (index, bytes) in rows.iter().enumerate() {
                assert_eq!(bytes[..], image.data[index * row_bytes..(index + 1) * row_bytes]);
            }
        }
        (Err(_), Err(_)) => {}
        (decoded, streamed) => panic!(
            "paths disagree: decode {} , decode_to {}",
            if decoded.is_ok() { "ok" } else { "err" },
            if streamed.is_ok() { "ok" } else { "err" },
        ),
    }
}
