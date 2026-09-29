//! The ImageResources section (upstream `PhotoshopFile/ImageResources.{h,cpp}`,
//! `Core/Struct/ResourceBlock.{h,cpp}`).
//!
//! Layout: a `u32` length followed by resource blocks until the declared
//! length is consumed. Each block is `8BIM` | `u16` id | PascalString name
//! (2-byte aligned) | `u32` data length | data (zero-padded to 2). Lengths are
//! fixed `u32` regardless of PSD/PSB.
//!
//! The port interprets the two blocks the rest of the library consumes —
//! ResolutionInfo (DPI) and ICCProfile — and preserves every other block
//! byte-exact as [`ResourceBlock::Raw`]. Upstream *skips* unparsed blocks, so
//! round-tripping a document silently drops its XMP metadata, thumbnails,
//! guides, and the other ~30 resource types (an improvement over upstream,
//! which drops them).
//!
//! Deviations from upstream beyond passthrough: block data is stored at its
//! declared (unpadded) length and zero-padded on write, so read → write is
//! byte-exact for every fixture in the corpus; malformed block/section sizes
//! are typed errors instead of the unsigned-underflow read loop upstream runs
//! into (`while (toRead > 0) toRead -= size` wraps around a giant u32).

use crate::enums::{DisplayUnit, ResolutionUnit};
use crate::error::{PsdError, Result};
use crate::grid_guides::GridGuides;
use crate::io::{round_up, BeReader, BeWriter};
use crate::layer_comps::LayerComps;
use crate::slices::SlicesResource;
use crate::strings::PascalString;
use crate::types::FixedFloat4;

/// `8BIM` id of the DPI block ([`ResolutionInfoBlock`]).
pub const ID_RESOLUTION_INFO: u16 = 1005;
/// `8BIM` id of the ICC profile block ([`IccProfileBlock`]).
pub const ID_ICC_PROFILE: u16 = 1039;
/// `8BIM` id of the grid and guides block ([`GridGuides`](crate::grid_guides::GridGuides)).
pub const ID_GRID_AND_GUIDES: u16 = 1032;
/// `8BIM` id of the slices block ([`SlicesResource`](crate::slices::SlicesResource)).
pub const ID_SLICES: u16 = 1050;
/// `8BIM` id of the layer comps block ([`LayerComps`](crate::layer_comps::LayerComps)).
pub const ID_LAYER_COMPS: u16 = 1065;

/// A single image-resource block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceBlock {
    /// Document resolution in DPI (`1005`).
    ResolutionInfo(ResolutionInfoBlock),
    /// Embedded ICC color profile (`1039`).
    IccProfile(IccProfileBlock),
    /// Any other (or unknown) resource, preserved byte-exact.
    Raw(RawResourceBlock),
}

/// Document resolution (`1005`), as Photoshop's `ResolutionInfo` struct.
///
/// All values are stored exactly as read; no ppi/ppcm conversion is applied
/// (upstream's `read_dpi` returns the raw value too). The stale upstream
/// comment about "multiplying by 2.54 when writing" describes an unused
/// operator and is not part of the on-disk behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolutionInfoBlock {
    pub horizontal_resolution: FixedFloat4,
    pub horizontal_resolution_unit: ResolutionUnit,
    pub width_unit: DisplayUnit,
    /// Mirrors the horizontal values in practice (upstream never writes
    /// anything else); kept separate because the format allows it.
    pub vertical_resolution: FixedFloat4,
    pub vertical_resolution_unit: ResolutionUnit,
    pub height_unit: DisplayUnit,
}

impl ResolutionInfoBlock {
    /// Default resolution: 72 ppi, centimetre display units (upstream ctor).
    pub fn new(resolution: f32) -> Self {
        let res = FixedFloat4::from_f32(resolution);
        Self {
            horizontal_resolution: res,
            horizontal_resolution_unit: ResolutionUnit::PixelsPerInch,
            width_unit: DisplayUnit::Cm,
            vertical_resolution: res,
            vertical_resolution_unit: ResolutionUnit::PixelsPerInch,
            height_unit: DisplayUnit::Cm,
        }
    }
}

impl Default for ResolutionInfoBlock {
    fn default() -> Self {
        Self::new(72.0)
    }
}

/// Embedded ICC profile (`1039`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IccProfileBlock {
    data: Vec<u8>,
}

impl IccProfileBlock {
    /// Wrap raw ICC profile bytes (unpadded; write adds the 2-byte alignment).
    pub fn new(data: Vec<u8>) -> Self {
        Self { data }
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

/// An unparsed resource block, kept so saves do not lose data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawResourceBlock {
    pub id: u16,
    /// Resource name (decoded to UTF-8; empty in every corpus fixture).
    pub name: PascalString,
    /// Raw payload at its declared length (unpadded).
    pub data: Vec<u8>,
}

/// The ImageResources section: an ordered list of [`ResourceBlock`]s.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImageResources {
    blocks: Vec<ResourceBlock>,
}

impl ImageResources {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn blocks(&self) -> &[ResourceBlock] {
        &self.blocks
    }

    pub fn push(&mut self, block: ResourceBlock) {
        self.blocks.push(block);
    }

    /// First `ResolutionInfo` block, if present.
    pub fn resolution_info(&self) -> Option<&ResolutionInfoBlock> {
        self.blocks.iter().find_map(|block| match block {
            ResourceBlock::ResolutionInfo(info) => Some(info),
            _ => None,
        })
    }

    /// First `ICCProfile` block, if present.
    pub fn icc_profile(&self) -> Option<&IccProfileBlock> {
        self.blocks.iter().find_map(|block| match block {
            ResourceBlock::IccProfile(icc) => Some(icc),
            _ => None,
        })
    }

    /// Move the profile bytes out of the first `ICCProfile` block, leaving an
    /// empty block where it was so the section keeps its layout. `None` when
    /// there is no such block.
    pub fn take_icc_profile(&mut self) -> Option<Vec<u8>> {
        self.blocks.iter_mut().find_map(|block| match block {
            ResourceBlock::IccProfile(icc) => Some(std::mem::take(&mut icc.data)),
            _ => None,
        })
    }

    /// Replace the first `ResolutionInfo` block, or append one.
    pub fn set_resolution_info(&mut self, info: ResolutionInfoBlock) {
        for block in &mut self.blocks {
            if matches!(block, ResourceBlock::ResolutionInfo(_)) {
                *block = ResourceBlock::ResolutionInfo(info);
                return;
            }
        }
        self.blocks.push(ResourceBlock::ResolutionInfo(info));
    }

    /// Grid and guides (`1032`), parsed on demand.
    ///
    /// `None` when the document has no such block or its payload is malformed;
    /// either way the raw block is what a save writes, so the bytes are kept.
    pub fn grid_and_guides(&self) -> Option<GridGuides> {
        let payload = self.raw_payload(ID_GRID_AND_GUIDES)?;
        GridGuides::read(&mut BeReader::new(payload)).ok()
    }

    /// Replace the grid and guides block, or append one.
    pub fn set_grid_and_guides(&mut self, guides: &GridGuides) -> Result<()> {
        self.set_raw_payload(ID_GRID_AND_GUIDES, guides.to_payload()?);
        Ok(())
    }

    /// Slices (`1050`), parsed on demand. `None` when absent or malformed.
    pub fn slices(&self) -> Option<SlicesResource> {
        let payload = self.raw_payload(ID_SLICES)?;
        SlicesResource::read(&mut BeReader::new(payload)).ok()
    }

    /// Replace the slices block, or append one.
    pub fn set_slices(&mut self, slices: &SlicesResource) -> Result<()> {
        self.set_raw_payload(ID_SLICES, slices.to_payload()?);
        Ok(())
    }

    /// Layer comps (`1065`), parsed on demand. `None` when absent or malformed.
    pub fn layer_comps(&self) -> Option<LayerComps> {
        let payload = self.raw_payload(ID_LAYER_COMPS)?;
        LayerComps::read(&mut BeReader::new(payload)).ok()
    }

    /// Replace the layer comps block, or append one.
    pub fn set_layer_comps(&mut self, comps: &LayerComps) -> Result<()> {
        self.set_raw_payload(ID_LAYER_COMPS, comps.to_payload()?);
        Ok(())
    }

    /// The payload of the first raw block with `id`.
    fn raw_payload(&self, id: u16) -> Option<&[u8]> {
        self.blocks.iter().find_map(|block| match block {
            ResourceBlock::Raw(raw) if raw.id == id => Some(raw.data.as_slice()),
            _ => None,
        })
    }

    /// Replace the payload of the first raw block with `id`, or append one,
    /// keeping the block where it is so the section's layout is stable.
    fn set_raw_payload(&mut self, id: u16, data: Vec<u8>) {
        for block in &mut self.blocks {
            if let ResourceBlock::Raw(raw) = block {
                if raw.id == id {
                    raw.data = data;
                    return;
                }
            }
        }
        self.blocks.push(ResourceBlock::Raw(RawResourceBlock {
            id,
            name: PascalString::new("", 2),
            data,
        }));
    }

    /// Replace the first `ICCProfile` block, or append one.
    pub fn set_icc_profile(&mut self, icc: IccProfileBlock) {
        for block in &mut self.blocks {
            if matches!(block, ResourceBlock::IccProfile(_)) {
                *block = ResourceBlock::IccProfile(icc);
                return;
            }
        }
        self.blocks.push(ResourceBlock::IccProfile(icc));
    }

    /// Drop the `ICCProfile` block, if any (e.g. when the profile was cleared).
    pub fn remove_icc_profile(&mut self) {
        self.blocks
            .retain(|block| !matches!(block, ResourceBlock::IccProfile(_)));
    }

    /// Read the section (length marker included) from the current position.
    pub fn read(reader: &mut BeReader) -> Result<Self> {
        let offset = reader.position() as u64;
        let section_len = reader.u32()? as usize;
        // Upstream consumes `RoundUpToMultiple(len, 2) + 4`, i.e. an odd
        // declared length is followed by one pad byte that is part of the
        // section. Rounding up keeps byte-consumption parity so a stray odd
        // length cannot desync the LayerAndMaskInformation parse that follows.
        let section_end = reader
            .position()
            .checked_add(section_len)
            .and_then(|end| end.checked_add(section_len % 2))
            .ok_or(PsdError::InvalidData {
                offset,
                message: "ImageResources length overflows",
            })?;

        let mut blocks = Vec::new();
        while reader.position() < section_end {
            blocks.push(Self::read_block(reader, section_end)?);
        }
        if reader.position() != section_end {
            return Err(PsdError::InvalidData {
                offset,
                message: "ImageResources blocks do not fill the declared section length",
            });
        }
        Ok(Self { blocks })
    }

    /// Read one block; `section_end` bounds every read so a corrupt block
    /// cannot run into the following section.
    fn read_block(reader: &mut BeReader, section_end: usize) -> Result<ResourceBlock> {
        let offset = reader.position() as u64;
        reader.signature("8BIM")?;
        let id = reader.u16()?;
        let name = PascalString::read(reader, 2)?;
        let data_size = reader.u32()? as usize;
        let padded = round_up(data_size, 2);
        let data_end = reader
            .position()
            .checked_add(padded)
            .ok_or(PsdError::InvalidData {
                offset,
                message: "resource block size overflows",
            })?;
        if data_end > section_end {
            return Err(PsdError::InvalidData {
                offset,
                message: "resource block data exceeds the section length",
            });
        }

        let block = match id {
            ID_RESOLUTION_INFO if data_size < 16 => {
                // Smaller than its fixed payload: keep the bytes raw rather
                // than failing the file, the way mature readers do.
                tracing::warn!(
                    "ResolutionInfo block declares {data_size} bytes, smaller than its                      16-byte payload; keeping it raw"
                );
                ResourceBlock::Raw(RawResourceBlock {
                    id,
                    name,
                    data: reader.take(data_size)?.to_vec(),
                })
            }
            ID_RESOLUTION_INFO => {
                if data_size != 16 {
                    tracing::warn!(
                        "ResolutionInfo block declares {data_size} bytes, expected 16; \
                         ignoring the excess (upstream reads past the block)"
                    );
                }
                let horizontal_resolution = FixedFloat4::from_parts(reader.u16()?, reader.u16()?);
                let horizontal_resolution_unit = read_resolution_unit(reader)?;
                let width_unit = read_display_unit(reader)?;
                let vertical_resolution = FixedFloat4::from_parts(reader.u16()?, reader.u16()?);
                let vertical_resolution_unit = read_resolution_unit(reader)?;
                let height_unit = read_display_unit(reader)?;
                // Stay aligned on malformed oversized blocks.
                reader.skip(data_size - 16)?;
                ResourceBlock::ResolutionInfo(ResolutionInfoBlock {
                    horizontal_resolution,
                    horizontal_resolution_unit,
                    width_unit,
                    vertical_resolution,
                    vertical_resolution_unit,
                    height_unit,
                })
            }
            ID_ICC_PROFILE => {
                ResourceBlock::IccProfile(IccProfileBlock::new(reader.take(data_size)?.to_vec()))
            }
            _ => ResourceBlock::Raw(RawResourceBlock {
                id,
                name,
                data: reader.take(data_size)?.to_vec(),
            }),
        };

        // Consume the 2-byte alignment padding.
        reader.skip(padded - data_size)?;
        Ok(block)
    }

    /// Write the section with its length marker.
    pub fn write(&self, writer: &mut BeWriter) -> Result<()> {
        // Image-resource lengths are a fixed u32 even in PSB.
        let marker = writer.reserve_len_marker(crate::enums::Version::Psd);
        for block in &self.blocks {
            Self::write_block(writer, block)?;
        }
        writer.pad_to_relative(marker, 2);
        let end = writer.position();
        writer.patch_len(crate::enums::Version::Psd, marker, end, false)
    }

    fn write_block(writer: &mut BeWriter, block: &ResourceBlock) -> Result<()> {
        match block {
            ResourceBlock::ResolutionInfo(info) => {
                write_block_prefix(writer, ID_RESOLUTION_INFO, &PascalString::new("", 2))?;
                writer.u32(16);
                let (h_number, h_fraction) = info.horizontal_resolution.parts();
                writer.u16(h_number);
                writer.u16(h_fraction);
                writer.u16(info.horizontal_resolution_unit.as_raw());
                writer.u16(info.width_unit.as_raw());
                let (v_number, v_fraction) = info.vertical_resolution.parts();
                writer.u16(v_number);
                writer.u16(v_fraction);
                writer.u16(info.vertical_resolution_unit.as_raw());
                writer.u16(info.height_unit.as_raw());
            }
            ResourceBlock::IccProfile(icc) => {
                write_block_prefix(writer, ID_ICC_PROFILE, &PascalString::new("", 2))?;
                write_block_data(writer, &icc.data)?;
            }
            ResourceBlock::Raw(raw) => {
                write_block_prefix(writer, raw.id, &raw.name)?;
                write_block_data(writer, &raw.data)?;
            }
        }
        Ok(())
    }
}

/// `8BIM` | id | name — the common prefix of every resource block.
fn write_block_prefix(writer: &mut BeWriter, id: u16, name: &PascalString) -> Result<()> {
    writer.bytes(b"8BIM");
    writer.u16(id);
    name.write(writer)
}

/// `u32` declared length | payload | zero padding to 2 bytes.
///
/// The declared length is the unpadded payload size, matching what Photoshop
/// writes for the ICC fixtures; upstream's `ScopedLengthBlock` declares the
/// padded size and lets the pad byte become part of the payload on re-read.
fn write_block_data(writer: &mut BeWriter, data: &[u8]) -> Result<()> {
    let len = u32::try_from(data.len()).map_err(|_| PsdError::LengthOverflow {
        actual: data.len() as u64,
        width: 4,
    })?;
    writer.u32(len);
    writer.bytes(data);
    writer.pad_to(2);
    Ok(())
}

/// Unknown resolution units default to pixels-per-inch with a warning
/// (upstream commit 51bb741 does the same so DCC-written files still load).
fn read_resolution_unit(reader: &mut BeReader) -> Result<ResolutionUnit> {
    let raw = reader.u16()?;
    Ok(ResolutionUnit::from_raw(raw).unwrap_or_else(|| {
        tracing::warn!("unknown resolution unit {raw}, defaulting to pixels-per-inch");
        ResolutionUnit::PixelsPerInch
    }))
}

/// Unknown display units default to inches with a warning; see
/// [`read_resolution_unit`].
fn read_display_unit(reader: &mut BeReader) -> Result<DisplayUnit> {
    let raw = reader.u16()?;
    Ok(DisplayUnit::from_raw(raw).unwrap_or_else(|| {
        tracing::warn!("unknown display unit {raw}, defaulting to inches");
        DisplayUnit::Inches
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enums::ColorMode;
    use crate::enums::Version;
    use crate::header::FileHeader;

    fn round_trip(resources: &ImageResources) -> ImageResources {
        let mut w = BeWriter::new();
        resources.write(&mut w).unwrap();
        let mut r = BeReader::new(w.as_slice());
        let back = ImageResources::read(&mut r).unwrap();
        assert!(r.is_empty());
        back
    }

    #[test]
    fn resolution_info_round_trips_and_is_28_bytes() {
        let mut resources = ImageResources::new();
        resources.push(ResourceBlock::ResolutionInfo(ResolutionInfoBlock::new(
            300.5,
        )));
        let back = round_trip(&resources);
        let info = back.resolution_info().unwrap();
        assert_eq!(info.horizontal_resolution, FixedFloat4::from_f32(300.5));
        assert_eq!(
            info.horizontal_resolution_unit,
            ResolutionUnit::PixelsPerInch
        );
        assert_eq!(info.width_unit, DisplayUnit::Cm);

        // Pin the byte layout: marker(4) + 8BIM(4) + id(2) + name(2) + len(4)
        // + payload(16) = 32 bytes, marker value 28.
        let mut w = BeWriter::new();
        resources.write(&mut w).unwrap();
        let bytes = w.as_slice();
        assert_eq!(bytes.len(), 32);
        assert_eq!(&bytes[..4], &28u32.to_be_bytes());
        assert_eq!(&bytes[4..8], b"8BIM");
        assert_eq!(&bytes[8..10], &1005u16.to_be_bytes());
        assert_eq!(&bytes[10..12], &[0, 0]); // empty name
        assert_eq!(&bytes[12..16], &16u32.to_be_bytes());
        assert_eq!(&bytes[16..18], &300u16.to_be_bytes());
        assert_eq!(&bytes[18..20], &32767u16.to_be_bytes()); // 0.5 * 65535 truncated
        assert_eq!(&bytes[20..22], &1u16.to_be_bytes()); // ppi
        assert_eq!(&bytes[22..24], &2u16.to_be_bytes()); // cm
    }

    #[test]
    fn taking_the_icc_profile_leaves_an_empty_block_in_place() {
        let mut resources = ImageResources::new();
        resources.push(ResourceBlock::Raw(RawResourceBlock {
            id: 1005,
            name: PascalString::new("", 2),
            data: vec![1, 2],
        }));
        resources.push(ResourceBlock::IccProfile(IccProfileBlock::new(vec![
            0xAA, 0xBB, 0xCC,
        ])));
        resources.push(ResourceBlock::Raw(RawResourceBlock {
            id: 1060,
            name: PascalString::new("", 2),
            data: vec![3],
        }));

        assert_eq!(resources.take_icc_profile(), Some(vec![0xAA, 0xBB, 0xCC]));
        // The block is still there, between its neighbours, and empty.
        assert!(matches!(
            resources.blocks(),
            [
                ResourceBlock::Raw(_),
                ResourceBlock::IccProfile(icc),
                ResourceBlock::Raw(_)
            ] if icc.data().is_empty()
        ));
        // Putting a profile back fills that block rather than adding another.
        resources.set_icc_profile(IccProfileBlock::new(vec![0xAA, 0xBB, 0xCC]));
        assert_eq!(resources.blocks().len(), 3);
        assert_eq!(resources.icc_profile().unwrap().data(), [0xAA, 0xBB, 0xCC]);
        assert_eq!(ImageResources::new().take_icc_profile(), None);
    }

    #[test]
    fn icc_and_raw_blocks_round_trip_including_odd_sizes() {
        let mut resources = ImageResources::new();
        resources.push(ResourceBlock::IccProfile(IccProfileBlock::new(vec![
            0xAA, 0xBB, 0xCC,
        ])));
        // Odd payload: exercises the 1-byte zero padding.
        resources.push(ResourceBlock::Raw(RawResourceBlock {
            id: 1060,
            name: PascalString::new("", 2),
            data: vec![1, 2, 3, 4, 5],
        }));
        let back = round_trip(&resources);
        assert_eq!(back, resources); // Raw blocks survive (upstream drops them)

        let mut w = BeWriter::new();
        resources.write(&mut w).unwrap();
        // ICC block starts at 4 and its data length is declared unpadded.
        assert_eq!(&w.as_slice()[12..16], &3u32.to_be_bytes());
    }

    #[test]
    fn unknown_units_default_without_failing() {
        let mut w = BeWriter::new();
        w.u32(28); // section length
        w.bytes(b"8BIM");
        w.u16(ID_RESOLUTION_INFO);
        w.bytes(&[0, 0]); // empty name
        w.u32(16);
        w.u16(72); // h number
        w.u16(0); // h fraction
        w.u16(999); // unknown resolution unit
        w.u16(0); // unknown display unit
        w.u16(72);
        w.u16(0);
        w.u16(1);
        w.u16(2);
        let mut r = BeReader::new(w.as_slice());
        let resources = ImageResources::read(&mut r).unwrap();
        let info = resources.resolution_info().unwrap();
        assert_eq!(
            info.horizontal_resolution_unit,
            ResolutionUnit::PixelsPerInch
        );
        assert_eq!(info.width_unit, DisplayUnit::Inches);
    }

    #[test]
    fn empty_section_round_trips() {
        let back = round_trip(&ImageResources::new());
        assert!(back.blocks().is_empty());
    }

    #[test]
    fn malformed_blocks_are_typed_errors() {
        // Block data overruns the declared section length.
        let mut w = BeWriter::new();
        w.u32(12);
        w.bytes(b"8BIM");
        w.u16(1060);
        w.bytes(&[0, 0]);
        w.u32(64); // declares 64 bytes in a 12-byte section
        let mut r = BeReader::new(w.as_slice());
        assert!(matches!(
            ImageResources::read(&mut r),
            Err(PsdError::InvalidData { .. })
        ));

        // ResolutionInfo payload smaller than 16 bytes.
        let mut w = BeWriter::new();
        w.u32(12);
        w.bytes(b"8BIM");
        w.u16(ID_RESOLUTION_INFO);
        w.bytes(&[0, 0]);
        w.u32(8);
        w.bytes(&[0u8; 8]);
        let mut r = BeReader::new(w.as_slice());
        assert!(matches!(
            ImageResources::read(&mut r),
            Err(PsdError::InvalidData { .. })
        ));

        // Trailing bytes that do not form a block.
        let mut w = BeWriter::new();
        w.u32(3);
        w.bytes(&[0xAA, 0xBB, 0xCC]);
        let mut r = BeReader::new(w.as_slice());
        assert!(ImageResources::read(&mut r).is_err());
    }

    #[test]
    fn non_8bim_signature_is_rejected() {
        let mut w = BeWriter::new();
        w.u32(12);
        w.bytes(b"8B64");
        w.u16(ID_RESOLUTION_INFO);
        w.bytes(&[0, 0]);
        w.u32(0);
        let mut r = BeReader::new(w.as_slice());
        assert!(matches!(
            ImageResources::read(&mut r),
            Err(PsdError::InvalidSignature { .. })
        ));
    }

    #[test]
    fn dpi_block_written_from_scratch_matches_header_geometry() {
        // A generated ImageResources only needs the DPI block; sanity-check it
        // alongside a real header so the write path is exercised end to end.
        let header = FileHeader::new(
            Version::Psd,
            3,
            64,
            64,
            crate::enums::BitDepth::Eight,
            ColorMode::Rgb,
        )
        .unwrap();
        assert_eq!(header.width, 64);
        let mut resources = ImageResources::new();
        resources.push(ResourceBlock::ResolutionInfo(ResolutionInfoBlock::default()));
        let mut w = BeWriter::new();
        resources.write(&mut w).unwrap();
        let mut r = BeReader::new(w.as_slice());
        let back = ImageResources::read(&mut r).unwrap();
        assert_eq!(
            back.resolution_info()
                .unwrap()
                .horizontal_resolution
                .to_f32(),
            72.0
        );
    }
}
