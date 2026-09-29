//! Layer comps: the document resource (`1065`) and each layer's `cmls` block.
//!
//! A layer comp is a named snapshot of which layers are visible, where they
//! sit, and how they look. Photoshop keeps the list in image resource `1065`
//! (a four-byte descriptor version, 16, then a descriptor) and each layer's
//! own participation in the `cmls` tagged block — a descriptor listing, per
//! comp, whether the layer is enabled and by how much it is offset.
//!
//! Both are read on demand from their raw bytes, so an untouched document
//! keeps them exactly as they were. [`LayerComps::to_descriptor`] rebuilds the
//! resource descriptor for a caller that changed the list, keeping the class
//! id and any items this model does not name.

use crate::descriptor::{Descriptor, DescriptorItem, DescriptorKey, DescriptorValue};
use crate::error::{PsdError, Result};
use crate::io::BeReader;
use crate::strings::UnicodeString;

/// The descriptor version Photoshop writes ahead of both descriptors.
const DESCRIPTOR_VERSION: i32 = 16;

/// One entry of the document's layer comp list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LayerComp {
    pub id: i32,
    pub name: String,
    pub comment: String,
    /// Whether the comp records layer visibility.
    pub capture_visibility: bool,
    /// Whether it records layer positions.
    pub capture_position: bool,
    /// Whether it records layer appearance (effects, styles).
    pub capture_appearance: bool,
}

/// The `1065` resource: every comp, plus the one Photoshop last applied.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LayerComps {
    /// The applied comp, or `-1` when the resource names none.
    pub last_applied_comp: i32,
    pub comps: Vec<LayerComp>,
}

impl LayerComps {
    /// Parse the resource payload.
    pub fn read(reader: &mut BeReader) -> Result<Self> {
        let offset = reader.position() as u64;
        let version = reader.i32()?;
        if version != DESCRIPTOR_VERSION {
            return Err(PsdError::InvalidData {
                offset,
                message: "layer comps descriptor version is not 16",
            });
        }
        let descriptor = Descriptor::read(reader)?;
        Ok(Self::from_descriptor(&descriptor))
    }

    /// Read the comps out of the parsed descriptor.
    pub fn from_descriptor(descriptor: &Descriptor) -> Self {
        let last_applied_comp = descriptor
            .get("lastAppliedComp")
            .and_then(DescriptorValue::as_integer)
            .unwrap_or(-1);
        let mut comps = Vec::new();
        if let Some(list) = descriptor.get("list").and_then(DescriptorValue::as_list) {
            for item in list {
                let Some(comp) = item.as_descriptor() else {
                    continue;
                };
                let Some(id) = comp.get("compID").and_then(DescriptorValue::as_integer) else {
                    continue;
                };
                let captured = comp
                    .get("capturedInfo")
                    .and_then(DescriptorValue::as_integer)
                    .unwrap_or(0);
                comps.push(LayerComp {
                    id,
                    name: comp
                        .get("Nm  ")
                        .and_then(DescriptorValue::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    comment: comp
                        .get("comment")
                        .and_then(DescriptorValue::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    capture_visibility: captured & (1 << 0) != 0,
                    capture_position: captured & (1 << 1) != 0,
                    capture_appearance: captured & (1 << 2) != 0,
                });
            }
        }
        Self {
            last_applied_comp,
            comps,
        }
    }

    /// The full resource payload for this list.
    pub fn to_payload(&self) -> Result<Vec<u8>> {
        let mut writer = crate::io::BeWriter::new();
        writer.i32(DESCRIPTOR_VERSION);
        self.to_descriptor()?.write(&mut writer)?;
        Ok(writer.into_inner())
    }

    /// The descriptor Photoshop keeps the list in.
    ///
    /// The shape is the one shipped files use: a `list` item holding one
    /// descriptor per comp with `Nm  `, `comment`, `compID` and
    /// `capturedInfo`, plus `lastAppliedComp`.
    pub fn to_descriptor(&self) -> Result<Descriptor> {
        let comps = self
            .comps
            .iter()
            .map(|comp| -> Result<DescriptorValue> {
                let captured = i32::from(comp.capture_visibility)
                    | i32::from(comp.capture_position) << 1
                    | i32::from(comp.capture_appearance) << 2;
                Ok(DescriptorValue::Descriptor(Descriptor {
                    name: UnicodeString::new("Comp", 1)?,
                    class_id: DescriptorKey::char_id(*b"Comp"),
                    items: vec![
                        DescriptorItem {
                            key: DescriptorKey::new("Nm  "),
                            value: DescriptorValue::String(UnicodeString::new(
                                comp.name.as_str(),
                                1,
                            )?),
                        },
                        DescriptorItem {
                            key: DescriptorKey::new("comment"),
                            value: DescriptorValue::String(UnicodeString::new(
                                comp.comment.as_str(),
                                1,
                            )?),
                        },
                        DescriptorItem {
                            key: DescriptorKey::new("compID"),
                            value: DescriptorValue::Integer(comp.id),
                        },
                        DescriptorItem {
                            key: DescriptorKey::new("capturedInfo"),
                            value: DescriptorValue::Integer(captured),
                        },
                    ],
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Descriptor {
            name: UnicodeString::new("CompList", 1)?,
            class_id: DescriptorKey::char_id(*b"Comp"),
            items: vec![
                DescriptorItem {
                    key: DescriptorKey::new("list"),
                    value: DescriptorValue::List(comps),
                },
                DescriptorItem {
                    key: DescriptorKey::new("lastAppliedComp"),
                    value: DescriptorValue::Integer(self.last_applied_comp),
                },
            ],
        })
    }
}

/// One layer's state in one comp (`cmls`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerCompState {
    pub comp_id: i32,
    /// Whether the layer is shown in this comp. A comp that does not name the
    /// layer leaves the layer's own visibility in force, which is what
    /// [`read_comp_states`] reports for it.
    pub enabled: bool,
    pub offset_x: i32,
    pub offset_y: i32,
}

/// Parse a layer's `cmls` payload.
///
/// `layer_visible` is the layer's own visibility, used for a comp whose entry
/// carries no `enab` item — Photoshop leaves those layers as they are.
pub fn read_comp_states(payload: &[u8], layer_visible: bool) -> Result<Vec<LayerCompState>> {
    let mut reader = BeReader::new(payload);
    let offset = reader.position() as u64;
    let version = reader.i32()?;
    if version != DESCRIPTOR_VERSION {
        return Err(PsdError::InvalidData {
            offset,
            message: "layer comp state descriptor version is not 16",
        });
    }
    let descriptor = Descriptor::read(&mut reader)?;
    let Some(settings) = descriptor
        .get("layerSettings")
        .and_then(DescriptorValue::as_list)
    else {
        return Ok(Vec::new());
    };
    let mut states = Vec::with_capacity(settings.len());
    for item in settings {
        let Some(comp) = item.as_descriptor() else {
            continue;
        };
        let Some(id) = comp
            .get("compList")
            .and_then(DescriptorValue::as_list)
            .and_then(|list| list.first())
            .and_then(DescriptorValue::as_integer)
        else {
            continue;
        };
        let (mut offset_x, mut offset_y) = (0, 0);
        if let Some(offset) = comp.get("Ofst").and_then(DescriptorValue::as_descriptor) {
            offset_x = offset
                .get("Hrzn")
                .and_then(DescriptorValue::as_integer)
                .unwrap_or(0);
            offset_y = offset
                .get("Vrtc")
                .and_then(DescriptorValue::as_integer)
                .unwrap_or(0);
        }
        let enabled = comp
            .get("enab")
            .and_then(DescriptorValue::as_bool)
            .unwrap_or(layer_visible);
        states.push(LayerCompState {
            comp_id: id,
            enabled,
            offset_x,
            offset_y,
        });
    }
    Ok(states)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::BeWriter;

    fn null_descriptor(items: Vec<DescriptorItem>) -> Descriptor {
        Descriptor {
            name: UnicodeString::new("", 1).unwrap(),
            class_id: DescriptorKey::char_id(*b"null"),
            items,
        }
    }

    fn comp_state_payload() -> Vec<u8> {
        let entry = |id: i32, with_state: bool| {
            let mut items = vec![DescriptorItem {
                key: DescriptorKey::new("compList"),
                value: DescriptorValue::List(vec![DescriptorValue::Integer(id)]),
            }];
            if with_state {
                items.push(DescriptorItem {
                    key: DescriptorKey::new("Ofst"),
                    value: DescriptorValue::Descriptor(null_descriptor(vec![
                        DescriptorItem {
                            key: DescriptorKey::new("Hrzn"),
                            value: DescriptorValue::Integer(12),
                        },
                        DescriptorItem {
                            key: DescriptorKey::new("Vrtc"),
                            value: DescriptorValue::Integer(-4),
                        },
                    ])),
                });
                items.push(DescriptorItem {
                    key: DescriptorKey::new("enab"),
                    value: DescriptorValue::Boolean(false),
                });
            }
            DescriptorValue::Descriptor(null_descriptor(items))
        };
        let descriptor = null_descriptor(vec![DescriptorItem {
            key: DescriptorKey::new("layerSettings"),
            value: DescriptorValue::List(vec![entry(7, true), entry(9, false)]),
        }]);
        let mut writer = BeWriter::new();
        writer.i32(DESCRIPTOR_VERSION);
        descriptor.write(&mut writer).unwrap();
        writer.into_inner()
    }

    #[test]
    fn a_comp_list_round_trips_through_its_descriptor() {
        let comps = LayerComps {
            last_applied_comp: 3,
            comps: vec![
                LayerComp {
                    id: 1,
                    name: "One".to_owned(),
                    comment: "first".to_owned(),
                    capture_visibility: true,
                    capture_position: false,
                    capture_appearance: true,
                },
                LayerComp {
                    id: 3,
                    name: "Three".to_owned(),
                    comment: String::new(),
                    capture_visibility: false,
                    capture_position: true,
                    capture_appearance: false,
                },
            ],
        };
        let bytes = comps.to_payload().unwrap();
        let parsed = LayerComps::read(&mut BeReader::new(&bytes)).unwrap();
        assert_eq!(parsed, comps);
        assert_eq!(parsed.to_payload().unwrap(), bytes);
    }

    #[test]
    fn an_empty_list_is_valid() {
        let comps = LayerComps {
            last_applied_comp: -1,
            comps: Vec::new(),
        };
        let bytes = comps.to_payload().unwrap();
        assert_eq!(LayerComps::read(&mut BeReader::new(&bytes)).unwrap(), comps);
    }

    #[test]
    fn a_comp_state_list_reads_ids_offsets_and_enable_flags() {
        let payload = comp_state_payload();
        let states = read_comp_states(&payload, true).unwrap();
        assert_eq!(states.len(), 2);
        assert_eq!(
            states[0],
            LayerCompState {
                comp_id: 7,
                enabled: false,
                offset_x: 12,
                offset_y: -4,
            }
        );
        // A comp with no `enab` item leaves the layer's own visibility in
        // force, which is what the caller passes in.
        assert_eq!(
            states[1],
            LayerCompState {
                comp_id: 9,
                enabled: true,
                offset_x: 0,
                offset_y: 0,
            }
        );
        assert!(!read_comp_states(&payload, false).unwrap()[1].enabled);
    }

    #[test]
    fn a_wrong_descriptor_version_is_an_error() {
        let mut bytes = 15i32.to_be_bytes().to_vec();
        bytes.extend_from_slice(&[0; 8]);
        assert!(LayerComps::read(&mut BeReader::new(&bytes)).is_err());
        assert!(read_comp_states(&bytes, true).is_err());
    }
}
