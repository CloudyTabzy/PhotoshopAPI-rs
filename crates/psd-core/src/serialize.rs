//! `serde::Serialize` views of descriptors and EngineData (feature `serde`).
//!
//! Upstream offers `Descriptor::to_json()` (nlohmann `ordered_json`) to
//! inspect descriptor trees; the optional `serde` feature here lets callers
//! pick any serde format. The output is a
//! readable view in source order, not a round-trippable encoding: the binary
//! types remain the source of truth.
//!
//! - A [`Descriptor`] serializes as `{"name", "class", "items": {key: value}}`.
//! - A [`DescriptorValue`] serializes as a one-entry map keyed by its OSType
//!   (`{"doub": 1.5}`, `{"enum": {"type": "Ornt", "value": "Hrzn"}}`,
//!   `{"UntF": {"unit": "#Pnt", "value": 0.0}}`, …).
//! - An [`EngineValue`] serializes as the natural JSON shape: dictionaries as
//!   maps, arrays as sequences, numbers as integers or floats, names as
//!   `"/Name"`, and UTF-16BE literal strings decoded to text (other literal
//!   bytes as byte sequences).

use serde::ser::{Serialize, SerializeMap, SerializeStruct, Serializer};

use crate::descriptor::{Descriptor, DescriptorItem, DescriptorKey, DescriptorValue, ObjectArray};
use crate::engine_data::{self, EngineValue, EngineValueKind};
use crate::strings::UnicodeString;

impl Serialize for DescriptorKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_str())
    }
}

impl Serialize for UnicodeString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.value())
    }
}

/// Descriptor items in source order (keys may repeat, so this is a map
/// written entry by entry rather than a collected map).
struct Items<'a>(&'a [DescriptorItem]);

impl Serialize for Items<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for item in self.0 {
            map.serialize_entry(&item.key, &item.value)?;
        }
        map.end()
    }
}

impl Serialize for Descriptor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("Descriptor", 3)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("class", &self.class_id)?;
        state.serialize_field("items", &Items(&self.items))?;
        state.end()
    }
}

impl Serialize for ObjectArray {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ObjectArray", 4)?;
        state.serialize_field("count", &self.items_count)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("class", &self.class_id)?;
        state.serialize_field("items", &Items(&self.items))?;
        state.end()
    }
}

/// The leaf types of the struct-like OSType fields.
enum Primitive<'a> {
    Str(&'a str),
    F64(f64),
    U32(u32),
    Key(&'a DescriptorKey),
    Unicode(&'a UnicodeString),
}

impl Serialize for Primitive<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Str(value) => serializer.serialize_str(value),
            Self::F64(value) => serializer.serialize_f64(*value),
            Self::U32(value) => serializer.serialize_u32(*value),
            Self::Key(value) => value.serialize(serializer),
            Self::Unicode(value) => value.serialize(serializer),
        }
    }
}

/// One-entry map `{tag: value}`.
fn tagged<S: Serializer, V: Serialize + ?Sized>(
    serializer: S,
    tag: &str,
    value: &V,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(Some(1))?;
    map.serialize_entry(tag, value)?;
    map.end()
}

/// `{tag: {field: value, …}}` for the struct-like OSTypes.
fn tagged_fields<S: Serializer>(
    serializer: S,
    tag: &str,
    fields: &[(&str, Primitive<'_>)],
) -> Result<S::Ok, S::Error> {
    struct Record<'a>(&'a [(&'a str, Primitive<'a>)]);
    impl Serialize for Record<'_> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut map = serializer.serialize_map(Some(self.0.len()))?;
            for (key, value) in self.0 {
                map.serialize_entry(key, value)?;
            }
            map.end()
        }
    }
    tagged(serializer, tag, &Record(fields))
}

fn os_tag(value: &DescriptorValue) -> String {
    String::from_utf8_lossy(&value.os_key()).into_owned()
}

impl Serialize for DescriptorValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let tag = os_tag(self);
        match self {
            Self::Descriptor(descriptor) => tagged(serializer, &tag, descriptor),
            Self::ObjectArray(array) => tagged(serializer, &tag, array),
            Self::List(items) | Self::Reference(items) => tagged(serializer, &tag, items),
            Self::Double(value) => tagged(serializer, &tag, value),
            Self::UnitFloat { unit, value } => tagged_fields(
                serializer,
                &tag,
                &[
                    ("unit", Primitive::Str(&String::from_utf8_lossy(unit))),
                    ("value", Primitive::F64(*value)),
                ],
            ),
            Self::UnitFloats { unit, values } => {
                struct Floats<'a>(&'a str, &'a [f64]);
                impl Serialize for Floats<'_> {
                    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                        let mut map = serializer.serialize_map(Some(2))?;
                        map.serialize_entry("unit", self.0)?;
                        map.serialize_entry("values", self.1)?;
                        map.end()
                    }
                }
                tagged(
                    serializer,
                    &tag,
                    &Floats(&String::from_utf8_lossy(unit), values),
                )
            }
            Self::String(value) => tagged(serializer, &tag, value),
            Self::Enumerated { type_id, value } => tagged_fields(
                serializer,
                &tag,
                &[
                    ("type", Primitive::Key(type_id)),
                    ("value", Primitive::Key(value)),
                ],
            ),
            Self::Integer(value) | Self::Identifier(value) | Self::Index(value) => {
                tagged(serializer, &tag, value)
            }
            Self::LargeInteger(value) => tagged(serializer, &tag, value),
            Self::Boolean(value) => tagged(serializer, &tag, value),
            Self::Class { name, class_id, .. } => tagged_fields(
                serializer,
                &tag,
                &[
                    ("name", Primitive::Unicode(name)),
                    ("class", Primitive::Key(class_id)),
                ],
            ),
            Self::RawData { data, .. } => {
                struct Bytes<'a>(&'a [u8]);
                impl Serialize for Bytes<'_> {
                    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                        serializer.serialize_bytes(self.0)
                    }
                }
                tagged(serializer, &tag, &Bytes(data))
            }
            Self::Property {
                name,
                class_id,
                key_id,
            } => tagged_fields(
                serializer,
                &tag,
                &[
                    ("name", Primitive::Unicode(name)),
                    ("class", Primitive::Key(class_id)),
                    ("key", Primitive::Key(key_id)),
                ],
            ),
            Self::EnumeratedReference {
                name,
                class_id,
                type_id,
                value,
            } => tagged_fields(
                serializer,
                &tag,
                &[
                    ("name", Primitive::Unicode(name)),
                    ("class", Primitive::Key(class_id)),
                    ("type", Primitive::Key(type_id)),
                    ("value", Primitive::Key(value)),
                ],
            ),
            Self::Offset {
                name,
                class_id,
                offset,
            } => tagged_fields(
                serializer,
                &tag,
                &[
                    ("name", Primitive::Unicode(name)),
                    ("class", Primitive::Key(class_id)),
                    ("offset", Primitive::U32(*offset)),
                ],
            ),
            Self::Name {
                name,
                class_id,
                value,
            } => tagged_fields(
                serializer,
                &tag,
                &[
                    ("name", Primitive::Unicode(name)),
                    ("class", Primitive::Key(class_id)),
                    ("value", Primitive::Unicode(value)),
                ],
            ),
        }
    }
}

impl Serialize for EngineValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.kind {
            EngineValueKind::Dictionary(items) => {
                let mut map = serializer.serialize_map(Some(items.len()))?;
                for (key, value) in items {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
            EngineValueKind::Array(items) => items.serialize(serializer),
            EngineValueKind::Number(number) => match number.integer {
                Some(integer) => serializer.serialize_i64(integer),
                None => serializer.serialize_f64(number.value),
            },
            EngineValueKind::Boolean(value) => serializer.serialize_bool(*value),
            EngineValueKind::Name(name) => serializer.serialize_str(&format!("/{name}")),
            EngineValueKind::LiteralString(text) | EngineValueKind::LiteralStringValue(text) => {
                serializer.serialize_str(text)
            }
            EngineValueKind::LiteralStringBytes(bytes) => {
                match engine_data::decode_utf16be_literal(bytes) {
                    Ok(text) => serializer.serialize_str(&text),
                    Err(_) => serializer.serialize_bytes(bytes),
                }
            }
            EngineValueKind::HexString(hex) => serializer.serialize_str(&format!("<{hex}>")),
            EngineValueKind::Identifier(identifier) => serializer.serialize_str(identifier),
        }
    }
}
