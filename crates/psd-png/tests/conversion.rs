//! Checks pixel-format conversion against hand-computed expectations.

use psd_png::common::{BitDepth, ColorType, Info};
use psd_png::decoder::Image;

fn image(info: Info, data: Vec<u8>) -> Image {
    Image { info, data }
}

fn info(width: u32, height: u32, color_type: ColorType, bit_depth: BitDepth) -> Info {
    Info::new(width, height, color_type, bit_depth)
}

#[test]
fn grayscale_bit_depths_scale_to_the_full_range() {
    // 1-bit: 0b1010_0000 is white, black, white, black across four pixels.
    let one = image(info(4, 1, ColorType::Grayscale, BitDepth::One), vec![0b1010_0000]);
    assert_eq!(
        one.to_rgba8().unwrap(),
        [255, 255, 255, 255, 0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 255,]
    );

    // 2-bit: the four levels must land on 0, 85, 170, 255.
    let two = image(info(4, 1, ColorType::Grayscale, BitDepth::Two), vec![0b00_01_10_11]);
    let rgb = two.to_rgb8().unwrap();
    assert_eq!(rgb.iter().step_by(3).copied().collect::<Vec<_>>(), [0, 85, 170, 255]);

    // 4-bit: each nibble repeats into a byte.
    let four = image(info(2, 1, ColorType::Grayscale, BitDepth::Four), vec![0x0F]);
    assert_eq!(four.to_rgb8().unwrap(), [0, 0, 0, 255, 255, 255]);

    // 16-bit keeps the high byte.
    let sixteen =
        image(info(2, 1, ColorType::Grayscale, BitDepth::Sixteen), vec![0x12, 0x34, 0xAB, 0xCD]);
    assert_eq!(sixteen.to_rgb8().unwrap(), [0x12, 0x12, 0x12, 0xAB, 0xAB, 0xAB]);
}

#[test]
fn transparency_becomes_alpha() {
    let mut grey = info(3, 1, ColorType::Grayscale, BitDepth::Eight);
    grey.transparency = Some(vec![0x00, 0x40]);
    let converted = image(grey, vec![0x10, 0x40, 0x80]).to_rgba8().unwrap();
    assert_eq!(converted[3], 255);
    assert_eq!(converted[7], 0, "the sample matching tRNS is transparent");
    assert_eq!(converted[11], 255);

    let mut rgb = info(2, 1, ColorType::Rgb, BitDepth::Eight);
    rgb.transparency = Some(vec![0, 1, 0, 2, 0, 3]);
    let converted = image(rgb, vec![1, 2, 3, 4, 5, 6]).to_rgba8().unwrap();
    assert_eq!(converted[3], 0, "the pixel matching tRNS is transparent");
    assert_eq!(converted[7], 255);
}

#[test]
fn palettes_resolve_with_per_entry_alpha() {
    let mut indexed = info(4, 1, ColorType::Indexed, BitDepth::Two);
    indexed.palette = Some(vec![10, 11, 12, 20, 21, 22, 30, 31, 32, 40, 41, 42]);
    indexed.transparency = Some(vec![0, 128]);

    let converted = image(indexed, vec![0b00_01_10_11]).to_rgba8().unwrap();
    assert_eq!(
        converted,
        [
            10, 11, 12, 0, // entry 0, tRNS 0
            20, 21, 22, 128, // entry 1, tRNS 128
            30, 31, 32, 255, // entry 2, past the end of tRNS
            40, 41, 42, 255,
        ]
    );
}

#[test]
fn alpha_channels_pass_through() {
    let rgba = image(
        info(1, 1, ColorType::Rgba, BitDepth::Sixteen),
        vec![0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88],
    );
    assert_eq!(rgba.to_rgba8().unwrap(), [0x11, 0x33, 0x55, 0x77]);
    assert_eq!(rgba.to_rgb8().unwrap(), [0x11, 0x33, 0x55]);

    let grey_alpha =
        image(info(2, 1, ColorType::GrayscaleAlpha, BitDepth::Eight), vec![9, 200, 60, 0]);
    assert_eq!(grey_alpha.to_rgba8().unwrap(), [9, 9, 9, 200, 60, 60, 60, 0]);
}

/// The eight-bit layouts at their own depth, including the rows that convert by a straight
/// copy: an 8-bit RGB asked for as RGB and an 8-bit RGBA asked for as RGBA must come back
/// byte for byte, and grey at 8 bits must not be rescaled on the way through.
#[test]
fn eight_bit_sources_convert_at_their_own_depth() {
    let grey = image(info(2, 1, ColorType::Grayscale, BitDepth::Eight), vec![0x10, 0x20]);
    assert_eq!(grey.to_rgba8().unwrap(), [0x10, 0x10, 0x10, 255, 0x20, 0x20, 0x20, 255]);

    let rgb = image(info(2, 1, ColorType::Rgb, BitDepth::Eight), vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(rgb.to_rgb8().unwrap(), [1, 2, 3, 4, 5, 6], "the copy path is exact");
    assert_eq!(rgb.to_rgba8().unwrap(), [1, 2, 3, 255, 4, 5, 6, 255]);

    let rgba = image(info(1, 1, ColorType::Rgba, BitDepth::Eight), vec![9, 8, 7, 6]);
    assert_eq!(rgba.to_rgba8().unwrap(), [9, 8, 7, 6], "the copy path is exact");
    assert_eq!(rgba.to_rgb8().unwrap(), [9, 8, 7]);
}

/// Sixteen-bit RGB converts by its high byte, and a `tRNS` key is matched at sixteen bits:
/// two colours that share a high byte are still different colours in the file.
#[test]
fn sixteen_bit_rgb_converts_by_high_byte_and_compares_at_full_depth() {
    let rgb = image(
        info(2, 1, ColorType::Rgb, BitDepth::Sixteen),
        vec![0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06],
    );
    assert_eq!(rgb.to_rgb8().unwrap(), [0x12, 0x56, 0x9A, 0x01, 0x03, 0x05]);

    // The key names (1, 2, 3) at full depth. The first pixel has the same high bytes but
    // differs below them, so it must stay opaque.
    let mut keyed = info(2, 1, ColorType::Rgb, BitDepth::Sixteen);
    keyed.transparency = Some(vec![0x00, 0x01, 0x00, 0x02, 0x00, 0x03]);
    let keyed =
        image(keyed, vec![0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0x00, 0x01, 0x00, 0x02, 0x00, 0x03]);
    let converted = keyed.to_rgba8().unwrap();
    assert_eq!(converted[3], 255, "a shared high byte is not a match");
    assert_eq!(converted[7], 0, "the exact 16-bit colour is transparent");

    let grey_alpha = image(
        info(1, 1, ColorType::GrayscaleAlpha, BitDepth::Sixteen),
        vec![0x11, 0x22, 0x33, 0x44],
    );
    assert_eq!(grey_alpha.to_rgba8().unwrap(), [0x11, 0x11, 0x11, 0x33]);
}

/// Rows wide enough to fill the SIMD kernels' register blocks, with a `tRNS` key that recurs
/// at every position within a block. The expectations are worked out from the samples alone
/// rather than from the crate's own scalar path, which is what the kernels are pinned to
/// elsewhere: a kernel that put the transparency on the wrong pixel once passed every parity
/// test over random rows, because random rows almost never contain the key.
#[test]
fn a_transparency_key_lands_on_the_pixels_that_match_it_in_wide_rows() {
    for width in [1u32, 2, 7, 8, 9, 15, 16, 17, 40, 67] {
        // 16-bit grey: every third pixel repeats the key, the others are distinct samples.
        let sample16 = |x: u32| if x % 3 == 1 { 0x1234u16 } else { 0x2000 + 0x0111 * x as u16 };
        let mut grey16 = info(width, 1, ColorType::Grayscale, BitDepth::Sixteen);
        grey16.transparency = Some(vec![0x12, 0x34]);
        let data: Vec<u8> = (0..width).flat_map(|x| sample16(x).to_be_bytes()).collect();
        let want: Vec<u8> = (0..width)
            .flat_map(|x| {
                let sample = sample16(x);
                let high = (sample >> 8) as u8;
                [high, high, high, if sample == 0x1234 { 0 } else { 255 }]
            })
            .collect();
        assert_eq!(image(grey16, data).to_rgba8().unwrap(), want, "16-bit grey, width {width}");

        // 8-bit grey, the same shape at the depth whose kernel reads one byte per pixel.
        let sample8 = |x: u32| if x % 3 == 1 { 0x5Au8 } else { 0x60 + x as u8 };
        let mut grey8 = info(width, 1, ColorType::Grayscale, BitDepth::Eight);
        grey8.transparency = Some(vec![0x00, 0x5A]);
        let data: Vec<u8> = (0..width).map(sample8).collect();
        let want: Vec<u8> = (0..width)
            .flat_map(|x| {
                let sample = sample8(x);
                [sample, sample, sample, if sample == 0x5A { 0 } else { 255 }]
            })
            .collect();
        assert_eq!(image(grey8, data).to_rgba8().unwrap(), want, "8-bit grey, width {width}");
    }
}

/// Palettes at every depth a PNG allows an indexed image to use, since the index is read
/// from a differently packed row each time.
#[test]
fn narrow_palettes_resolve() {
    let mut palette = Some(Vec::new());
    for index in 0..16u8 {
        palette.as_mut().unwrap().extend_from_slice(&[index, index, index]);
    }

    let mut one = info(2, 1, ColorType::Indexed, BitDepth::One);
    one.palette = palette.clone();
    assert_eq!(image(one, vec![0b0100_0000]).to_rgb8().unwrap(), [0, 0, 0, 1, 1, 1]);

    let mut four = info(2, 1, ColorType::Indexed, BitDepth::Four);
    four.palette = palette.clone();
    assert_eq!(image(four, vec![0x0F]).to_rgb8().unwrap(), [0, 0, 0, 15, 15, 15]);

    let mut eight = info(2, 1, ColorType::Indexed, BitDepth::Eight);
    eight.palette = palette;
    assert_eq!(image(eight, vec![0, 15]).to_rgb8().unwrap(), [0, 0, 0, 15, 15, 15]);
}

/// Every image in the generated corpus must convert without panicking, and the result must
/// have exactly one pixel per pixel.
#[test]
fn corpus_converts_cleanly() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp/png");
    if !dir.exists() {
        eprintln!("skipping: {} not generated", dir.display());
        return;
    }
    let mut decoder = psd_png::decoder::Decoder::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("png") {
            continue;
        }
        let decoded = decoder.decode(&std::fs::read(&path).unwrap()).unwrap();
        let pixels = decoded.width() as usize * decoded.height() as usize;
        assert_eq!(decoded.to_rgba8().unwrap().len(), pixels * 4, "{}", path.display());
        assert_eq!(decoded.to_rgb8().unwrap().len(), pixels * 3, "{}", path.display());
    }
}
