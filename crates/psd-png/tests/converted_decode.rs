//! `Decoder::decode_to_rgba8` and its siblings: conversion happens as each row is
//! reconstructed, so the concatenated output must equal the whole-image conversion, row for
//! row, with no full-image intermediate in between.

use std::path::Path;

mod common;

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
    common::build_png(&info, data, 1)
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
fn converted_streaming_passes_a_ceiling_the_image_exceeds() {
    // 512x512 RGBA8 is 1 MiB filtered, well over the ceiling set here. A converting decode
    // holds one converted row and a stage of about 300 KiB, and those are what the ceiling
    // bounds when streaming, so the image streams although `decode` must refuse it.
    let width = 512u32;
    let height = 512u32;
    let data = vec![0x5Au8; (width * height * 4) as usize];
    let png =
        common::build_png(&Info::new(width, height, ColorType::Rgba, BitDepth::Eight), &data, 1);

    let mut decoder = Decoder::new();
    decoder.max_decompressed_size(Some(512 << 10));
    assert!(decoder.decode(&png).is_err(), "decode must respect the ceiling");

    let mut rows = 0usize;
    let mut converted = 0usize;
    let mut decoder = Decoder::new();
    decoder.max_decompressed_size(Some(512 << 10));
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

/// When the requested layout is the file's own — RGBA to RGBA, RGB to RGB, at the file's
/// sample width — the rows are handed over without a scratch copy, and must still be exactly
/// the rows `decode` produces. RGB passes through even with a `tRNS` colour, since the alpha
/// it would decide is dropped from a three-channel row.
#[test]
fn rows_already_in_the_requested_layout_pass_through() {
    let (width, height) = (5u32, 3u32);
    let bytes = |len: usize| -> Vec<u8> { (0..len).map(|i| (i * 37 + 11) as u8).collect() };

    let mut rgb_keyed = Info::new(width, height, ColorType::Rgb, BitDepth::Eight);
    rgb_keyed.transparency = Some(vec![0, 11, 0, 48, 0, 85]);
    let cases = [
        (
            Info::new(width, height, ColorType::Rgba, BitDepth::Eight),
            rows_rgba8 as fn(&[u8]) -> Vec<u8>,
        ),
        (Info::new(width, height, ColorType::Rgba, BitDepth::Sixteen), rows_rgba16),
        (Info::new(width, height, ColorType::Rgb, BitDepth::Eight), rows_rgb8),
        (Info::new(width, height, ColorType::Rgb, BitDepth::Sixteen), rows_rgb16),
        (rgb_keyed, rows_rgb8),
    ];
    for (info, stream) in cases {
        let data = bytes(info.output_size());
        let png = common::build_png(&info, &data, 1);
        assert_eq!(stream(&png), data, "{:?} {:?}", info.color_type, info.bit_depth);
    }
}

/// A palette index past the end of `PLTE` fails a converting decode, as it fails `decode`,
/// rather than reading a colour the file never defined.
#[test]
fn an_index_past_the_palette_fails_a_converting_decode() {
    // Two-bit indices 0, 1, 2, 3 against a two-entry palette: the last two are out of range.
    // The stream is a single stored block carrying one row of one byte: zlib header, the
    // final-block byte, length and its one's complement, the filter byte and the row byte,
    // then the Adler-32 of those two data bytes.
    let idat = [0x78u8, 0x01, 0x01, 0x02, 0x00, 0xFD, 0xFF, 0x00, 0x1B, 0x00, 0x1D, 0x00, 0x1C];

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut chunk = |kind: &[u8; 4], data: &[u8]| {
        png.extend((data.len() as u32).to_be_bytes());
        let start = png.len();
        png.extend(kind);
        png.extend(data);
        let crc = psd_png::crc32::crc32(&png[start..]);
        png.extend(crc.to_be_bytes());
    };
    chunk(b"IHDR", &[0, 0, 0, 4, 0, 0, 0, 1, 2, 3, 0, 0, 0]);
    chunk(b"PLTE", &[10, 20, 30, 40, 50, 60]);
    chunk(b"IDAT", &idat);
    chunk(b"IEND", &[]);

    assert_eq!(Decoder::new().decode(&png).unwrap_err(), psd_png::Error::PaletteIndexOutOfRange);
    let streamed = Decoder::new().decode_to_rgba8(&png, |_: Row<'_>| Ok::<(), psd_png::Error>(()));
    assert_eq!(streamed, Err(psd_png::Error::PaletteIndexOutOfRange));
    let streamed = Decoder::new().decode_to_rgb16(&png, |_: Row<'_>| Ok::<(), psd_png::Error>(()));
    assert_eq!(streamed, Err(psd_png::Error::PaletteIndexOutOfRange));
}
