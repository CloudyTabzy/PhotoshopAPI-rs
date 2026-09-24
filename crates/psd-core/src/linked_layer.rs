//! Linked-layer tagged blocks (`lnk2` / `lnkD` / `lnkE` / `lnk3`).
//!
//! Mirrors `Core/TaggedBlocks/LinkedLayerTaggedBlock.{h,cpp}`: the document
//! level blocks that hold smart-object file references. Each block carries one
//! or more [`LinkedLayer`] records whose uuid mirrors the `PlLd`/`SoLd` block
//! of the corresponding layer.
//!
//! Layout of one record: `u64` size (padded to 4) | type signature
//! (`liFD` stored / `liFE` external / `liFA` alias) | `u32` version (1–8) |
//! uuid (Pascal, 1-aligned) | file name (Unicode, 2-aligned) | file type |
//! creator | `u64` data size | open-descriptor flag (+ descriptor) | type
//! specific payload | version ≥ 5/6/7/8 extras | version == 2 external raw
//! bytes.
//!
//! Fixes over upstream:
//! - The source file size field (which upstream re-reads from disk with
//!   `ifstream::tellg` on write, producing `-1` when the file is gone) is
//!   preserved from the read data.
//! - The version == 2 external trailing payload is written for the same
//!   condition it is read under (upstream reads it only for `External` but
//!   writes it for every type).
//! - A missing date on write produces zeros, not the current system time
//!   (deterministic output).
//! - External records keep their cached `raw_file_bytes` on write; upstream
//!   clears them and writes data size 0. Preservation is lossless (real
//!   Photoshop files carry the cache and the read side accepts it either way)
//!   at the cost of larger documents. Deliberate deviation from upstream.

use crate::descriptor::Descriptor;
use crate::error::{PsdError, Result};
use crate::io::{BeReader, BeWriter};
use crate::strings::{PascalString, UnicodeString};

fn padded_record_size(size: usize, offset: u64) -> Result<usize> {
    size.checked_add(3)
        .map(|padded| padded & !3)
        .ok_or(PsdError::InvalidData {
            offset,
            message: "linked layer size overflows",
        })
}

/// Type signature of a stored (embedded) linked file.
pub const LINK_TYPE_DATA: [u8; 4] = *b"liFD";
/// Type signature of an externally linked file.
pub const LINK_TYPE_EXTERNAL: [u8; 4] = *b"liFE";
/// Type signature of an alias.
pub const LINK_TYPE_ALIAS: [u8; 4] = *b"liFA";

/// How a linked layer's payload is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkedDataKind {
    /// Embedded in the document (`liFD`).
    Data,
    /// Linked to a file on disk (`liFE`).
    External,
    /// Alias (`liFA`).
    Alias,
}

impl LinkedDataKind {
    pub fn from_signature(signature: [u8; 4]) -> Option<Self> {
        match &signature {
            b"liFD" => Some(Self::Data),
            b"liFE" => Some(Self::External),
            b"liFA" => Some(Self::Alias),
            _ => None,
        }
    }

    pub const fn signature(self) -> [u8; 4] {
        match self {
            Self::Data => LINK_TYPE_DATA,
            Self::External => LINK_TYPE_EXTERNAL,
            Self::Alias => LINK_TYPE_ALIAS,
        }
    }
}

/// Timestamp of an externally linked file (version > 3).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Date {
    pub year: u32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub seconds: f64,
}

impl Date {
    fn read(reader: &mut BeReader) -> Result<Self> {
        Ok(Self {
            year: reader.u32()?,
            month: reader.u8()?,
            day: reader.u8()?,
            hour: reader.u8()?,
            minute: reader.u8()?,
            seconds: reader.f64()?,
        })
    }

    fn write(&self, writer: &mut BeWriter) {
        writer.u32(self.year);
        writer.u8(self.month);
        writer.u8(self.day);
        writer.u8(self.hour);
        writer.u8(self.minute);
        writer.f64(self.seconds);
    }
}

/// One linked-file record of a linked-layer block.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkedLayer {
    pub kind: LinkedDataKind,
    /// 1–8; version 5+ carry a child document id, 6+ a modification time,
    /// 7+ a locked flag and 8 a content-id descriptor.
    pub version: i32,
    /// UUID shared with the layer's `PlLd`/`SoLd` block (Pascal, 1-aligned).
    pub unique_id: PascalString,
    /// File name (not necessarily a real path).
    pub file_name: UnicodeString,
    /// Four-character file type, e.g. `png ` or `JPEG`.
    pub file_type: [u8; 4],
    pub file_creator: u32,
    /// `null`-class descriptor Photoshop stores for the open dialog state.
    pub file_open_descriptor: Option<Descriptor>,
    /// Descriptor describing the external link (external files only).
    pub linked_file_descriptor: Option<Descriptor>,
    /// Version 8+ content descriptor.
    pub content_id: Option<Descriptor>,
    /// Modification timestamp (external, version > 3).
    pub date: Option<Date>,
    /// Embedded or external payload bytes (its length is the `u64` data size
    /// field on disk).
    pub raw_file_bytes: Vec<u8>,
    pub child_document_id: Option<UnicodeString>,
    pub asset_mod_time: Option<f64>,
    pub asset_is_locked: Option<bool>,
    /// Source file size; preserved from the file (upstream re-derives it from
    /// disk on write).
    pub file_size: u64,
}

/// Borrowed view of one linked-layer record. The encoded source payload stays
/// in the containing tagged block instead of being cloned while inspecting
/// unrelated records.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkedLayerView<'a> {
    pub kind: LinkedDataKind,
    pub version: i32,
    pub unique_id: PascalString,
    pub file_name: UnicodeString,
    pub file_type: [u8; 4],
    pub file_creator: u32,
    pub file_open_descriptor: Option<Descriptor>,
    pub linked_file_descriptor: Option<Descriptor>,
    pub content_id: Option<Descriptor>,
    pub date: Option<Date>,
    pub raw_file_bytes: &'a [u8],
    pub child_document_id: Option<UnicodeString>,
    pub asset_mod_time: Option<f64>,
    pub asset_is_locked: Option<bool>,
    pub file_size: u64,
}

impl LinkedLayer {
    /// Read one record; the reader must span the record's content (the size
    /// marker is handled by the caller/`LinkedLayerTaggedBlock`). The typed
    /// record does not retain the fixed eight-byte alias payload; keep the
    /// enclosing raw tagged-block data authoritative for lossless passthrough.
    pub fn read(reader: &mut BeReader) -> Result<Self> {
        LinkedLayerView::read(reader).map(Into::into)
    }

    /// Serialize the record including its `u64` size marker and 4-byte
    /// alignment padding.
    pub fn write(&self, writer: &mut BeWriter) -> Result<()> {
        let marker = writer.reserve_len_marker64();
        let content_start = writer.position();

        writer.bytes(&self.kind.signature());
        writer.i32(self.version);
        self.unique_id.write(writer)?;
        self.file_name.write_verbatim(writer)?;
        writer.bytes(&self.file_type);
        writer.u32(self.file_creator);
        writer.u64(self.raw_file_bytes.len() as u64);
        writer.u8(u8::from(self.file_open_descriptor.is_some()));
        if let Some(descriptor) = &self.file_open_descriptor {
            writer.u32(16);
            descriptor.write(writer)?;
        }

        match self.kind {
            LinkedDataKind::External => {
                // The descriptor is always present; the date only for
                // version > 3.
                writer.u32(16);
                match &self.linked_file_descriptor {
                    Some(descriptor) => descriptor.write(writer)?,
                    None => {
                        return Err(PsdError::InvalidData {
                            offset: content_start as u64,
                            message: "external linked layer without a file descriptor",
                        })
                    }
                }
                if self.version > 3 {
                    match &self.date {
                        Some(date) => date.write(writer),
                        None => Date::default().write(writer),
                    }
                }
                writer.u64(self.file_size);
                if self.version > 2 {
                    writer.bytes(&self.raw_file_bytes);
                }
            }
            LinkedDataKind::Alias => writer.bytes(&[0u8; 8]),
            LinkedDataKind::Data => writer.bytes(&self.raw_file_bytes),
        }

        if self.version >= 5 {
            match &self.child_document_id {
                Some(id) => id.write_verbatim(writer)?,
                None => UnicodeString::default().write_verbatim(writer)?,
            }
        }
        if self.version >= 6 {
            writer.f64(self.asset_mod_time.unwrap_or(0.0));
        }
        if self.version >= 7 {
            writer.u8(u8::from(self.asset_is_locked.unwrap_or(false)));
        }
        if self.version >= 8 {
            writer.u32(16);
            match &self.content_id {
                Some(descriptor) => descriptor.write(writer)?,
                None => {
                    return Err(PsdError::InvalidData {
                        offset: content_start as u64,
                        message: "version 8 linked layer without a content descriptor",
                    })
                }
            }
        }
        if self.version == 2 && self.kind == LinkedDataKind::External {
            writer.bytes(&self.raw_file_bytes);
        }

        let content_end = writer.position();
        writer.pad_to_relative(content_start, 4);
        // The declared size is the unpadded content length (Photoshop pads the
        // record to 4 without counting the padding).
        let content_len = (content_end - marker - 8) as u64;
        writer.patch_len64(marker, content_len);
        Ok(())
    }
}

impl<'a> LinkedLayerView<'a> {
    /// Parse a linked-layer record without copying its encoded file payload.
    pub fn read(reader: &mut BeReader<'a>) -> Result<Self> {
        let offset = reader.position() as u64;
        let size = reader.u64()?;
        // The declared size is the unpadded content length; records are
        // aligned to 4 bytes (upstream reads the rounded size directly).
        let size = usize::try_from(size).map_err(|_| PsdError::InvalidData {
            offset,
            message: "linked layer size does not fit the platform",
        })?;
        let padded = padded_record_size(size, offset)?;
        let content_end = reader
            .position()
            .checked_add(padded)
            .ok_or(PsdError::InvalidData {
                offset,
                message: "linked layer size overflows",
            })?;

        let mut signature = [0u8; 4];
        signature.copy_from_slice(reader.take(4)?);
        let kind = LinkedDataKind::from_signature(signature).ok_or(PsdError::InvalidData {
            offset,
            message: "unknown linked layer type",
        })?;

        let version = reader.i32()?;
        // Upstream hard-errors on versions outside 1–8 (the field gates the
        // record grammar below; a future v9 would otherwise be parsed with the
        // v≤8 layout and only caught, if at all, by the size-overrun check).
        if !(1..=8).contains(&version) {
            return Err(PsdError::InvalidData {
                offset,
                message: "linked layer version is outside the supported 1–8 range",
            });
        }
        let unique_id = PascalString::read(reader, 1)?;
        let file_name = UnicodeString::read(reader, 2)?;
        let mut file_type = [0u8; 4];
        file_type.copy_from_slice(reader.take(4)?);
        let file_creator = reader.u32()?;
        let data_size = usize::try_from(reader.u64()?).map_err(|_| PsdError::InvalidData {
            offset,
            message: "linked source size does not fit the platform",
        })?;
        let has_open_descriptor = reader.u8()? != 0;
        let file_open_descriptor = if has_open_descriptor {
            let _descriptor_version = reader.u32()?;
            Some(Descriptor::read(reader)?)
        } else {
            None
        };

        let mut linked_file_descriptor = None;
        let mut date = None;
        let mut file_size = 0;
        let mut raw_file_bytes: &'a [u8] = &[];
        match kind {
            LinkedDataKind::External => {
                // The linked file descriptor is always present; the date only
                // for version > 3.
                let _descriptor_version = reader.u32()?;
                linked_file_descriptor = Some(Descriptor::read(reader)?);
                if version > 3 {
                    date = Some(Date::read(reader)?);
                }
                file_size = reader.u64()?;
                if version > 2 {
                    raw_file_bytes = reader.take(data_size)?;
                }
            }
            LinkedDataKind::Alias => reader.skip(8)?,
            LinkedDataKind::Data => {
                raw_file_bytes = reader.take(data_size)?;
            }
        }

        let child_document_id = if version >= 5 {
            Some(UnicodeString::read(reader, 2)?)
        } else {
            None
        };
        let asset_mod_time = if version >= 6 {
            Some(reader.f64()?)
        } else {
            None
        };
        let asset_is_locked = if version >= 7 {
            Some(reader.u8()? != 0)
        } else {
            None
        };
        let content_id = if version >= 8 {
            let _descriptor_version = reader.u32()?;
            Some(Descriptor::read(reader)?)
        } else {
            None
        };
        if version == 2 && kind == LinkedDataKind::External {
            raw_file_bytes = reader.take(data_size)?;
        }

        // Records are padded to 4 bytes; the declared size excludes padding.
        let position = reader.position();
        if position > content_end {
            return Err(PsdError::InvalidData {
                offset,
                message: "linked layer record exceeds its declared size",
            });
        }
        reader.skip(content_end - position)?;

        Ok(Self {
            kind,
            version,
            unique_id,
            file_name,
            file_type,
            file_creator,
            file_open_descriptor,
            linked_file_descriptor,
            content_id,
            date,
            raw_file_bytes,
            child_document_id,
            asset_mod_time,
            asset_is_locked,
            file_size,
        })
    }
}

impl From<LinkedLayerView<'_>> for LinkedLayer {
    fn from(view: LinkedLayerView<'_>) -> Self {
        Self {
            kind: view.kind,
            version: view.version,
            unique_id: view.unique_id,
            file_name: view.file_name,
            file_type: view.file_type,
            file_creator: view.file_creator,
            file_open_descriptor: view.file_open_descriptor,
            linked_file_descriptor: view.linked_file_descriptor,
            content_id: view.content_id,
            date: view.date,
            raw_file_bytes: view.raw_file_bytes.to_vec(),
            child_document_id: view.child_document_id,
            asset_mod_time: view.asset_mod_time,
            asset_is_locked: view.asset_is_locked,
            file_size: view.file_size,
        }
    }
}

/// A typed view of a `lnk2` / `lnkD` / `lnkE` / `lnk3` block payload.
///
/// This view contains only recognized records. Unknown signatures and block
/// tails are not represented, and alias payload bytes are not modeled by
/// [`LinkedLayer`]; direct `read` → `write` is therefore not a lossless raw
/// passthrough. Keep the original `TaggedBlock.data` when round-trip fidelity
/// is required, and use its raw-preserving append helper for edits.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LinkedLayerTaggedBlock {
    pub layers: Vec<LinkedLayer>,
}

impl LinkedLayerTaggedBlock {
    /// Parse the block payload (the outer tagged-block length is handled by
    /// [`crate::TaggedBlock`]).
    pub fn read(reader: &mut BeReader) -> Result<Self> {
        let mut layers = Vec::new();
        while reader.remaining() >= 8 {
            let before = reader.position();
            let mut probe = reader.clone();
            let size = usize::try_from(probe.u64()?).map_err(|_| PsdError::InvalidData {
                offset: before as u64,
                message: "linked layer size does not fit the platform",
            })?;
            let record_len = 8usize
                .checked_add(padded_record_size(size, before as u64)?)
                .ok_or(PsdError::InvalidData {
                    offset: before as u64,
                    message: "linked layer size overflows",
                })?;
            let record_bytes = reader.take(record_len)?;
            if size < 4 {
                return Err(PsdError::InvalidData {
                    offset: before as u64,
                    message: "linked layer record is shorter than its type signature",
                });
            }
            let mut signature = [0u8; 4];
            signature.copy_from_slice(&record_bytes[8..12]);
            if LinkedDataKind::from_signature(signature).is_some() {
                layers.push(LinkedLayer::read(&mut BeReader::new(record_bytes))?);
            } else {
                // The owning TaggedBlock still retains this raw record. Ignore
                // it in the typed view so a known UUID later in the block can
                // still resolve; edits append to the raw payload instead of
                // reserializing this lossy view.
                tracing::warn!(
                    signature = %String::from_utf8_lossy(&signature),
                    "unknown linked-layer record preserved as raw block data"
                );
            }
            if reader.position() <= before {
                return Err(PsdError::InvalidData {
                    offset: before as u64,
                    message: "linked layer record did not advance the reader",
                });
            }
        }
        // Any trailing bytes shorter than a record are block padding.
        let trailing = reader.remaining();
        if trailing > 0 {
            tracing::warn!("linked layer block has {trailing} trailing bytes");
        }
        reader.skip(trailing)?;
        Ok(Self { layers })
    }

    /// Parse borrowed record views from one linked-layer block payload. Source
    /// bytes remain borrowed from `payload`; use this for UUID lookup or
    /// deduplication so unrelated embedded documents are not cloned.
    pub fn read_views<'a>(payload: &'a [u8]) -> Result<Vec<LinkedLayerView<'a>>> {
        let mut reader = BeReader::new(payload);
        let mut layers = Vec::new();
        while reader.remaining() >= 8 {
            let before = reader.position();
            let offset = before as u64;
            let mut probe = reader.clone();
            let size = usize::try_from(probe.u64()?).map_err(|_| PsdError::InvalidData {
                offset,
                message: "linked layer size does not fit the platform",
            })?;
            let record_len = 8usize
                .checked_add(padded_record_size(size, offset)?)
                .ok_or(PsdError::InvalidData {
                    offset,
                    message: "linked layer size overflows",
                })?;
            let record_bytes = reader.take(record_len)?;
            if size < 4 {
                return Err(PsdError::InvalidData {
                    offset,
                    message: "linked layer record is shorter than its type signature",
                });
            }
            let mut signature = [0u8; 4];
            signature.copy_from_slice(&record_bytes[8..12]);
            if LinkedDataKind::from_signature(signature).is_some() {
                layers.push(LinkedLayerView::read(&mut BeReader::new(record_bytes))?);
            } else {
                tracing::warn!(
                    signature = %String::from_utf8_lossy(&signature),
                    "unknown linked-layer record preserved as raw block data"
                );
            }
            if reader.position() <= before {
                return Err(PsdError::InvalidData {
                    offset,
                    message: "linked layer record did not advance the reader",
                });
            }
        }
        let trailing = reader.remaining();
        if trailing > 0 {
            tracing::warn!("linked layer block has {trailing} trailing bytes");
            reader.skip(trailing)?;
        }
        Ok(layers)
    }

    /// Serialize the recognized typed records. This intentionally cannot
    /// reproduce unknown records, block suffixes, or opaque alias payloads;
    /// preserve the owning raw tagged-block bytes for lossless roundtrips.
    pub fn write(&self, writer: &mut BeWriter) -> Result<()> {
        for layer in &self.layers {
            layer.write(writer)?;
        }
        Ok(())
    }

    /// Append a new record to an existing block payload without parsing or
    /// reserializing its records. This deliberately scans only each record's
    /// declared `u64` size, so unknown future signatures and opaque record
    /// fields remain byte-exact. A short block-level tail (fewer than the
    /// eight bytes needed to begin another record) stays after the new record.
    pub fn append_record_preserving_bytes(payload: &[u8], layer: &LinkedLayer) -> Result<Vec<u8>> {
        let mut output = payload.to_vec();
        Self::append_record_preserving_bytes_in_place(&mut output, layer)?;
        Ok(output)
    }

    /// Append to a mutable raw payload without cloning the existing block.
    /// The record and capacity are prepared before the original bytes move, so
    /// a parse, serialization, overflow, or reservation error leaves `payload`
    /// unchanged.
    pub fn append_record_preserving_bytes_in_place(
        payload: &mut Vec<u8>,
        layer: &LinkedLayer,
    ) -> Result<()> {
        let mut reader = BeReader::new(payload);
        while reader.remaining() >= 8 {
            let before = reader.position();
            let offset = before as u64;
            let size = reader.u64()?;
            let size = usize::try_from(size).map_err(|_| PsdError::InvalidData {
                offset,
                message: "linked layer size does not fit the platform",
            })?;
            let padded_size = padded_record_size(size, offset)?;
            reader.skip(padded_size)?;
            if reader.position() <= before {
                return Err(PsdError::InvalidData {
                    offset,
                    message: "linked layer record did not advance the reader",
                });
            }
        }

        let suffix_start = reader.position();
        let mut record_writer = BeWriter::new();
        layer.write(&mut record_writer)?;
        let record = record_writer.into_inner();
        payload
            .len()
            .checked_add(record.len())
            .ok_or(PsdError::LengthOverflow {
                actual: payload.len() as u64 + record.len() as u64,
                width: 8,
            })?;
        payload
            .try_reserve(record.len())
            .map_err(|_| PsdError::InvalidData {
                offset: suffix_start as u64,
                message: "unable to reserve memory for linked-layer append",
            })?;
        payload.extend_from_slice(&record);
        payload[suffix_start..].rotate_right(record.len());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{DescriptorKey, DescriptorValue};

    fn sample_descriptor() -> Descriptor {
        let mut descriptor = Descriptor {
            name: UnicodeString::new("null", 1).unwrap(),
            class_id: DescriptorKey::new("null"),
            items: Vec::new(),
        };
        descriptor.insert(
            "Idnt",
            DescriptorValue::String(UnicodeString::new("uuid-1", 1).unwrap()),
        );
        descriptor
    }

    #[test]
    fn embedded_linked_layer_round_trips() {
        let layer = LinkedLayer {
            kind: LinkedDataKind::Data,
            version: 7,
            unique_id: PascalString::new("abc-uuid", 1),
            file_name: UnicodeString::new("texture.png", 2).unwrap(),
            file_type: *b"png ",
            file_creator: 0,
            file_open_descriptor: Some(sample_descriptor()),
            linked_file_descriptor: None,
            content_id: None,
            date: None,
            raw_file_bytes: vec![1, 2, 3, 4, 5],
            child_document_id: Some(UnicodeString::new("child-uuid", 2).unwrap()),
            asset_mod_time: Some(1234.5),
            asset_is_locked: Some(false),
            file_size: 0,
        };

        let mut writer = BeWriter::new();
        layer.write(&mut writer).unwrap();
        let views = LinkedLayerTaggedBlock::read_views(writer.as_slice()).unwrap();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].raw_file_bytes, layer.raw_file_bytes);
        let block_start = writer.as_slice().as_ptr() as usize;
        let source_start = views[0].raw_file_bytes.as_ptr() as usize;
        assert!((block_start..block_start + writer.as_slice().len()).contains(&source_start));

        let mut reader = BeReader::new(writer.as_slice());
        let back = LinkedLayer::read(&mut reader).unwrap();
        assert!(reader.is_empty());
        assert_eq!(back, layer);

        // The size marker is the unpadded content length; the record is
        // padded to 4 bytes.
        let padded_content = writer.as_slice().len() - 8;
        let size = u64::from_be_bytes(writer.as_slice()[..8].try_into().unwrap());
        assert!((size as usize) <= padded_content);
        assert!((padded_content - size as usize) < 4);
    }

    #[test]
    fn external_linked_layer_round_trips_with_size_field() {
        let layer = LinkedLayer {
            kind: LinkedDataKind::External,
            version: 5,
            unique_id: PascalString::new("ext-uuid", 1),
            file_name: UnicodeString::new("photo.jpg", 2).unwrap(),
            file_type: *b"JPEG",
            file_creator: 0,
            file_open_descriptor: None,
            linked_file_descriptor: Some(sample_descriptor()),
            content_id: None,
            date: Some(Date {
                year: 2024,
                month: 6,
                day: 1,
                hour: 12,
                minute: 30,
                seconds: 5.5,
            }),
            raw_file_bytes: Vec::new(),
            child_document_id: Some(UnicodeString::new("child", 2).unwrap()),
            asset_mod_time: None,
            asset_is_locked: None,
            file_size: 4242,
        };
        let mut writer = BeWriter::new();
        layer.write(&mut writer).unwrap();
        let mut reader = BeReader::new(writer.as_slice());
        let back = LinkedLayer::read(&mut reader).unwrap();
        assert!(reader.is_empty());
        assert_eq!(back, layer);
        assert_eq!(back.file_size, 4242);
    }

    #[test]
    fn multi_record_block_round_trips() {
        let mut block = LinkedLayerTaggedBlock::default();
        for (index, kind) in [LinkedDataKind::Data, LinkedDataKind::Alias]
            .into_iter()
            .enumerate()
        {
            // Alias records carry no payload by format definition.
            let raw_file_bytes = match kind {
                LinkedDataKind::Data => vec![index as u8; 3],
                _ => Vec::new(),
            };
            block.layers.push(LinkedLayer {
                kind,
                version: 7,
                unique_id: PascalString::new(format!("uuid-{index}"), 1),
                file_name: UnicodeString::new(format!("file{index}.png"), 2).unwrap(),
                file_type: *b"png ",
                file_creator: 0,
                file_open_descriptor: None,
                linked_file_descriptor: None,
                content_id: None,
                date: None,
                raw_file_bytes,
                // Version >= 5 always carries a child document id on disk.
                child_document_id: Some(UnicodeString::new("", 2).unwrap()),
                asset_mod_time: Some(0.0),
                asset_is_locked: Some(false),
                file_size: 0,
            });
        }

        let mut writer = BeWriter::new();
        block.write(&mut writer).unwrap();
        let mut reader = BeReader::new(writer.as_slice());
        let back = LinkedLayerTaggedBlock::read(&mut reader).unwrap();
        assert!(reader.is_empty());
        assert_eq!(back, block);
    }

    #[test]
    fn append_preserves_unknown_record_bytes_and_short_block_tail() {
        let original = LinkedLayer {
            kind: LinkedDataKind::Data,
            version: 7,
            unique_id: PascalString::new("old-id", 1),
            file_name: UnicodeString::new("old.png", 2).unwrap(),
            file_type: *b"png ",
            file_creator: 0,
            file_open_descriptor: None,
            linked_file_descriptor: None,
            content_id: None,
            date: None,
            raw_file_bytes: vec![1, 2, 3],
            child_document_id: Some(UnicodeString::new("child", 2).unwrap()),
            asset_mod_time: Some(0.0),
            asset_is_locked: Some(false),
            file_size: 0,
        };
        let mut writer = BeWriter::new();
        original.write(&mut writer).unwrap();
        let known_record = writer.into_inner();

        // Simulate a newer writer adding opaque bytes inside a record, then
        // changing the signature to a record kind this parser does not know.
        let old_size = u64::from_be_bytes(known_record[..8].try_into().unwrap());
        let old_content_end = 8 + old_size as usize;
        let mut unknown_record = known_record[..old_content_end].to_vec();
        unknown_record.extend_from_slice(&[0xFA, 0xCE, 0x01]);
        unknown_record[0..8].copy_from_slice(&(old_size + 3).to_be_bytes());
        unknown_record[8..12].copy_from_slice(b"liZZ");
        while !(unknown_record.len() - 8).is_multiple_of(4) {
            unknown_record.push(0);
        }

        let alias = LinkedLayer {
            kind: LinkedDataKind::Alias,
            version: 7,
            unique_id: PascalString::new("alias-id", 1),
            file_name: UnicodeString::new("asset.png", 2).unwrap(),
            file_type: *b"png ",
            file_creator: 0,
            file_open_descriptor: None,
            linked_file_descriptor: None,
            content_id: None,
            date: None,
            raw_file_bytes: Vec::new(),
            child_document_id: Some(UnicodeString::new("asset", 2).unwrap()),
            asset_mod_time: Some(0.0),
            asset_is_locked: Some(false),
            file_size: 0,
        };
        let mut alias_writer = BeWriter::new();
        alias.write(&mut alias_writer).unwrap();
        let mut alias_record = alias_writer.into_inner();
        let mut alias_reader = BeReader::new(&alias_record);
        alias_reader.u64().unwrap();
        alias_reader.take(4).unwrap(); // liFA
        alias_reader.i32().unwrap();
        PascalString::read(&mut alias_reader, 1).unwrap();
        UnicodeString::read(&mut alias_reader, 2).unwrap();
        alias_reader.take(4).unwrap(); // file type
        alias_reader.u32().unwrap();
        alias_reader.u64().unwrap(); // data size
        assert_eq!(alias_reader.u8().unwrap(), 0); // no open descriptor
        let alias_payload_offset = alias_reader.position();
        let alias_payload = [0x31, 0x41, 0x59, 0x26, 0x53, 0x58, 0x97, 0x93];
        alias_record[alias_payload_offset..alias_payload_offset + 8]
            .copy_from_slice(&alias_payload);

        let block_tail = [0xDE, 0xAD, 0xBE];
        let mut protected_prefix = unknown_record.clone();
        protected_prefix.extend_from_slice(&alias_record);
        let mut payload = protected_prefix.clone();
        payload.extend_from_slice(&block_tail);
        let new_record = LinkedLayer {
            kind: LinkedDataKind::Data,
            version: 7,
            unique_id: PascalString::new("new-id", 1),
            file_name: UnicodeString::new("new.png", 2).unwrap(),
            file_type: *b"png ",
            file_creator: 0,
            file_open_descriptor: None,
            linked_file_descriptor: None,
            content_id: None,
            date: None,
            raw_file_bytes: vec![4, 5, 6],
            child_document_id: Some(UnicodeString::new("child-new", 2).unwrap()),
            asset_mod_time: Some(0.0),
            asset_is_locked: Some(false),
            file_size: 0,
        };

        let appended =
            LinkedLayerTaggedBlock::append_record_preserving_bytes(&payload, &new_record).unwrap();
        let mut appended_in_place = payload.clone();
        LinkedLayerTaggedBlock::append_record_preserving_bytes_in_place(
            &mut appended_in_place,
            &new_record,
        )
        .unwrap();
        assert_eq!(appended_in_place, appended);
        assert_eq!(
            &appended[..protected_prefix.len()],
            protected_prefix.as_slice()
        );
        assert_eq!(&appended[appended.len() - block_tail.len()..], &block_tail);

        let parsed = LinkedLayerTaggedBlock::read(&mut BeReader::new(&appended)).unwrap();
        assert_eq!(parsed.layers.len(), 2);
        assert_eq!(parsed.layers[0].kind, LinkedDataKind::Alias);
        assert_eq!(parsed.layers[0].unique_id.value(), "alias-id");
        assert_eq!(parsed.layers[1].unique_id.value(), "new-id");

        let new_start = protected_prefix.len();
        let mut appended_record = BeReader::new(&appended[new_start..appended.len() - 3]);
        assert_eq!(LinkedLayer::read(&mut appended_record).unwrap(), new_record);
        assert!(appended_record.is_empty());
    }
}
