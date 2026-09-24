//! Editable text layers (`LayerTypes/TextLayer/` upstream).
//!
//! A text layer is a [`Layer`] whose [`LayerKind::Text`](crate::LayerKind)
//! keeps its rendered preview as ordinary planar channels, while the editable
//! text lives in the layer's raw `TySh` tagged block: a transform, a `TxLr`
//! descriptor holding the visible `Txt ` string and the EngineData payload
//! (runs, sheets, fonts, shapes), and a `warp` descriptor. All accessors here
//! read that block on demand, and every edit patches only the bytes it means
//! to change, so untouched descriptor items, EngineData bytes, and unknown
//! data survive verbatim.
//!
//! # Reading
//!
//! ```
//! # fn main() -> psd::core::Result<()> {
//! # let layer = psd::TextLayerBuilder::new("Caption", "Hello World")
//! #     .font_size(36.0)
//! #     .build::<u8>()?;
//! assert_eq!(layer.text().as_deref(), Some("Hello World"));
//! // Run lengths count UTF-16 code units, including the terminal `\r`.
//! assert_eq!(layer.style_run_lengths(), Some(vec![12]));
//! let run = layer.style_run(0).unwrap(); // an owned snapshot
//! assert_eq!(run.font_size(), Some(36.0));
//! assert_eq!(layer.font(run.font_index().unwrap()).unwrap().postscript_name, "ArialMT");
//! assert_eq!(layer.text_position(), Some((20.0, 50.0)));
//! # Ok(())
//! # }
//! ```
//!
//! # Editing text and styles
//!
//! Text matching and every offset use UTF-16 code units, the unit of
//! Photoshop's run lengths. Replacements remap the style and paragraph runs
//! so formatting stays on the characters it belonged to. Line breaks are
//! `\r`, as Photoshop writes them.
//!
//! ```
//! use psd::{Justification, Occurrence, TextWritingDirection};
//! # fn main() -> psd::core::Result<()> {
//! let mut layer = psd::Layer::<u8>::new_text("Caption", "Hello World")?;
//! layer.replace_text("World", "Rust")?;
//!
//! // Character styles by range, by matched text, or for everything.
//! layer.style_text("Rust", Occurrence::All).set_faux_bold(true)?.set_font_size(48.0)?;
//! layer.style_range(0..5).set_underline(true)?;
//! layer.style_all().set_fill_color([1.0, 0.0, 0.4, 0.8])?; // [A, R, G, B]
//! // "Hello" | " " | "Rust" | the terminal "\r"
//! assert_eq!(layer.style_run_lengths(), Some(vec![5, 1, 4, 1]));
//!
//! // Run-level control; setters chain and are all-or-nothing.
//! layer.style_run_mut(2).set_tracking(50)?.set_font("Arial-BoldMT")?;
//! assert_eq!(layer.style_run(2).unwrap().faux_bold(), Some(true));
//! layer.paragraph_all().set_justification(Justification::Center)?;
//! layer.set_orientation(TextWritingDirection::Vertical)?;
//! layer.convert_to_point_text()?;
//! assert_eq!(layer.text().as_deref(), Some("Hello Rust"));
//! # Ok(())
//! # }
//! ```
//!
//! Properties without a typed accessor stay reachable through
//! [`CharacterStyle::property`] / [`CharacterStyleMut::set_property`] (and the
//! paragraph equivalents), and the whole EngineData tree through
//! [`Layer::engine_data`].
//!
//! # Editing an existing document
//!
//! Photoshop keeps a document-level `Txt2` text cache next to the per-layer
//! `TySh` data. [`LayeredFile`](crate::LayeredFile) notices when text layers
//! changed since reading and leaves that stale cache out when writing, so
//! Photoshop offers to update the text layers on open instead of showing
//! outdated or invisible text (see
//! [`LayeredFile::text_cache_is_stale`](crate::LayeredFile::text_cache_is_stale)).
//! The raster previews of edited layers are also stale until Photoshop
//! re-renders them; this crate does not shape or render text.
//!
//! # Scope
//!
//! Only `TySh` is an editable text source; a layer carrying just a per-layer
//! `Txt2` block is recognized and preserved but not editable (upstream
//! parity). Text stays readable and editable next to descriptor values whose
//! OSType this port cannot parse; the warp and anti-aliasing settings, which
//! need the whole descriptor, then fail with an error instead.

use psd_core::engine_data::{self, EngineValue, EngineValueKind, PayloadPatch};
use psd_core::{
    DescriptorValue, PsdError, TaggedBlockKey, TypeToolTaggedBlock, TypeToolTextPayloadSpans,
};
use std::ops::Range;

use crate::{BitDepth, Layer};

mod builder;
mod cache;
mod enums;
mod range;
mod style;
mod transform;
mod warp;

pub use builder::TextLayerBuilder;
pub(crate) use cache::TextCacheBaseline;
pub use enums::{
    AntiAliasMethod, BaselineDirection, CharacterDirection, DiacriticPosition, FontBaseline,
    FontCaps, FontScript, FontType, Justification, KinsokuOrder, LeadingType, TextShape,
    TextWarpRotation, TextWarpStyle, TextWritingDirection,
};
pub use range::{CharacterStyleRange, Occurrence, ParagraphStyleRange};
pub use style::{CharacterStyle, CharacterStyleMut, ParagraphStyle, ParagraphStyleMut};

const TYSH: TaggedBlockKey = TaggedBlockKey::new(*b"TySh");
const SENTINEL_FONT: &str = "AdobeInvisFont";

/// Box-text bounds in Photoshop text-space order: top, left, bottom, right.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextBoxBounds {
    pub top: f64,
    pub left: f64,
    pub bottom: f64,
    pub right: f64,
}

/// One entry in EngineData's `ResourceDict/FontSet`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextFont {
    pub postscript_name: String,
    pub script: FontScript,
    pub font_type: FontType,
    pub synthetic: i32,
}

impl TextFont {
    /// Photoshop's invisible fallback font (`AdobeInvisFont`), present in
    /// most FontSets but never a user-visible font.
    pub fn is_sentinel(&self) -> bool {
        self.postscript_name == SENTINEL_FONT
    }
}

impl TextBoxBounds {
    /// `right - left`.
    pub const fn width(self) -> f64 {
        self.right - self.left
    }

    /// `bottom - top`.
    pub const fn height(self) -> f64 {
        self.bottom - self.top
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TextReplacement {
    old_start: usize,
    old_length: usize,
    new_length: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TextEdit {
    new_text: Vec<u16>,
    replacements: Vec<TextReplacement>,
}

fn invalid(message: &'static str) -> PsdError {
    PsdError::InvalidData { offset: 0, message }
}

/// Exact `Txt `/EngineData spans of one TySh payload. A text descriptor
/// holding a value with an unknown OSType falls back to the exact-signature
/// scan (upstream always scans), so text stays readable and editable next to
/// values this port cannot parse.
fn tysh_spans(data: &[u8]) -> Option<TypeToolTextPayloadSpans> {
    TypeToolTaggedBlock::locate_text_payloads(data).ok()
}

/// All UTF-16 units of the `Txt ` UnicodeString at `span` (count-prefixed),
/// including trailing NUL units.
fn unicode_string_units(data: &[u8], span: &Range<usize>) -> Option<Vec<u16>> {
    let bytes = data.get(span.clone())?;
    let (count, units) = bytes.split_at_checked(4)?;
    let count = u32::from_be_bytes(count.try_into().ok()?) as usize;
    (units.len() == count.checked_mul(2)?).then(|| {
        units
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect()
    })
}

/// Visible `Txt ` units: the UnicodeString without its trailing NUL units.
fn parse_tysh_text_units(data: &[u8]) -> Option<Vec<u16>> {
    let mut units = unicode_string_units(data, tysh_spans(data)?.text.as_ref()?)?;
    while units.last() == Some(&0) {
        units.pop();
    }
    Some(units)
}

/// Parse one TySh payload's EngineData.
fn tysh_engine_data(data: &[u8]) -> Option<EngineValue> {
    let span = tysh_spans(data)?.engine_data?;
    engine_data::parse(data.get(span)?).ok()
}

fn text_units_to_string(units: &[u16]) -> Option<String> {
    String::from_utf16(units).ok()
}

fn first_engine_data(layer: &Layer<impl BitDepth>) -> Option<EngineValue> {
    layer
        .blocks
        .blocks
        .iter()
        .filter(|block| block.key == TYSH)
        .find_map(|block| tysh_engine_data(&block.data))
}

fn array_items(value: &EngineValue) -> Option<&[EngineValue]> {
    value.as_array()
}

fn integer_value(value: &EngineValue) -> Option<i32> {
    let number = value.as_number()?;
    let rounded = number.value.round();
    (number.value.is_finite()
        && (number.value - rounded).abs() <= 1e-6
        && rounded >= i32::MIN as f64
        && rounded <= i32::MAX as f64)
        .then_some(rounded as i32)
}

fn float_value(value: f64) -> EngineValue {
    EngineValue {
        span: 0..0,
        kind: EngineValueKind::Number(engine_data::EngineNumber {
            value,
            integer: None,
        }),
    }
}

fn style_run_data(root: &EngineValue, index: usize) -> Option<&EngineValue> {
    root.get_path(["EngineDict", "StyleRun", "RunArray"])?
        .as_array()?
        .get(index)?
        .get_path(["StyleSheet", "StyleSheetData"])
}

fn paragraph_run_data(root: &EngineValue, index: usize) -> Option<&EngineValue> {
    root.get_path(["EngineDict", "ParagraphRun", "RunArray"])?
        .as_array()?
        .get(index)?
        .get_path(["ParagraphSheet", "Properties"])
}

fn normal_sheet_data(root: &EngineValue, paragraph: bool) -> Option<&EngineValue> {
    let index_key = if paragraph {
        "TheNormalParagraphSheet"
    } else {
        "TheNormalStyleSheet"
    };
    let set_key = if paragraph {
        "ParagraphSheetSet"
    } else {
        "StyleSheetSet"
    };
    let normal_index = integer_value(root.get_path(["ResourceDict", index_key])?)?;
    let sheet = root
        .get_path(["ResourceDict", set_key])?
        .as_array()?
        .get(usize::try_from(normal_index).ok()?)?;
    let data = if paragraph {
        sheet.get("Properties")?
    } else {
        sheet.get("StyleSheetData")?
    };
    Some(data)
}

fn compatible_value_kinds(left: &EngineValueKind, right: &EngineValueKind) -> bool {
    let left_is_string = matches!(
        left,
        EngineValueKind::LiteralString(_)
            | EngineValueKind::LiteralStringValue(_)
            | EngineValueKind::LiteralStringBytes(_)
    );
    let right_is_string = matches!(
        right,
        EngineValueKind::LiteralString(_)
            | EngineValueKind::LiteralStringValue(_)
            | EngineValueKind::LiteralStringBytes(_)
    );
    (left_is_string && right_is_string)
        || matches!(
            (left, right),
            (EngineValueKind::Number(_), EngineValueKind::Number(_))
                | (EngineValueKind::Boolean(_), EngineValueKind::Boolean(_))
                | (EngineValueKind::Array(_), EngineValueKind::Array(_))
                | (
                    EngineValueKind::Dictionary(_),
                    EngineValueKind::Dictionary(_)
                )
                | (EngineValueKind::Name(_), EngineValueKind::Name(_))
                | (
                    EngineValueKind::Identifier(_),
                    EngineValueKind::Identifier(_)
                )
                | (EngineValueKind::HexString(_), EngineValueKind::HexString(_))
        )
}

fn property_patch(
    payload: &[u8],
    current: &EngineValue,
    replacement: EngineValue,
) -> psd_core::Result<PayloadPatch> {
    if !compatible_value_kinds(&current.kind, &replacement.kind) {
        return Err(invalid("replacement EngineData value has the wrong type"));
    }
    Ok(engine_data::replace_value_patch(
        payload,
        current,
        &replacement,
    ))
}

/// EngineData names are currently read as simple ASCII tokens without escape
/// decoding, so only insert identifiers that serialize unambiguously.
fn is_safe_engine_dictionary_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn write_dictionary_property(
    payload: &mut Vec<u8>,
    dictionary: &EngineValue,
    key: &str,
    replacement: &EngineValue,
) -> psd_core::Result<()> {
    // Deliberate fidelity delta: the generic Rust setter
    // inserts absent fields uniformly for style and paragraph dictionaries.
    // Upstream paragraph-specific setters reject missing fields, while style
    // setters already insert them.
    if let Some(current) = dictionary.get(key) {
        let patch = property_patch(payload, current, replacement.clone())?;
        engine_data::apply_patches_checked(payload, &mut [patch])
    } else {
        if !is_safe_engine_dictionary_key(key) {
            return Err(invalid(
                "new EngineData dictionary keys must be safe ASCII identifiers",
            ));
        }
        if engine_data::insert_dict_entry_bytes(payload, dictionary, key, replacement) {
            Ok(())
        } else {
            Err(invalid("cannot insert the requested EngineData property"))
        }
    }
}

fn number_patch(current: &EngineValue, value: f64) -> psd_core::Result<PayloadPatch> {
    let mut replacement = current.clone();
    if !replacement.set_number(value) {
        return Err(invalid("EngineData value is not numeric"));
    }
    Ok(PayloadPatch {
        range: current.span.clone(),
        new_bytes: engine_data::format_value_bytes(&replacement, 0),
    })
}

fn font_entries(root: &EngineValue) -> Option<&[EngineValue]> {
    array_items(root.get_path(["ResourceDict", "FontSet"])?)
}

/// Sorted, de-duplicated in-range font indices referenced by style runs.
fn used_font_indices_in(root: &EngineValue) -> Vec<usize> {
    let font_count = font_entries(root).map_or(0, <[_]>::len);
    let mut indices: Vec<_> = root
        .get_path(["EngineDict", "StyleRun", "RunArray"])
        .and_then(EngineValue::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(|run| run.get_path(["StyleSheet", "StyleSheetData", "Font"]))
        .filter_map(integer_value)
        .filter_map(|index| usize::try_from(index).ok())
        .filter(|&index| index < font_count)
        .collect();
    indices.sort_unstable();
    indices.dedup();
    indices
}

fn font_from_value(value: &EngineValue) -> Option<TextFont> {
    Some(TextFont {
        postscript_name: engine_data::decode_utf16be_literal(
            value.get("Name")?.as_literal_bytes()?,
        )
        .ok()?,
        script: FontScript::from_raw(integer_value(value.get("Script")?)?),
        font_type: FontType::from_raw(integer_value(value.get("FontType")?)?),
        synthetic: integer_value(value.get("Synthetic")?)?,
    })
}

fn engine_dictionary(items: impl IntoIterator<Item = (&'static str, EngineValue)>) -> EngineValue {
    let mut dictionary = EngineValue::dict();
    for (key, value) in items {
        dictionary.insert(key, value);
    }
    dictionary
}

fn engine_array(items: impl IntoIterator<Item = EngineValue>) -> EngineValue {
    let mut array = EngineValue::array();
    for item in items {
        array.push(item);
    }
    array
}

impl<T: BitDepth> Layer<T> {
    /// Read visible text from the first parseable TySh block. Txt2-only layers
    /// are recognized and preserved but, like upstream, do not expose text
    /// mutation until a TySh source is available.
    pub fn text(&self) -> Option<String> {
        self.blocks
            .blocks
            .iter()
            .filter(|block| block.key == TYSH)
            .find_map(|block| {
                parse_tysh_text_units(&block.data).and_then(|units| text_units_to_string(&units))
            })
    }

    /// Parse the first usable TySh EngineData payload.
    pub fn engine_data(&self) -> Option<EngineValue> {
        first_engine_data(self)
    }

    /// Writing direction from EngineData `Rendered/Shapes/WritingDirection`.
    pub fn orientation(&self) -> Option<TextWritingDirection> {
        let root = first_engine_data(self)?;
        let value = root.get_path(["EngineDict", "Rendered", "Shapes", "WritingDirection"])?;
        integer_value(value).map(TextWritingDirection::from_raw)
    }

    /// Whether the text runs vertically.
    pub fn is_vertical(&self) -> bool {
        self.orientation() == Some(TextWritingDirection::Vertical)
    }

    /// Change the writing direction: EngineData `WritingDirection` (shape and
    /// line level) and child `Procession` (1 for vertical, as Photoshop
    /// writes it), plus the TySh `Ornt` descriptor enum. Every TySh block is
    /// updated; the edit is all-or-nothing.
    ///
    /// Like upstream, `Ornt` is updated where the text descriptor parses and
    /// holds it, and skipped otherwise; EngineData is the authority.
    pub fn set_orientation(&mut self, direction: TextWritingDirection) -> psd_core::Result<()> {
        let (ornt, procession) = match direction {
            TextWritingDirection::Horizontal => ("Hrzn", 0.0),
            TextWritingDirection::Vertical => ("Vrtc", 1.0),
            TextWritingDirection::Other(_) => {
                return Err(invalid(
                    "only horizontal and vertical orientations can be written",
                ));
            }
        };
        let raw = f64::from(direction.raw());
        let snapshot = self.blocks.clone();
        self.patch_all_engine_data(|root, payload| {
            let shapes = root
                .get_path(["EngineDict", "Rendered", "Shapes"])
                .ok_or_else(|| invalid("EngineData Rendered/Shapes is missing"))?;
            let writing_direction = shapes
                .get("WritingDirection")
                .ok_or_else(|| invalid("EngineData WritingDirection is missing"))?;
            let mut patches = vec![number_patch(writing_direction, raw)?];
            for child in shapes
                .get("Children")
                .and_then(EngineValue::as_array)
                .unwrap_or_default()
            {
                if let Some(value) = child.get("Procession") {
                    patches.push(number_patch(value, procession)?);
                }
                if let Some(value) = child.get_path(["Lines", "WritingDirection"]) {
                    patches.push(number_patch(value, raw)?);
                }
            }
            engine_data::apply_patches_checked(payload, &mut patches)?;
            Ok(true)
        })?;
        let ornt_result = self.edit_tysh_descriptors(warp::TyShDescriptor::Text, |text| {
            if let Some(DescriptorValue::Enumerated { value, .. }) = text.get_mut("Ornt") {
                *value = value.replace_text_preserving_encoding(ornt)?;
            }
            Ok(())
        });
        if let Err(error) = ornt_result {
            self.blocks = snapshot;
            return Err(error);
        }
        Ok(())
    }

    /// Style-run text lengths in UTF-16 code units (including Photoshop's
    /// terminal EngineData carriage return).
    pub fn style_run_lengths(&self) -> Option<Vec<i32>> {
        first_engine_data(self)?
            .get_path(["EngineDict", "StyleRun", "RunLengthArray"])?
            .as_int32_vector()
    }

    /// Paragraph-run text lengths in UTF-16 code units.
    pub fn paragraph_run_lengths(&self) -> Option<Vec<i32>> {
        first_engine_data(self)?
            .get_path(["EngineDict", "ParagraphRun", "RunLengthArray"])?
            .as_int32_vector()
    }

    /// Number of character style runs.
    pub fn style_run_count(&self) -> usize {
        first_engine_data(self)
            .and_then(|root| {
                root.get_path(["EngineDict", "StyleRun", "RunArray"])?
                    .as_array()
                    .map(<[_]>::len)
            })
            .unwrap_or(0)
    }

    /// Number of paragraph runs.
    pub fn paragraph_run_count(&self) -> usize {
        first_engine_data(self)
            .and_then(|root| {
                root.get_path(["EngineDict", "ParagraphRun", "RunArray"])?
                    .as_array()
                    .map(<[_]>::len)
            })
            .unwrap_or(0)
    }

    /// Number of entries in `ResourceDict/StyleSheetSet`.
    pub fn style_sheet_count(&self) -> usize {
        first_engine_data(self)
            .and_then(|root| {
                root.get_path(["ResourceDict", "StyleSheetSet"])?
                    .as_array()
                    .map(<[_]>::len)
            })
            .unwrap_or(0)
    }

    /// Number of entries in `ResourceDict/ParagraphSheetSet`.
    pub fn paragraph_sheet_count(&self) -> usize {
        first_engine_data(self)
            .and_then(|root| {
                root.get_path(["ResourceDict", "ParagraphSheetSet"])?
                    .as_array()
                    .map(<[_]>::len)
            })
            .unwrap_or(0)
    }

    /// FontSet index of the first style run's font, falling back to the
    /// normal style sheet's font.
    pub fn primary_font_index(&self) -> Option<usize> {
        let root = first_engine_data(self)?;
        let font = style_run_data(&root, 0)
            .and_then(|style| style.get("Font"))
            .or_else(|| normal_sheet_data(&root, false)?.get("Font"))?;
        usize::try_from(integer_value(font)?).ok()
    }

    /// PostScript name of [`primary_font_index`](Self::primary_font_index).
    pub fn primary_font_name(&self) -> Option<String> {
        self.font(self.primary_font_index()?)
            .map(|font| font.postscript_name)
    }

    /// Number of entries in `ResourceDict/FontSet`.
    pub fn font_count(&self) -> usize {
        first_engine_data(self)
            .and_then(|root| font_entries(&root).map(<[_]>::len))
            .unwrap_or(0)
    }

    /// Every decodable `ResourceDict/FontSet` entry in index order.
    /// Photoshop always writes complete entries; a malformed one is skipped
    /// here, so use [`font`](Self::font) for index-exact access.
    pub fn fonts(&self) -> Vec<TextFont> {
        first_engine_data(self)
            .and_then(|root| {
                font_entries(&root)
                    .map(|entries| entries.iter().filter_map(font_from_value).collect())
            })
            .unwrap_or_default()
    }

    /// The FontSet entry at `index`.
    pub fn font(&self, index: usize) -> Option<TextFont> {
        font_from_value(font_entries(&first_engine_data(self)?)?.get(index)?)
    }

    /// FontSet index of the first entry with this PostScript name.
    pub fn font_index(&self, postscript_name: &str) -> Option<usize> {
        font_entries(&first_engine_data(self)?)?
            .iter()
            .position(|entry| {
                font_from_value(entry).is_some_and(|font| font.postscript_name == postscript_name)
            })
    }

    /// Sorted, de-duplicated FontSet indices referenced by the style runs.
    pub fn used_font_indices(&self) -> Vec<usize> {
        let Some(root) = first_engine_data(self) else {
            return Vec::new();
        };
        used_font_indices_in(&root)
    }

    /// PostScript names of [`used_font_indices`](Self::used_font_indices),
    /// excluding empty names and the `AdobeInvisFont` sentinel.
    pub fn used_font_names(&self) -> Vec<String> {
        let Some(root) = first_engine_data(self) else {
            return Vec::new();
        };
        let entries = font_entries(&root).unwrap_or_default();
        used_font_indices_in(&root)
            .into_iter()
            .filter_map(|index| font_from_value(entries.get(index)?))
            .map(|font| font.postscript_name)
            .filter(|name| !name.is_empty() && name != SENTINEL_FONT)
            .collect()
    }

    /// Whether the FontSet entry at `index` is the `AdobeInvisFont` sentinel.
    pub fn is_sentinel_font(&self, index: usize) -> bool {
        self.font(index).is_some_and(|font| font.is_sentinel())
    }

    /// Append a FontSet entry (mirrored into `DocumentResources/FontSet`
    /// when present) and return its index.
    pub fn add_font(
        &mut self,
        postscript_name: &str,
        font_type: FontType,
        script: FontScript,
        synthetic: i32,
    ) -> psd_core::Result<usize> {
        let mut new_index = None;
        self.patch_engine_data(|root, payload| {
            let font_set = root
                .get_path(["ResourceDict", "FontSet"])
                .ok_or_else(|| invalid("ResourceDict/FontSet is missing"))?;
            let count = font_set
                .as_array()
                .ok_or_else(|| invalid("ResourceDict/FontSet is not an array"))?
                .len();
            let mut font = EngineValue::dict();
            font.insert(
                "Name",
                EngineValue::literal_string_bytes(engine_data::encode_utf16be_literal(
                    postscript_name,
                )),
            );
            font.insert("Script", EngineValue::integer(i64::from(script.raw())));
            font.insert("FontType", EngineValue::integer(i64::from(font_type.raw())));
            font.insert("Synthetic", EngineValue::integer(i64::from(synthetic)));
            if !engine_data::insert_array_item_bytes(payload, font_set, &font) {
                return Err(invalid("cannot append an entry to ResourceDict/FontSet"));
            }
            let reparsed = engine_data::parse(payload)?;
            if let Some(document_font_set) = reparsed.get_path(["DocumentResources", "FontSet"]) {
                if document_font_set.as_array().is_none()
                    || !engine_data::insert_array_item_bytes(payload, document_font_set, &font)
                {
                    return Err(invalid("DocumentResources/FontSet is not an array"));
                }
            }
            new_index = Some(count);
            Ok(true)
        })?;
        new_index.ok_or_else(|| invalid("font could not be added"))
    }

    /// Index of the named font, adding it with the given metadata if absent.
    pub fn find_or_add_font(
        &mut self,
        postscript_name: &str,
        font_type: FontType,
        script: FontScript,
        synthetic: i32,
    ) -> psd_core::Result<usize> {
        if let Some(index) = self.font_index(postscript_name) {
            Ok(index)
        } else {
            self.add_font(postscript_name, font_type, script, synthetic)
        }
    }

    /// Assign one font to every character run and the normal character
    /// sheet, adding it to the FontSet (as OpenType Roman) when needed. Runs
    /// without an explicit `Font` get one, like upstream's per-run setters.
    /// All-or-nothing.
    pub fn set_font(&mut self, postscript_name: &str) -> psd_core::Result<()> {
        self.transaction(|layer| {
            let index = layer.find_or_add_font(
                postscript_name,
                FontType::OpenType,
                FontScript::Roman,
                0,
            )?;
            for run in 0..layer.style_run_count() {
                layer.style_run_mut(run).set_font_index(index)?;
            }
            layer.style_normal_mut().set_font_index(index)?;
            Ok(())
        })
    }

    /// Rename the FontSet entry at `index` (and its `DocumentResources`
    /// mirror when present).
    pub fn rename_font(&mut self, index: usize, postscript_name: &str) -> psd_core::Result<()> {
        let replacement =
            EngineValue::literal_string_bytes(engine_data::encode_utf16be_literal(postscript_name));
        self.patch_engine_data(|root, payload| {
            let name = font_entries(root)
                .and_then(|entries| entries.get(index))
                .and_then(|entry| entry.get("Name"))
                .ok_or_else(|| invalid("font index is out of range or has no Name"))?;
            let mut patches = vec![property_patch(payload, name, replacement.clone())?];
            if let Some(mirror) = root
                .get_path(["DocumentResources", "FontSet"])
                .and_then(EngineValue::as_array)
                .and_then(|entries| entries.get(index))
                .and_then(|entry| entry.get("Name"))
            {
                patches.push(property_patch(payload, mirror, replacement.clone())?);
            }
            engine_data::apply_patches_checked(payload, &mut patches)?;
            Ok(true)
        })
    }

    /// Run `edit`, restoring every tagged block if it fails, so multi-step
    /// operations are all-or-nothing.
    pub(crate) fn transaction<R>(
        &mut self,
        edit: impl FnOnce(&mut Self) -> psd_core::Result<R>,
    ) -> psd_core::Result<R> {
        let snapshot = self.blocks.clone();
        let result = edit(self);
        if result.is_err() {
            self.blocks = snapshot;
        }
        result
    }

    /// Split style run `run` at `code_unit_offset` (UTF-16 units from the
    /// run start). Both halves keep the original style. Like upstream, every
    /// parseable TySh block is split, so duplicate blocks keep matching run
    /// arrays; the operation is all-or-nothing.
    pub fn split_style_run(&mut self, run: usize, code_unit_offset: usize) -> psd_core::Result<()> {
        self.split_run(run, code_unit_offset, true)
    }

    /// Split paragraph run `run` at `code_unit_offset`; see
    /// [`split_style_run`](Self::split_style_run).
    pub fn split_paragraph_run(
        &mut self,
        run: usize,
        code_unit_offset: usize,
    ) -> psd_core::Result<()> {
        self.split_run(run, code_unit_offset, false)
    }

    fn split_run(&mut self, run: usize, offset: usize, style: bool) -> psd_core::Result<()> {
        self.patch_all_engine_data(|root, payload| {
            let run_path = if style {
                ["EngineDict", "StyleRun", "RunArray"]
            } else {
                ["EngineDict", "ParagraphRun", "RunArray"]
            };
            let length_path = if style {
                ["EngineDict", "StyleRun", "RunLengthArray"]
            } else {
                ["EngineDict", "ParagraphRun", "RunLengthArray"]
            };
            let original_runs = root
                .get_path(run_path)
                .ok_or_else(|| invalid("text run array is missing"))?
                .as_array()
                .ok_or_else(|| invalid("text run array is not an array"))?;
            let original_lengths = root
                .get_path(length_path)
                .ok_or_else(|| invalid("text run lengths are missing"))?
                .as_int32_vector()
                .ok_or_else(|| invalid("text run lengths are not an integer array"))?;
            if run >= original_runs.len() || run >= original_lengths.len() {
                return Err(invalid("text run index is out of range"));
            }
            let current_length = usize::try_from(original_lengths[run])
                .map_err(|_| invalid("text run length is negative"))?;
            if offset == 0 || offset >= current_length {
                return Err(invalid("text run split offset must be inside the run"));
            }
            let mut new_runs = root.get_path(run_path).unwrap().clone();
            let EngineValueKind::Array(items) = &mut new_runs.kind else {
                return Err(invalid("text run array is not an array"));
            };
            items.insert(run + 1, items[run].clone());
            let mut new_lengths = original_lengths;
            new_lengths[run] =
                i32::try_from(offset).map_err(|_| invalid("split offset exceeds i32"))?;
            new_lengths.insert(
                run + 1,
                i32::try_from(current_length - offset)
                    .map_err(|_| invalid("run length exceeds i32"))?,
            );
            let old_lengths = root.get_path(length_path).unwrap();
            let replacement_lengths = EngineValue {
                span: old_lengths.span.clone(),
                kind: EngineValueKind::Array(
                    new_lengths
                        .into_iter()
                        .map(|value| EngineValue::integer(i64::from(value)))
                        .collect(),
                ),
            };
            let mut patches = vec![
                engine_data::replace_value_patch(
                    payload,
                    root.get_path(run_path).unwrap(),
                    &new_runs,
                ),
                engine_data::replace_value_patch(payload, old_lengths, &replacement_lengths),
            ];
            engine_data::apply_patches_checked(payload, &mut patches)?;
            Ok(true)
        })
    }

    /// Return point/box classification from the primary EngineData shape path.
    pub fn text_shape(&self) -> Option<TextShape> {
        let root = first_engine_data(self)?;
        let child = first_shape_child(&root)?;
        integer_value(child.get_path(["Cookie", "Photoshop", "ShapeType"])?)
            .map(TextShape::from_raw)
    }

    /// Area (box) text, bounded by `BoxBounds`.
    pub fn is_box_text(&self) -> bool {
        self.text_shape() == Some(TextShape::Box)
    }

    /// Point text, anchored at a single point.
    pub fn is_point_text(&self) -> bool {
        self.text_shape() == Some(TextShape::Point)
    }

    /// Box-text bounds in `{top, left, bottom, right}` order. Point text or
    /// malformed shape data returns `None`.
    pub fn box_bounds(&self) -> Option<TextBoxBounds> {
        let root = first_engine_data(self)?;
        let child = first_shape_child(&root)?;
        let values = child
            .get_path(["Cookie", "Photoshop", "BoxBounds"])?
            .as_double_vector()?;
        if values.len() < 4 || !values[..4].iter().all(|value| value.is_finite()) {
            return None;
        }
        Some(TextBoxBounds {
            top: values[0],
            left: values[1],
            bottom: values[3],
            right: values[2],
        })
    }

    /// Box-text width; `None` for point text.
    pub fn box_width(&self) -> Option<f64> {
        self.box_bounds().map(TextBoxBounds::width)
    }

    /// Box-text height; `None` for point text.
    pub fn box_height(&self) -> Option<f64> {
        self.box_bounds().map(TextBoxBounds::height)
    }

    /// Replace the box bounds of box text (all values finite).
    pub fn set_box_bounds(&mut self, bounds: TextBoxBounds) -> psd_core::Result<()> {
        if ![bounds.top, bounds.left, bounds.bottom, bounds.right]
            .iter()
            .all(|value| value.is_finite())
        {
            return Err(invalid("text box bounds must be finite"));
        }
        if self.text_shape() != Some(TextShape::Box) {
            return Err(invalid("layer is not box text"));
        }
        self.patch_engine_data(|root, payload| {
            let child = first_shape_child(root)
                .ok_or_else(|| invalid("EngineData has no text shape child"))?;
            let current = child
                .get_path(["Cookie", "Photoshop", "BoxBounds"])
                .ok_or_else(|| invalid("EngineData has no BoxBounds array"))?;
            let mut replacement = EngineValue::array();
            for value in [bounds.top, bounds.left, bounds.right, bounds.bottom] {
                replacement.push(EngineValue::number(value));
            }
            let patch = property_patch(payload, current, replacement)?;
            engine_data::apply_patches_checked(payload, &mut [patch])?;
            Ok(true)
        })
    }

    /// Resize the box keeping its top-left corner.
    pub fn set_box_size(&mut self, width: f64, height: f64) -> psd_core::Result<()> {
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return Err(invalid("text box size must be finite and positive"));
        }
        let mut bounds = self
            .box_bounds()
            .ok_or_else(|| invalid("layer has no box-text bounds"))?;
        bounds.right = bounds.left + width;
        bounds.bottom = bounds.top + height;
        self.set_box_bounds(bounds)
    }

    /// Resize the box horizontally, keeping `left`, `top`, and the height.
    pub fn set_box_width(&mut self, width: f64) -> psd_core::Result<()> {
        if !width.is_finite() || width <= 0.0 {
            return Err(invalid("text box width must be finite and positive"));
        }
        let mut bounds = self
            .box_bounds()
            .ok_or_else(|| invalid("layer has no box-text bounds"))?;
        bounds.right = bounds.left + width;
        self.set_box_bounds(bounds)
    }

    /// Resize the box vertically, keeping `top`, `left`, and the width.
    pub fn set_box_height(&mut self, height: f64) -> psd_core::Result<()> {
        if !height.is_finite() || height <= 0.0 {
            return Err(invalid("text box height must be finite and positive"));
        }
        let mut bounds = self
            .box_bounds()
            .ok_or_else(|| invalid("layer has no box-text bounds"))?;
        bounds.bottom = bounds.top + height;
        self.set_box_bounds(bounds)
    }

    /// Turn point text into box text of the given size at the text origin;
    /// fails when the layer already is box text.
    pub fn convert_to_box_text(&mut self, width: f64, height: f64) -> psd_core::Result<()> {
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return Err(invalid(
                "box-text width and height must be finite and positive",
            ));
        }
        self.patch_engine_data(|root, payload| {
            let child = first_shape_child(root)
                .ok_or_else(|| invalid("EngineData has no text shape child"))?;
            if shape_type_value(child) == Some(1) {
                return Err(invalid("layer is already box text"));
            }
            let top_shape = child
                .get("ShapeType")
                .ok_or_else(|| invalid("top-level ShapeType is missing"))?;
            let photoshop = child
                .get_path(["Cookie", "Photoshop"])
                .ok_or_else(|| invalid("Cookie/Photoshop dictionary is missing"))?;
            let mut updated_photoshop = photoshop.clone();
            let shape = updated_photoshop
                .get_mut("ShapeType")
                .ok_or_else(|| invalid("Photoshop ShapeType is missing"))?;
            if !shape.set_number(1.0) {
                return Err(invalid("Photoshop ShapeType is not numeric"));
            }
            let base_shape = updated_photoshop
                .get_mut("Base")
                .and_then(|base| base.get_mut("ShapeType"))
                .ok_or_else(|| invalid("base ShapeType is missing"))?;
            if !base_shape.set_number(1.0) {
                return Err(invalid("base ShapeType is not numeric"));
            }
            if !updated_photoshop.remove("PointBase") {
                return Err(invalid("point-text PointBase is missing"));
            }
            updated_photoshop.insert(
                "BoxBounds",
                engine_array([0.0, 0.0, width, height].into_iter().map(float_value)),
            );
            let mut patches = vec![
                number_patch(top_shape, 1.0)?,
                property_patch(payload, photoshop, updated_photoshop)?,
            ];
            engine_data::apply_patches_checked(payload, &mut patches)?;
            Ok(true)
        })
    }

    /// Turn box text into point text; fails when it already is point text.
    pub fn convert_to_point_text(&mut self) -> psd_core::Result<()> {
        self.patch_engine_data(|root, payload| {
            let child = first_shape_child(root)
                .ok_or_else(|| invalid("EngineData has no text shape child"))?;
            if shape_type_value(child) == Some(0) {
                return Err(invalid("layer is already point text"));
            }
            let top_shape = child
                .get("ShapeType")
                .ok_or_else(|| invalid("top-level ShapeType is missing"))?;
            let photoshop = child
                .get_path(["Cookie", "Photoshop"])
                .ok_or_else(|| invalid("Cookie/Photoshop dictionary is missing"))?;
            let mut updated_photoshop = photoshop.clone();
            let shape = updated_photoshop
                .get_mut("ShapeType")
                .ok_or_else(|| invalid("Photoshop ShapeType is missing"))?;
            if !shape.set_number(0.0) {
                return Err(invalid("Photoshop ShapeType is not numeric"));
            }
            let base_shape = updated_photoshop
                .get_mut("Base")
                .and_then(|base| base.get_mut("ShapeType"))
                .ok_or_else(|| invalid("base ShapeType is missing"))?;
            if !base_shape.set_number(0.0) {
                return Err(invalid("base ShapeType is not numeric"));
            }
            if !updated_photoshop.remove("BoxBounds") {
                return Err(invalid("box-text BoxBounds is missing"));
            }
            updated_photoshop.insert(
                "PointBase",
                engine_array([float_value(0.0), float_value(0.0)]),
            );
            let mut patches = vec![
                number_patch(top_shape, 0.0)?,
                property_patch(payload, photoshop, updated_photoshop)?,
            ];
            engine_data::apply_patches_checked(payload, &mut patches)?;
            Ok(true)
        })
    }

    /// Replace the visible text while preserving the old UTF-16 tail (usually
    /// the terminal carriage return), run boundaries, and opaque TySh bytes.
    pub fn set_text(&mut self, new_text: &str) -> psd_core::Result<()> {
        let replacement: Vec<u16> = new_text.encode_utf16().collect();
        self.apply_text_edits(|old| {
            if old == replacement {
                return Ok(None);
            }
            Ok(Some(TextEdit {
                new_text: replacement.clone(),
                replacements: vec![TextReplacement {
                    old_start: 0,
                    old_length: old.len(),
                    new_length: replacement.len(),
                }],
            }))
        })
    }

    /// Strict replacement variant requiring equal UTF-16 code-unit lengths.
    pub fn set_text_equal_length(&mut self, new_text: &str) -> psd_core::Result<()> {
        let replacement: Vec<u16> = new_text.encode_utf16().collect();
        self.apply_text_edits(|old| {
            if old.len() != replacement.len() {
                return Err(invalid(
                    "replacement text must have equal UTF-16 code-unit length",
                ));
            }
            if old == replacement {
                return Ok(None);
            }
            Ok(Some(TextEdit {
                new_text: replacement.clone(),
                replacements: vec![TextReplacement {
                    old_start: 0,
                    old_length: old.len(),
                    new_length: replacement.len(),
                }],
            }))
        })
    }

    /// Replace non-overlapping occurrences of `old_text`. Matches are
    /// searched left-to-right in UTF-16 code units; a missing match is a no-op.
    pub fn replace_text(&mut self, old_text: &str, new_text: &str) -> psd_core::Result<()> {
        self.replace_text_with_options(old_text, new_text, true)
    }

    /// Replace text with an explicit all-versus-first occurrence choice.
    pub fn replace_text_with_options(
        &mut self,
        old_text: &str,
        new_text: &str,
        replace_all: bool,
    ) -> psd_core::Result<()> {
        let old: Vec<u16> = old_text.encode_utf16().collect();
        if old.is_empty() {
            return Err(invalid("old text must not be empty"));
        }
        let new: Vec<u16> = new_text.encode_utf16().collect();
        self.apply_text_edits(|source| Ok(replace_utf16(source, &old, &new, replace_all)))
    }

    /// Equal-length replacement of non-overlapping text occurrences.
    pub fn replace_text_equal_length(
        &mut self,
        old_text: &str,
        new_text: &str,
    ) -> psd_core::Result<()> {
        self.replace_text_equal_length_with_options(old_text, new_text, true)
    }

    /// [`replace_text_equal_length`](Self::replace_text_equal_length) with an
    /// explicit all-versus-first occurrence choice.
    pub fn replace_text_equal_length_with_options(
        &mut self,
        old_text: &str,
        new_text: &str,
        replace_all: bool,
    ) -> psd_core::Result<()> {
        let old: Vec<u16> = old_text.encode_utf16().collect();
        let new: Vec<u16> = new_text.encode_utf16().collect();
        if old.is_empty() {
            return Err(invalid("old text must not be empty"));
        }
        if old.len() != new.len() {
            return Err(invalid(
                "replacement text must have equal UTF-16 code-unit length",
            ));
        }
        self.apply_text_edits(|source| Ok(replace_utf16(source, &old, &new, replace_all)))
    }

    fn apply_text_edits(
        &mut self,
        mut make_edit: impl FnMut(&[u16]) -> psd_core::Result<Option<TextEdit>>,
    ) -> psd_core::Result<()> {
        if !self.is_text_layer() {
            return Err(invalid("layer has no TySh or Txt2 text metadata"));
        }

        // Deliberate fidelity delta: upstream mutates each
        // parseable TySh block in place and can fail after earlier blocks have
        // changed. Stage every block so a failed multi-TySh edit is atomic.
        let mut staged = self.blocks.clone();
        let mut found_parseable = false;
        for block in &mut staged.blocks {
            if block.key != TYSH {
                continue;
            }
            let Some(old_text) = parse_tysh_text_units(&block.data) else {
                continue;
            };
            found_parseable = true;
            if let Some(edit) = make_edit(&old_text)? {
                if edit.new_text != old_text {
                    block.data = mutate_tysh_text(&block.data, &old_text, &edit)?;
                }
            }
        }
        if !found_parseable {
            return Err(invalid("layer has no parseable TySh text payload"));
        }
        self.blocks = staged;
        Ok(())
    }

    /// Patch the first parseable TySh EngineData payload whose `edit`
    /// returns `true` (upstream's policy for property setters).
    fn patch_engine_data(
        &mut self,
        edit: impl FnMut(&EngineValue, &mut Vec<u8>) -> psd_core::Result<bool>,
    ) -> psd_core::Result<()> {
        self.patch_engine_data_blocks(false, edit)
    }

    /// Patch every parseable TySh EngineData payload (upstream's policy for
    /// structural edits such as run splits). Staged: any error changes nothing.
    fn patch_all_engine_data(
        &mut self,
        edit: impl FnMut(&EngineValue, &mut Vec<u8>) -> psd_core::Result<bool>,
    ) -> psd_core::Result<()> {
        self.patch_engine_data_blocks(true, edit)
    }

    fn patch_engine_data_blocks(
        &mut self,
        all_blocks: bool,
        mut edit: impl FnMut(&EngineValue, &mut Vec<u8>) -> psd_core::Result<bool>,
    ) -> psd_core::Result<()> {
        if !self.is_text_layer() {
            return Err(invalid("layer has no TySh or Txt2 text metadata"));
        }
        let mut staged = self.blocks.clone();
        let mut patched = false;
        for block in &mut staged.blocks {
            if block.key != TYSH {
                continue;
            }
            let Some(spans) = tysh_spans(&block.data) else {
                continue;
            };
            let Some(engine_span) = spans.engine_data else {
                continue;
            };
            let Some(payload_slice) = block.data.get(engine_span) else {
                continue;
            };
            let mut payload = payload_slice.to_vec();
            let Ok(root) = engine_data::parse(&payload) else {
                continue;
            };
            if edit(&root, &mut payload)? {
                block.data = replace_engine_data_payload(&block.data, &payload)?;
                patched = true;
                if !all_blocks {
                    break;
                }
            }
        }
        if !patched {
            return Err(invalid(
                "no TySh EngineData payload contained the requested target",
            ));
        }
        self.blocks = staged;
        Ok(())
    }
}

fn first_shape_child(root: &EngineValue) -> Option<&EngineValue> {
    root.get_path(["EngineDict", "Rendered", "Shapes", "Children"])?
        .as_array()?
        .first()
}

fn shape_type_value(child: &EngineValue) -> Option<i32> {
    integer_value(child.get_path(["Cookie", "Photoshop", "ShapeType"])?)
}

fn replace_utf16(source: &[u16], old: &[u16], new: &[u16], replace_all: bool) -> Option<TextEdit> {
    if old.is_empty() || old.len() > source.len() {
        return None;
    }
    let mut cursor = 0;
    let mut search = 0;
    let mut output = Vec::with_capacity(source.len());
    let mut replacements = Vec::new();
    while search + old.len() <= source.len() {
        let Some(relative) = source[search..]
            .windows(old.len())
            .position(|candidate| candidate == old)
        else {
            break;
        };
        let start = search + relative;
        output.extend_from_slice(&source[cursor..start]);
        output.extend_from_slice(new);
        replacements.push(TextReplacement {
            old_start: start,
            old_length: old.len(),
            new_length: new.len(),
        });
        cursor = start + old.len();
        search = cursor;
        if !replace_all {
            break;
        }
    }
    if replacements.is_empty() {
        return None;
    }
    output.extend_from_slice(&source[cursor..]);
    Some(TextEdit {
        new_text: output,
        replacements,
    })
}

fn mutate_tysh_text(data: &[u8], old_text: &[u16], edit: &TextEdit) -> psd_core::Result<Vec<u8>> {
    let spans = TypeToolTaggedBlock::locate_text_payloads(data)?;
    let engine_span = spans
        .engine_data
        .as_ref()
        .ok_or_else(|| invalid("TySh has no EngineData raw-data payload"))?;
    let text_span = spans
        .text
        .as_ref()
        .ok_or_else(|| invalid("TySh has no Txt UnicodeString payload"))?;
    if engine_span.start < 4 || engine_span.end > data.len() || text_span.end > data.len() {
        return Err(invalid("TySh text payload span is outside the block"));
    }

    let engine_len_offset = engine_span.start - 4;
    let declared_engine_len = u32::from_be_bytes(
        data[engine_len_offset..engine_span.start]
            .try_into()
            .map_err(|_| invalid("invalid TySh EngineData length field"))?,
    ) as usize;
    if declared_engine_len != engine_span.len() {
        return Err(invalid(
            "TySh EngineData length does not match its payload span",
        ));
    }

    let full_text_units = unicode_string_units(data, text_span)
        .ok_or_else(|| invalid("TySh Txt span does not match its UTF-16 unit count"))?;
    let null_suffix = full_text_units
        .iter()
        .rev()
        .take_while(|&&unit| unit == 0)
        .count();
    if full_text_units.len().checked_sub(null_suffix) != Some(old_text.len())
        || full_text_units[..old_text.len()] != *old_text
    {
        return Err(invalid("TySh Txt value changed while preparing text edit"));
    }
    let engine_payload = &data[engine_span.clone()];
    let mut engine_root = engine_data::parse(engine_payload)?;
    let (engine_units, has_bom, engine_text_span) = {
        let engine_text = editor_text(&mut engine_root)
            .ok_or_else(|| invalid("EngineData has no EngineDict/Editor/Text literal"))?;
        let source = engine_text
            .as_literal_bytes()
            .ok_or_else(|| invalid("EngineDict/Editor/Text is not a literal string"))?;
        let decoded = engine_data::decode_utf16be_literal_units(source)?;
        (decoded.0, decoded.1, engine_text.span.clone())
    };
    if engine_units.len() < old_text.len() || engine_units[..old_text.len()] != *old_text {
        return Err(invalid(
            "EngineData text does not begin with the TySh Txt value",
        ));
    }

    // Checked before the run remap below rewrites the parsed run lengths.
    let paragraphs_were_aligned = paragraph_runs_aligned(&engine_root, &engine_units);
    let mut new_engine_units = edit.new_text.clone();
    new_engine_units.extend_from_slice(&engine_units[old_text.len()..]);
    let new_engine_contents = engine_data::encode_utf16be_literal_units(&new_engine_units, has_bom);
    let old_engine_unit_count = engine_units.len();
    let mut engine_patches = vec![PayloadPatch {
        range: engine_text_span,
        new_bytes: {
            let mut token = vec![b'('];
            token.extend_from_slice(&new_engine_contents);
            token.push(b')');
            token
        },
    }];
    remap_run_length_arrays(
        &mut engine_root,
        old_engine_unit_count,
        old_text.len(),
        &edit.replacements,
        &mut engine_patches,
    );
    let mut new_engine_payload = engine_payload.to_vec();
    engine_data::apply_patches_checked(&mut new_engine_payload, &mut engine_patches)?;
    if paragraphs_were_aligned {
        align_paragraph_runs(&mut new_engine_payload)?;
    }
    let new_engine_len = u32::try_from(new_engine_payload.len())
        .map_err(|_| invalid("EngineData payload exceeds its u32 length field"))?;

    let mut new_text_value = Vec::with_capacity(4 + (edit.new_text.len() + null_suffix) * 2);
    let new_text_units = edit
        .new_text
        .len()
        .checked_add(null_suffix)
        .ok_or_else(|| invalid("new text unit count overflow"))?;
    let new_text_count = u32::try_from(new_text_units)
        .map_err(|_| invalid("new text exceeds the UnicodeString u32 unit limit"))?;
    new_text_value.extend_from_slice(&new_text_count.to_be_bytes());
    for &unit in &edit.new_text {
        new_text_value.extend_from_slice(&unit.to_be_bytes());
    }
    for _ in 0..null_suffix {
        new_text_value.extend_from_slice(&0u16.to_be_bytes());
    }

    let mut block_patches = vec![
        PayloadPatch {
            range: text_span.clone(),
            new_bytes: new_text_value,
        },
        PayloadPatch {
            range: engine_len_offset..engine_span.start,
            new_bytes: new_engine_len.to_be_bytes().to_vec(),
        },
        PayloadPatch {
            range: engine_span.clone(),
            new_bytes: new_engine_payload,
        },
    ];
    let mut updated = data.to_vec();
    engine_data::apply_patches_checked(&mut updated, &mut block_patches)?;
    remap_legacy_descriptor_ranges(updated, old_text.len(), &edit.replacements)
}

/// UTF-16 lengths of the `\r`-terminated paragraphs of EngineData text (a
/// trailing unterminated remainder counts as a final paragraph).
fn paragraph_lengths(units: &[u16]) -> Vec<usize> {
    let mut lengths: Vec<usize> = units
        .split_inclusive(|&unit| unit == u16::from(b'\r'))
        .map(<[u16]>::len)
        .collect();
    if lengths.is_empty() {
        lengths.push(0);
    }
    lengths
}

/// The paragraph `RunArray`/`RunLengthArray` pair and run lengths, when they
/// are consistent with each other.
fn paragraph_runs(root: &EngineValue) -> Option<(&EngineValue, &EngineValue, Vec<usize>)> {
    let run = root.get_path(["EngineDict", "ParagraphRun"])?;
    let entries = run.get("RunArray")?;
    let lengths_value = run.get("RunLengthArray")?;
    let lengths = lengths_value
        .as_int32_vector()?
        .into_iter()
        .map(|length| usize::try_from(length).ok())
        .collect::<Option<Vec<_>>>()?;
    (entries.as_array()?.len() == lengths.len()).then_some((entries, lengths_value, lengths))
}

/// Whether the paragraph runs cover exactly one paragraph each, the layout
/// Photoshop writes (e.g. `Line 1\rLine 2\r` has paragraph runs `[7, 7]`).
fn paragraph_runs_aligned(root: &EngineValue, units: &[u16]) -> bool {
    paragraph_runs(root).is_some_and(|(_, _, lengths)| lengths == paragraph_lengths(units))
}

/// Rebuild the paragraph runs on the current paragraph boundaries after an
/// edit added or removed `\r`: each paragraph takes the run covering its
/// first character, so a split keeps both halves' attributes and a join
/// keeps the first paragraph's, as Photoshop does.
///
/// Deliberate fidelity delta: upstream only remaps run
/// lengths, which leaves one paragraph run spanning several paragraphs (or
/// several runs inside one); only runs that were aligned before the edit are
/// re-aligned, so deliberate mid-paragraph splits are kept.
fn align_paragraph_runs(payload: &mut Vec<u8>) -> psd_core::Result<()> {
    let root = engine_data::parse(payload)?;
    let Some(text) = root
        .get_path(["EngineDict", "Editor", "Text"])
        .and_then(EngineValue::as_literal_bytes)
    else {
        return Ok(());
    };
    let (units, _) = engine_data::decode_utf16be_literal_units(text)?;
    let paragraphs = paragraph_lengths(&units);
    let Some((entries, lengths_value, lengths)) = paragraph_runs(&root) else {
        return Ok(());
    };
    if lengths == paragraphs || lengths.iter().sum::<usize>() != units.len() {
        return Ok(());
    }
    let items = entries.as_array().unwrap_or_default();
    let mut run_ends = Vec::with_capacity(lengths.len());
    let mut end = 0;
    for length in &lengths {
        end += length;
        run_ends.push(end);
    }
    let mut new_entries = EngineValue::array();
    let mut start = 0;
    for &length in &paragraphs {
        let run = run_ends
            .iter()
            .position(|&run_end| start < run_end)
            .unwrap_or(items.len() - 1);
        new_entries.push(items[run].clone());
        start += length;
    }
    let mut new_lengths = EngineValue::array();
    for &length in &paragraphs {
        let length = i64::try_from(length)
            .ok()
            .filter(|&length| length <= i64::from(i32::MAX))
            .ok_or_else(|| invalid("paragraph length exceeds the EngineData i32 range"))?;
        new_lengths.push(EngineValue::integer(length));
    }
    let mut patches = [
        engine_data::replace_value_patch(payload, entries, &new_entries),
        engine_data::replace_value_patch(payload, lengths_value, &new_lengths),
    ];
    engine_data::apply_patches_checked(payload, &mut patches)
}

fn editor_text(root: &mut EngineValue) -> Option<&mut EngineValue> {
    root.get_mut("EngineDict")?
        .get_mut("Editor")?
        .get_mut("Text")
}

fn remap_run_length_arrays(
    node: &mut EngineValue,
    old_engine_units: usize,
    old_visible_units: usize,
    replacements: &[TextReplacement],
    patches: &mut Vec<PayloadPatch>,
) {
    if let EngineValueKind::Dictionary(items) = &mut node.kind {
        for (key, child) in items {
            if key == "RunLengthArray" {
                if let Some(lengths) = child.as_int32_vector() {
                    let total = lengths.iter().try_fold(0usize, |sum, length| {
                        usize::try_from(*length)
                            .ok()
                            .and_then(|length| sum.checked_add(length))
                    });
                    if !lengths.is_empty() && total == Some(old_engine_units) {
                        if let Some(remapped) =
                            remap_lengths(&lengths, old_visible_units, replacements)
                        {
                            let span = child.span.clone();
                            if child.set_int32_array(&remapped) {
                                patches.push(PayloadPatch {
                                    range: span,
                                    new_bytes: engine_data::format_value_bytes(child, 0),
                                });
                            }
                        }
                    }
                }
            }
            remap_run_length_arrays(
                child,
                old_engine_units,
                old_visible_units,
                replacements,
                patches,
            );
        }
    } else if let EngineValueKind::Array(items) = &mut node.kind {
        for item in items {
            remap_run_length_arrays(
                item,
                old_engine_units,
                old_visible_units,
                replacements,
                patches,
            );
        }
    }
}

fn remap_lengths(
    lengths: &[i32],
    old_visible_units: usize,
    replacements: &[TextReplacement],
) -> Option<Vec<i32>> {
    let mut old_boundaries = Vec::with_capacity(lengths.len() + 1);
    old_boundaries.push(0usize);
    for &length in lengths {
        let length = usize::try_from(length).ok()?;
        old_boundaries.push(old_boundaries.last()?.checked_add(length)?);
    }

    let mut new_boundaries = Vec::with_capacity(old_boundaries.len());
    for boundary in old_boundaries {
        let mapped = remap_index(boundary, old_visible_units, replacements)?;
        new_boundaries.push(new_boundaries.last().copied().unwrap_or(0).max(mapped));
    }

    new_boundaries
        .windows(2)
        .map(|pair| i32::try_from(pair[1] - pair[0]).ok())
        .collect()
}

fn remap_index(
    index: usize,
    old_visible_units: usize,
    replacements: &[TextReplacement],
) -> Option<usize> {
    let mut delta = 0i128;
    let mut remapped_plain = None;
    for replacement in replacements {
        let start = replacement.old_start;
        let end = start.checked_add(replacement.old_length)?;
        if end > old_visible_units {
            return None;
        }
        if index < start {
            break;
        }
        if index > end {
            delta += replacement.new_length as i128 - replacement.old_length as i128;
            continue;
        }
        let mapped = if index == start {
            start as i128 + delta
        } else if index == end {
            start as i128 + delta + replacement.new_length as i128
        } else {
            start as i128 + delta + (index - start).min(replacement.new_length) as i128
        };
        remapped_plain = Some(usize::try_from(mapped).ok()?);
        break;
    }
    if let Some(mapped) = remapped_plain {
        return Some(mapped);
    }
    let mapped = index as i128 + delta;
    // Beyond the visible text, the accumulated delta preserves the sentinel
    // and any opaque tail after its original offset.
    usize::try_from(mapped).ok()
}

fn replace_engine_data_payload(data: &[u8], payload: &[u8]) -> psd_core::Result<Vec<u8>> {
    let range = TypeToolTaggedBlock::locate_text_payloads(data)?
        .engine_data
        .ok_or_else(|| invalid("TySh has no EngineData payload"))?;
    if range.start < 4 || range.end > data.len() {
        return Err(invalid("TySh EngineData span is outside the block"));
    }
    let declared_len = u32::from_be_bytes(
        data[range.start - 4..range.start]
            .try_into()
            .map_err(|_| invalid("TySh EngineData length field is truncated"))?,
    ) as usize;
    if declared_len != range.len() {
        return Err(invalid(
            "TySh EngineData length does not match its payload span",
        ));
    }
    let new_len = u32::try_from(payload.len())
        .map_err(|_| invalid("EngineData payload exceeds its u32 length field"))?;
    let mut patches = vec![
        PayloadPatch {
            range: range.start - 4..range.start,
            new_bytes: new_len.to_be_bytes().to_vec(),
        },
        PayloadPatch {
            range,
            new_bytes: payload.to_vec(),
        },
    ];
    let mut output = data.to_vec();
    engine_data::apply_patches_checked(&mut output, &mut patches)?;
    Ok(output)
}

fn remap_legacy_descriptor_ranges(
    data: Vec<u8>,
    old_visible_units: usize,
    replacements: &[TextReplacement],
) -> psd_core::Result<Vec<u8>> {
    // Like upstream, the legacy `From`/`T   ` ranges are remapped only when
    // the text descriptor parses; next to an unknown OSType they are left as
    // they are (Photoshop rebuilds them from EngineData).
    let spans = match TypeToolTaggedBlock::legacy_range_integer_spans(&data) {
        Ok(spans) => spans,
        Err(error) => {
            tracing::warn!(%error, "TySh legacy text ranges were not remapped");
            return Ok(data);
        }
    };
    let mut patches = Vec::new();
    for span in spans {
        if span.value < 0 {
            continue;
        }
        let Some(mapped) = remap_index(span.value as usize, old_visible_units, replacements)
            .and_then(|mapped| i32::try_from(mapped).ok())
        else {
            continue;
        };
        if mapped != span.value {
            patches.push(PayloadPatch {
                range: span.range,
                new_bytes: mapped.to_be_bytes().to_vec(),
            });
        }
    }
    if patches.is_empty() {
        return Ok(data);
    }
    let mut updated = data;
    engine_data::apply_patches_checked(&mut updated, &mut patches)?;
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_replacement_is_left_to_right_and_non_overlapping() {
        let source: Vec<u16> = "aaaa".encode_utf16().collect();
        let edit =
            replace_utf16(&source, &[b'a' as u16, b'a' as u16], &[b'x' as u16], true).unwrap();
        assert_eq!(String::from_utf16(&edit.new_text).unwrap(), "xx");
        assert_eq!(edit.replacements.len(), 2);
    }

    #[test]
    fn remapping_keeps_trailing_sentinel_in_the_final_run() {
        let replacement = [TextReplacement {
            old_start: 5,
            old_length: 4,
            new_length: 7,
        }];
        assert_eq!(
            remap_lengths(&[5, 1, 4, 1, 6], 17, &replacement),
            Some(vec![5, 1, 7, 1, 6])
        );
    }
}
