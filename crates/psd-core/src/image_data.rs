//! The merged composite ImageData section (`PhotoshopFile/ImageData.h`).
//!
//! This section holds a flattened composite of the layer tree and exists for
//! interoperability (Lightroom et al.). The core reader leaves it raw. The
//! document layer can preserve a source section verbatim when it is the only
//! pixel data in a layerless document; normal writes synthesize an RLE fill
//! section, which Photoshop accepts and which cuts 20–50% off the file size.
//!
//! Layout: `u16` compression (`1` = RLE) | for each channel a scanline-size
//! table (`u16` PSD / `u32` PSB) | for each channel the concatenated
//! per-scanline PackBits streams. Because every pixel is the fill constant,
//! each scanline compresses to runs of 128 plus a remainder, and this module
//! can synthesize the exact bytes without depending on `psd-codecs`
//! (dependency direction).

use crate::enums::{BitDepth, Version};
use crate::error::{PsdError, Result};
use crate::header::FileHeader;
use crate::io::BeWriter;

/// The merged composite section.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImageData {
    /// Number of channels written for the merged image. Photoshop's header
    /// counts alpha channels; the merged data does not, so this is set
    /// explicitly by the document layer (upstream does the same).
    pub num_channels: u16,
    /// Original merged section, when a document is layerless and the caller
    /// has retained it for compositing and lossless round trips.
    raw_section: Option<std::sync::Arc<Vec<u8>>>,
}

impl ImageData {
    pub fn new(num_channels: u16) -> Self {
        Self {
            num_channels,
            raw_section: None,
        }
    }

    /// Preserve an existing merged section verbatim on write.
    pub fn set_raw_section(&mut self, section: Option<Vec<u8>>) {
        self.raw_section = section.map(std::sync::Arc::new);
    }

    /// Share immutable retained bytes with a document without cloning its
    /// merged image for each serialization or document snapshot.
    pub fn set_shared_raw_section(&mut self, section: Option<std::sync::Arc<Vec<u8>>>) {
        self.raw_section = section;
    }

    /// The retained section, including its two-byte compression marker.
    pub fn raw_section(&self) -> Option<&[u8]> {
        self.raw_section.as_deref().map(Vec::as_slice)
    }

    /// Emit retained bytes directly or stream the repeated merged placeholder
    /// through bounded blocks.
    pub(crate) fn write_to<W: std::io::Write>(
        &self,
        sink: &mut W,
        header: &FileHeader,
    ) -> Result<()> {
        if let Some(section) = self.raw_section() {
            if section.len() < 2 {
                return Err(PsdError::InvalidData {
                    offset: 0,
                    message: "retained merged image section is truncated",
                });
            }
            sink.write_all(section)?;
        } else {
            let scanline_bytes = Self::scanline_bytes(header);
            let scanline_size = Self::scanline_size(scanline_bytes);
            if header.version == Version::Psd && scanline_size > u64::from(u16::MAX) {
                return Err(PsdError::LengthOverflow {
                    actual: scanline_size,
                    width: 2,
                });
            }
            sink.write_all(&1u16.to_be_bytes())?;
            let rows = u64::from(self.num_channels) * u64::from(header.height);
            if header.version == Version::Psd {
                write_repeated(sink, &(scanline_size as u16).to_be_bytes(), rows)?;
            } else {
                write_repeated(sink, &(scanline_size as u32).to_be_bytes(), rows)?;
            }
            let fill = if header.depth == BitDepth::ThirtyTwo {
                0
            } else {
                MERGED_FILL
            };
            let mut row = BeWriter::new();
            write_fill_scanline(&mut row, scanline_bytes, fill);
            write_repeated(sink, row.as_slice(), rows)?;
        }
        Ok(())
    }

    /// Bytes per scanline (one channel) for the given header.
    fn scanline_bytes(header: &FileHeader) -> u64 {
        u64::from(header.width) * u64::from(header.depth.bytes_per_sample())
    }

    /// Compressed size of one constant scanline: two bytes per 128-byte run,
    /// with a final two-byte literal when the remainder is one byte.
    fn scanline_size(byte_len: u64) -> u64 {
        2 * byte_len.div_ceil(128)
    }

    /// Write the section as a constant fill (upstream `ImageData::write`).
    ///
    /// The fill is **white**, not black: Photoshop stores a transparent merge
    /// already matted over white (`[255, 255, 255, 0]`), and a document saved
    /// without a merge carries a solid fill of that shape. A three-channel
    /// document has no alpha to hide behind, so white is also what a reader
    /// that only shows the merged image expects to see.
    ///
    /// The exception is 32-bit: Photoshop's own 32-bit no-merge saves fill
    /// `0x00` — a float `0.0`, transparent black — where `0xFF` bytes would
    /// decode as NaN. (Fixture evidence: the `*MaximizeCompatibilityOff_32bit`
    /// corpus documents carry RLE rows of `0x81 0x00`; the 8/16-bit ones carry
    /// `0x81 0xFF`.)
    pub fn write(&self, writer: &mut BeWriter, header: &FileHeader) -> Result<()> {
        if let Some(section) = &self.raw_section {
            if section.len() < 2 {
                return Err(PsdError::InvalidData {
                    offset: 0,
                    message: "retained merged image section is truncated",
                });
            }
            writer.bytes(section);
            return Ok(());
        }
        writer.u16(1); // RLE compression marker

        let scanline_bytes = Self::scanline_bytes(header);
        let scanline_size = Self::scanline_size(scanline_bytes);
        if header.version == Version::Psd && scanline_size > u64::from(u16::MAX) {
            return Err(PsdError::LengthOverflow {
                actual: scanline_size,
                width: 2,
            });
        }

        // Scanline-size tables, one full table per channel.
        for _ in 0..self.num_channels {
            for _ in 0..header.height {
                if header.version == Version::Psd {
                    writer.u16(scanline_size as u16);
                } else {
                    writer.u32(scanline_size as u32);
                }
            }
        }
        // One compressed stream per channel. The fill is white at integer
        // depths and float-zero at 32 bits (see `write`'s docs).
        let fill = if header.depth == BitDepth::ThirtyTwo {
            0
        } else {
            MERGED_FILL
        };
        for _ in 0..self.num_channels {
            for _ in 0..header.height {
                write_fill_scanline(writer, scanline_bytes, fill);
            }
        }
        Ok(())
    }
}

fn write_repeated<W: std::io::Write>(sink: &mut W, pattern: &[u8], mut count: u64) -> Result<()> {
    if count == 0 || pattern.is_empty() {
        return Ok(());
    }
    let copies = count.min((32 * 1024 / pattern.len()).max(1) as u64) as usize;
    let mut block = Vec::with_capacity(pattern.len() * copies);
    for _ in 0..copies {
        block.extend_from_slice(pattern);
    }
    while count >= copies as u64 {
        sink.write_all(&block)?;
        count -= copies as u64;
    }
    sink.write_all(&block[..count as usize * pattern.len()])?;
    Ok(())
}

/// The byte the merged-image fill uses at integer depths; see
/// [`ImageData::write`] for the 32-bit exception.
pub const MERGED_FILL: u8 = 255;

/// PackBits-encode a scanline of `byte_len` copies of `fill` (the only scanline
/// the port ever writes). Mirrors upstream `RLE_Impl::CompressPackBits` for
/// all-equal input: runs of at most 128, a trailing literal when a single byte
/// remains, and even sizing by construction.
fn write_fill_scanline(writer: &mut BeWriter, byte_len: u64, fill: u8) {
    let mut remaining = byte_len;
    while remaining >= 2 {
        let run = remaining.min(128);
        writer.u8((257 - run) as u8);
        writer.u8(fill);
        remaining -= run;
    }
    if remaining == 1 {
        // A single byte cannot be a run; encode as a one-byte literal.
        writer.u8(0);
        writer.u8(fill);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enums::{BitDepth, ColorMode};

    fn header(version: Version, depth: BitDepth, width: u32) -> FileHeader {
        FileHeader::new(version, 3, width, 2, depth, ColorMode::Rgb).unwrap()
    }

    #[test]
    fn fill_scanline_encodings_match_packbits() {
        let cases: &[(u64, &[u8])] = &[
            (1, &[0x00, 0x00]),
            (2, &[0xFF, 0x00]),
            (4, &[0xFD, 0x00]),
            (128, &[0x81, 0x00]),
            (129, &[0x81, 0x00, 0x00, 0x00]),
            (256, &[0x81, 0x00, 0x81, 0x00]),
        ];
        for (len, expected) in cases {
            let mut w = BeWriter::new();
            write_fill_scanline(&mut w, *len, 0);
            assert_eq!(w.as_slice(), *expected, "byte_len {len}");
            assert_eq!(w.as_slice().len() as u64, ImageData::scanline_size(*len));
        }
        // The fill the port actually writes is white, and it lands in every run.
        let mut w = BeWriter::new();
        write_fill_scanline(&mut w, 256, MERGED_FILL);
        assert_eq!(w.as_slice(), &[0x81, 0xFF, 0x81, 0xFF]);
    }

    #[test]
    fn placeholder_fill_is_white_at_16_bit_and_black_at_32_bit() {
        // Photoshop's own no-merge saves: 8/16-bit fill 0xFF, 32-bit fills
        // 0x00 (float 0.0 — 0xFF bytes would be NaN).
        for (depth, expected) in [
            (BitDepth::Eight, 0xFFu8),
            (BitDepth::Sixteen, 0xFF),
            (BitDepth::ThirtyTwo, 0x00),
        ] {
            let header = header(Version::Psd, depth, 4);
            let mut w = BeWriter::new();
            ImageData::new(1).write(&mut w, &header).unwrap();
            let bytes = w.as_slice();
            // Marker, then a u16 scanline-size table (one entry per row of the
            // single channel), then the scanlines as (run header, fill) pairs.
            assert_eq!(bytes[0..2], [0, 1]);
            let body = &bytes[2 + 2 * usize::try_from(header.height).unwrap()..];
            assert!(
                body.chunks_exact(2).all(|pair| pair[1] == expected),
                "depth {depth:?}: unexpected bytes {body:02x?}"
            );
        }
    }

    #[test]
    fn streamed_placeholder_matches_buffered_across_versions_depths_and_row_tails() {
        for version in [Version::Psd, Version::Psb] {
            for depth in [BitDepth::Eight, BitDepth::Sixteen, BitDepth::ThirtyTwo] {
                for width in [1, 2, 127, 128, 129, 300] {
                    let header = header(version, depth, width);
                    let image = ImageData::new(4);
                    let mut buffered = BeWriter::new();
                    image.write(&mut buffered, &header).unwrap();
                    let mut streamed = Vec::new();
                    image.write_to(&mut streamed, &header).unwrap();
                    assert_eq!(
                        streamed,
                        buffered.as_slice(),
                        "{version:?} {depth:?} {width}"
                    );
                }
            }
        }
    }

    #[test]
    fn writes_marker_tables_and_streams() {
        // 4-pixel-wide 8-bit PSD: 2 scanlines, 1 channel.
        let header = header(Version::Psd, BitDepth::Eight, 4);
        let mut w = BeWriter::new();
        ImageData::new(1).write(&mut w, &header).unwrap();
        let bytes = w.as_slice();
        assert_eq!(&bytes[..2], &1u16.to_be_bytes()); // RLE
        assert_eq!(&bytes[2..4], &2u16.to_be_bytes()); // scanline size
        assert_eq!(&bytes[4..6], &2u16.to_be_bytes());
        // White fill, one run per scanline (see MERGED_FILL).
        assert_eq!(&bytes[6..], &[0xFD, MERGED_FILL, 0xFD, MERGED_FILL]);
        assert_eq!(bytes.len(), 2 + 2 * 2 + 2 * 2);
    }

    #[test]
    fn psb_uses_four_byte_scanline_sizes() {
        let header = header(Version::Psb, BitDepth::ThirtyTwo, 1);
        let mut w = BeWriter::new();
        ImageData::new(2).write(&mut w, &header).unwrap();
        let bytes = w.as_slice();
        // 1 px * 4 bytes = 4-byte scanline -> 2 compressed bytes.
        assert_eq!(&bytes[2..6], &2u32.to_be_bytes());
        // 2 channels * 2 scanlines of size table + 2 channels * 2 scanlines.
        assert_eq!(bytes.len(), 2 + 2 * 2 * 4 + 2 * 2 * 2);
    }

    #[test]
    fn zero_channels_write_only_the_marker() {
        let mut w = BeWriter::new();
        ImageData::default()
            .write(&mut w, &header(Version::Psd, BitDepth::Eight, 4))
            .unwrap();
        assert_eq!(w.as_slice(), &1u16.to_be_bytes());
    }
}
