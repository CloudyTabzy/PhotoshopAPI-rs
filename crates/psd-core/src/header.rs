//! The 26-byte file header every PSD/PSB starts with.
//!
//! Mirrors `PhotoshopFile/FileHeader.h/.cpp`. One deliberate reshape: upstream
//! re-derives the version from the output file's *extension* on write; here
//! the header writes the version it holds, and choosing `.psd`/`.psb` from the
//! target path is the `psd` crate's job at save time.

use crate::enums::{BitDepth, ColorMode, Version};
use crate::error::{PsdError, Result};
use crate::io::{BeReader, BeWriter};

/// On-disk file signature, ASCII `8BPS`.
const SIGNATURE: &str = "8BPS";

/// Minimum/maximum channel count supported by Photoshop
/// (does not account for mask channels).
const MIN_CHANNELS: u16 = 1;
const MAX_CHANNELS: u16 = 56;

/// Maximum width/height for PSD files.
const MAX_DIMENSION_PSD: u32 = 30_000;
/// Maximum width/height for PSB files.
const MAX_DIMENSION_PSB: u32 = 300_000;

/// Section 1 of the file: signature, version, geometry, depth, color mode.
///
/// Layout (26 bytes, all big-endian):
/// `8BPS` | u16 version | 6 reserved zero bytes | u16 channels |
/// u32 height | u32 width | u16 depth | u16 color mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileHeader {
    /// Container version; drives every variable-width length downstream.
    pub version: Version,
    /// Number of channels in the merged image data (1–56).
    pub num_channels: u16,
    /// Document height in pixels (1–30,000 PSD / 1–300,000 PSB).
    pub height: u32,
    /// Document width in pixels (same bounds as height).
    pub width: u32,
    /// Channel bit depth.
    pub depth: BitDepth,
    /// Document color mode.
    pub color_mode: ColorMode,
}

impl FileHeader {
    /// Total on-disk size of the header section in bytes.
    pub const SIZE: usize = 26;

    /// Create a validated header.
    pub fn new(
        version: Version,
        num_channels: u16,
        width: u32,
        height: u32,
        depth: BitDepth,
        color_mode: ColorMode,
    ) -> Result<Self> {
        let header = Self {
            version,
            num_channels,
            height,
            width,
            depth,
            color_mode,
        };
        header.validate(0)?;
        Ok(header)
    }

    /// Maximum width/height for this header's version.
    pub const fn max_dimension(&self) -> u32 {
        match self.version {
            Version::Psd => MAX_DIMENSION_PSD,
            Version::Psb => MAX_DIMENSION_PSB,
        }
    }

    /// Enforce Photoshop's documented range limits.
    fn validate(&self, offset: u64) -> Result<()> {
        if !(MIN_CHANNELS..=MAX_CHANNELS).contains(&self.num_channels) {
            return Err(PsdError::InvalidData {
                offset,
                message: "channel count is not between 1 and 56",
            });
        }
        let max = self.max_dimension();
        if self.height < 1 || self.height > max {
            return Err(PsdError::InvalidData {
                offset,
                message: "height is outside the version-dependent limit",
            });
        }
        if self.width < 1 || self.width > max {
            return Err(PsdError::InvalidData {
                offset,
                message: "width is outside the version-dependent limit",
            });
        }
        Ok(())
    }

    /// Read and validate the 26-byte header from the start of a file.
    pub fn read(reader: &mut BeReader) -> Result<Self> {
        let offset = reader.position() as u64;
        reader.signature(SIGNATURE)?;
        let version = Version::from_raw(reader.u16()?)?;
        // Reserved filler bytes (skipped, must be zero on write).
        reader.skip(6)?;
        let num_channels = reader.u16()?;
        let height = reader.u32()?;
        let width = reader.u32()?;
        let depth = BitDepth::from_raw(reader.u16()?)?;
        let color_mode = ColorMode::from_raw(reader.u16()?)?;

        let header = Self {
            version,
            num_channels,
            height,
            width,
            depth,
            color_mode,
        };
        header.validate(offset)?;
        Ok(header)
    }

    /// Write the header (infallible: the struct is validated at construction).
    pub fn write(&self, writer: &mut BeWriter) {
        writer.bytes(SIGNATURE.as_bytes());
        writer.u16(self.version.as_raw());
        writer.bytes(&[0u8; 6]);
        writer.u16(self.num_channels);
        writer.u32(self.height);
        writer.u32(self.width);
        writer.u16(self.depth.as_raw());
        writer.u16(self.color_mode.as_raw());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_bytes() -> [u8; FileHeader::SIZE] {
        let mut w = BeWriter::new();
        w.bytes(b"8BPS");
        w.u16(1); // PSD
        w.bytes(&[0u8; 6]);
        w.u16(3); // RGB channels
        w.u32(64); // height
        w.u32(64); // width
        w.u16(8); // depth
        w.u16(3); // RGB
        w.into_inner().try_into().unwrap()
    }

    #[test]
    fn reads_valid_header() {
        let bytes = header_bytes();
        let mut r = BeReader::new(&bytes);
        let header = FileHeader::read(&mut r).unwrap();
        assert_eq!(header.version, Version::Psd);
        assert_eq!(header.num_channels, 3);
        assert_eq!(header.height, 64);
        assert_eq!(header.width, 64);
        assert_eq!(header.depth, BitDepth::Eight);
        assert_eq!(header.color_mode, ColorMode::Rgb);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn write_is_byte_exact_inverse_of_read() {
        let bytes = header_bytes();
        let mut r = BeReader::new(&bytes);
        let header = FileHeader::read(&mut r).unwrap();
        let mut w = BeWriter::new();
        header.write(&mut w);
        assert_eq!(w.as_slice(), &bytes);
    }

    #[test]
    fn rejects_bad_signature() {
        let mut bytes = header_bytes();
        bytes[0] = b'X';
        let mut r = BeReader::new(&bytes);
        assert!(matches!(
            FileHeader::read(&mut r),
            Err(PsdError::InvalidSignature { offset: 0, .. })
        ));
    }

    #[test]
    fn rejects_bad_version_and_enums() {
        for (byte_offset, value) in [(4usize, 3u8), (22, 7), (24, 5)] {
            let mut bytes = header_bytes();
            // version=3, depth=7, color_mode=5 are all invalid.
            bytes[byte_offset + 1] = value;
            let mut r = BeReader::new(&bytes);
            assert!(FileHeader::read(&mut r).is_err());
        }
    }

    #[test]
    fn enforces_channel_and_dimension_limits() {
        // Channels.
        assert!(FileHeader::new(Version::Psd, 0, 64, 64, BitDepth::Eight, ColorMode::Rgb).is_err());
        assert!(
            FileHeader::new(Version::Psd, 57, 64, 64, BitDepth::Eight, ColorMode::Rgb).is_err()
        );
        // PSD caps at 30,000.
        assert!(
            FileHeader::new(Version::Psd, 3, 30_001, 64, BitDepth::Eight, ColorMode::Rgb).is_err()
        );
        // PSB allows up to 300,000.
        assert!(FileHeader::new(
            Version::Psb,
            3,
            300_000,
            64,
            BitDepth::Eight,
            ColorMode::Rgb
        )
        .is_ok());
        assert!(FileHeader::new(
            Version::Psb,
            3,
            300_001,
            64,
            BitDepth::Eight,
            ColorMode::Rgb
        )
        .is_err());
        // Zero dimensions always fail.
        assert!(FileHeader::new(Version::Psd, 3, 0, 64, BitDepth::Eight, ColorMode::Rgb).is_err());
    }

    #[test]
    fn psb_header_round_trip() {
        let header = FileHeader::new(
            Version::Psb,
            4,
            300_000,
            17,
            BitDepth::Sixteen,
            ColorMode::Cmyk,
        )
        .unwrap();
        let mut w = BeWriter::new();
        header.write(&mut w);
        assert_eq!(w.as_slice().len(), FileHeader::SIZE);
        let mut r = BeReader::new(w.as_slice());
        assert_eq!(FileHeader::read(&mut r).unwrap(), header);
    }
}
