//! The slices resource (`1050`).
//!
//! Version 6 (Photoshop 6) is a binary record list: a bounding rectangle, the
//! group name, and one record per slice with its name, bounds, URL, target,
//! message, alt tag, cell text, and alignment. Versions 7 and 8 (CS and later)
//! replace the records with a descriptor, and a v6 payload may still end with
//! one — the spec records that 7.0 "added a descriptor at the end of the block
//! for the individual slice info".
//!
//! Two details follow what shipped readers agree on rather than the spec's
//! prose: the rectangle is stored left, top, right, bottom (the prose says
//! top, left, bottom, right), and the Unicode strings are a four-byte code
//! unit count plus UTF-16BE with no padding. [`UnicodeString`] is kept rather
//! than a `String` so a value that carries a terminator inside its count
//! round-trips unchanged.
//!
//! The resource is read on demand and written back only when a caller changes
//! it, so an untouched document keeps its own bytes.

use crate::descriptor::Descriptor;
use crate::error::{PsdError, Result};
use crate::io::{BeReader, BeWriter};
use crate::strings::UnicodeString;

/// Version 6's slice list, the shape Photoshop 6 wrote and later versions
/// still read.
#[derive(Debug, Clone, PartialEq)]
pub struct SlicesV6 {
    /// Bounding rectangle of every slice, left/top/right/bottom.
    pub bounds: SliceBounds,
    /// Name of the slice group.
    pub group_name: UnicodeString,
    pub slices: Vec<Slice>,
}

/// One slice record (version 6).
#[derive(Debug, Clone, PartialEq)]
pub struct Slice {
    pub id: u32,
    pub group_id: u32,
    /// 0 for a user slice, 1 for one generated from a layer.
    pub origin: u32,
    /// The layer a generated slice belongs to; present on disk only when
    /// `origin` is 1.
    pub associated_layer_id: Option<u32>,
    pub name: UnicodeString,
    pub slice_type: u32,
    pub bounds: SliceBounds,
    pub url: UnicodeString,
    pub target: UnicodeString,
    pub message: UnicodeString,
    pub alt_tag: UnicodeString,
    pub cell_is_html: bool,
    pub cell_text: UnicodeString,
    pub horizontal_alignment: u32,
    pub vertical_alignment: u32,
    pub alpha: u8,
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

/// A slice rectangle, in the order the file stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SliceBounds {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl SliceBounds {
    fn read(reader: &mut BeReader) -> Result<Self> {
        Ok(Self {
            left: reader.i32()?,
            top: reader.i32()?,
            right: reader.i32()?,
            bottom: reader.i32()?,
        })
    }

    fn write(&self, writer: &mut BeWriter) {
        writer.i32(self.left);
        writer.i32(self.top);
        writer.i32(self.right);
        writer.i32(self.bottom);
    }
}

/// The `1050` resource.
#[derive(Debug, Clone, PartialEq)]
pub struct SlicesResource {
    /// 6, 7 or 8.
    pub version: u32,
    /// The binary record list of version 6.
    pub v6: Option<SlicesV6>,
    /// The descriptor of versions 7 and 8, or a v6 payload's trailing one.
    pub descriptor: Option<Descriptor>,
}

/// The descriptor version Photoshop writes ahead of a slices descriptor.
const DESCRIPTOR_VERSION: u32 = 16;

impl SlicesResource {
    /// Parse the resource payload.
    ///
    /// An unknown version is an error: the bytes after it have no meaning this
    /// reader can check, so the caller keeps the raw block instead.
    pub fn read(reader: &mut BeReader) -> Result<Self> {
        let offset = reader.position() as u64;
        let version = reader.u32()?;
        match version {
            6 => {
                let bounds = SliceBounds::read(reader)?;
                let group_name = UnicodeString::read(reader, 1)?;
                let count = reader.u32()? as usize;
                // Each slice is at least 24 bytes of fixed fields, so a count
                // that cannot fit is refused before anything is allocated.
                if count.saturating_mul(24) > reader.remaining() {
                    return Err(PsdError::InvalidData {
                        offset,
                        message: "slice count exceeds the resource",
                    });
                }
                let mut slices = Vec::with_capacity(count);
                for _ in 0..count {
                    slices.push(read_slice(reader)?);
                }
                // 7.0 added a descriptor after the records; it is optional.
                let descriptor = read_trailing_descriptor(reader)?;
                Ok(Self {
                    version,
                    v6: Some(SlicesV6 {
                        bounds,
                        group_name,
                        slices,
                    }),
                    descriptor,
                })
            }
            7 | 8 => {
                let descriptor_version = reader.u32()?;
                if descriptor_version != DESCRIPTOR_VERSION {
                    return Err(PsdError::InvalidData {
                        offset,
                        message: "slices descriptor version is not 16",
                    });
                }
                Ok(Self {
                    version,
                    v6: None,
                    descriptor: Some(Descriptor::read(reader)?),
                })
            }
            _ => Err(PsdError::InvalidData {
                offset,
                message: "slices resource version is not 6, 7 or 8",
            }),
        }
    }

    /// Serialize to the resource payload.
    pub fn to_payload(&self) -> Result<Vec<u8>> {
        let mut writer = BeWriter::new();
        writer.u32(self.version);
        if let Some(v6) = &self.v6 {
            v6.bounds.write(&mut writer);
            v6.group_name.write_verbatim(&mut writer)?;
            writer.u32(
                u32::try_from(v6.slices.len()).map_err(|_| PsdError::LengthOverflow {
                    actual: v6.slices.len() as u64,
                    width: 4,
                })?,
            );
            for slice in &v6.slices {
                write_slice(&mut writer, slice)?;
            }
            if let Some(descriptor) = &self.descriptor {
                writer.u32(DESCRIPTOR_VERSION);
                descriptor.write(&mut writer)?;
            }
        } else if let Some(descriptor) = &self.descriptor {
            writer.u32(DESCRIPTOR_VERSION);
            descriptor.write(&mut writer)?;
        }
        Ok(writer.into_inner())
    }
}

fn read_slice(reader: &mut BeReader) -> Result<Slice> {
    let id = reader.u32()?;
    let group_id = reader.u32()?;
    let origin = reader.u32()?;
    let associated_layer_id = if origin == 1 {
        Some(reader.u32()?)
    } else {
        None
    };
    let name = UnicodeString::read(reader, 1)?;
    let slice_type = reader.u32()?;
    let bounds = SliceBounds::read(reader)?;
    let url = UnicodeString::read(reader, 1)?;
    let target = UnicodeString::read(reader, 1)?;
    let message = UnicodeString::read(reader, 1)?;
    let alt_tag = UnicodeString::read(reader, 1)?;
    let cell_is_html = reader.u8()? != 0;
    let cell_text = UnicodeString::read(reader, 1)?;
    let horizontal_alignment = reader.u32()?;
    let vertical_alignment = reader.u32()?;
    let alpha = reader.u8()?;
    let red = reader.u8()?;
    let green = reader.u8()?;
    let blue = reader.u8()?;
    Ok(Slice {
        id,
        group_id,
        origin,
        associated_layer_id,
        name,
        slice_type,
        bounds,
        url,
        target,
        message,
        alt_tag,
        cell_is_html,
        cell_text,
        horizontal_alignment,
        vertical_alignment,
        alpha,
        red,
        green,
        blue,
    })
}

fn write_slice(writer: &mut BeWriter, slice: &Slice) -> Result<()> {
    writer.u32(slice.id);
    writer.u32(slice.group_id);
    writer.u32(slice.origin);
    if slice.origin == 1 {
        writer.u32(slice.associated_layer_id.unwrap_or(0));
    }
    slice.name.write_verbatim(writer)?;
    writer.u32(slice.slice_type);
    slice.bounds.write(writer);
    slice.url.write_verbatim(writer)?;
    slice.target.write_verbatim(writer)?;
    slice.message.write_verbatim(writer)?;
    slice.alt_tag.write_verbatim(writer)?;
    writer.u8(u8::from(slice.cell_is_html));
    slice.cell_text.write_verbatim(writer)?;
    writer.u32(slice.horizontal_alignment);
    writer.u32(slice.vertical_alignment);
    writer.u8(slice.alpha);
    writer.u8(slice.red);
    writer.u8(slice.green);
    writer.u8(slice.blue);
    Ok(())
}

/// A descriptor may follow the v6 records; it is present only when the next
/// word is the descriptor version, which is what shipped readers test for.
fn read_trailing_descriptor(reader: &mut BeReader) -> Result<Option<Descriptor>> {
    if reader.remaining() < 8 {
        return Ok(None);
    }
    let position = reader.position();
    if reader.u32()? != DESCRIPTOR_VERSION {
        reader.seek(position)?;
        return Ok(None);
    }
    match Descriptor::read(reader) {
        Ok(descriptor) => Ok(Some(descriptor)),
        // The trailing bytes are not a descriptor this reader understands;
        // leave them out of the typed view rather than failing the block.
        Err(_) => {
            reader.seek(position)?;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{DescriptorItem, DescriptorKey, DescriptorValue};

    fn sample() -> SlicesResource {
        SlicesResource {
            version: 6,
            v6: Some(SlicesV6 {
                bounds: SliceBounds {
                    left: 0,
                    top: 0,
                    right: 64,
                    bottom: 64,
                },
                group_name: UnicodeString::new("group", 1).unwrap(),
                slices: vec![Slice {
                    id: 1,
                    group_id: 0,
                    origin: 1,
                    associated_layer_id: Some(7),
                    name: UnicodeString::new("slice 1", 1).unwrap(),
                    slice_type: 1,
                    bounds: SliceBounds {
                        left: 0,
                        top: 0,
                        right: 32,
                        bottom: 64,
                    },
                    url: UnicodeString::new("", 1).unwrap(),
                    target: UnicodeString::new("", 1).unwrap(),
                    message: UnicodeString::new("", 1).unwrap(),
                    alt_tag: UnicodeString::new("alt", 1).unwrap(),
                    cell_is_html: true,
                    cell_text: UnicodeString::new("<b>x</b>", 1).unwrap(),
                    horizontal_alignment: 1,
                    vertical_alignment: 2,
                    alpha: 0,
                    red: 255,
                    green: 128,
                    blue: 0,
                }],
            }),
            descriptor: None,
        }
    }

    #[test]
    fn v6_round_trips() {
        let bytes = sample().to_payload().unwrap();
        let parsed = SlicesResource::read(&mut BeReader::new(&bytes)).unwrap();
        assert_eq!(parsed, sample());
        assert_eq!(parsed.to_payload().unwrap(), bytes);
    }

    #[test]
    fn a_slice_without_a_layer_has_no_id_on_disk() {
        let mut resource = sample();
        let slice = &mut resource.v6.as_mut().unwrap().slices[0];
        slice.origin = 0;
        slice.associated_layer_id = None;
        let bytes = resource.to_payload().unwrap();
        let parsed = SlicesResource::read(&mut BeReader::new(&bytes)).unwrap();
        assert_eq!(parsed.v6.unwrap().slices[0].associated_layer_id, None);
        // Four bytes shorter than the generated-slice form.
        let generated = sample().to_payload().unwrap();
        assert_eq!(bytes.len() + 4, generated.len());
    }

    #[test]
    fn a_trailing_descriptor_is_kept_and_written_back() {
        let mut bytes = sample().to_payload().unwrap();
        let mut tail = BeWriter::new();
        tail.u32(DESCRIPTOR_VERSION);
        // A realistic descriptor: a class id and one item, as Photoshop writes.
        Descriptor {
            name: UnicodeString::new("sliceInfo", 1).unwrap(),
            class_id: DescriptorKey::char_id(*b"null"),
            items: vec![DescriptorItem {
                key: DescriptorKey::new("slices"),
                value: DescriptorValue::Integer(1),
            }],
        }
        .write(&mut tail)
        .unwrap();
        bytes.extend_from_slice(&tail.into_inner());

        let parsed = SlicesResource::read(&mut BeReader::new(&bytes)).unwrap();
        assert!(
            parsed.descriptor.is_some(),
            "the trailing descriptor parses"
        );
        assert_eq!(parsed.to_payload().unwrap(), bytes);
    }

    #[test]
    fn malformed_payloads_are_refused() {
        // Unknown version.
        let mut bytes = sample().to_payload().unwrap();
        bytes[..4].copy_from_slice(&9u32.to_be_bytes());
        assert!(SlicesResource::read(&mut BeReader::new(&bytes)).is_err());
        // A count past the payload.
        let mut bytes = sample().to_payload().unwrap();
        // version (4) + bounds (16) then the group name, whose count is at 20.
        let name_len = u32::from_be_bytes(bytes[20..24].try_into().unwrap()) as usize;
        let count_at = 24 + name_len * 2;
        bytes[count_at..count_at + 4].copy_from_slice(&1000u32.to_be_bytes());
        assert!(SlicesResource::read(&mut BeReader::new(&bytes)).is_err());
        // A v7 header whose descriptor version is not 16.
        let mut bytes = 7u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(&1u32.to_be_bytes());
        assert!(SlicesResource::read(&mut BeReader::new(&bytes)).is_err());
    }
}
