//! `Checks::Full` verifies the zlib Adler-32, which covers the *filtered* bytes the
//! decompressor produces. The fused path reverses scanline filters in place while it
//! inflates, so the checksum has to be taken over those bytes before the frontier rewrites
//! them — otherwise a valid image is measured against reconstructed data and refused.
//!
//! The fixtures here are built in memory rather than read from `tmp/`, because a 512x512
//! RGBA8 image is 1 MiB filtered: comfortably past the 32 KiB match window, so the fused
//! path is the one under test rather than the two-pass fallback.

use psd_png::common::{BitDepth, ColorType, Info};
use psd_png::{Checks, Decoder, Row};

mod common;

use common::build_png;

/// 512x512 RGBA8: 1 MiB of filtered data, so the frontier reconstructs mid-stream.
fn fused_png() -> Vec<u8> {
    let (width, height) = (512u32, 512u32);
    let data: Vec<u8> = (0..(width * height * 4) as usize).map(|i| (i % 251) as u8).collect();
    build_png(&Info::new(width, height, ColorType::Rgba, BitDepth::Eight), &data, 1)
}

/// Damages the Adler-32 trailer inside the zlib stream and repairs the chunk CRC, so the
/// image data still decodes and only the checksum can notice.
fn corrupt_trailer(png: &[u8]) -> Vec<u8> {
    let mut png = png.to_vec();
    let mut pos = 8;
    let mut last_idat = None;
    while pos + 8 <= png.len() {
        let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
        if &png[pos + 4..pos + 8] == b"IDAT" {
            last_idat = Some((pos, len));
        }
        pos += 12 + len;
    }
    let (start, len) = last_idat.expect("the encoder writes IDAT");
    let trailer = start + 8 + len - 4;
    png[trailer + 3] ^= 0x01;
    let crc = psd_png::crc32::crc32(&png[start + 4..start + 8 + len]);
    png[start + 8 + len..start + 12 + len].copy_from_slice(&crc.to_be_bytes());
    png
}

#[test]
fn full_checks_accepts_a_fused_whole_image_decode() {
    let png = fused_png();
    let image = Decoder::new()
        .checks(Checks::Full)
        .decode(&png)
        .expect("a valid file must pass its own checksum");
    assert_eq!(image.width(), 512);
}

#[test]
fn full_checks_accepts_a_fused_streaming_decode() {
    let png = fused_png();
    let mut rows = 0usize;
    Decoder::new()
        .checks(Checks::Full)
        .decode_to(&png, |_row: Row<'_>| -> Result<(), psd_png::Error> {
            rows += 1;
            Ok(())
        })
        .expect("a valid file must pass its own checksum while streaming");
    assert_eq!(rows, 512);
}

#[test]
fn full_checks_accepts_a_fused_converting_decode() {
    let png = fused_png();
    let mut bytes = 0usize;
    Decoder::new()
        .checks(Checks::Full)
        .decode_to_rgba8(&png, |row: Row<'_>| -> Result<(), psd_png::Error> {
            bytes += row.bytes.len();
            Ok(())
        })
        .expect("conversion must not disturb the checksum");
    assert_eq!(bytes, 512 * 512 * 4);
}

#[test]
fn full_checks_still_reject_a_damaged_stream() {
    // The point of hashing inside the hook: the bytes must be covered, not skipped.
    let png = corrupt_trailer(&fused_png());

    // The chunk CRC is intact, so the default check level sees a sound file...
    assert!(
        Decoder::new().decode(&png).is_ok(),
        "only the Adler-32 is damaged, so the CRC check must pass it"
    );

    let error = Decoder::new()
        .checks(Checks::Full)
        .decode(&png)
        .expect_err("a damaged checksum must be refused");
    assert_eq!(error, psd_png::Error::Inflate(psd_png::inflate::InflateError::WrongChecksum));
}

#[test]
fn the_check_level_does_not_change_the_pixels() {
    let png = fused_png();
    let unchecked = Decoder::new().decode(&png).unwrap();
    let checked = Decoder::new().checks(Checks::Full).decode(&png).unwrap();
    assert_eq!(unchecked.data, checked.data, "verification must not alter output");

    let mut streamed = Vec::new();
    Decoder::new()
        .checks(Checks::Full)
        .decode_to(&png, |row: Row<'_>| -> Result<(), psd_png::Error> {
            streamed.extend_from_slice(row.bytes);
            Ok(())
        })
        .unwrap();
    assert_eq!(streamed, unchecked.data, "streaming must equal the whole image");
}
