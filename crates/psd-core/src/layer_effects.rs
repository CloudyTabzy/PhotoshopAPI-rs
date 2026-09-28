//! Read-only views of layer effects stored in `lfx2`, `lmfx`, `lfxs`, and `lrFX`.
//!
//! The four-key dispatch and effect names follow the public `ag-psd-rs` reference
//! (<https://github.com/Vasyanator/ag-psd-rs>). Unlike that parser, these views
//! retain every effect block and unknown legacy record. The original tagged
//! block remains authoritative for writing, so reading effects changes no bytes.

use crate::descriptor::{Descriptor, DescriptorKey};
use crate::enums::BlendMode;
use crate::error::{PsdError, Result};
use crate::io::BeReader;
use crate::tagged_blocks::{TaggedBlock, TaggedBlockKey};
use crate::types::RawColor;

/// One of the standard Photoshop layer effect families or the legacy common state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectKind {
    CommonState,
    DropShadow,
    InnerShadow,
    OuterGlow,
    InnerGlow,
    Bevel,
    SolidFill,
    Satin,
    GradientOverlay,
    PatternOverlay,
    Stroke,
    Unknown,
}

/// A parsed view of one effect-bearing tagged block.
#[derive(Debug, Clone, PartialEq)]
pub struct LayerEffectsBlock {
    pub key: TaggedBlockKey,
    pub data: LayerEffectsData,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LayerEffectsData {
    Modern(ModernLayerEffects),
    Legacy(LegacyLayerEffects),
}

/// Descriptor-based layer effects. `descriptor` retains unknown fields and their order.
#[derive(Debug, Clone, PartialEq)]
pub struct ModernLayerEffects {
    pub descriptor: Descriptor,
    /// Bytes following the descriptor, commonly one or two alignment bytes.
    pub trailing_bytes: Vec<u8>,
}

/// An effect descriptor borrowed from the modern block.
#[derive(Debug, Clone, Copy)]
pub struct EffectDescriptor<'a> {
    pub kind: EffectKind,
    pub source_key: &'a DescriptorKey,
    pub descriptor: &'a Descriptor,
    /// Index within a multi-effect list; zero for a singleton.
    pub index: usize,
}

impl EffectDescriptor<'_> {
    pub fn enabled(&self) -> Option<bool> {
        self.descriptor.get("enab")?.as_bool()
    }

    pub fn present(&self) -> Option<bool> {
        self.descriptor.get("present")?.as_bool()
    }

    pub fn show_in_dialog(&self) -> Option<bool> {
        self.descriptor.get("showInDialog")?.as_bool()
    }

    /// Photoshop's percent value, normally in the range 0–100.
    pub fn opacity_percent(&self) -> Option<f64> {
        self.unit_value("Opct").map(|(_, value)| value)
    }

    /// Unit and value of a named measurement (`blur`, `Dstn`, `lagl`, etc.).
    pub fn unit_value(&self, key: &str) -> Option<([u8; 4], f64)> {
        self.descriptor
            .get(key)?
            .as_unit_float()
            .map(|(unit, value)| (*unit, value))
    }

    /// Raw blend-mode enumerator. Unknown and long-form values remain intact.
    pub fn blend_mode(&self) -> Option<(&DescriptorKey, &DescriptorKey)> {
        self.descriptor.get("Md  ")?.as_enum()
    }

    /// The blend mode decoded from either the historical ID (`Mltp`) or the
    /// Photoshop 2026 ID (`multiply`); `None` when absent or unrecognized.
    pub fn blend_mode_value(&self) -> Option<BlendMode> {
        BlendMode::from_descriptor_enum(self.blend_mode()?.1.as_bytes())
    }

    /// Nested color descriptor, when this effect has a direct `Clr ` field.
    pub fn color(&self) -> Option<&Descriptor> {
        self.descriptor.get("Clr ")?.as_descriptor()
    }
}

impl ModernLayerEffects {
    pub fn scale_percent(&self) -> Option<f64> {
        self.descriptor
            .get("Scl ")?
            .as_unit_float()
            .map(|(_, value)| value)
    }

    pub fn enabled(&self) -> Option<bool> {
        self.descriptor.get("masterFXSwitch")?.as_bool()
    }

    /// Effects in descriptor order, with separate entries for multi-effect lists.
    /// Unknown root fields remain available through `descriptor`.
    ///
    /// An effect whose value is not the object the format requires is skipped
    /// with a warning rather than failing the block: such a file renders
    /// without that effect, and the raw block stays the write source, so
    /// nothing is lost by reading past it.
    pub fn effects(&self) -> Result<Vec<EffectDescriptor<'_>>> {
        let mut effects = Vec::new();
        for item in &self.descriptor.items {
            let Some((kind, multi)) = modern_kind(item.key.as_bytes()) else {
                continue;
            };
            if multi {
                let Some(list) = item.value.as_list() else {
                    tracing::warn!("skipping malformed multi-effect list {:?}", item.key);
                    continue;
                };
                for (index, value) in list.iter().enumerate() {
                    let Some(descriptor) = value.as_descriptor() else {
                        tracing::warn!("skipping malformed effect {index} in {:?}", item.key);
                        continue;
                    };
                    effects.push(EffectDescriptor {
                        kind,
                        source_key: &item.key,
                        descriptor,
                        index,
                    });
                }
            } else {
                let Some(descriptor) = item.value.as_descriptor() else {
                    tracing::warn!("skipping malformed effect {:?}", item.key);
                    continue;
                };
                effects.push(EffectDescriptor {
                    kind,
                    source_key: &item.key,
                    descriptor,
                    index: 0,
                });
            }
        }
        Ok(effects)
    }
}

fn modern_kind(key: &[u8]) -> Option<(EffectKind, bool)> {
    // Keys and multi-effect families are drawn from ag-psd-rs's `effects_keys.rs`.
    Some(match key {
        b"DrSh" => (EffectKind::DropShadow, false),
        b"IrSh" => (EffectKind::InnerShadow, false),
        b"OrGl" => (EffectKind::OuterGlow, false),
        b"IrGl" => (EffectKind::InnerGlow, false),
        b"ebbl" => (EffectKind::Bevel, false),
        b"SoFi" => (EffectKind::SolidFill, false),
        b"ChFX" => (EffectKind::Satin, false),
        b"GrFl" => (EffectKind::GradientOverlay, false),
        b"patternFill" => (EffectKind::PatternOverlay, false),
        b"FrFX" => (EffectKind::Stroke, false),
        b"dropShadowMulti" => (EffectKind::DropShadow, true),
        b"innerShadowMulti" => (EffectKind::InnerShadow, true),
        b"solidFillMulti" => (EffectKind::SolidFill, true),
        b"gradientFillMulti" => (EffectKind::GradientOverlay, true),
        b"frameFXMulti" => (EffectKind::Stroke, true),
        _ => return None,
    })
}

/// The legacy `lrFX` container and its bounded records.
#[derive(Debug, Clone, PartialEq)]
pub struct LegacyLayerEffects {
    pub records: Vec<LegacyEffectRecord>,
    pub trailing_bytes: Vec<u8>,
}

/// Raw PSD color components. Photoshop uses different interpretations by color space.
pub type LegacyEffectColor = RawColor;

/// One legacy effect record. `payload` contains the complete original body,
/// including its version, and is kept even for unknown types.
#[derive(Debug, Clone, PartialEq)]
pub struct LegacyEffectRecord {
    pub key: [u8; 4],
    pub kind: EffectKind,
    pub payload: Vec<u8>,
    pub version: Option<u32>,
    pub enabled: Option<bool>,
    /// Opacity in the range 0–1, as stored by the 8-bit fixed-point field.
    pub opacity: Option<f64>,
    pub size: Option<f64>,
    pub angle: Option<f64>,
    pub distance: Option<f64>,
    /// The main blend mode (highlight mode for bevel records).
    pub blend_mode: Option<[u8; 4]>,
    /// The main color (highlight color for bevel records).
    pub color: Option<LegacyEffectColor>,
}

impl LayerEffectsBlock {
    /// Parse a recognized effect block. Other tagged blocks return `None`.
    /// Malformed effect payloads return an error only when this view is requested.
    pub fn read(block: &TaggedBlock) -> Result<Option<Self>> {
        let key = block.key;
        let data = match &key.as_bytes() {
            b"lrFX" => LayerEffectsData::Legacy(read_legacy(&block.data)?),
            b"lfx2" | b"lmfx" | b"lfxs" => LayerEffectsData::Modern(read_modern(&block.data)?),
            _ => return Ok(None),
        };
        Ok(Some(Self { key, data }))
    }
}

fn read_modern(data: &[u8]) -> Result<ModernLayerEffects> {
    let mut reader = BeReader::new(data);
    if reader.u32()? != 0 || reader.u32()? != 16 {
        return Err(invalid_effect());
    }
    let descriptor = Descriptor::read(&mut reader)?;
    let trailing_bytes = reader.take(reader.remaining())?.to_vec();
    Ok(ModernLayerEffects {
        descriptor,
        trailing_bytes,
    })
}

fn read_legacy(data: &[u8]) -> Result<LegacyLayerEffects> {
    let mut reader = BeReader::new(data);
    if reader.u16()? != 0 {
        return Err(invalid_effect());
    }
    let count = reader.u16()? as usize;
    let mut records = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        let mut signature = [0; 4];
        signature.copy_from_slice(reader.take(4)?);
        if signature != *b"8BIM" {
            return Err(PsdError::InvalidSignature {
                expected: "8BIM",
                found: signature,
                offset: (reader.position() - 4) as u64,
            });
        }
        let mut key = [0; 4];
        key.copy_from_slice(reader.take(4)?);
        let size = reader.u32()? as usize;
        let payload = reader.take(size)?.to_vec();
        records.push(read_legacy_record(key, payload)?);
    }
    Ok(LegacyLayerEffects {
        records,
        trailing_bytes: reader.take(reader.remaining())?.to_vec(),
    })
}

fn read_legacy_record(key: [u8; 4], payload: Vec<u8>) -> Result<LegacyEffectRecord> {
    let kind = match &key {
        b"cmnS" => EffectKind::CommonState,
        b"dsdw" => EffectKind::DropShadow,
        b"isdw" => EffectKind::InnerShadow,
        b"oglw" => EffectKind::OuterGlow,
        b"iglw" => EffectKind::InnerGlow,
        b"bevl" => EffectKind::Bevel,
        b"sofi" => EffectKind::SolidFill,
        _ => EffectKind::Unknown,
    };
    let mut result = LegacyEffectRecord {
        key,
        kind,
        payload,
        version: None,
        enabled: None,
        opacity: None,
        size: None,
        angle: None,
        distance: None,
        blend_mode: None,
        color: None,
    };
    if kind == EffectKind::Unknown {
        return Ok(result);
    }
    let mut reader = BeReader::new(&result.payload);
    let version = reader.u32()?;
    result.version = Some(version);
    match &key {
        b"cmnS" if result.payload.len() == 7 && version == 0 => {
            result.enabled = Some(reader.u8()? != 0);
        }
        b"dsdw" | b"isdw"
            if matches!(result.payload.len(), 41 | 51) && matches!(version, 0 | 2) =>
        {
            result.size = Some(fixed_16_16(&mut reader)?);
            fixed_16_16(&mut reader)?; // intensity
            result.angle = Some(fixed_16_16(&mut reader)?);
            result.distance = Some(fixed_16_16(&mut reader)?);
            result.color = Some(read_color(&mut reader)?);
            result.blend_mode = Some(read_blend_mode(&mut reader)?);
            result.enabled = Some(reader.u8()? != 0);
            reader.u8()?; // global light
            result.opacity = Some(f64::from(reader.u8()?) / 255.0);
            if result.payload.len() == 51 {
                read_color(&mut reader)?; // native color
            }
        }
        b"oglw" | b"iglw"
            if ((key == *b"oglw" && matches!(result.payload.len(), 32 | 42))
                || (key == *b"iglw" && matches!(result.payload.len(), 32 | 43)))
                && matches!(version, 0 | 2) =>
        {
            result.size = Some(fixed_16_16(&mut reader)?);
            fixed_16_16(&mut reader)?; // intensity
            result.color = Some(read_color(&mut reader)?);
            result.blend_mode = Some(read_blend_mode(&mut reader)?);
            result.enabled = Some(reader.u8()? != 0);
            result.opacity = Some(f64::from(reader.u8()?) / 255.0);
            if key == *b"iglw" && result.payload.len() == 43 {
                reader.u8()?; // inverted
            }
            if result.payload.len() >= 42 {
                read_color(&mut reader)?; // native color
            }
        }
        b"bevl" if matches!(result.payload.len(), 58 | 78) && matches!(version, 0 | 2) => {
            result.angle = Some(fixed_16_16(&mut reader)?);
            fixed_16_16(&mut reader)?; // strength
            result.size = Some(fixed_16_16(&mut reader)?);
            result.blend_mode = Some(read_blend_mode(&mut reader)?); // highlight
            read_blend_mode(&mut reader)?; // shadow
            result.color = Some(read_color(&mut reader)?); // highlight
            read_color(&mut reader)?; // shadow
            reader.take(3)?; // style, highlight opacity, shadow opacity
            result.enabled = Some(reader.u8()? != 0);
            reader.take(2)?; // global light, direction
            if result.payload.len() == 78 {
                read_color(&mut reader)?;
                read_color(&mut reader)?;
            }
        }
        b"sofi" if result.payload.len() == 34 && version == 2 => {
            result.blend_mode = Some(read_blend_mode(&mut reader)?);
            result.color = Some(read_color(&mut reader)?);
            result.opacity = Some(f64::from(reader.u8()?) / 255.0);
            result.enabled = Some(reader.u8()? != 0);
            read_color(&mut reader)?; // native color
        }
        _ => return Err(invalid_effect()),
    }
    Ok(result)
}

fn fixed_16_16(reader: &mut BeReader) -> Result<f64> {
    Ok(f64::from(reader.i32()?) / 65_536.0)
}

fn read_color(reader: &mut BeReader) -> Result<LegacyEffectColor> {
    RawColor::read(reader)
}

fn read_blend_mode(reader: &mut BeReader) -> Result<[u8; 4]> {
    let mut signature = [0; 4];
    signature.copy_from_slice(reader.take(4)?);
    if signature != *b"8BIM" {
        return Err(invalid_effect());
    }
    let mut key = [0; 4];
    key.copy_from_slice(reader.take(4)?);
    Ok(key)
}

fn invalid_effect() -> PsdError {
    PsdError::InvalidData {
        offset: 0,
        message: "invalid layer effects payload",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{DescriptorItem, DescriptorValue};
    use crate::io::BeWriter;
    use crate::strings::UnicodeString;

    fn descriptor(items: Vec<DescriptorItem>) -> Descriptor {
        Descriptor {
            name: UnicodeString::new("", 1).unwrap(),
            class_id: DescriptorKey::char_id(*b"null"),
            items,
        }
    }

    fn item(key: &str, value: DescriptorValue) -> DescriptorItem {
        DescriptorItem {
            key: DescriptorKey::new(key),
            value,
        }
    }

    #[test]
    fn a_malformed_effect_is_skipped_and_the_rest_read() {
        // One effect carries the object the format requires; the other is a
        // bare string. The block still parses, exposing the good effect and
        // skipping the bad one: the raw block stays the write source, so
        // nothing is lost, and the file renders without that one effect.
        let shadow = descriptor(vec![item("enab", DescriptorValue::Boolean(true))]);
        let root = descriptor(vec![
            item("DrSh", DescriptorValue::Descriptor(shadow)),
            item(
                "IrSh",
                DescriptorValue::String(UnicodeString::new("bad", 1).unwrap()),
            ),
        ]);
        let mut writer = BeWriter::new();
        writer.u32(0);
        writer.u32(16);
        root.write(&mut writer).unwrap();
        let block = TaggedBlock::new(TaggedBlockKey::new(*b"lfx2"), writer.into_inner());
        let parsed = LayerEffectsBlock::read(&block).unwrap().unwrap();
        let LayerEffectsData::Modern(modern) = parsed.data else {
            panic!("modern expected")
        };
        let effects = modern.effects().unwrap();
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].kind, EffectKind::DropShadow);
    }

    #[test]
    fn all_modern_keys_expose_single_and_multi_effects() {
        let shadow = descriptor(vec![
            item("enab", DescriptorValue::Boolean(true)),
            item(
                "Opct",
                DescriptorValue::UnitFloat {
                    unit: *b"#Prc",
                    value: 72.5,
                },
            ),
        ]);
        let root = descriptor(vec![
            item("masterFXSwitch", DescriptorValue::Boolean(true)),
            item("DrSh", DescriptorValue::Descriptor(shadow.clone())),
            item(
                "dropShadowMulti",
                DescriptorValue::List(vec![
                    DescriptorValue::Descriptor(shadow.clone()),
                    DescriptorValue::Descriptor(shadow),
                ]),
            ),
            item("futureField", DescriptorValue::Integer(42)),
        ]);
        let mut writer = BeWriter::new();
        writer.u32(0);
        writer.u32(16);
        root.write(&mut writer).unwrap();
        writer.u8(0x5a);
        for key in [*b"lfx2", *b"lmfx", *b"lfxs"] {
            let block = TaggedBlock::new(TaggedBlockKey::new(key), writer.as_slice().to_vec());
            let parsed = LayerEffectsBlock::read(&block).unwrap().unwrap();
            let LayerEffectsData::Modern(modern) = parsed.data else {
                panic!("modern expected")
            };
            assert_eq!(
                modern.descriptor.get("futureField").unwrap().as_integer(),
                Some(42)
            );
            assert_eq!(modern.trailing_bytes, [0x5a]);
            assert_eq!(modern.enabled(), Some(true));
            let effects = modern.effects().unwrap();
            assert_eq!(effects.len(), 3);
            assert_eq!(effects[0].kind, EffectKind::DropShadow);
            assert_eq!(effects[1].index, 0);
            assert_eq!(effects[2].index, 1);
            assert_eq!(effects[0].enabled(), Some(true));
            assert_eq!(effects[0].opacity_percent(), Some(72.5));
        }
    }

    #[test]
    fn effect_blend_modes_decode_historical_and_2026_ids() {
        let effect = |value: DescriptorKey| {
            descriptor(vec![item(
                "Md  ",
                DescriptorValue::Enumerated {
                    type_id: DescriptorKey::char_id(*b"BlnM"),
                    value,
                },
            )])
        };
        let historical = effect(DescriptorKey::char_id(*b"CBrn"));
        let current = effect(DescriptorKey::new("colorBurn"));
        let unknown = effect(DescriptorKey::new("futureMode"));
        let root = descriptor(vec![
            item("DrSh", DescriptorValue::Descriptor(historical)),
            item("IrSh", DescriptorValue::Descriptor(current)),
            item("OrGl", DescriptorValue::Descriptor(unknown)),
        ]);
        let modern = ModernLayerEffects {
            descriptor: root,
            trailing_bytes: Vec::new(),
        };
        let modes: Vec<_> = modern
            .effects()
            .unwrap()
            .iter()
            .map(EffectDescriptor::blend_mode_value)
            .collect();
        assert_eq!(
            modes,
            [
                Some(BlendMode::COLOR_BURN),
                Some(BlendMode::COLOR_BURN),
                None
            ]
        );
        // The raw enumerator stays available for the unknown value.
        let effects = modern.effects().unwrap();
        assert_eq!(effects[2].blend_mode().unwrap().1.as_bytes(), b"futureMode");
    }

    #[test]
    fn unknown_legacy_record_and_trailing_bytes_are_available() {
        let mut writer = BeWriter::new();
        writer.u16(0);
        writer.u16(1);
        writer.bytes(b"8BIM");
        writer.bytes(b"newE");
        writer.u32(3);
        writer.bytes(&[1, 2, 3]);
        writer.u8(0x7f);
        let block = TaggedBlock::new(TaggedBlockKey::new(*b"lrFX"), writer.into_inner());
        let parsed = LayerEffectsBlock::read(&block).unwrap().unwrap();
        let LayerEffectsData::Legacy(legacy) = parsed.data else {
            panic!("legacy expected")
        };
        assert_eq!(legacy.records.len(), 1);
        assert_eq!(legacy.records[0].kind, EffectKind::Unknown);
        assert_eq!(legacy.records[0].payload, [1, 2, 3]);
        assert_eq!(legacy.trailing_bytes, [0x7f]);
    }

    #[test]
    fn malformed_effect_blocks_fail_without_changing_raw_blocks() {
        // A block that does not parse at all still reports an error — the
        // document layer skips it — and the raw bytes are untouched.
        let invalid = TaggedBlock::new(TaggedBlockKey::new(*b"lfx2"), vec![0; 8]);
        assert!(LayerEffectsBlock::read(&invalid).is_err());
        assert_eq!(invalid.data, vec![0; 8]);

        let mut bytes = vec![0, 0, 0, 1]; // legacy version and count
        bytes.extend_from_slice(b"8BIMnewE");
        bytes.extend_from_slice(&10u32.to_be_bytes());
        bytes.extend_from_slice(&[1, 2, 3]);
        let invalid = TaggedBlock::new(TaggedBlockKey::new(*b"lrFX"), bytes.clone());
        assert!(LayerEffectsBlock::read(&invalid).is_err());
        assert_eq!(invalid.data, bytes);

        // A malformed EFFECT inside a well-formed block no longer fails the
        // block: the view skips it and the rest stays readable (a multi-list
        // that is not a list at all exposes nothing).
        let mut writer = BeWriter::new();
        writer.u32(0);
        writer.u32(16);
        descriptor(vec![item("dropShadowMulti", DescriptorValue::Integer(1))])
            .write(&mut writer)
            .unwrap();
        let block = TaggedBlock::new(TaggedBlockKey::new(*b"lmfx"), writer.into_inner());
        let parsed = LayerEffectsBlock::read(&block).unwrap().unwrap();
        let LayerEffectsData::Modern(modern) = parsed.data else {
            panic!("modern expected")
        };
        assert!(modern.effects().unwrap().is_empty());
    }
}
