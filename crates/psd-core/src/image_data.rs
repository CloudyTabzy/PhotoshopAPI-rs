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
//! per-scanline PackBits streams. Because every pixel is zero, each scanline
//! compresses to runs of 128 plus a remainder, and this module can synthesize
//! the exact bytes without depending on `psd-codecs` (dependency direction).

use crate::enums::Version;
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
    raw_section: Option<Vec<u8>>,
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
        self.raw_section = section;
    }

    /// The retained section, including its two-byte compression marker.
    pub fn raw_section(&self) -> Option<&[u8]> {
        self.raw_section.as_deref()
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
        // One compressed stream per channel.
        for _ in 0..self.num_channels {
            for _ in 0..header.height {
                write_fill_scanline(writer, scanline_bytes, MERGED_FILL);
            }
        }
        Ok(())
    }
}

/// The byte the merged-image fill uses; see [`ImageData::write`].
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
