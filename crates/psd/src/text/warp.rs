//! TySh descriptor-backed text settings: the text warp (`warp` descriptor,
//! `TextLayerWarpMixin.h` upstream) and anti-aliasing (`TxLr/AntA`).
//!
//! Edits re-serialize only the one descriptor they touch and splice it back
//! over its exact source span, so the transform, the other descriptor, the
//! EngineData payload, and trailing TySh bytes stay verbatim.

use psd_core::{BeReader, BeWriter, Descriptor, DescriptorValue, TypeToolTaggedBlock};

use super::{invalid, AntiAliasMethod, TextWarpRotation, TextWarpStyle, TYSH};
use crate::{BitDepth, Layer};

#[derive(Debug, Clone, Copy)]
pub(super) enum TyShDescriptor {
    Text,
    Warp,
}

impl<T: BitDepth> Layer<T> {
    /// Return the first parseable TySh warp descriptor.
    pub fn text_warp_descriptor(&self) -> Option<Descriptor> {
        self.first_type_tool().map(|parsed| parsed.warp)
    }

    /// Whether the text warp style is anything other than Photoshop's
    /// explicit `warpNone` value.
    pub fn has_text_warp(&self) -> bool {
        self.text_warp_style()
            .is_some_and(|style| style != TextWarpStyle::NoWarp)
    }

    /// Read the text warp's `warpStyle` enum while retaining unknown identifiers.
    pub fn text_warp_style(&self) -> Option<TextWarpStyle> {
        let warp = self.text_warp_descriptor()?;
        let (_, value) = warp.get("warpStyle")?.as_enum()?;
        Some(TextWarpStyle::from_identifier(value.as_bytes()))
    }

    /// Read the text warp's `warpValue` bend amount (`doub` or `UntF`).
    pub fn text_warp_value(&self) -> Option<f64> {
        self.text_warp_number("warpValue")
    }

    /// Read the text warp's horizontal perspective distortion (`warpPerspective`).
    pub fn text_warp_horizontal_distortion(&self) -> Option<f64> {
        self.text_warp_number("warpPerspective")
    }

    /// Read the text warp's vertical perspective distortion (`warpPerspectiveOther`).
    pub fn text_warp_vertical_distortion(&self) -> Option<f64> {
        self.text_warp_number("warpPerspectiveOther")
    }

    /// Read the text warp's `warpRotate` enum while retaining unknown identifiers.
    pub fn text_warp_rotation(&self) -> Option<TextWarpRotation> {
        let warp = self.text_warp_descriptor()?;
        let (_, value) = warp.get("warpRotate")?.as_enum()?;
        Some(TextWarpRotation::from_identifier(value.as_bytes()))
    }

    /// Change the text warp style. Like upstream, the `warpStyle` item must
    /// already exist; Photoshop writes all five warp items on every text layer.
    pub fn set_text_warp_style(&mut self, style: &TextWarpStyle) -> psd_core::Result<()> {
        self.edit_tysh_descriptor(TyShDescriptor::Warp, |warp| {
            set_enum_item(warp, "warpStyle", |previous| style.descriptor_key(previous))
        })
    }

    /// Set the warp bend amount (`warpValue`).
    pub fn set_text_warp_value(&mut self, value: f64) -> psd_core::Result<()> {
        self.set_text_warp_number("warpValue", value)
    }

    /// Set the horizontal perspective distortion (`warpPerspective`).
    pub fn set_text_warp_horizontal_distortion(&mut self, value: f64) -> psd_core::Result<()> {
        self.set_text_warp_number("warpPerspective", value)
    }

    /// Set the vertical perspective distortion (`warpPerspectiveOther`).
    pub fn set_text_warp_vertical_distortion(&mut self, value: f64) -> psd_core::Result<()> {
        self.set_text_warp_number("warpPerspectiveOther", value)
    }

    /// Set the warp orientation (`warpRotate`).
    pub fn set_text_warp_rotation(&mut self, rotation: &TextWarpRotation) -> psd_core::Result<()> {
        self.edit_tysh_descriptor(TyShDescriptor::Warp, |warp| {
            set_enum_item(warp, "warpRotate", |previous| {
                rotation.descriptor_key(previous)
            })
        })
    }

    /// Anti-aliasing method from the TySh text descriptor (`AntA`).
    pub fn anti_alias(&self) -> Option<AntiAliasMethod> {
        let parsed = self.first_type_tool()?;
        let (_, value) = parsed.text.get("AntA")?.as_enum()?;
        Some(AntiAliasMethod::from_identifier(value.as_bytes()))
    }

    /// Set the anti-aliasing method, writing the identifier Photoshop uses
    /// (char IDs, except the string ID `antiAliasSharp`).
    pub fn set_anti_alias(&mut self, method: &AntiAliasMethod) -> psd_core::Result<()> {
        self.edit_tysh_descriptor(TyShDescriptor::Text, |text| {
            set_enum_item(text, "AntA", |previous| method.descriptor_key(previous))
        })
    }

    fn first_type_tool(&self) -> Option<TypeToolTaggedBlock> {
        self.blocks
            .blocks
            .iter()
            .filter(|block| block.key == TYSH)
            .find_map(|block| TypeToolTaggedBlock::read(&mut BeReader::new(&block.data)).ok())
    }

    fn text_warp_number(&self, key: &str) -> Option<f64> {
        let warp = self.text_warp_descriptor()?;
        let value = warp.get(key)?;
        value
            .as_double()
            .or_else(|| value.as_unit_float().map(|(_, number)| number))
    }

    fn set_text_warp_number(&mut self, key: &str, value: f64) -> psd_core::Result<()> {
        if !value.is_finite() {
            return Err(invalid("text warp values must be finite"));
        }
        self.edit_tysh_descriptor(TyShDescriptor::Warp, |warp| match warp.get_mut(key) {
            Some(DescriptorValue::Double(number)) => {
                *number = value;
                Ok(())
            }
            Some(DescriptorValue::UnitFloat { value: number, .. }) => {
                *number = value;
                Ok(())
            }
            _ => Err(invalid("text warp item is missing or not numeric")),
        })
    }

    /// Re-serialize one descriptor of every parseable TySh block, failing
    /// when no block's descriptors parse.
    ///
    /// Deliberate fidelity delta: upstream edits each block
    /// in place and succeeds if any block accepted the change, which can leave
    /// duplicate TySh blocks disagreeing. Here every parseable block must
    /// accept the edit and all blocks are staged, so failure changes nothing.
    fn edit_tysh_descriptor(
        &mut self,
        which: TyShDescriptor,
        edit: impl FnMut(&mut Descriptor) -> psd_core::Result<()>,
    ) -> psd_core::Result<()> {
        if self.edit_tysh_descriptors(which, edit)? == 0 {
            return Err(invalid("layer has no parseable TySh descriptors"));
        }
        Ok(())
    }

    /// Staged edit of one descriptor in every TySh block whose descriptors
    /// parse; returns how many blocks were edited.
    pub(super) fn edit_tysh_descriptors(
        &mut self,
        which: TyShDescriptor,
        mut edit: impl FnMut(&mut Descriptor) -> psd_core::Result<()>,
    ) -> psd_core::Result<usize> {
        let mut staged = self.blocks.clone();
        let mut edited = 0;
        for block in &mut staged.blocks {
            if block.key != TYSH {
                continue;
            }
            let Ok(spans) = TypeToolTaggedBlock::descriptor_spans(&block.data) else {
                continue;
            };
            let range = match which {
                TyShDescriptor::Text => spans.text,
                TyShDescriptor::Warp => spans.warp,
            };
            let mut descriptor = Descriptor::read(&mut BeReader::new(&block.data[range.clone()]))?;
            edit(&mut descriptor)?;
            let mut writer = BeWriter::new();
            descriptor.write(&mut writer)?;
            block.data.splice(range, writer.into_inner());
            edited += 1;
        }
        self.blocks = staged;
        Ok(edited)
    }
}

fn set_enum_item(
    descriptor: &mut Descriptor,
    key: &str,
    new_value: impl FnOnce(&psd_core::DescriptorKey) -> psd_core::DescriptorKey,
) -> psd_core::Result<()> {
    let Some(DescriptorValue::Enumerated { value, .. }) = descriptor.get_mut(key) else {
        return Err(invalid("TySh descriptor enum item is missing"));
    };
    *value = new_value(value);
    Ok(())
}
