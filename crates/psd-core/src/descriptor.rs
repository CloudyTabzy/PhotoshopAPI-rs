//! Photoshop action descriptors: the OSType-keyed tree used by smart-object
//! (`SoLd`) and text (`TySh`) tagged blocks.
//!
//! Mirrors `Core/Struct/DescriptorStructure.{h,cpp}`. A descriptor is a class
//! name (Unicode string), a class id (a *length-denoted key*) and an ordered
//! list of `key → OSType + value` items; values nest arbitrarily (descriptors,
//! object arrays, lists, references).
//!
//! Key encoding: a `u32` length followed by that many bytes — except that a
//! length of **0** means "the next 4 bytes are the key" (Photoshop's "known
//! four-byte key" convention). Upstream decides which 4-byte keys get the
//! zero length from a large hardcoded list; this port instead records how the
//! key was encoded when reading and reproduces it when writing, which is
//! byte-exact without the list. Keys created with [`DescriptorKey::new`] get an
//! explicit length (Photoshop parses those fine); [`DescriptorKey::char_id`]
//! reproduces the zero-length encoding for Photoshop's four-character IDs.
//!
//! Fidelity notes over upstream:
//! - `UntF`/`UnFl` unit keys are kept verbatim. Upstream maps them to an enum
//!   and silently rewrites unknown units as `#Ang`.
//! - `Class` and raw-data values keep their original OSType bytes
//!   (`type`/`GlbC`/`Clss`, `tdta`/`Pth `/`alis`).
//! - Unknown OSTypes are a typed error; upstream falls back to raw-data
//!   parsing and desyncs the stream.

use crate::error::{PsdError, Result};
use crate::io::{BeReader, BeWriter};
use crate::strings::UnicodeString;

/// OSType keys this port understands (`Impl::descriptorKeys` upstream).
pub const OS_DESCRIPTOR: [u8; 4] = *b"Objc";
pub const OS_GLOBAL_OBJECT: [u8; 4] = *b"GlbO";
pub const OS_OBJECT_ARRAY: [u8; 4] = *b"ObAr";
pub const OS_LIST: [u8; 4] = *b"VlLs";
pub const OS_REFERENCE: [u8; 4] = *b"obj ";
pub const OS_DOUBLE: [u8; 4] = *b"doub";
pub const OS_UNIT_FLOAT: [u8; 4] = *b"UntF";
pub const OS_UNIT_FLOATS: [u8; 4] = *b"UnFl";
pub const OS_STRING: [u8; 4] = *b"TEXT";
pub const OS_ENUMERATED: [u8; 4] = *b"enum";
pub const OS_INTEGER: [u8; 4] = *b"long";
pub const OS_LARGE_INTEGER: [u8; 4] = *b"comp";
pub const OS_BOOLEAN: [u8; 4] = *b"bool";
pub const OS_CLASS_1: [u8; 4] = *b"type";
pub const OS_CLASS_2: [u8; 4] = *b"GlbC";
pub const OS_CLASS_3: [u8; 4] = *b"Clss";
pub const OS_ALIAS: [u8; 4] = *b"alis";
pub const OS_RAW_DATA: [u8; 4] = *b"tdta";
pub const OS_PATH: [u8; 4] = *b"Pth ";
pub const OS_PROPERTY: [u8; 4] = *b"prop";
pub const OS_ENUMERATED_REFERENCE: [u8; 4] = *b"Enmr";
pub const OS_OFFSET: [u8; 4] = *b"rele";
pub const OS_IDENTIFIER: [u8; 4] = *b"Idnt";
pub const OS_INDEX: [u8; 4] = *b"indx";
pub const OS_NAME: [u8; 4] = *b"name";

/// Safety cap on descriptor nesting depth. Photoshop's own descriptor trees
/// stay well under 50 levels; without a cap a hostile payload of nested
/// `Objc`/`ObAr` values recurses until the stack overflows. Upstream C++
/// would hit the same wall — the cap is a hardening fix. The
/// value also keeps the fattest debug-build frames within a 1 MiB thread
/// stack: each level costs two nested calls with sizable locals.
const MAX_NESTING_DEPTH: usize = 64;

#[inline]
fn check_nesting_depth(reader: &BeReader, depth: usize) -> Result<()> {
    if depth > MAX_NESTING_DEPTH {
        return Err(PsdError::InvalidData {
            offset: reader.position() as u64,
            message: "descriptor nesting exceeds the safety limit",
        });
    }
    Ok(())
}

/// A descriptor key: a short string with a length-denoted encoding.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct DescriptorKey {
    bytes: Vec<u8>,
    /// The length field was 0 (the "known four-byte key" encoding).
    implicit_len: bool,
}

impl DescriptorKey {
    /// Create a key written with an explicit length field.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            bytes: text.into().into_bytes(),
            implicit_len: false,
        }
    }

    /// Create a key from raw bytes written with an explicit length field.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            bytes: bytes.into(),
            implicit_len: false,
        }
    }

    /// Create a Photoshop four-character ID written with the zero-length
    /// ("known four-byte key") encoding, e.g. `AnCr` or `Hrzn`.
    pub fn char_id(code: [u8; 4]) -> Self {
        Self {
            bytes: code.to_vec(),
            implicit_len: true,
        }
    }

    /// The key as a (possibly lossy) string.
    pub fn as_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Replace a key's text while retaining its explicit/implicit length
    /// encoding. An implicit key is necessarily four bytes on disk.
    pub fn replace_text_preserving_encoding(&self, text: &str) -> Result<Self> {
        let bytes = text.as_bytes().to_vec();
        if self.implicit_len && bytes.len() != 4 {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "implicit descriptor keys must remain four bytes",
            });
        }
        Ok(Self {
            bytes,
            implicit_len: self.implicit_len,
        })
    }

    pub fn uses_implicit_length(&self) -> bool {
        self.implicit_len
    }

    pub fn read(reader: &mut BeReader) -> Result<Self> {
        let offset = reader.position() as u64;
        let len = reader.u32()? as usize;
        if len == 0 {
            let bytes = reader.take(4)?.to_vec();
            Ok(Self {
                bytes,
                implicit_len: true,
            })
        } else {
            let bytes = reader.take(len)?.to_vec();
            if bytes.len() != len {
                return Err(PsdError::InvalidData {
                    offset,
                    message: "descriptor key shorter than its length",
                });
            }
            Ok(Self {
                bytes,
                implicit_len: false,
            })
        }
    }

    pub fn write(&self, writer: &mut BeWriter) -> Result<()> {
        if self.implicit_len {
            debug_assert_eq!(self.bytes.len(), 4);
            writer.u32(0);
        } else {
            writer.u32(
                u32::try_from(self.bytes.len()).map_err(|_| PsdError::LengthOverflow {
                    actual: self.bytes.len() as u64,
                    width: 4,
                })?,
            );
        }
        writer.bytes(&self.bytes);
        Ok(())
    }
}

impl Default for DescriptorKey {
    /// An empty key (written with an explicit zero length).
    fn default() -> Self {
        Self::new("")
    }
}

impl std::fmt::Debug for DescriptorKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

/// One `key → value` pair of a descriptor or object array.
#[derive(Debug, Clone, PartialEq)]
pub struct DescriptorItem {
    pub key: DescriptorKey,
    pub value: DescriptorValue,
}

/// A tracked integer value inside a descriptor tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptorIntegerSpan {
    pub key: Vec<u8>,
    pub range: std::ops::Range<usize>,
    pub value: i32,
}

/// Optional source ranges captured while parsing direct descriptor items.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DescriptorPayloadSpans {
    /// Raw-data bytes, excluding their four-byte length field.
    pub raw_data: Option<std::ops::Range<usize>>,
    /// A `TEXT` UnicodeString value, including its unit-count prefix.
    pub unicode_string: Option<std::ops::Range<usize>>,
}

/// A descriptor value, tagged by its OSType.
#[derive(Debug, Clone, PartialEq)]
pub enum DescriptorValue {
    /// `Objc` / `GlbO`: a nested descriptor.
    Descriptor(Descriptor),
    /// `ObAr`: object array (a descriptor with an extra item count).
    ObjectArray(ObjectArray),
    /// `VlLs`: a list of keyless values.
    List(Vec<DescriptorValue>),
    /// `obj `: a reference (same layout as a list).
    Reference(Vec<DescriptorValue>),
    /// `doub`.
    Double(f64),
    /// `UntF`: a 4-byte unit key plus a value.
    UnitFloat { unit: [u8; 4], value: f64 },
    /// `UnFl`: a unit key plus a list of values.
    UnitFloats { unit: [u8; 4], values: Vec<f64> },
    /// `TEXT`.
    String(UnicodeString),
    /// `enum`: type id and enumerator (both length-denoted keys).
    Enumerated {
        type_id: DescriptorKey,
        value: DescriptorKey,
    },
    /// `long`.
    Integer(i32),
    /// `comp`.
    LargeInteger(i64),
    /// `bool`.
    Boolean(bool),
    /// `type` / `GlbC` / `Clss`.
    Class {
        os_key: [u8; 4],
        name: UnicodeString,
        class_id: DescriptorKey,
    },
    /// `tdta` / `Pth ` / `alis`: length-prefixed raw bytes.
    RawData { os_key: [u8; 4], data: Vec<u8> },
    /// `prop`.
    Property {
        name: UnicodeString,
        class_id: DescriptorKey,
        key_id: DescriptorKey,
    },
    /// `Enmr`.
    EnumeratedReference {
        name: UnicodeString,
        class_id: DescriptorKey,
        type_id: DescriptorKey,
        value: DescriptorKey,
    },
    /// `rele`.
    Offset {
        name: UnicodeString,
        class_id: DescriptorKey,
        offset: u32,
    },
    /// `Idnt`.
    Identifier(i32),
    /// `indx`.
    Index(i32),
    /// `name`.
    Name {
        name: UnicodeString,
        class_id: DescriptorKey,
        value: UnicodeString,
    },
}

impl DescriptorValue {
    /// The OSType key this value serializes as.
    pub fn os_key(&self) -> [u8; 4] {
        match self {
            Self::Descriptor(_) => OS_DESCRIPTOR,
            Self::ObjectArray(_) => OS_OBJECT_ARRAY,
            Self::List(_) => OS_LIST,
            Self::Reference(_) => OS_REFERENCE,
            Self::Double(_) => OS_DOUBLE,
            Self::UnitFloat { .. } => OS_UNIT_FLOAT,
            Self::UnitFloats { .. } => OS_UNIT_FLOATS,
            Self::String(_) => OS_STRING,
            Self::Enumerated { .. } => OS_ENUMERATED,
            Self::Integer(_) => OS_INTEGER,
            Self::LargeInteger(_) => OS_LARGE_INTEGER,
            Self::Boolean(_) => OS_BOOLEAN,
            Self::Class { os_key, .. } | Self::RawData { os_key, .. } => *os_key,
            Self::Property { .. } => OS_PROPERTY,
            Self::EnumeratedReference { .. } => OS_ENUMERATED_REFERENCE,
            Self::Offset { .. } => OS_OFFSET,
            Self::Identifier(_) => OS_IDENTIFIER,
            Self::Index(_) => OS_INDEX,
            Self::Name { .. } => OS_NAME,
        }
    }

    /// Read the value for a given OSType (the key and OSType bytes have
    /// already been consumed by the caller).
    pub fn read(reader: &mut BeReader, os_key: [u8; 4]) -> Result<Self> {
        Self::read_with_depth(reader, os_key, 0)
    }

    fn read_with_depth(reader: &mut BeReader, os_key: [u8; 4], depth: usize) -> Result<Self> {
        check_nesting_depth(reader, depth)?;
        Ok(match os_key {
            OS_DESCRIPTOR | OS_GLOBAL_OBJECT => {
                Self::Descriptor(Descriptor::read_with_depth(reader, depth + 1)?)
            }
            OS_OBJECT_ARRAY => Self::ObjectArray(ObjectArray::read_with_depth(reader, depth + 1)?),
            OS_LIST => Self::List(read_keyless_values(reader, depth + 1)?),
            OS_REFERENCE => Self::Reference(read_keyless_values(reader, depth + 1)?),
            OS_DOUBLE => Self::Double(reader.f64()?),
            OS_UNIT_FLOAT => Self::UnitFloat {
                unit: read_os_key(reader)?,
                value: reader.f64()?,
            },
            OS_UNIT_FLOATS => {
                let unit = read_os_key(reader)?;
                let count = reader.u32()? as usize;
                let mut values = Vec::with_capacity(count.min(1024));
                for _ in 0..count {
                    values.push(reader.f64()?);
                }
                Self::UnitFloats { unit, values }
            }
            OS_STRING => Self::String(UnicodeString::read(reader, 1)?),
            OS_ENUMERATED => Self::Enumerated {
                type_id: DescriptorKey::read(reader)?,
                value: DescriptorKey::read(reader)?,
            },
            OS_INTEGER => Self::Integer(reader.i32()?),
            OS_LARGE_INTEGER => Self::LargeInteger(reader.u64()? as i64),
            OS_BOOLEAN => Self::Boolean(reader.u8()? != 0),
            OS_CLASS_1 | OS_CLASS_2 | OS_CLASS_3 => Self::Class {
                os_key,
                name: UnicodeString::read(reader, 1)?,
                class_id: DescriptorKey::read(reader)?,
            },
            OS_ALIAS | OS_RAW_DATA | OS_PATH => {
                let len = reader.u32()? as usize;
                Self::RawData {
                    os_key,
                    data: reader.take(len)?.to_vec(),
                }
            }
            OS_PROPERTY => Self::Property {
                name: UnicodeString::read(reader, 1)?,
                class_id: DescriptorKey::read(reader)?,
                key_id: DescriptorKey::read(reader)?,
            },
            OS_ENUMERATED_REFERENCE => Self::EnumeratedReference {
                name: UnicodeString::read(reader, 1)?,
                class_id: DescriptorKey::read(reader)?,
                type_id: DescriptorKey::read(reader)?,
                value: DescriptorKey::read(reader)?,
            },
            OS_OFFSET => Self::Offset {
                name: UnicodeString::read(reader, 1)?,
                class_id: DescriptorKey::read(reader)?,
                offset: reader.u32()?,
            },
            OS_IDENTIFIER => Self::Identifier(reader.i32()?),
            OS_INDEX => Self::Index(reader.i32()?),
            OS_NAME => Self::Name {
                name: UnicodeString::read(reader, 1)?,
                class_id: DescriptorKey::read(reader)?,
                value: UnicodeString::read(reader, 1)?,
            },
            other => {
                tracing::warn!(
                    "unsupported descriptor OSType {:?} at offset {}",
                    String::from_utf8_lossy(&other),
                    reader.position()
                );
                return Err(PsdError::InvalidData {
                    offset: reader.position() as u64,
                    message: "unsupported descriptor OSType",
                });
            }
        })
    }

    /// Write the value (the caller writes the key and OSType bytes).
    pub fn write(&self, writer: &mut BeWriter) -> Result<()> {
        match self {
            Self::Descriptor(descriptor) => descriptor.write(writer)?,
            Self::ObjectArray(array) => array.write(writer)?,
            Self::List(values) | Self::Reference(values) => {
                writer.u32(
                    u32::try_from(values.len()).map_err(|_| PsdError::LengthOverflow {
                        actual: values.len() as u64,
                        width: 4,
                    })?,
                );
                for value in values {
                    writer.bytes(&value.os_key());
                    value.write(writer)?;
                }
            }
            Self::Double(value) => writer.f64(*value),
            Self::UnitFloat { unit, value } => {
                writer.bytes(unit);
                writer.f64(*value);
            }
            Self::UnitFloats { unit, values } => {
                writer.bytes(unit);
                writer.u32(
                    u32::try_from(values.len()).map_err(|_| PsdError::LengthOverflow {
                        actual: values.len() as u64,
                        width: 4,
                    })?,
                );
                for value in values {
                    writer.f64(*value);
                }
            }
            Self::String(value) => value.write_verbatim(writer)?,
            Self::Enumerated { type_id, value } => {
                type_id.write(writer)?;
                value.write(writer)?;
            }
            Self::Integer(value) => writer.i32(*value),
            Self::LargeInteger(value) => writer.bytes(&value.to_be_bytes()),
            Self::Boolean(value) => writer.u8(u8::from(*value)),
            Self::Class { name, class_id, .. } => {
                name.write_verbatim(writer)?;
                class_id.write(writer)?;
            }
            Self::RawData { data, .. } => {
                writer.u32(
                    u32::try_from(data.len()).map_err(|_| PsdError::LengthOverflow {
                        actual: data.len() as u64,
                        width: 4,
                    })?,
                );
                writer.bytes(data);
            }
            Self::Property {
                name,
                class_id,
                key_id,
            } => {
                name.write_verbatim(writer)?;
                class_id.write(writer)?;
                key_id.write(writer)?;
            }
            Self::EnumeratedReference {
                name,
                class_id,
                type_id,
                value,
            } => {
                name.write_verbatim(writer)?;
                class_id.write(writer)?;
                type_id.write(writer)?;
                value.write(writer)?;
            }
            Self::Offset {
                name,
                class_id,
                offset,
            } => {
                name.write_verbatim(writer)?;
                class_id.write(writer)?;
                writer.u32(*offset);
            }
            Self::Identifier(value) | Self::Index(value) => writer.i32(*value),
            Self::Name {
                name,
                class_id,
                value,
            } => {
                name.write_verbatim(writer)?;
                class_id.write(writer)?;
                value.write_verbatim(writer)?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Typed accessors for consumers (warp/text engines)
    // ------------------------------------------------------------------

    pub fn as_descriptor(&self) -> Option<&Descriptor> {
        match self {
            Self::Descriptor(descriptor) => Some(descriptor),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[DescriptorValue]> {
        match self {
            Self::List(values) | Self::Reference(values) => Some(values),
            _ => None,
        }
    }

    pub fn as_double(&self) -> Option<f64> {
        match self {
            Self::Double(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_integer(&self) -> Option<i32> {
        match self {
            Self::Integer(value) | Self::Identifier(value) | Self::Index(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_large_integer(&self) -> Option<i64> {
        match self {
            Self::LargeInteger(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Boolean(value) => Some(*value),
            _ => None,
        }
    }

    /// The UTF-8 text of a `TEXT` value.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value.value()),
            _ => None,
        }
    }

    /// `(type_id, enumerator)` of an `enum` value.
    pub fn as_enum(&self) -> Option<(&DescriptorKey, &DescriptorKey)> {
        match self {
            Self::Enumerated { type_id, value } => Some((type_id, value)),
            _ => None,
        }
    }

    /// `(unit key, value)` of a `UntF` value.
    pub fn as_unit_float(&self) -> Option<(&[u8; 4], f64)> {
        match self {
            Self::UnitFloat { unit, value } => Some((unit, *value)),
            _ => None,
        }
    }
}

fn read_os_key(reader: &mut BeReader) -> Result<[u8; 4]> {
    let mut key = [0u8; 4];
    key.copy_from_slice(reader.take(4)?);
    Ok(key)
}

/// `u32` count followed by keyless values (lists and references).
fn read_keyless_values(reader: &mut BeReader, depth: usize) -> Result<Vec<DescriptorValue>> {
    let count = reader.u32()? as usize;
    let mut values = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let os_key = read_os_key(reader)?;
        values.push(DescriptorValue::read_with_depth(reader, os_key, depth)?);
    }
    Ok(values)
}

fn read_descriptor_with_integer_spans(
    reader: &mut BeReader,
    keys: &[&[u8]],
    spans: &mut Vec<DescriptorIntegerSpan>,
    depth: usize,
) -> Result<Descriptor> {
    check_nesting_depth(reader, depth)?;
    let name = UnicodeString::read(reader, 1)?;
    let class_id = DescriptorKey::read(reader)?;
    let count = reader.u32()? as usize;
    let mut items = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        items.push(read_item_with_integer_spans(reader, keys, spans, depth)?);
    }
    Ok(Descriptor {
        name,
        class_id,
        items,
    })
}

fn read_object_array_with_integer_spans(
    reader: &mut BeReader,
    keys: &[&[u8]],
    spans: &mut Vec<DescriptorIntegerSpan>,
    depth: usize,
) -> Result<ObjectArray> {
    check_nesting_depth(reader, depth)?;
    let items_count = reader.u32()?;
    let name = UnicodeString::read(reader, 1)?;
    let class_id = DescriptorKey::read(reader)?;
    let count = reader.u32()? as usize;
    let mut items = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        items.push(read_item_with_integer_spans(reader, keys, spans, depth)?);
    }
    Ok(ObjectArray {
        items_count,
        name,
        class_id,
        items,
    })
}

fn read_keyless_values_with_integer_spans(
    reader: &mut BeReader,
    keys: &[&[u8]],
    spans: &mut Vec<DescriptorIntegerSpan>,
    depth: usize,
) -> Result<Vec<DescriptorValue>> {
    let count = reader.u32()? as usize;
    let mut values = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let os_key = read_os_key(reader)?;
        values.push(read_value_with_integer_spans(
            reader,
            os_key,
            &[],
            keys,
            spans,
            depth,
        )?);
    }
    Ok(values)
}

fn read_item_with_integer_spans(
    reader: &mut BeReader,
    keys: &[&[u8]],
    spans: &mut Vec<DescriptorIntegerSpan>,
    depth: usize,
) -> Result<DescriptorItem> {
    let key = DescriptorKey::read(reader)?;
    let os_key = read_os_key(reader)?;
    let value_start = reader.position();
    let value = read_value_with_integer_spans(reader, os_key, key.as_bytes(), keys, spans, depth)?;
    if os_key == OS_INTEGER && keys.iter().any(|tracked| *tracked == key.as_bytes()) {
        if let DescriptorValue::Integer(value) = value {
            spans.push(DescriptorIntegerSpan {
                key: key.as_bytes().to_vec(),
                range: value_start..reader.position(),
                value,
            });
            return Ok(DescriptorItem {
                key,
                value: DescriptorValue::Integer(value),
            });
        }
    }
    Ok(DescriptorItem { key, value })
}

fn read_value_with_integer_spans(
    reader: &mut BeReader,
    os_key: [u8; 4],
    _item_key: &[u8],
    keys: &[&[u8]],
    spans: &mut Vec<DescriptorIntegerSpan>,
    depth: usize,
) -> Result<DescriptorValue> {
    Ok(match os_key {
        OS_DESCRIPTOR | OS_GLOBAL_OBJECT => DescriptorValue::Descriptor(
            read_descriptor_with_integer_spans(reader, keys, spans, depth + 1)?,
        ),
        OS_OBJECT_ARRAY => DescriptorValue::ObjectArray(read_object_array_with_integer_spans(
            reader,
            keys,
            spans,
            depth + 1,
        )?),
        OS_LIST => DescriptorValue::List(read_keyless_values_with_integer_spans(
            reader,
            keys,
            spans,
            depth + 1,
        )?),
        OS_REFERENCE => DescriptorValue::Reference(read_keyless_values_with_integer_spans(
            reader,
            keys,
            spans,
            depth + 1,
        )?),
        _ => DescriptorValue::read_with_depth(reader, os_key, depth)?,
    })
}

/// A descriptor: class name, class id, and ordered items.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Descriptor {
    pub name: UnicodeString,
    pub class_id: DescriptorKey,
    pub items: Vec<DescriptorItem>,
}

impl Descriptor {
    pub fn read(reader: &mut BeReader) -> Result<Self> {
        Self::read_tracking_raw_data(reader, None).map(|(descriptor, _)| descriptor)
    }

    fn read_with_depth(reader: &mut BeReader, depth: usize) -> Result<Self> {
        Self::read_tracking_payloads_with_depth(reader, None, None, depth)
            .map(|(descriptor, _)| descriptor)
    }

    /// Parse a descriptor tree and record direct or nested `long` values whose
    /// item keys match `keys`. Spans cover the four-byte integer contents and
    /// are relative to the reader's underlying slice.
    pub fn read_tracking_integer_items(
        reader: &mut BeReader,
        keys: &[&[u8]],
    ) -> Result<(Self, Vec<DescriptorIntegerSpan>)> {
        let mut spans = Vec::new();
        let descriptor = read_descriptor_with_integer_spans(reader, keys, &mut spans, 0)?;
        Ok((descriptor, spans))
    }

    /// Parse a descriptor while recording the payload range of the first
    /// direct raw-data item with `raw_key` (for example, TySh's `EngineData`).
    /// The range is relative to the reader's underlying byte slice and omits
    /// the raw-data length prefix. This lets callers patch opaque descriptor
    /// payloads without rebuilding unrelated descriptor bytes.
    pub fn read_tracking_raw_data(
        reader: &mut BeReader,
        raw_key: Option<&[u8]>,
    ) -> Result<(Self, Option<std::ops::Range<usize>>)> {
        Self::read_tracking_payloads(reader, raw_key, None)
            .map(|(descriptor, spans)| (descriptor, spans.raw_data))
    }

    /// Parse a descriptor while tracking one direct raw-data payload and one
    /// direct `TEXT` Unicode-string value. Returned ranges are relative to the
    /// reader's underlying slice; the string range includes its four-byte
    /// code-unit count and UTF-16BE units.
    pub fn read_tracking_payloads(
        reader: &mut BeReader,
        raw_key: Option<&[u8]>,
        string_key: Option<&[u8]>,
    ) -> Result<(Self, DescriptorPayloadSpans)> {
        Self::read_tracking_payloads_with_depth(reader, raw_key, string_key, 0)
    }

    fn read_tracking_payloads_with_depth(
        reader: &mut BeReader,
        raw_key: Option<&[u8]>,
        string_key: Option<&[u8]>,
        depth: usize,
    ) -> Result<(Self, DescriptorPayloadSpans)> {
        check_nesting_depth(reader, depth)?;
        let name = UnicodeString::read(reader, 1)?;
        let class_id = DescriptorKey::read(reader)?;
        let count = reader.u32()? as usize;
        let mut items = Vec::with_capacity(count.min(1024));
        let mut raw_data_range = None;
        let mut unicode_string_range = None;
        let mut raw_key_seen = false;
        let mut string_key_seen = false;
        for _ in 0..count {
            let key = DescriptorKey::read(reader)?;
            let os_key = read_os_key(reader)?;
            let matches_raw_key = raw_key.is_some_and(|raw_key| key.as_bytes() == raw_key);
            let track_raw_data = matches_raw_key && !raw_key_seen && os_key == OS_RAW_DATA;
            if matches_raw_key {
                raw_key_seen = true;
            }
            let matches_string_key =
                string_key.is_some_and(|string_key| key.as_bytes() == string_key);
            let track_unicode_string =
                matches_string_key && !string_key_seen && os_key == OS_STRING;
            if matches_string_key {
                string_key_seen = true;
            }
            let value = if track_raw_data {
                let length = reader.u32()? as usize;
                let start = reader.position();
                let data = reader.take(length)?.to_vec();
                raw_data_range = Some(start..reader.position());
                DescriptorValue::RawData { os_key, data }
            } else {
                let value_start = reader.position();
                let value = DescriptorValue::read_with_depth(reader, os_key, depth)?;
                if track_unicode_string {
                    unicode_string_range = Some(value_start..reader.position());
                }
                value
            };
            items.push(DescriptorItem { key, value });
        }
        Ok((
            Self {
                name,
                class_id,
                items,
            },
            DescriptorPayloadSpans {
                raw_data: raw_data_range,
                unicode_string: unicode_string_range,
            },
        ))
    }

    pub fn write(&self, writer: &mut BeWriter) -> Result<()> {
        self.name.write_verbatim(writer)?;
        self.class_id.write(writer)?;
        writer.u32(
            u32::try_from(self.items.len()).map_err(|_| PsdError::LengthOverflow {
                actual: self.items.len() as u64,
                width: 4,
            })?,
        );
        for item in &self.items {
            write_item(writer, item)?;
        }
        Ok(())
    }

    /// First value stored under `key`.
    pub fn get(&self, key: &str) -> Option<&DescriptorValue> {
        self.items
            .iter()
            .find(|item| item.key.as_bytes() == key.as_bytes())
            .map(|item| &item.value)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut DescriptorValue> {
        self.items
            .iter_mut()
            .find(|item| item.key.as_bytes() == key.as_bytes())
            .map(|item| &mut item.value)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Insert a value, replacing any existing item with the same key.
    pub fn insert(&mut self, key: impl Into<String>, value: DescriptorValue) {
        let key = DescriptorKey::new(key);
        // Match by encoded bytes, not the implicit-length flag: replacing a
        // value must retain the key representation that was read from disk.
        if let Some(item) = self
            .items
            .iter_mut()
            .find(|item| item.key.as_bytes() == key.as_bytes())
        {
            item.value = value;
        } else {
            self.items.push(DescriptorItem { key, value });
        }
    }
}

/// `ObAr`: an object array — a descriptor with a leading item count.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ObjectArray {
    pub items_count: u32,
    pub name: UnicodeString,
    pub class_id: DescriptorKey,
    pub items: Vec<DescriptorItem>,
}

impl ObjectArray {
    pub fn read(reader: &mut BeReader) -> Result<Self> {
        Self::read_with_depth(reader, 0)
    }

    fn read_with_depth(reader: &mut BeReader, depth: usize) -> Result<Self> {
        check_nesting_depth(reader, depth)?;
        let items_count = reader.u32()?;
        let name = UnicodeString::read(reader, 1)?;
        let class_id = DescriptorKey::read(reader)?;
        let count = reader.u32()? as usize;
        let mut items = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            items.push(read_item_with_depth(reader, depth)?);
        }
        Ok(Self {
            items_count,
            name,
            class_id,
            items,
        })
    }

    pub fn write(&self, writer: &mut BeWriter) -> Result<()> {
        writer.u32(self.items_count);
        self.name.write_verbatim(writer)?;
        self.class_id.write(writer)?;
        writer.u32(
            u32::try_from(self.items.len()).map_err(|_| PsdError::LengthOverflow {
                actual: self.items.len() as u64,
                width: 4,
            })?,
        );
        for item in &self.items {
            write_item(writer, item)?;
        }
        Ok(())
    }
}

/// Read one `key + OSType + value` item.
pub fn read_item(reader: &mut BeReader) -> Result<DescriptorItem> {
    read_item_with_depth(reader, 0)
}

fn read_item_with_depth(reader: &mut BeReader, depth: usize) -> Result<DescriptorItem> {
    let key = DescriptorKey::read(reader)?;
    let os_key = read_os_key(reader)?;
    let value = DescriptorValue::read_with_depth(reader, os_key, depth)?;
    Ok(DescriptorItem { key, value })
}

/// Write one `key + OSType + value` item.
pub fn write_item(writer: &mut BeWriter, item: &DescriptorItem) -> Result<()> {
    item.key.write(writer)?;
    writer.bytes(&item.value.os_key());
    item.value.write(writer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(descriptor: &Descriptor) -> Descriptor {
        let mut writer = BeWriter::new();
        descriptor.write(&mut writer).unwrap();
        let mut reader = BeReader::new(writer.as_slice());
        let back = Descriptor::read(&mut reader).unwrap();
        assert!(reader.is_empty());
        back
    }

    #[test]
    fn descriptor_with_scalar_items_round_trips() {
        let mut descriptor = Descriptor {
            name: UnicodeString::new("null", 1).unwrap(),
            class_id: DescriptorKey::new("null"),
            items: Vec::new(),
        };
        descriptor.insert(
            "Nm  ",
            DescriptorValue::String(UnicodeString::new("Hello", 1).unwrap()),
        );
        descriptor.insert(
            "Opct",
            DescriptorValue::UnitFloat {
                unit: *b"#Prc",
                value: 100.0,
            },
        );
        descriptor.insert("Vrsn", DescriptorValue::Integer(16));
        descriptor.insert("Bln ", DescriptorValue::Boolean(true));
        descriptor.insert("Dbl ", DescriptorValue::Double(-2.5));
        descriptor.insert("Big ", DescriptorValue::LargeInteger(1 << 40));
        descriptor.insert(
            "Enm ",
            DescriptorValue::Enumerated {
                type_id: DescriptorKey::new("warpStyle"),
                value: DescriptorKey::new("warpRise"),
            },
        );

        let back = round_trip(&descriptor);
        // UnicodeString::write appends a null terminator, so compare values.
        assert_eq!(back.name.value(), "null");
        assert_eq!(back.class_id, descriptor.class_id);
        assert_eq!(back.items.len(), descriptor.items.len());
        assert_eq!(back.get("Vrsn").unwrap().as_integer(), Some(16));
        assert_eq!(back.get("Bln ").unwrap().as_bool(), Some(true));
        assert_eq!(back.get("Nm  ").unwrap().as_str(), Some("Hello"));
        let (type_id, value) = back.get("Enm ").unwrap().as_enum().unwrap();
        assert_eq!(type_id.as_str(), "warpStyle");
        assert_eq!(value.as_str(), "warpRise");
        assert_eq!(
            back.get("Opct").unwrap().as_unit_float(),
            Some((b"#Prc", 100.0))
        );
    }

    #[test]
    fn inserting_replaces_a_known_four_byte_key_without_changing_its_encoding() {
        let key = DescriptorKey {
            bytes: b"Trnf".to_vec(),
            implicit_len: true,
        };
        let mut descriptor = Descriptor {
            name: UnicodeString::new("null", 1).unwrap(),
            class_id: DescriptorKey::new("null"),
            items: vec![DescriptorItem {
                key,
                value: DescriptorValue::Double(1.0),
            }],
        };

        descriptor.insert("Trnf", DescriptorValue::Double(2.0));

        assert_eq!(descriptor.items.len(), 1);
        assert!(descriptor.items[0].key.implicit_len);
        assert_eq!(descriptor.get("Trnf").unwrap().as_double(), Some(2.0));
        let back = round_trip(&descriptor);
        assert!(back.items[0].key.implicit_len);
        assert_eq!(back.get("Trnf").unwrap().as_double(), Some(2.0));
    }

    #[test]
    fn changing_a_key_can_keep_its_implicit_encoding() {
        let key = DescriptorKey {
            bytes: b"Hrzn".to_vec(),
            implicit_len: true,
        };
        let changed = key.replace_text_preserving_encoding("Vrtc").unwrap();
        assert_eq!(changed.as_bytes(), b"Vrtc");
        assert!(changed.uses_implicit_length());
        assert!(key.replace_text_preserving_encoding("warpStyle").is_err());
    }

    #[test]
    fn nested_descriptors_lists_and_arrays_round_trip() {
        let mut inner = Descriptor {
            name: UnicodeString::new("warp", 1).unwrap(),
            class_id: DescriptorKey::new("warp"),
            items: Vec::new(),
        };
        inner.insert(
            "warpStyle",
            DescriptorValue::Enumerated {
                type_id: DescriptorKey::new("warpStyle"),
                value: DescriptorKey::new("warpCustom"),
            },
        );
        inner.insert(
            "warpValues",
            DescriptorValue::List(vec![
                DescriptorValue::Double(1.0),
                DescriptorValue::Double(2.0),
            ]),
        );

        let mut descriptor = Descriptor {
            name: UnicodeString::new("", 1).unwrap(),
            class_id: DescriptorKey::new("null"),
            items: Vec::new(),
        };
        descriptor.insert("warp", DescriptorValue::Descriptor(inner.clone()));
        descriptor.insert(
            "refs",
            DescriptorValue::Reference(vec![DescriptorValue::Integer(7)]),
        );

        let back = round_trip(&descriptor);
        assert_eq!(back.items.len(), descriptor.items.len());
        let warp = back.get("warp").unwrap().as_descriptor().unwrap();
        assert_eq!(warp.class_id.as_str(), "warp");
        assert_eq!(
            warp.get("warpValues").unwrap().as_list().map(<[_]>::len),
            Some(2)
        );
        assert_eq!(back.get("refs").unwrap().as_list().map(<[_]>::len), Some(1));
    }

    #[test]
    fn key_encoding_is_preserved() {
        // A key read with a zero length must be written back with one.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0u32.to_be_bytes()); // implicit-length key
        bytes.extend_from_slice(b"Vrsn");
        let mut reader = BeReader::new(&bytes);
        let key = DescriptorKey::read(&mut reader).unwrap();
        assert_eq!(key.as_str(), "Vrsn");
        let mut writer = BeWriter::new();
        key.write(&mut writer).unwrap();
        assert_eq!(writer.as_slice(), &bytes);

        // Explicit 4-byte keys keep their explicit length.
        let key = DescriptorKey::new("Vrsn");
        let mut writer = BeWriter::new();
        key.write(&mut writer).unwrap();
        assert_eq!(&writer.as_slice()[..4], &4u32.to_be_bytes());
        assert_eq!(&writer.as_slice()[4..], b"Vrsn");
    }

    #[test]
    fn unknown_ostype_is_an_error() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&4u32.to_be_bytes());
        bytes.extend_from_slice(b"key ");
        bytes.extend_from_slice(b"zzzz");
        let mut reader = BeReader::new(&bytes);
        assert!(read_item(&mut reader).is_err());
    }

    #[test]
    fn deeply_nested_descriptors_are_a_typed_error_not_a_stack_overflow() {
        // Build a descriptor that nests `Objc` values deeper than the safety
        // cap and confirm the parse fails with a typed error. Without the cap
        // this payload recurses until the stack overflows.
        let mut bytes = Vec::new();
        // Each level: empty name, zero-length class id (whose 4 implicit
        // bytes follow), one item `k` holding a nested `Objc` value.
        for _ in 0..(MAX_NESTING_DEPTH + 2) {
            bytes.extend_from_slice(&0u32.to_be_bytes()); // name
            bytes.extend_from_slice(&0u32.to_be_bytes()); // class id: zero length
            bytes.extend_from_slice(b"null"); // ... then the 4 implicit bytes
            bytes.extend_from_slice(&1u32.to_be_bytes()); // one item
            bytes.extend_from_slice(&1u32.to_be_bytes()); // key length
            bytes.extend_from_slice(b"k");
            bytes.extend_from_slice(&OS_DESCRIPTOR);
        }
        // Innermost descriptor: empty name/class id, no items.
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(b"null");
        bytes.extend_from_slice(&0u32.to_be_bytes());
        let mut reader = BeReader::new(&bytes);
        assert!(matches!(
            Descriptor::read(&mut reader),
            Err(PsdError::InvalidData { .. })
        ));
    }
}
