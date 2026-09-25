//! The streaming decode over hand-built files: multi-`IDAT` joining, every colour type,
//! and the palette-plus-`tRNS` layout.
//!
//! The encoder is gone from the crate, so the fixtures here come from
//! [`common::build_png`], which writes every scanline stored and uncompressed. That is
//! enough for these tests: what they pin is that the decoder joins `IDAT` chunks in order
//! and reconstructs whatever layout the header names, not that anything compresses well.

mod common;

use common::build_png;
use psd_png::common::{BitDepth, ColorType, Info};

/// Pseudo-random bytes, so the image does not reduce to a trivial pattern.
fn noise(len: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

/// Chunk types in order, so a test can see how the image data was split.
fn chunk_kinds(png: &[u8]) -> Vec<[u8; 4]> {
    let mut kinds = Vec::new();
    let mut pos = 8;
    while pos + 12 <= png.len() {
        let length = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
        let kind: [u8; 4] = png[pos + 4..pos + 8].try_into().unwrap();
        kinds.push(kind);
        pos += 12 + length;
    }
    kinds
}

#[test]
fn a_streamed_image_decodes_to_the_pixels_that_went_in() {
    for (width, height) in [(1u32, 1u32), (17, 3), (64, 64), (300, 200)] {
        let info = Info::new(width, height, ColorType::Rgba, BitDepth::Eight);
        let pixels = noise(info.output_size());
        let png = build_png(&info, &pixels, 3);

        let image = psd_png::decode(&png).unwrap();
        assert_eq!(image.data, pixels, "{width}x{height}");
        assert_eq!(image.width(), width);
        assert_eq!(image.height(), height);
    }
}

#[test]
fn an_image_larger_than_one_chunk_is_joined_from_several_idats() {
    // 512x512 of noise is far more than one 64 KiB chunk can carry.
    let info = Info::new(512, 512, ColorType::Rgba, BitDepth::Eight);
    let pixels = noise(info.output_size());
    let png = build_png(&info, &pixels, 4);

    let kinds = chunk_kinds(&png);
    let idats = kinds.iter().filter(|kind| *kind == b"IDAT").count();
    assert!(idats > 1, "expected several IDAT chunks, got {idats}");

    // Order still has to be right: header first, image data in the middle, IEND last.
    assert_eq!(kinds.first(), Some(b"IHDR"));
    assert_eq!(kinds.last(), Some(b"IEND"));

    assert_eq!(psd_png::decode(&png).unwrap().data, pixels);
}

#[test]
fn every_colour_type_survives_the_streamed_path() {
    let cases = [
        (ColorType::Grayscale, BitDepth::Eight),
        (ColorType::Grayscale, BitDepth::Sixteen),
        (ColorType::Rgb, BitDepth::Eight),
        (ColorType::Rgb, BitDepth::Sixteen),
        (ColorType::GrayscaleAlpha, BitDepth::Eight),
        (ColorType::Rgba, BitDepth::Eight),
        (ColorType::Rgba, BitDepth::Sixteen),
        (ColorType::Grayscale, BitDepth::One),
        (ColorType::Grayscale, BitDepth::Two),
        (ColorType::Grayscale, BitDepth::Four),
    ];
    for (color_type, bit_depth) in cases {
        let info = Info::new(70, 50, color_type, bit_depth);
        let pixels = noise(info.output_size());
        let png = build_png(&info, &pixels, 1);
        let image = psd_png::decode(&png).unwrap();
        assert_eq!(image.data, pixels, "{color_type:?} {bit_depth:?}");
    }
}

#[test]
fn a_palette_and_its_transparency_survive_the_streamed_path() {
    let mut info = Info::new(40, 40, ColorType::Indexed, BitDepth::Eight);
    info.palette = Some((0..256).flat_map(|i| [i as u8, 255 - i as u8, 128]).collect());
    info.transparency = Some((0..256).map(|i| i as u8).collect());
    let pixels = noise(info.output_size());

    let png = build_png(&info, &pixels, 2);

    let image = psd_png::decode(&png).unwrap();
    assert_eq!(image.data, pixels);
    assert_eq!(image.info.palette, info.palette);
    assert_eq!(image.info.transparency, info.transparency);
}
