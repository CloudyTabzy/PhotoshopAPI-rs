//! Conversion from a PNG's native pixel layout to the interleaved 8-bit and 16-bit formats
//! callers usually want.
//!
//! Decoding leaves pixels exactly as the file stores them, because that is the only form
//! that is always correct and always free. These conversions are separate so that a caller
//! who already handles the native layout never pays for them.
//!
//! The work is per row. The row converter resolves what the file says about colour — the
//! palette, the `tRNS` key — once, then converts one reconstructed scanline at a time. The
//! whole-image methods on [`Image`] and the decoder's streaming methods are therefore the
//! same code over a different row source, and cannot drift apart.

use crate::common::{BitDepth, ColorType, Info};
use crate::decoder::Image;
use crate::error::Error;

/// One sample of a converted row: a byte, or a big-endian `u16` pair.
///
/// Implemented for the two sample widths rather than taking one as a type parameter, so a
/// converted row is a plain byte buffer either way and the sink sees one row type.
pub(crate) trait RowSample: Copy {
    /// Bytes each sample occupies in a converted row.
    const WIDTH: usize;
    /// Fully opaque at this width: the top of the output range.
    const OPAQUE: Self;
    /// Fully transparent.
    const TRANSPARENT: Self;
    /// A file sample of `BITS` bits per sample, scaled to fill the output range.
    fn from_raw<const BITS: usize>(value: u16) -> Self;
    /// Writes the sample big-endian at `offset`.
    fn write_be(self, target: &mut [u8], offset: usize);
}

impl RowSample for u8 {
    const WIDTH: usize = 1;
    const OPAQUE: Self = 255;
    const TRANSPARENT: Self = 0;

    #[inline(always)]
    fn from_raw<const BITS: usize>(value: u16) -> Self {
        match BITS {
            1 => (value as u8) * 255,
            2 => (value as u8) * 85,
            4 => (value as u8) * 17,
            8 => value as u8,
            16 => (value >> 8) as u8,
            _ => unreachable!("a PNG sample is 1, 2, 4, 8 or 16 bits"),
        }
    }

    #[inline(always)]
    fn write_be(self, target: &mut [u8], offset: usize) {
        target[offset] = self;
    }
}

/// The sixteen-bit output sample: 8-bit input is scaled up, so a 16-bit consumer sees the
/// full range rather than every sample in the bottom quarter of it.
impl RowSample for u16 {
    const WIDTH: usize = 2;
    const OPAQUE: Self = u16::MAX;
    const TRANSPARENT: Self = 0;

    #[inline(always)]
    fn from_raw<const BITS: usize>(value: u16) -> Self {
        match BITS {
            1 => value * 0xFFFF,
            2 => value * 21845,
            4 => value * 4369,
            8 => value * 257,
            16 => value,
            _ => unreachable!("a PNG sample is 1, 2, 4, 8 or 16 bits"),
        }
    }

    #[inline(always)]
    fn write_be(self, target: &mut [u8], offset: usize) {
        target[offset..offset + 2].copy_from_slice(&self.to_be_bytes());
    }
}

/// Converts reconstructed rows of one image's native layout into an interleaved target
/// layout, at a chosen sample width.
///
/// The per-image facts a row depends on — the colour type, the bit depth, the palette and
/// the `tRNS` key — are resolved once in [`RowConverter::new`], so each row costs only the
/// conversion itself.
pub(crate) struct RowConverter {
    color_type: ColorType,
    bit_depth: BitDepth,
    /// The `tRNS` grey level of a greyscale image, at the file's own depth.
    grey_key: Option<u16>,
    /// The `tRNS` colour of an RGB image, at the file's own depth.
    rgb_key: Option<[u16; 3]>,
    /// The palette of an indexed image, with its `tRNS` alpha folded in.
    palette: Option<Box<Palette>>,
}

/// `PLTE` and `tRNS` resolved together: every entry's colour and alpha as 8-bit RGBA.
///
/// Built once per image, so converting a pixel is one bounds comparison and one table load,
/// instead of a slice lookup into `PLTE` and another into `tRNS` for every pixel.
struct Palette {
    /// One entry per possible index. Entries at or past `len` are never read.
    rgba: [[u8; 4]; 256],
    /// Entries `PLTE` defines; an index at or past this is out of range.
    len: usize,
}

impl Palette {
    fn new(plte: &[u8], trns: Option<&[u8]>) -> Self {
        let mut rgba = [[0, 0, 0, 255]; 256];
        for (entry, rgb) in rgba.iter_mut().zip(plte.chunks_exact(3)) {
            entry[..3].copy_from_slice(rgb);
        }
        // A `tRNS` shorter than the palette leaves the entries past its end opaque.
        for (entry, &alpha) in rgba.iter_mut().zip(trns.unwrap_or_default()) {
            entry[3] = alpha;
        }
        Self { rgba, len: plte.len() / 3 }
    }
}

impl RowConverter {
    /// Resolves the conversion for one image, or reports the missing piece it needs.
    ///
    /// Everything a row depends on besides its own bytes — the `tRNS` key, the palette —
    /// is read here once rather than for every row.
    pub(crate) fn new(info: &Info) -> Result<Self, Error> {
        let transparency = info.transparency.as_deref();
        let (mut grey_key, mut rgb_key, mut palette) = (None, None, None);
        match info.color_type {
            ColorType::Grayscale => {
                grey_key = transparency.filter(|t| t.len() >= 2).map(|t| be16(t, 0));
            }
            ColorType::Rgb => {
                rgb_key = transparency
                    .filter(|t| t.len() >= 6)
                    .map(|t| [be16(t, 0), be16(t, 2), be16(t, 4)]);
            }
            ColorType::Indexed => {
                let plte = info.palette.as_deref().ok_or(Error::MissingPalette)?;
                palette = Some(Box::new(Palette::new(plte, transparency)));
            }
            ColorType::GrayscaleAlpha | ColorType::Rgba => {}
        }
        Ok(Self {
            color_type: info.color_type,
            bit_depth: info.bit_depth,
            grey_key,
            rgb_key,
            palette,
        })
    }

    /// Whether converting to `CHANNELS` samples of `S` leaves every row byte for byte as the
    /// file stores it: RGBA to RGBA, or RGB to RGB, at the file's own sample width. A `tRNS`
    /// colour does not stand in the way of the RGB case, since the alpha it would decide is
    /// dropped from a three-channel row anyway.
    ///
    /// A streaming caller hands such rows straight through instead of copying each one
    /// into a scratch row first.
    pub(crate) fn passes_through<const CHANNELS: usize, S: RowSample>(&self) -> bool {
        let same_width = match self.bit_depth {
            BitDepth::Eight => S::WIDTH == 1,
            BitDepth::Sixteen => S::WIDTH == 2,
            _ => false,
        };
        let same_layout = match self.color_type {
            ColorType::Rgba => CHANNELS == 4,
            ColorType::Rgb => CHANNELS == 3,
            _ => false,
        };
        same_width && same_layout
    }

    /// Converts one native-layout row into `CHANNELS` samples per pixel, `S` wide each.
    ///
    /// `target` is written in full: every byte of it belongs to the converted row.
    pub(crate) fn convert<const CHANNELS: usize, S: RowSample>(
        &self,
        row: &[u8],
        target: &mut [u8],
    ) -> Result<(), Error> {
        if self.passes_through::<CHANNELS, S>() {
            target.copy_from_slice(&row[..target.len()]);
            return Ok(());
        }
        let (grey_key, rgb_key) = (self.grey_key, self.rgb_key);

        // Colour type and bit depth belong to the file, not to the pixel, so they are
        // resolved once per row. Each helper is then straight-line code over a fixed
        // stride, which is what lets the offsets fold. Deciding per pixel instead cost more
        // than the conversion did.
        match (self.color_type, self.bit_depth) {
            (ColorType::Grayscale, BitDepth::One) => {
                grey_row::<CHANNELS, 1, S>(row, target, grey_key)
            }
            (ColorType::Grayscale, BitDepth::Two) => {
                grey_row::<CHANNELS, 2, S>(row, target, grey_key)
            }
            (ColorType::Grayscale, BitDepth::Four) => {
                grey_row::<CHANNELS, 4, S>(row, target, grey_key)
            }
            (ColorType::Grayscale, BitDepth::Eight) => {
                grey_row::<CHANNELS, 8, S>(row, target, grey_key)
            }
            (ColorType::Grayscale, BitDepth::Sixteen) => {
                grey_row::<CHANNELS, 16, S>(row, target, grey_key)
            }

            (ColorType::GrayscaleAlpha, BitDepth::Sixteen) => {
                grey_alpha_row::<CHANNELS, true, S>(row, target)
            }
            (ColorType::GrayscaleAlpha, _) => grey_alpha_row::<CHANNELS, false, S>(row, target),

            (ColorType::Rgb, BitDepth::Sixteen) => {
                rgb_row::<CHANNELS, true, S>(row, target, rgb_key)
            }
            (ColorType::Rgb, _) => rgb_row::<CHANNELS, false, S>(row, target, rgb_key),

            (ColorType::Rgba, BitDepth::Sixteen) => rgba_row::<CHANNELS, true, S>(row, target),
            (ColorType::Rgba, _) => rgba_row::<CHANNELS, false, S>(row, target),

            (ColorType::Indexed, depth) => {
                let palette = self.palette.as_deref().ok_or(Error::MissingPalette)?;
                match depth {
                    BitDepth::One => indexed_row::<CHANNELS, 1, S>(row, target, palette)?,
                    BitDepth::Two => indexed_row::<CHANNELS, 2, S>(row, target, palette)?,
                    BitDepth::Four => indexed_row::<CHANNELS, 4, S>(row, target, palette)?,
                    BitDepth::Eight => indexed_row::<CHANNELS, 8, S>(row, target, palette)?,
                    BitDepth::Sixteen => {
                        unreachable!("indexed PNGs have depths 1, 2, 4 or 8")
                    }
                }
            }
        }
        Ok(())
    }
}

impl Image {
    /// Converts to tightly packed 8-bit RGBA.
    ///
    /// Sub-byte grey levels are scaled to the full range, 16-bit samples are truncated to
    /// their high byte, palette indices are resolved, and `tRNS` becomes real alpha. Fully
    /// opaque images get an alpha of 255.
    pub fn to_rgba8(&self) -> Result<Vec<u8>, Error> {
        let pixels = self.pixel_count();
        let mut output = vec![0u8; pixels * 4];
        self.expand::<4, u8>(&mut output)?;
        Ok(output)
    }

    /// Converts to tightly packed 8-bit RGB, discarding any alpha.
    pub fn to_rgb8(&self) -> Result<Vec<u8>, Error> {
        let pixels = self.pixel_count();
        let mut output = vec![0u8; pixels * 3];
        self.expand::<3, u8>(&mut output)?;
        Ok(output)
    }

    fn pixel_count(&self) -> usize {
        self.info.width as usize * self.info.height as usize
    }

    fn expand<const CHANNELS: usize, S: RowSample>(&self, output: &mut [u8]) -> Result<(), Error> {
        let converter = RowConverter::new(&self.info)?;
        let width = self.info.width as usize;
        let height = self.info.height as usize;
        let row_bytes = self.info.row_bytes();
        let stride = width * CHANNELS * S::WIDTH;

        for y in 0..height {
            let row = &self.data[y * row_bytes..(y + 1) * row_bytes];
            converter.convert::<CHANNELS, S>(row, &mut output[y * stride..(y + 1) * stride])?;
        }
        Ok(())
    }
}

/// Writes one pixel's channels, dropping the alpha when three were asked for.
#[inline(always)]
fn write_pixel<const CHANNELS: usize, S: RowSample>(pixel: &mut [u8], values: [S; 4]) {
    let mut offset = 0;
    for value in values.iter().take(CHANNELS) {
        value.write_be(pixel, offset);
        offset += S::WIDTH;
    }
}

/// Grey levels, scaled to the full output range, with `tRNS` matched against the raw sample.
fn grey_row<const CHANNELS: usize, const BITS: usize, S: RowSample>(
    row: &[u8],
    target: &mut [u8],
    key: Option<u16>,
) {
    for (x, pixel) in target.chunks_exact_mut(CHANNELS * S::WIDTH).enumerate() {
        let raw = sample::<BITS>(row, x);
        let grey = S::from_raw::<BITS>(raw);
        let alpha = if key == Some(raw) { S::TRANSPARENT } else { S::OPAQUE };
        write_pixel::<CHANNELS, S>(pixel, [grey, grey, grey, alpha]);
    }
}

/// Grey plus alpha. `tRNS` does not apply: the alpha channel is already explicit.
fn grey_alpha_row<const CHANNELS: usize, const WIDE: bool, S: RowSample>(
    row: &[u8],
    target: &mut [u8],
) {
    let (stride, step) = if WIDE { (4, 2) } else { (2, 1) };
    for (x, pixel) in target.chunks_exact_mut(CHANNELS * S::WIDTH).enumerate() {
        let base = x * stride;
        let (grey, alpha) = if WIDE {
            (S::from_raw::<16>(be16(row, base)), S::from_raw::<16>(be16(row, base + 2)))
        } else {
            (S::from_raw::<8>(row[base] as u16), S::from_raw::<8>(row[base + step] as u16))
        };
        write_pixel::<CHANNELS, S>(pixel, [grey, grey, grey, alpha]);
    }
}

fn rgb_row<const CHANNELS: usize, const WIDE: bool, S: RowSample>(
    row: &[u8],
    target: &mut [u8],
    key: Option<[u16; 3]>,
) {
    // RGB asked for as RGB at its own width never reaches here: `RowConverter::convert`
    // copies it through whole.
    let (stride, step) = if WIDE { (6, 2) } else { (3, 1) };
    for (x, pixel) in target.chunks_exact_mut(CHANNELS * S::WIDTH).enumerate() {
        let base = x * stride;
        let (r, g, b) = (row[base], row[base + step], row[base + 2 * step]);
        // `tRNS` names a colour at the file's own depth, so a 16-bit image is compared at
        // 16 bits: two colours can differ there and still truncate to the same byte.
        let alpha = match key {
            Some(key) => {
                let raw = if WIDE {
                    [be16(row, base), be16(row, base + 2), be16(row, base + 4)]
                } else {
                    [r as u16, g as u16, b as u16]
                };
                if raw == key { S::TRANSPARENT } else { S::OPAQUE }
            }
            None => S::OPAQUE,
        };
        let channels = if WIDE {
            [
                S::from_raw::<16>(be16(row, base)),
                S::from_raw::<16>(be16(row, base + 2)),
                S::from_raw::<16>(be16(row, base + 4)),
            ]
        } else {
            [S::from_raw::<8>(r as u16), S::from_raw::<8>(g as u16), S::from_raw::<8>(b as u16)]
        };
        write_pixel::<CHANNELS, S>(pixel, [channels[0], channels[1], channels[2], alpha]);
    }
}

fn rgba_row<const CHANNELS: usize, const WIDE: bool, S: RowSample>(row: &[u8], target: &mut [u8]) {
    // RGBA asked for as RGBA at its own width never reaches here: `RowConverter::convert`
    // copies it through whole.
    let stride = if WIDE { 8 } else { 4 };
    for (x, pixel) in target.chunks_exact_mut(CHANNELS * S::WIDTH).enumerate() {
        let base = x * stride;
        let read = |offset: usize| {
            if WIDE {
                S::from_raw::<16>(be16(row, base + 2 * offset))
            } else {
                S::from_raw::<8>(row[base + offset] as u16)
            }
        };
        write_pixel::<CHANNELS, S>(pixel, [read(0), read(1), read(2), read(3)]);
    }
}

fn indexed_row<const CHANNELS: usize, const BITS: usize, S: RowSample>(
    row: &[u8],
    target: &mut [u8],
    palette: &Palette,
) -> Result<(), Error> {
    for (x, pixel) in target.chunks_exact_mut(CHANNELS * S::WIDTH).enumerate() {
        let index = sample::<BITS>(row, x) as usize;
        if index >= palette.len {
            return Err(Error::PaletteIndexOutOfRange);
        }
        // A sample is at most eight bits, so the mask only restates what is already true;
        // it lets the table load go unchecked.
        let rgba = palette.rgba[index & 0xff];
        let channels = rgba.map(|value| S::from_raw::<8>(value as u16));
        write_pixel::<CHANNELS, S>(pixel, channels);
    }
    Ok(())
}

/// Reads the `x`-th sample of a row stored at `BITS` bits per sample.
#[inline(always)]
fn sample<const BITS: usize>(row: &[u8], x: usize) -> u16 {
    match BITS {
        8 => row[x] as u16,
        16 => be16(row, x * 2),
        _ => {
            let offset = x * BITS;
            let mask = (1u8 << BITS) - 1;
            ((row[offset / 8] >> (8 - BITS - offset % 8)) & mask) as u16
        }
    }
}

#[inline]
fn be16(data: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([data[offset], data[offset + 1]])
}
