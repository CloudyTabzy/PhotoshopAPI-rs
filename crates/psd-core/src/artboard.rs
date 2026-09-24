//! Read-only views of artboard data.
//!
//! An artboard is a layer group whose group record carries an artboard block:
//! `artb` in the Photoshop-saved documents seen so far, or `artd` or `abdd`,
//! which `psd-tools` (<https://github.com/psd-tools/psd-tools>) and upstream's
//! `Util/Enum.h` also list as layer keys. The block is a version-16
//! descriptor with the artboard's rectangle in document pixels, its preset
//! name, and its background. The document-level `artd` block holds the
//! artboard tool settings and the default background for new artboards.
//!
//! Upstream round-trips these blocks as opaque data. As with the other views,
//! the original tagged block is still what gets written.

use crate::descriptor::{Descriptor, DescriptorValue};
use crate::error::Result;
use crate::io::BeReader;
use crate::tagged_blocks::{TaggedBlock, TaggedBlockKey};
use crate::views::{number, read_versioned_descriptor, trailing};

/// Layer-level keys that hold artboard data.
pub const ARTBOARD_KEYS: [[u8; 4]; 3] = [*b"artb", *b"artd", *b"abdd"];

/// Whether `key` holds layer-level artboard data.
pub fn is_artboard_key(key: TaggedBlockKey) -> bool {
    ARTBOARD_KEYS.contains(&key.as_bytes())
}

/// A rectangle in document pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArtboardRect {
    pub top: f64,
    pub left: f64,
    pub bottom: f64,
    pub right: f64,
}

impl ArtboardRect {
    pub fn width(&self) -> f64 {
        self.right - self.left
    }

    pub fn height(&self) -> f64 {
        self.bottom - self.top
    }
}

/// An artboard's background (`artboardBackgroundType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtboardBackground {
    White,
    Black,
    Transparent,
    /// The color in [`Artboard::background_color`].
    Custom,
    Other(i32),
}

impl ArtboardBackground {
    pub fn from_raw(raw: i32) -> Self {
        match raw {
            1 => Self::White,
            2 => Self::Black,
            3 => Self::Transparent,
            4 => Self::Custom,
            other => Self::Other(other),
        }
    }
}

/// A layer's artboard block (`artb`, `artd`, or `abdd`).
#[derive(Debug, Clone, PartialEq)]
pub struct Artboard {
    pub key: TaggedBlockKey,
    pub descriptor: Descriptor,
    pub trailing_bytes: Vec<u8>,
}

impl Artboard {
    /// Parse a recognized artboard block. Other tagged blocks return `None`.
    /// A malformed payload is an error only when this view is requested.
    pub fn read(block: &TaggedBlock) -> Result<Option<Self>> {
        if !is_artboard_key(block.key) {
            return Ok(None);
        }
        let mut reader = BeReader::new(&block.data);
        let descriptor = read_versioned_descriptor(&mut reader)?;
        Ok(Some(Self {
            key: block.key,
            descriptor,
            trailing_bytes: trailing(&mut reader)?,
        }))
    }

    /// The artboard's bounds (`artboardRect`).
    pub fn rect(&self) -> Option<ArtboardRect> {
        let rect = self.descriptor.get("artboardRect")?.as_descriptor()?;
        Some(ArtboardRect {
            top: number(rect, "Top ")?,
            left: number(rect, "Left")?,
            bottom: number(rect, "Btom")?,
            right: number(rect, "Rght")?,
        })
    }

    /// The preset the artboard was created from (`artboardPresetName`);
    /// empty for a custom size.
    pub fn preset_name(&self) -> Option<&str> {
        self.descriptor.get("artboardPresetName")?.as_str()
    }

    pub fn background(&self) -> Option<ArtboardBackground> {
        let raw = self
            .descriptor
            .get("artboardBackgroundType")?
            .as_integer()?;
        Some(ArtboardBackground::from_raw(raw))
    }

    /// The background color descriptor (`Clr `), used by
    /// [`ArtboardBackground::Custom`].
    pub fn background_color(&self) -> Option<&Descriptor> {
        self.descriptor.get("Clr ")?.as_descriptor()
    }

    /// Indices of the document guides that belong to this artboard
    /// (`guideIndeces`, as Photoshop spells it).
    pub fn guide_indices(&self) -> Option<Vec<i32>> {
        self.descriptor
            .get("guideIndeces")?
            .as_list()?
            .iter()
            .map(DescriptorValue::as_integer)
            .collect()
    }
}

/// The document-level `artd` block: artboard tool settings.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtboardSettings {
    pub descriptor: Descriptor,
    pub trailing_bytes: Vec<u8>,
}

impl ArtboardSettings {
    /// Parse an `artd` payload.
    pub fn read(data: &[u8]) -> Result<Self> {
        let mut reader = BeReader::new(data);
        let descriptor = read_versioned_descriptor(&mut reader)?;
        Ok(Self {
            descriptor,
            trailing_bytes: trailing(&mut reader)?,
        })
    }

    /// The number of artboards Photoshop recorded (`Cnt `).
    pub fn count(&self) -> Option<i32> {
        self.descriptor.get("Cnt ")?.as_integer()
    }

    /// The artboard coordinate origin (`origin`) as `(x, y)`.
    pub fn origin(&self) -> Option<(f64, f64)> {
        point(self.descriptor.get("origin")?.as_descriptor()?)
    }

    /// The canvas auto-expand offset (`autoExpandOffset`) as `(x, y)`.
    pub fn auto_expand_offset(&self) -> Option<(f64, f64)> {
        point(self.descriptor.get("autoExpandOffset")?.as_descriptor()?)
    }

    pub fn auto_expand_enabled(&self) -> Option<bool> {
        self.descriptor.get("autoExpandEnabled")?.as_bool()
    }

    pub fn auto_nest_enabled(&self) -> Option<bool> {
        self.descriptor.get("autoNestEnabled")?.as_bool()
    }

    pub fn auto_position_enabled(&self) -> Option<bool> {
        self.descriptor.get("autoPositionEnabled")?.as_bool()
    }

    pub fn shrinkwrap_on_save_enabled(&self) -> Option<bool> {
        self.descriptor.get("shrinkwrapOnSaveEnabled")?.as_bool()
    }

    /// The background of newly created artboards.
    pub fn default_background(&self) -> Option<ArtboardBackground> {
        let raw = self
            .descriptor
            .get("docDefaultNewArtboardBackgroundType")?
            .as_integer()?;
        Some(ArtboardBackground::from_raw(raw))
    }

    /// The color for [`default_background`](Self::default_background) when it
    /// is [`ArtboardBackground::Custom`].
    pub fn default_background_color(&self) -> Option<&Descriptor> {
        self.descriptor
            .get("docDefaultNewArtboardBackgroundColor")?
            .as_descriptor()
    }
}

fn point(descriptor: &Descriptor) -> Option<(f64, f64)> {
    Some((number(descriptor, "Hrzn")?, number(descriptor, "Vrtc")?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{DescriptorItem, DescriptorKey};
    use crate::io::BeWriter;
    use crate::strings::UnicodeString;

    fn key(text: &str) -> DescriptorKey {
        match <[u8; 4]>::try_from(text.as_bytes()) {
            Ok(code) => DescriptorKey::char_id(code),
            Err(_) => DescriptorKey::new(text),
        }
    }

    fn descriptor(class: &str, items: Vec<(&str, DescriptorValue)>) -> Descriptor {
        Descriptor {
            name: UnicodeString::new("", 1).unwrap(),
            class_id: key(class),
            items: items
                .into_iter()
                .map(|(name, value)| DescriptorItem {
                    key: key(name),
                    value,
                })
                .collect(),
        }
    }

    fn versioned(descriptor: &Descriptor, trailing: &[u8]) -> Vec<u8> {
        let mut writer = BeWriter::new();
        writer.u32(16);
        descriptor.write(&mut writer).unwrap();
        writer.bytes(trailing);
        writer.into_inner()
    }

    fn pnt(x: f64, y: f64) -> DescriptorValue {
        DescriptorValue::Descriptor(descriptor(
            "Pnt ",
            vec![
                ("Hrzn", DescriptorValue::Double(x)),
                ("Vrtc", DescriptorValue::Double(y)),
            ],
        ))
    }

    #[test]
    fn artboard_blocks_expose_rect_preset_background_and_guides() {
        let rect = descriptor(
            "classFloatRect",
            vec![
                ("Top ", DescriptorValue::Double(10.0)),
                ("Left", DescriptorValue::Double(20.0)),
                ("Btom", DescriptorValue::Double(110.0)),
                ("Rght", DescriptorValue::Double(220.0)),
            ],
        );
        let color = descriptor("RGBC", vec![("Rd  ", DescriptorValue::Double(217.0))]);
        let artboard = descriptor(
            "artboard",
            vec![
                ("artboardRect", DescriptorValue::Descriptor(rect)),
                (
                    "guideIndeces",
                    DescriptorValue::List(vec![
                        DescriptorValue::Integer(0),
                        DescriptorValue::Integer(3),
                    ]),
                ),
                (
                    "artboardPresetName",
                    DescriptorValue::String(UnicodeString::new("iPhone", 1).unwrap()),
                ),
                ("Clr ", DescriptorValue::Descriptor(color)),
                ("artboardBackgroundType", DescriptorValue::Integer(4)),
                ("futureField", DescriptorValue::Boolean(true)),
            ],
        );
        for block_key in ARTBOARD_KEYS {
            let block = TaggedBlock::new(
                TaggedBlockKey::new(block_key),
                versioned(&artboard, &[0, 0]),
            );
            let parsed = Artboard::read(&block).unwrap().unwrap();
            let rect = parsed.rect().unwrap();
            assert_eq!((rect.width(), rect.height()), (200.0, 100.0));
            assert_eq!(parsed.preset_name(), Some("iPhone"));
            assert_eq!(parsed.background(), Some(ArtboardBackground::Custom));
            let red = parsed.background_color().unwrap().get("Rd  ").unwrap();
            assert_eq!(red.as_double(), Some(217.0));
            assert_eq!(parsed.guide_indices(), Some(vec![0, 3]));
            assert!(parsed.descriptor.contains("futureField"));
            assert_eq!(parsed.trailing_bytes, [0, 0]);
        }
        let other = TaggedBlock::new(TaggedBlockKey::new(*b"lsct"), vec![0; 4]);
        assert!(Artboard::read(&other).unwrap().is_none());
        assert_eq!(
            ArtboardBackground::from_raw(9),
            ArtboardBackground::Other(9)
        );
    }

    #[test]
    fn document_settings_expose_tool_switches_and_defaults() {
        let settings = descriptor(
            "null",
            vec![
                ("Cnt ", DescriptorValue::Integer(2)),
                ("autoExpandOffset", pnt(-5.0, 7.0)),
                ("origin", pnt(1.0, 2.0)),
                ("autoExpandEnabled", DescriptorValue::Boolean(true)),
                ("autoNestEnabled", DescriptorValue::Boolean(false)),
                ("autoPositionEnabled", DescriptorValue::Boolean(true)),
                ("shrinkwrapOnSaveEnabled", DescriptorValue::Boolean(true)),
                (
                    "docDefaultNewArtboardBackgroundType",
                    DescriptorValue::Integer(3),
                ),
            ],
        );
        let parsed = ArtboardSettings::read(&versioned(&settings, &[])).unwrap();
        assert_eq!(parsed.count(), Some(2));
        assert_eq!(parsed.origin(), Some((1.0, 2.0)));
        assert_eq!(parsed.auto_expand_offset(), Some((-5.0, 7.0)));
        assert_eq!(parsed.auto_nest_enabled(), Some(false));
        assert_eq!(parsed.shrinkwrap_on_save_enabled(), Some(true));
        assert_eq!(
            parsed.default_background(),
            Some(ArtboardBackground::Transparent)
        );
        assert!(parsed.default_background_color().is_none());
    }

    #[test]
    fn malformed_artboard_payloads_fail_without_changing_blocks() {
        for data in [vec![0, 0, 0, 15], vec![0, 0, 0, 16, 0, 0]] {
            let block = TaggedBlock::new(TaggedBlockKey::new(*b"artb"), data.clone());
            assert!(Artboard::read(&block).is_err());
            assert_eq!(block.data, data);
            assert!(ArtboardSettings::read(&data).is_err());
        }
    }
}
