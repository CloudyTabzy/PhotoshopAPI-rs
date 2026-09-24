//! Placed-layer (smart object) tagged blocks: `PlLd` and `SoLd`.
//!
//! Mirrors `Core/TaggedBlocks/PlacedLayerTaggedBlock.{h,cpp}`.
//!
//! `PlLd` is the binary placed-layer record (uuid, page info, transform, warp
//! descriptor); `SoLd`/`SoLE` is the descriptor-based successor Photoshop
//! writes alongside it. Both are parsed as *views* over the raw block data —
//! the document layer keeps the original bytes in its tagged-block list, so
//! these parsers never affect round-trips.
//!
//! Fix over upstream: upstream writes `PlLd` with a *variadic* length (8 bytes
//! in PSB) while its own reader always reads a `u32` — a PSB round-trip of a
//! placed layer breaks upstream. The port keeps `u32` on both sides, matching
//! the reader and the PSB 64-bit-key list.

use crate::descriptor::Descriptor;
use crate::error::{PsdError, Result};
use crate::io::{BeReader, BeWriter};
use crate::strings::PascalString;

/// Signature at the start of a `PlLd` payload.
pub const PLACED_LAYER_SIGNATURE: [u8; 4] = *b"plcL";
/// Signature at the start of a `SoLd` payload.
pub const PLACED_LAYER_DATA_SIGNATURE: [u8; 4] = *b"soLD";

/// What the placed layer contains (`PlacedLayer::Type` upstream).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacedLayerType {
    Unknown,
    Vector,
    Raster,
    ImageStack,
}

impl PlacedLayerType {
    pub fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Unknown),
            1 => Some(Self::Vector),
            2 => Some(Self::Raster),
            3 => Some(Self::ImageStack),
            _ => None,
        }
    }

    pub const fn as_raw(self) -> u32 {
        match self {
            Self::Unknown => 0,
            Self::Vector => 1,
            Self::Raster => 2,
            Self::ImageStack => 3,
        }
    }
}

/// A 2D point of the placed transform.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// The four corners of a placed layer, in on-disk order.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Transform {
    pub top_left: Point,
    pub top_right: Point,
    pub bottom_right: Point,
    pub bottom_left: Point,
}

impl Transform {
    fn read(reader: &mut BeReader) -> Result<Self> {
        Ok(Self {
            top_left: Point {
                x: reader.f64()?,
                y: reader.f64()?,
            },
            top_right: Point {
                x: reader.f64()?,
                y: reader.f64()?,
            },
            bottom_right: Point {
                x: reader.f64()?,
                y: reader.f64()?,
            },
            bottom_left: Point {
                x: reader.f64()?,
                y: reader.f64()?,
            },
        })
    }

    fn write(&self, writer: &mut BeWriter) {
        for point in [
            self.top_left,
            self.top_right,
            self.bottom_right,
            self.bottom_left,
        ] {
            writer.f64(point.x);
            writer.f64(point.y);
        }
    }
}

/// A parsed `PlLd` block payload.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedLayer {
    pub version: u32,
    /// UUID linking the layer to its linked-data entry (`PascalString` with
    /// 1-byte alignment).
    pub unique_id: PascalString,
    pub page_number: u32,
    pub total_pages: u32,
    pub anti_alias_policy: u32,
    pub layer_type: PlacedLayerType,
    pub transform: Transform,
    pub warp_version: u32,
    pub descriptor_version: u32,
    pub warp: Descriptor,
}

impl PlacedLayer {
    /// Parse a `PlLd` payload (the reader must span the block data).
    pub fn read(reader: &mut BeReader) -> Result<Self> {
        Self::read_with_trailing(reader).map(|(placed, _trailing)| placed)
    }

    /// Parse a `PlLd` payload and return every byte after its known fields.
    /// Smart-object edits use this to preserve padding and future suffix data.
    pub fn read_with_trailing(reader: &mut BeReader) -> Result<(Self, Vec<u8>)> {
        let offset = reader.position() as u64;
        let mut signature = [0u8; 4];
        signature.copy_from_slice(reader.take(4)?);
        if signature != PLACED_LAYER_SIGNATURE {
            return Err(PsdError::InvalidSignature {
                expected: "plcL",
                found: signature,
                offset,
            });
        }

        let version = reader.u32()?;
        let unique_id = PascalString::read(reader, 1)?;
        let page_number = reader.u32()?;
        let total_pages = reader.u32()?;
        let anti_alias_policy = reader.u32()?;
        let raw_type = reader.u32()?;
        let layer_type = PlacedLayerType::from_raw(raw_type).ok_or(PsdError::InvalidData {
            offset,
            message: "unknown placed layer type",
        })?;
        let transform = Transform::read(reader)?;
        let warp_version = reader.u32()?;
        let descriptor_version = reader.u32()?;
        let warp = Descriptor::read(reader)?;

        // Keep the raw suffix separate so callers can preserve it on edits.
        let trailing_len = reader.remaining();
        if trailing_len > 4 {
            tracing::warn!("placed layer block has {trailing_len} trailing bytes");
        }
        let trailing = reader.take(trailing_len)?.to_vec();

        Ok((
            Self {
                version,
                unique_id,
                page_number,
                total_pages,
                anti_alias_policy,
                layer_type,
                transform,
                warp_version,
                descriptor_version,
                warp,
            },
            trailing,
        ))
    }

    /// Serialize the payload (no length marker or alignment padding; the
    /// tagged-block wrapper adds those).
    pub fn write(&self, writer: &mut BeWriter) -> Result<()> {
        writer.bytes(&PLACED_LAYER_SIGNATURE);
        writer.u32(self.version);
        self.unique_id.write(writer)?;
        writer.u32(self.page_number);
        writer.u32(self.total_pages);
        writer.u32(self.anti_alias_policy);
        writer.u32(self.layer_type.as_raw());
        self.transform.write(writer);
        writer.u32(self.warp_version);
        writer.u32(self.descriptor_version);
        self.warp.write(writer)
    }
}

/// A parsed `SoLd`/`SoLE` block payload (the descriptor-based placed layer).
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedLayerData {
    pub version: u32,
    pub descriptor_version: u32,
    pub descriptor: Descriptor,
}

impl PlacedLayerData {
    pub fn read(reader: &mut BeReader) -> Result<Self> {
        Self::read_with_trailing(reader).map(|(placed, _trailing)| placed)
    }

    /// Parse a `SoLd`/`SoLE` payload and return every byte after its descriptor.
    /// Smart-object edits use this to preserve padding and future suffix data.
    pub fn read_with_trailing(reader: &mut BeReader) -> Result<(Self, Vec<u8>)> {
        let offset = reader.position() as u64;
        let mut signature = [0u8; 4];
        signature.copy_from_slice(reader.take(4)?);
        if signature != PLACED_LAYER_DATA_SIGNATURE {
            return Err(PsdError::InvalidSignature {
                expected: "soLD",
                found: signature,
                offset,
            });
        }
        let version = reader.u32()?;
        let descriptor_version = reader.u32()?;
        let descriptor = Descriptor::read(reader)?;

        let trailing_len = reader.remaining();
        if trailing_len > 4 {
            tracing::warn!("placed layer data block has {trailing_len} trailing bytes");
        }
        let trailing = reader.take(trailing_len)?.to_vec();

        Ok((
            Self {
                version,
                descriptor_version,
                descriptor,
            },
            trailing,
        ))
    }

    pub fn write(&self, writer: &mut BeWriter) -> Result<()> {
        writer.bytes(&PLACED_LAYER_DATA_SIGNATURE);
        writer.u32(self.version);
        writer.u32(self.descriptor_version);
        self.descriptor.write(writer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{DescriptorKey, DescriptorValue};
    use crate::strings::UnicodeString;

    #[test]
    fn placed_layer_round_trips() {
        let mut warp = Descriptor {
            name: UnicodeString::new("", 1).unwrap(),
            class_id: DescriptorKey::new("warp"),
            items: Vec::new(),
        };
        warp.insert(
            "warpStyle",
            DescriptorValue::Enumerated {
                type_id: DescriptorKey::new("warpStyle"),
                value: DescriptorKey::new("warpNone"),
            },
        );
        let placed = PlacedLayer {
            version: 3,
            unique_id: PascalString::new("9e1f3c1a-0000-0000-0000-000000000000", 1),
            page_number: 1,
            total_pages: 1,
            anti_alias_policy: 0,
            layer_type: PlacedLayerType::Raster,
            transform: Transform {
                top_left: Point { x: 0.0, y: 0.0 },
                top_right: Point { x: 64.0, y: 0.0 },
                bottom_right: Point { x: 64.0, y: 64.0 },
                bottom_left: Point { x: 0.0, y: 64.0 },
            },
            warp_version: 0,
            descriptor_version: 16,
            warp: warp.clone(),
        };

        let mut writer = BeWriter::new();
        placed.write(&mut writer).unwrap();
        let mut reader = BeReader::new(writer.as_slice());
        let back = PlacedLayer::read(&mut reader).unwrap();
        assert!(reader.is_empty());
        assert_eq!(back.unique_id.value(), placed.unique_id.value());
        assert_eq!(back.layer_type, PlacedLayerType::Raster);
        assert_eq!(back.transform, placed.transform);
        assert_eq!(back.warp.class_id.as_str(), "warp");
        // A second write is byte-stable.
        let mut writer2 = BeWriter::new();
        back.write(&mut writer2).unwrap();
        assert_eq!(writer2.as_slice(), writer.as_slice());
    }

    #[test]
    fn placed_layer_data_round_trips() {
        let mut descriptor = Descriptor {
            name: UnicodeString::new("", 1).unwrap(),
            class_id: DescriptorKey::new("null"),
            items: Vec::new(),
        };
        descriptor.insert(
            "Idnt",
            DescriptorValue::String(UnicodeString::new("abc-123", 1).unwrap()),
        );
        let data = PlacedLayerData {
            version: 4,
            descriptor_version: 16,
            descriptor,
        };
        let mut writer = BeWriter::new();
        data.write(&mut writer).unwrap();
        let mut reader = BeReader::new(writer.as_slice());
        let back = PlacedLayerData::read(&mut reader).unwrap();
        assert!(reader.is_empty());
        assert_eq!(back.version, 4);
        assert_eq!(
            back.descriptor.get("Idnt").unwrap().as_str(),
            Some("abc-123")
        );
    }

    #[test]
    fn wrong_signature_is_rejected() {
        let mut reader = BeReader::new(b"nope\0\0\0\x03");
        assert!(matches!(
            PlacedLayer::read(&mut reader),
            Err(PsdError::InvalidSignature { .. })
        ));
        let mut reader = BeReader::new(b"nope\0\0\0\x04");
        assert!(matches!(
            PlacedLayerData::read(&mut reader),
            Err(PsdError::InvalidSignature { .. })
        ));
    }
}
