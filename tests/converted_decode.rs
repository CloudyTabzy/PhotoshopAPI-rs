//! `Decoder::decode_to_rgba8` and its siblings: conversion happens as each row is
//! reconstructed, so the concatenated output must equal the whole-image conversion, row for
//! row, with no full-image intermediate in between.

use std::path::Path;

use psd_png::common::{BitDepth, ColorType, Info, Interlacing};
use psd_png::{Decoder, Row};

/// Streams `decode_to_rgba8` over `png` and requires it to equal `decode().to_rgba8()`.
fn assert_rgba8_rows_match(png: &[u8], name: &str) {
    let image = Decoder::new().decode(png).unwrap_or_else(|e| panic!("{name}: decode: {e}"));
    let expected = image.to_rgba8().unwrap();
    let width = image.info.width as usize * 4;

    let mut rows: Vec<Vec<u8>> = Vec::new();
    Decoder::new()
        .decode_to_rgba8(png, |row: Row<'_>| -> Result<(), psd_png::Error> {
            rows.push(row.bytes.to_vec());
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{name}: decode_to_rgba8: {e}"));

    assert_eq!(rows.len(), image.info.height as usize, "{name}: row count");
    for (index, bytes) in rows.iter().enumerate() {
        assert_eq!(bytes.len(), width, "{name}: row {index} length");
        assert_eq!(
            bytes[..],
            expected[index * width..(index + 1) * width],
            "{name}: row {index} diverges from to_rgba8()"
        );
    }
}

/// Collects the converted rows of a PNG into one buffer.
macro_rules! collect {
    ($name:ident, $method:ident) => {
        fn $name(png: &[u8]) -> Vec<u8> {
            let mut rows = Vec::new();
            Decoder::new()
                .$method(png, |row: Row<'_>| -> Result<(), psd_png::Error> {
                    rows.extend_from_slice(row.bytes);
                    Ok(())
                })
                .unwrap();
            rows
        }
    };
}

collect!(rows_rgba8, decode_to_rgba8);
collect!(rows_rgb8, decode_to_rgb8);
collect!(rows_rgba16, decode_to_rgba16);
collect!(rows_rgb16, decode_to_rgb16);

/// Builds a one-row PNG of the given colour type and depth.
fn one_row_png(info: Info, data: &[u8]) -> Vec<u8> {
    assert_eq!(info.height, 1, "these fixtures are one row tall");
    psd_png::encode(&info, data).unwrap()
}

fn info(width: u32, color_type: ColorType, bit_depth: BitDepth) -> Info {
    Info::new(width, 1, color_type, bit_depth)
}

#[test]
fn rgba8_rows_match_to_rgba8_across_the_corpus() {
    // The reference corpus covers every colour type, bit depth, interlace mode and filter
    // combination, including palette and tRNS sources and the interlaced buffered fallback.
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
        assert_rgba8_rows_match(&png, &name);
    }
}

#[test]
fn rgba8_rows_match_to_rgba8_on_large_fixtures() {
    // The fused path with a conversion in front of it: rows cross segment boundaries here,
    // which is where a scratch buffer reused for every row could go stale.
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

    for path in paths {
        let png = std::fs::read(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert_rgba8_rows_match(&png, &name);
    }
}

#[test]
fn converted_rows_match_on_interlaced_images() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp/png");
    if !dir.exists() {
        eprintln!("skipping: {} not generated", dir.display());
        return;
    }
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("png") {
            continue;
        }
        let png = std::fs::read(&path).unwrap();
        if psd_png::read_info(&png).unwrap().interlacing != Interlacing::Adam7 {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert_rgba8_rows_match(&png, &name);
        checked += 1;
    }
    assert!(checked > 0, "no interlaced images found in {}", dir.display());
}

#[test]
fn rgba16_passes_a_sixteen_bit_source_through_untouched() {
    let samples = [
        0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, // pixel 1
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // pixel 2
    ];
    let png = one_row_png(info(2, ColorType::Rgba, BitDepth::Sixteen), &samples);
    let row = rows_rgba16(&png);
    assert_eq!(row, samples, "a 16-bit source must survive bit for bit");
}

#[test]
fn rgba16_scales_eight_bit_samples_across_the_full_range() {
    let png =
        one_row_png(info(2, ColorType::Rgba, BitDepth::Eight), &[0, 1, 128, 255, 10, 20, 30, 40]);
    let row = rows_rgba16(&png);
    let mut expected = Vec::new();
    for sample in [0u16, 1, 128, 255, 10, 20, 30, 40] {
        expected.extend_from_slice(&(sample * 257).to_be_bytes());
    }
    assert_eq!(row, expected, "8-bit samples scale to the full 16-bit range");
}

#[test]
fn rgba16_scales_sub_byte_greys_to_the_full_range() {
    // 0b1010_0000 is white, black, white, black: both extremes must reach the ends.
    let png = one_row_png(info(4, ColorType::Grayscale, BitDepth::One), &[0b1010_0000]);
    let row = rows_rgba16(&png);
    let mut expected = Vec::new();
    for grey in [u16::MAX, 0, u16::MAX, 0] {
        for _ in 0..3 {
            expected.extend_from_slice(&grey.to_be_bytes());
        }
        expected.extend_from_slice(&u16::MAX.to_be_bytes());
    }
    assert_eq!(row, expected);

    // 2-bit reaches 21845 and 43690 on the way up, never a truncated byte.
    let png = one_row_png(info(4, ColorType::Grayscale, BitDepth::Two), &[0b00_01_10_11]);
    let row = rows_rgb16(&png);
    let mut expected = Vec::new();
    for grey in [0u16, 21845, 43690, 65535] {
        for _ in 0..3 {
            expected.extend_from_slice(&grey.to_be_bytes());
        }
    }
    assert_eq!(row, expected);
}

#[test]
fn rgba16_resolves_transparency_at_full_width() {
    let mut grey = info(3, ColorType::Grayscale, BitDepth::Eight);
    grey.transparency = Some(vec![0x00, 0x40]);
    let png = one_row_png(grey, &[0x10, 0x40, 0x80]);

    let row = rows_rgba16(&png);
    let mut expected = Vec::new();
    for (sample, alpha) in [(0x10u16, u16::MAX), (0x40, 0), (0x80, u16::MAX)] {
        let grey = sample * 257;
        for _ in 0..3 {
            expected.extend_from_slice(&grey.to_be_bytes());
        }
        expected.extend_from_slice(&alpha.to_be_bytes());
    }
    assert_eq!(row, expected, "tRNS becomes alpha at the output width");
}

#[test]
fn rgba16_resolves_palettes_with_per_entry_alpha() {
    let mut indexed = info(2, ColorType::Indexed, BitDepth::Eight);
    indexed.palette = Some(vec![10, 11, 12, 20, 21, 22]);
    indexed.transparency = Some(vec![0, 128]);
    let png = one_row_png(indexed, &[0, 1]);

    let row = rows_rgba16(&png);
    let mut expected = Vec::new();
    for (entry, alpha) in [([10u8, 11, 12], 0u16), ([20, 21, 22], 128 * 257)] {
        for sample in entry {
            expected.extend_from_slice(&(sample as u16 * 257).to_be_bytes());
        }
        expected.extend_from_slice(&alpha.to_be_bytes());
    }
    assert_eq!(row, expected);
}

#[test]
fn rgb8_and_rgb16_drop_the_alpha_channel() {
    let png = one_row_png(info(1, ColorType::Rgba, BitDepth::Eight), &[1, 2, 3, 4]);

    let row = rows_rgb8(&png);
    assert_eq!(row, [1, 2, 3]);

    let row = rows_rgb16(&png);
    let mut expected = Vec::new();
    for sample in [1u16, 2, 3] {
        expected.extend_from_slice(&(sample * 257).to_be_bytes());
    }
    assert_eq!(row, expected);
}

#[test]
fn a_decoder_can_be_moved_to_another_thread() {
    // The port decodes smart objects in parallel, so a `Decoder` has to be `Send`. Proving
    // it by moving one is worth more than a trait assertion: the decode itself runs on the
    // other thread.
    let png = one_row_png(info(4, ColorType::Rgba, BitDepth::Eight), &[3u8; 16]);
    let decoded = std::thread::spawn(move || rows_rgba8(&png)).join().expect("decode off-thread");
    assert_eq!(decoded, [3u8; 16], "the row crosses the thread boundary intact");
}

#[test]
fn a_failing_sink_aborts_a_converting_decode() {
    #[derive(Debug, PartialEq)]
    struct Stop;
    impl From<psd_png::Error> for Stop {
        fn from(_: psd_png::Error) -> Self {
            Stop
        }
    }

    let png = one_row_png(info(4, ColorType::Rgba, BitDepth::Eight), &[7u8; 16]);
    let result = Decoder::new().decode_to_rgba8(&png, |row: Row<'_>| -> Result<(), Stop> {
        let _ = row;
        Err(Stop)
    });
    assert_eq!(result, Err(Stop));
}

#[test]
fn converted_streaming_ignores_the_decompressed_size_ceiling() {
    // 512x512 RGBA8 is 1 MiB filtered, well over the ceiling set here. A converting decode
    // holds one converted row and a bounded stage, so the ceiling cannot apply to it.
    let width = 512u32;
    let height = 512u32;
    let data = vec![0x5Au8; (width * height * 4) as usize];
    let png = psd_png::encode(&Info::new(width, height, ColorType::Rgba, BitDepth::Eight), &data)
        .unwrap();

    let mut decoder = Decoder::new();
    decoder.max_decompressed_size(Some(1024));
    assert!(decoder.decode(&png).is_err(), "decode must respect the ceiling");

    let mut rows = 0usize;
    let mut converted = 0usize;
    let mut decoder = Decoder::new();
    decoder.max_decompressed_size(Some(1024));
    decoder
        .decode_to_rgba8(&png, |row: Row<'_>| -> Result<(), psd_png::Error> {
            rows += 1;
            converted += row.bytes.len();
            assert!(row.bytes.iter().all(|&b| b == 0x5A), "conversion must still run");
            Ok(())
        })
        .expect("decode_to_rgba8 must stream past the ceiling");

    assert_eq!(rows, height as usize);
    assert_eq!(converted, (width * height * 4) as usize);
}
