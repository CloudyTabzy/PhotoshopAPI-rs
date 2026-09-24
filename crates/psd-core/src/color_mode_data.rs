//! The ColorModeData section (upstream `PhotoshopFile/ColorModeData.{h,cpp}`).
//!
//! Layout: a `u32` length followed by `length` raw bytes. The port stores the
//! payload uninterpreted: only Indexed (256-entry RGB palette) and Duotone
//! (curve data) documents carry it per the spec; 32-bit documents carry
//! Photoshop's undocumented HDR-toning blob and everything else writes a zero
//! length.
//!
//! Two deliberate fixes over upstream:
//! - Upstream's `read` consumes `length + 4` bytes — the marker width leaks
//!   into the payload — and papers over the misalignment by having the caller
//!   compute the next section's offset from the inflated size. Indexed files
//!   therefore grow by 4 bytes on every save and their second re-read desyncs.
//!   This port reads exactly `length` bytes.
//! - Upstream's `write` replaces the data of 32-bit documents with its
//!   hardcoded default on *every* save and drops the data of any other
//!   non-Indexed mode. This port preserves whatever was read and only falls
//!   back to the 32-bit default when there is no data (create-from-scratch),
//!   which keeps read → write → read byte-exact.

use crate::enums::BitDepth;
use crate::error::{PsdError, Result};
use crate::header::FileHeader;
use crate::io::{BeReader, BeWriter};

/// The 112-byte HDR-toning blob Photoshop writes into 32-bit documents.
///
/// Taken verbatim from upstream `ColorModeData.cpp`, which captured it from a
/// Photoshop 23.3.2 save. Verified byte-equal to the 32-bit fixtures in
/// `fixtures/documents/Compression`.
const DEFAULT_32BIT_DATA: [u8; 112] = [
    0x68, 0x64, 0x72, 0x74, 0x00, 0x00, 0x00, 0x03, 0x3E, 0x6B, 0x85, 0x1F, 0x00, 0x00, 0x00, 0x02,
    0x00, 0x00, 0x00, 0x08, 0x00, 0x44, 0x00, 0x65, 0x00, 0x66, 0x00, 0x61, 0x00, 0x75, 0x00, 0x6C,
    0x00, 0x74, 0x00, 0x00, 0x00, 0x02, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0x00, 0xFF,
    0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x41, 0x80, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x3F, 0x80, 0x00, 0x00, 0x68, 0x64, 0x72, 0x61, 0x00, 0x00,
    0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x41, 0xA0, 0x00, 0x00, 0x41, 0xF0, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3F, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// The ColorModeData section: `u32` length + raw payload.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ColorModeData {
    data: Vec<u8>,
}

impl ColorModeData {
    /// Photoshop's 32-bit HDR-toning default, written for 32-bit documents
    /// that carry no data of their own.
    pub const DEFAULT_32BIT: &'static [u8; 112] = &DEFAULT_32BIT_DATA;

    pub fn new(data: Vec<u8>) -> Self {
        Self { data }
    }

    /// The raw payload; empty for most modes.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Read the section: `u32` length + exactly that many payload bytes.
    pub fn read(reader: &mut BeReader) -> Result<Self> {
        let offset = reader.position() as u64;
        let len = reader.u32()? as usize;
        if len > reader.remaining() {
            return Err(PsdError::InvalidData {
                offset,
                message: "ColorModeData length exceeds the remaining file data",
            });
        }
        Ok(Self {
            data: reader.take(len)?.to_vec(),
        })
    }

    /// Write the section. The length marker is a `u32` regardless of PSD/PSB.
    ///
    /// Payload selection: stored data if any, else the 32-bit default for
    /// 32-bit documents, else zero-length (see the module docs for how this
    /// differs from upstream).
    pub fn write(&self, writer: &mut BeWriter, header: &FileHeader) -> Result<()> {
        let data: &[u8] = if !self.data.is_empty() {
            &self.data
        } else if header.depth == BitDepth::ThirtyTwo {
            &DEFAULT_32BIT_DATA
        } else {
            &[]
        };
        let len = u32::try_from(data.len()).map_err(|_| PsdError::LengthOverflow {
            actual: data.len() as u64,
            width: 4,
        })?;
        writer.u32(len);
        writer.bytes(data);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enums::{ColorMode, Version};

    fn header(depth: BitDepth, color_mode: ColorMode) -> FileHeader {
        FileHeader::new(Version::Psd, 3, 64, 64, depth, color_mode).unwrap()
    }

    #[test]
    fn empty_data_writes_zero_length_for_non_32bit() {
        for mode in [ColorMode::Rgb, ColorMode::Cmyk, ColorMode::Grayscale] {
            let mut w = BeWriter::new();
            ColorModeData::default()
                .write(&mut w, &header(BitDepth::Eight, mode))
                .unwrap();
            assert_eq!(w.as_slice(), &[0, 0, 0, 0]);
        }
    }

    #[test]
    fn indexed_payload_round_trips_byte_exact() {
        let palette: Vec<u8> = (0..768u32).map(|i| (i % 256) as u8).collect();
        let section = ColorModeData::new(palette.clone());
        let mut w = BeWriter::new();
        section
            .write(&mut w, &header(BitDepth::Eight, ColorMode::Indexed))
            .unwrap();
        assert_eq!(&w.as_slice()[..4], &768u32.to_be_bytes());

        let mut r = BeReader::new(w.as_slice());
        let back = ColorModeData::read(&mut r).unwrap();
        assert_eq!(back.data(), palette);
        assert!(r.is_empty());
    }

    #[test]
    fn empty_32bit_data_falls_back_to_default_blob() {
        let mut w = BeWriter::new();
        ColorModeData::default()
            .write(&mut w, &header(BitDepth::ThirtyTwo, ColorMode::Rgb))
            .unwrap();
        assert_eq!(w.as_slice().len(), 4 + 112);
        assert_eq!(&w.as_slice()[4..], ColorModeData::DEFAULT_32BIT);

        let mut r = BeReader::new(w.as_slice());
        assert_eq!(
            ColorModeData::read(&mut r).unwrap().data(),
            ColorModeData::DEFAULT_32BIT
        );
    }

    #[test]
    fn stored_32bit_data_is_preserved_not_replaced() {
        // Upstream would overwrite this with the hardcoded default.
        let custom = vec![0x68, 0x64, 0x72, 0x74, 0xAA];
        let mut w = BeWriter::new();
        ColorModeData::new(custom.clone())
            .write(&mut w, &header(BitDepth::ThirtyTwo, ColorMode::Rgb))
            .unwrap();
        let mut r = BeReader::new(w.as_slice());
        assert_eq!(ColorModeData::read(&mut r).unwrap().data(), custom);
    }

    #[test]
    fn read_rejects_length_beyond_input() {
        let bytes = [0u8, 0, 0x10, 0x00, 0x01]; // declares 0x1000 bytes, has 1
        let mut r = BeReader::new(&bytes);
        assert!(ColorModeData::read(&mut r).is_err());
    }

    #[test]
    fn read_consumes_only_the_declared_length() {
        // The follow-on bytes must stay untouched (upstream reads 4 too many).
        let mut bytes = vec![0u8, 0, 0, 2, 0xAA, 0xBB];
        bytes.extend_from_slice(b"8BIM");
        let mut r = BeReader::new(&bytes);
        let section = ColorModeData::read(&mut r).unwrap();
        assert_eq!(section.data(), &[0xAA, 0xBB]);
        assert_eq!(r.position(), 6);
    }
}
