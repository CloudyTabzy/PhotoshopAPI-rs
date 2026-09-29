//! Typed artboard data and tagged-block writers.
//!
//! An artboard is a layer group whose group record carries an artboard block:
//! `artb` in the Photoshop-saved documents seen so far, or `artd` or `abdd`,
//! which another mature parser and upstream's `Util/Enum.h` also list as layer
//! keys. The block is a version-16
//! descriptor with the artboard's rectangle in document pixels, its preset
//! name, and its background. The document-level `artd` block holds the
//! artboard tool settings and the default background for new artboards.
//!
//! Upstream round-trips these blocks as opaque data. Untouched layer/document
//! blocks remain the write source; the typed writers are for explicit edits
//! and newly authored artboards.

use crate::color::Color;
use crate::descriptor::{Descriptor, DescriptorValue};
use crate::enums::Version;
use crate::error::{PsdError, Result};
use crate::io::{BeReader, BeWriter};
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

    /// The Photoshop action-descriptor integer for this background.
    pub fn as_raw(self) -> i32 {
        // Photoshop's Action Manager uses 1=white, 2=black, 3=transparent,
        // and 4=custom. A secondary parser assumes a different 1-3 mapping;
        // Adobe's published schema uses 1 for white and its scripting example
        // sets 3 for transparent. The shipped real artboard fixtures expose
        // only type 4, so keep this measured Photoshop mapping explicit.
        match self {
            Self::White => 1,
            Self::Black => 2,
            Self::Transparent => 3,
            Self::Custom => 4,
            Self::Other(raw) => raw,
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
    /// Build an artboard block with Photoshop's legacy artboard key.
    pub fn new(
        rect: ArtboardRect,
        preset_name: Option<&str>,
        background: ArtboardBackground,
        background_color: Option<Color>,
        guide_indices: &[i32],
    ) -> Result<Self> {
        let mut artboard = Self {
            key: TaggedBlockKey::new(*b"artb"),
            descriptor: Descriptor::with_class("artboard"),
            trailing_bytes: Vec::new(),
        };
        artboard.set_rect(rect)?;
        artboard.set_guide_indices(guide_indices);
        artboard.set_preset_name(preset_name);
        artboard.set_background(background, background_color)?;
        Ok(artboard)
    }

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

    /// Serialize the artboard descriptor payload.
    pub fn to_payload(&self) -> Result<Vec<u8>> {
        if !is_artboard_key(self.key) {
            return Err(invalid_artboard("unsupported artboard tagged-block key"));
        }
        let mut writer = BeWriter::new();
        writer.u32(16);
        self.descriptor.write(&mut writer)?;
        writer.bytes(&self.trailing_bytes);
        Ok(writer.into_inner())
    }

    /// Serialize this artboard as a layer tagged block.
    pub fn to_tagged_block(&self) -> Result<TaggedBlock> {
        Ok(TaggedBlock::new(self.key, self.to_payload()?))
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

    /// Set the artboard rectangle while retaining unknown nested fields.
    pub fn set_rect(&mut self, rect: ArtboardRect) -> Result<()> {
        validate_rect(rect)?;
        set_nested_descriptor(
            &mut self.descriptor,
            "artboardRect",
            "classFloatRect",
            ARTBOARD_ITEM_ORDER,
            |value| {
                value.set_ordered("Top ", DescriptorValue::double(rect.top), RECT_ITEM_ORDER);
                value.set_ordered("Left", DescriptorValue::double(rect.left), RECT_ITEM_ORDER);
                value.set_ordered(
                    "Btom",
                    DescriptorValue::double(rect.bottom),
                    RECT_ITEM_ORDER,
                );
                value.set_ordered("Rght", DescriptorValue::double(rect.right), RECT_ITEM_ORDER);
            },
        );
        Ok(())
    }

    /// Set or remove the device preset name.
    pub fn set_preset_name(&mut self, preset_name: Option<&str>) {
        match preset_name {
            Some(name) => {
                self.descriptor
                    .set_text_ordered("artboardPresetName", name, ARTBOARD_ITEM_ORDER)
            }
            None => {
                self.descriptor.remove("artboardPresetName");
            }
        }
    }

    /// Set the background type and optional RGB custom color.
    pub fn set_background(
        &mut self,
        background: ArtboardBackground,
        color: Option<Color>,
    ) -> Result<()> {
        if background == ArtboardBackground::Custom && color.is_none() {
            return Err(invalid_artboard(
                "custom artboard background needs an RGB color",
            ));
        }
        if color.is_some_and(|color| !matches!(color, Color::Rgb { .. })) {
            return Err(invalid_artboard("artboard background colors must be RGB"));
        }
        self.descriptor.set_ordered(
            "artboardBackgroundType",
            DescriptorValue::long(background.as_raw()),
            ARTBOARD_ITEM_ORDER,
        );
        if let Some(color) = color {
            set_nested_descriptor(
                &mut self.descriptor,
                "Clr ",
                color.class_id(),
                ARTBOARD_ITEM_ORDER,
                |descriptor| color.apply_to(descriptor),
            );
        } else if background != ArtboardBackground::Custom {
            self.descriptor.remove("Clr ");
        }
        Ok(())
    }

    /// Set guide indices while retaining any unknown artboard fields.
    pub fn set_guide_indices(&mut self, guide_indices: &[i32]) {
        self.descriptor.set_ordered(
            "guideIndeces",
            DescriptorValue::List(
                guide_indices
                    .iter()
                    .copied()
                    .map(DescriptorValue::long)
                    .collect(),
            ),
            ARTBOARD_ITEM_ORDER,
        );
    }
}

/// The document-level `artd` block: artboard tool settings.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtboardSettings {
    pub descriptor: Descriptor,
    pub trailing_bytes: Vec<u8>,
}

impl ArtboardSettings {
    /// Build document settings for `count` artboards with Photoshop's default
    /// zero origin and automatic expansion/nesting/positioning enabled.
    pub fn new(count: i32) -> Self {
        let mut settings = Self {
            descriptor: Descriptor::with_class("null"),
            trailing_bytes: Vec::new(),
        };
        settings.set_count(count);
        settings.set_auto_expand_offset((0.0, 0.0));
        settings.set_origin((0.0, 0.0));
        settings.set_auto_expand_enabled(true);
        settings.set_auto_nest_enabled(true);
        settings.set_auto_position_enabled(true);
        settings
    }

    /// Parse an `artd` payload.
    pub fn read(data: &[u8]) -> Result<Self> {
        let mut reader = BeReader::new(data);
        let descriptor = read_versioned_descriptor(&mut reader)?;
        Ok(Self {
            descriptor,
            trailing_bytes: trailing(&mut reader)?,
        })
    }

    /// Serialize the document-level artboard-settings payload.
    pub fn to_payload(&self) -> Result<Vec<u8>> {
        let mut writer = BeWriter::new();
        writer.u32(16);
        self.descriptor.write(&mut writer)?;
        writer.bytes(&self.trailing_bytes);
        Ok(writer.into_inner())
    }

    /// Serialize with the signature and length convention for `artd`.
    pub fn to_tagged_block(&self, version: Version) -> Result<TaggedBlock> {
        let mut block = TaggedBlock::new(TaggedBlockKey::new(*b"artd"), self.to_payload()?);
        if version == Version::Psb {
            block.signature = *b"8B64";
        }
        Ok(block)
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

    pub fn set_count(&mut self, count: i32) {
        self.descriptor.set_ordered(
            "Cnt ",
            DescriptorValue::long(count),
            ARTBOARD_SETTINGS_ORDER,
        );
    }

    pub fn set_origin(&mut self, origin: (f64, f64)) {
        set_point(
            &mut self.descriptor,
            "origin",
            origin,
            ARTBOARD_SETTINGS_ORDER,
        );
    }

    pub fn set_auto_expand_offset(&mut self, offset: (f64, f64)) {
        set_point(
            &mut self.descriptor,
            "autoExpandOffset",
            offset,
            ARTBOARD_SETTINGS_ORDER,
        );
    }

    pub fn set_auto_expand_enabled(&mut self, enabled: bool) {
        self.descriptor.set_ordered(
            "autoExpandEnabled",
            DescriptorValue::boolean(enabled),
            ARTBOARD_SETTINGS_ORDER,
        );
    }

    pub fn set_auto_nest_enabled(&mut self, enabled: bool) {
        self.descriptor.set_ordered(
            "autoNestEnabled",
            DescriptorValue::boolean(enabled),
            ARTBOARD_SETTINGS_ORDER,
        );
    }

    pub fn set_auto_position_enabled(&mut self, enabled: bool) {
        self.descriptor.set_ordered(
            "autoPositionEnabled",
            DescriptorValue::boolean(enabled),
            ARTBOARD_SETTINGS_ORDER,
        );
    }

    pub fn set_shrinkwrap_on_save_enabled(&mut self, enabled: bool) {
        self.descriptor.set_ordered(
            "shrinkwrapOnSaveEnabled",
            DescriptorValue::boolean(enabled),
            ARTBOARD_SETTINGS_ORDER,
        );
    }

    pub fn set_default_background(
        &mut self,
        background: ArtboardBackground,
        color: Option<Color>,
    ) -> Result<()> {
        if background == ArtboardBackground::Custom && color.is_none() {
            return Err(invalid_artboard(
                "custom default artboard background needs an RGB color",
            ));
        }
        if color.is_some_and(|color| !matches!(color, Color::Rgb { .. })) {
            return Err(invalid_artboard("artboard background colors must be RGB"));
        }
        self.descriptor.set_ordered(
            "docDefaultNewArtboardBackgroundType",
            DescriptorValue::long(background.as_raw()),
            ARTBOARD_SETTINGS_ORDER,
        );
        if let Some(color) = color {
            set_nested_descriptor(
                &mut self.descriptor,
                "docDefaultNewArtboardBackgroundColor",
                color.class_id(),
                ARTBOARD_SETTINGS_ORDER,
                |descriptor| color.apply_to(descriptor),
            );
        } else if background != ArtboardBackground::Custom {
            self.descriptor
                .remove("docDefaultNewArtboardBackgroundColor");
        }
        Ok(())
    }
}

impl Default for ArtboardSettings {
    fn default() -> Self {
        Self::new(0)
    }
}

const ARTBOARD_ITEM_ORDER: &[&str] = &[
    "Clr ",
    "artboardBackgroundType",
    "artboardRect",
    "guideIndeces",
    "artboardPresetName",
];

const RECT_ITEM_ORDER: &[&str] = &["Top ", "Left", "Btom", "Rght"];

const ARTBOARD_SETTINGS_ORDER: &[&str] = &[
    "Cnt ",
    "autoExpandOffset",
    "origin",
    "autoExpandEnabled",
    "autoNestEnabled",
    "autoPositionEnabled",
    "shrinkwrapOnSaveEnabled",
    "docDefaultNewArtboardBackgroundColor",
    "docDefaultNewArtboardBackgroundType",
];

fn validate_rect(rect: ArtboardRect) -> Result<()> {
    if ![rect.top, rect.left, rect.bottom, rect.right]
        .into_iter()
        .all(f64::is_finite)
        || rect.bottom < rect.top
        || rect.right < rect.left
    {
        return Err(invalid_artboard(
            "artboard bounds must be finite and non-inverted",
        ));
    }
    Ok(())
}

fn set_point(descriptor: &mut Descriptor, key: &str, point: (f64, f64), order: &[&str]) {
    set_nested_descriptor(descriptor, key, "Pnt ", order, |value| {
        value.set_ordered("Hrzn", DescriptorValue::double(point.0), &["Hrzn", "Vrtc"]);
        value.set_ordered("Vrtc", DescriptorValue::double(point.1), &["Hrzn", "Vrtc"]);
    });
}

fn set_nested_descriptor(
    descriptor: &mut Descriptor,
    key: &str,
    class_id: &str,
    order: &[&str],
    edit: impl FnOnce(&mut Descriptor),
) {
    if matches!(descriptor.get(key), Some(DescriptorValue::Descriptor(_))) {
        if let Some(DescriptorValue::Descriptor(nested)) = descriptor.get_mut(key) {
            edit(nested);
        }
    } else {
        let mut nested = Descriptor::with_class(class_id);
        edit(&mut nested);
        descriptor.set_ordered(key, DescriptorValue::Descriptor(nested), order);
    }
}

fn invalid_artboard(message: &'static str) -> PsdError {
    PsdError::InvalidData { offset: 0, message }
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
    fn typed_artboard_writers_preserve_descriptor_data_and_select_psb_signature() {
        assert_eq!(ArtboardBackground::White.as_raw(), 1);
        assert_eq!(ArtboardBackground::Black.as_raw(), 2);
        assert_eq!(ArtboardBackground::Transparent.as_raw(), 3);
        assert_eq!(ArtboardBackground::Custom.as_raw(), 4);

        let model = Artboard::new(
            ArtboardRect {
                top: 4.0,
                left: 6.0,
                bottom: 44.0,
                right: 66.0,
            },
            Some("Icon"),
            ArtboardBackground::Custom,
            Some(Color::Rgb {
                red: 18.0,
                green: 108.0,
                blue: 200.0,
            }),
            &[1, 4],
        )
        .unwrap();
        let block = model.to_tagged_block().unwrap();
        let parsed = Artboard::read(&block).unwrap().unwrap();
        assert_eq!(parsed.rect().unwrap().left, 6.0);
        assert_eq!(parsed.preset_name(), Some("Icon"));
        assert_eq!(parsed.background(), Some(ArtboardBackground::Custom));
        assert_eq!(parsed.guide_indices(), Some(vec![1, 4]));
        assert_eq!(
            Color::from_descriptor(parsed.background_color().unwrap()),
            Some(Color::Rgb {
                red: 18.0,
                green: 108.0,
                blue: 200.0,
            })
        );

        let mut settings = ArtboardSettings::new(1);
        settings.set_auto_position_enabled(false);
        settings
            .set_default_background(
                ArtboardBackground::White,
                Some(Color::Rgb {
                    red: 255.0,
                    green: 255.0,
                    blue: 255.0,
                }),
            )
            .unwrap();
        let psb = settings.to_tagged_block(Version::Psb).unwrap();
        assert_eq!(psb.key, TaggedBlockKey::new(*b"artd"));
        assert_eq!(psb.signature, *b"8B64");
        let reread = ArtboardSettings::read(&psb.data).unwrap();
        assert_eq!(reread.count(), Some(1));
        assert_eq!(reread.auto_position_enabled(), Some(false));
        assert_eq!(reread.default_background(), Some(ArtboardBackground::White));
        assert_eq!(
            Color::from_descriptor(reread.default_background_color().unwrap()),
            Some(Color::Rgb {
                red: 255.0,
                green: 255.0,
                blue: 255.0,
            })
        );
    }

    #[test]
    fn artboard_setters_keep_unknown_nested_fields() {
        let mut rect = descriptor(
            "classFloatRect",
            vec![
                ("Top ", DescriptorValue::Double(0.0)),
                ("Left", DescriptorValue::Double(0.0)),
                ("Btom", DescriptorValue::Double(10.0)),
                ("Rght", DescriptorValue::Double(20.0)),
                ("Future", DescriptorValue::Boolean(true)),
            ],
        );
        rect.set("Top ", DescriptorValue::Double(2.0));
        let descriptor = descriptor(
            "artboard",
            vec![("artboardRect", DescriptorValue::Descriptor(rect))],
        );
        let block = TaggedBlock::new(TaggedBlockKey::new(*b"artb"), versioned(&descriptor, &[]));
        let mut artboard = Artboard::read(&block).unwrap().unwrap();
        artboard
            .set_rect(ArtboardRect {
                top: 3.0,
                left: 4.0,
                bottom: 30.0,
                right: 40.0,
            })
            .unwrap();
        let rect = artboard
            .descriptor
            .get("artboardRect")
            .unwrap()
            .as_descriptor()
            .unwrap();
        assert_eq!(rect.get("Future").unwrap().as_bool(), Some(true));
        assert_eq!(artboard.rect().unwrap().top, 3.0);
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
