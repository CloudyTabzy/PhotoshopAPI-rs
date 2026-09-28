//! Typed character/paragraph style properties.
//!
//! Upstream exposes every EngineData style property four times over: flat
//! `style_run_*`/`style_normal_*`/`paragraph_run_*`/`paragraph_normal_*`
//! mixin methods plus proxy structs forwarding to them. The Rust shape is one
//! property table per sheet kind, generating
//!
//! - getters on owned snapshots ([`CharacterStyle`], [`ParagraphStyle`]),
//! - chainable setters on mutable handles ([`CharacterStyleMut`],
//!   [`ParagraphStyleMut`]), and
//! - the same setters on range handles (see [`super::range`]),
//!
//! all backed by the byte-patching EngineData writer, so untouched payload
//! bytes survive every edit.

use psd_core::engine_data::{self, EngineValue, EngineValueKind, PayloadPatch};

use super::range::{CharacterStyleRange, ParagraphStyleRange};
use super::{
    first_engine_data, integer_value, invalid, normal_sheet_data, paragraph_run_data,
    style_run_data, write_dictionary_property, BaselineDirection, CharacterDirection,
    DiacriticPosition, FontBaseline, FontCaps, FontScript, FontType, Justification, KinsokuOrder,
    LeadingType,
};
use crate::{BitDepth, Layer};

/// The EngineData property dictionary a typed accessor targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sheet {
    /// `EngineDict/StyleRun/RunArray[i]/StyleSheet/StyleSheetData`.
    StyleRun(usize),
    /// The normal sheet of `ResourceDict/StyleSheetSet`.
    StyleNormal,
    /// `EngineDict/ParagraphRun/RunArray[i]/ParagraphSheet/Properties`.
    ParagraphRun(usize),
    /// The normal sheet of `ResourceDict/ParagraphSheetSet`.
    ParagraphNormal,
}

impl Sheet {
    fn resolve(self, root: &EngineValue) -> Option<&EngineValue> {
        match self {
            Self::StyleRun(run) => style_run_data(root, run),
            Self::StyleNormal => normal_sheet_data(root, false),
            Self::ParagraphRun(run) => paragraph_run_data(root, run),
            Self::ParagraphNormal => normal_sheet_data(root, true),
        }
    }

    fn missing(self) -> psd_core::PsdError {
        invalid(match self {
            Self::StyleRun(_) => "style run index is out of range",
            Self::StyleNormal => "normal style sheet is missing",
            Self::ParagraphRun(_) => "paragraph run index is out of range",
            Self::ParagraphNormal => "normal paragraph sheet is missing",
        })
    }
}

/// Conversion between a typed property value and its EngineData token.
pub(crate) trait PropertyValue: Sized {
    fn from_engine(value: &EngineValue) -> Option<Self>;

    /// The token replacing `current`, or a new token to insert when the
    /// property is absent. Existing numbers keep their integer/float spelling.
    fn to_engine(&self, current: Option<&EngineValue>) -> psd_core::Result<EngineValue>;
}

fn number_token(value: f64, current: Option<&EngineValue>) -> psd_core::Result<EngineValue> {
    if !value.is_finite() {
        return Err(invalid("text property numbers must be finite"));
    }
    match current {
        Some(current) => {
            let mut replacement = current.clone();
            if replacement.set_number(value) {
                Ok(replacement)
            } else {
                Err(invalid("existing text property is not a number"))
            }
        }
        None => Ok(EngineValue::number(value)),
    }
}

impl PropertyValue for f64 {
    fn from_engine(value: &EngineValue) -> Option<Self> {
        value.as_double()
    }

    fn to_engine(&self, current: Option<&EngineValue>) -> psd_core::Result<EngineValue> {
        number_token(*self, current)
    }
}

/// A float paragraph property: `FirstLineIndent`, `StartIndent`, `EndIndent`,
/// `SpaceBefore`, `SpaceAfter`, `Zone`, `AutoLeading`.
///
/// Photoshop reads a bare integer token for these keys as 16.16 fixed point —
/// `FirstLineIndent 24` comes back as 0.000366 px, silently losing the indent —
/// so a value set through the typed setters always carries a decimal point,
/// even when the file's previous token was an integer. Photoshop's own files
/// write short decimals; the readback that pinned this is recorded in the
/// test below.
pub(crate) struct ParagraphFloat(pub f64);

impl PropertyValue for ParagraphFloat {
    fn from_engine(value: &EngineValue) -> Option<Self> {
        value.as_double().map(Self)
    }

    fn to_engine(&self, current: Option<&EngineValue>) -> psd_core::Result<EngineValue> {
        let mut token = number_token(self.0, current)?;
        if let EngineValueKind::Number(number) = &mut token.kind {
            number.integer = None;
        }
        Ok(token)
    }
}

impl PropertyValue for i32 {
    fn from_engine(value: &EngineValue) -> Option<Self> {
        integer_value(value)
    }

    fn to_engine(&self, current: Option<&EngineValue>) -> psd_core::Result<EngineValue> {
        number_token(f64::from(*self), current)
    }
}

impl PropertyValue for bool {
    fn from_engine(value: &EngineValue) -> Option<Self> {
        value.as_bool()
    }

    fn to_engine(&self, current: Option<&EngineValue>) -> psd_core::Result<EngineValue> {
        // Fidelity delta: upstream style setters silently
        // replace a non-boolean token with a boolean; like the paragraph
        // setters, a type mismatch is rejected here instead.
        match current.map(|current| &current.kind) {
            None | Some(EngineValueKind::Boolean(_)) => Ok(EngineValue::boolean(*self)),
            Some(_) => Err(invalid("existing text property is not a boolean")),
        }
    }
}

/// Number arrays (paragraph spacing) always serialize as floats, matching
/// Photoshop and upstream.
fn float_array(values: &[f64], current: Option<&EngineValue>) -> psd_core::Result<EngineValue> {
    if values.is_empty() || !values.iter().all(|value| value.is_finite()) {
        return Err(invalid("text property arrays must be non-empty and finite"));
    }
    if current.is_some_and(|current| current.as_array().is_none()) {
        return Err(invalid("existing text property is not an array"));
    }
    let mut array = EngineValue::array();
    for &value in values {
        array.push(EngineValue::float(value));
    }
    Ok(array)
}

macro_rules! enum_property_values {
    ($($name:ident),+ $(,)?) => {
        $(
            impl PropertyValue for $name {
                fn from_engine(value: &EngineValue) -> Option<Self> {
                    integer_value(value).map(Self::from_raw)
                }

                fn to_engine(
                    &self,
                    current: Option<&EngineValue>,
                ) -> psd_core::Result<EngineValue> {
                    number_token(f64::from(self.raw()), current)
                }
            }
        )+
    };
}

enum_property_values!(
    FontCaps,
    FontBaseline,
    CharacterDirection,
    BaselineDirection,
    DiacriticPosition,
    Justification,
    LeadingType,
    KinsokuOrder,
    FontType,
    FontScript,
);

fn positive(value: f64) -> psd_core::Result<()> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(invalid("text font size must be finite and positive"))
    }
}

// ---------------------------------------------------------------------------
// Snapshots and mutable handles
// ---------------------------------------------------------------------------

/// Character style values of one style run or of the normal style sheet,
/// captured when the snapshot was taken (`StyleRunProxy`/`StyleNormalProxy`).
#[derive(Debug, Clone, PartialEq)]
pub struct CharacterStyle {
    data: EngineValue,
}

/// Paragraph style values of one paragraph run or of the normal paragraph
/// sheet (`ParagraphRunProxy`/`ParagraphNormalProxy`).
#[derive(Debug, Clone, PartialEq)]
pub struct ParagraphStyle {
    data: EngineValue,
}

/// Mutable handle for one style run or the normal style sheet. Every setter
/// patches the TySh EngineData immediately and returns the handle for chaining.
pub struct CharacterStyleMut<'a, T: BitDepth> {
    layer: &'a mut Layer<T>,
    sheet: Sheet,
}

/// Mutable handle for one paragraph run or the normal paragraph sheet.
pub struct ParagraphStyleMut<'a, T: BitDepth> {
    layer: &'a mut Layer<T>,
    sheet: Sheet,
}

impl CharacterStyle {
    /// Raw access to any property, including ones without a typed accessor.
    pub fn property(&self, key: &str) -> Option<&EngineValue> {
        self.data.get(key)
    }

    /// The whole `StyleSheetData` dictionary.
    pub fn as_engine_value(&self) -> &EngineValue {
        &self.data
    }

    /// Index into the layer's FontSet (see [`Layer::font`]).
    pub fn font_index(&self) -> Option<usize> {
        usize::try_from(self.value::<i32>("Font")?).ok()
    }

    /// Fill color `Values` (typically `[alpha, red, green, blue]` in `0..=1`).
    pub fn fill_color(&self) -> Option<Vec<f64>> {
        color_values(&self.data, "FillColor")
    }

    /// Stroke color `Values`; absent when the run has no stroke color.
    pub fn stroke_color(&self) -> Option<Vec<f64>> {
        color_values(&self.data, "StrokeColor")
    }

    fn value<V: PropertyValue>(&self, key: &str) -> Option<V> {
        self.data.get(key).and_then(V::from_engine)
    }
}

impl ParagraphStyle {
    /// Raw access to any property, including ones without a typed accessor.
    pub fn property(&self, key: &str) -> Option<&EngineValue> {
        self.data.get(key)
    }

    /// The whole paragraph `Properties` dictionary.
    pub fn as_engine_value(&self) -> &EngineValue {
        &self.data
    }

    fn value<V: PropertyValue>(&self, key: &str) -> Option<V> {
        self.data.get(key).and_then(V::from_engine)
    }
}

fn color_values(style: &EngineValue, key: &str) -> Option<Vec<f64>> {
    let values = style.get(key)?.get("Values")?.as_double_vector()?;
    (!values.is_empty()).then_some(values)
}

impl<T: BitDepth> CharacterStyleMut<'_, T> {
    /// Set any property from a raw EngineData value. Existing values must keep
    /// their token kind; absent properties are inserted.
    pub fn set_property(&mut self, key: &str, value: EngineValue) -> psd_core::Result<&mut Self> {
        self.layer.set_sheet_property(self.sheet, key, value)?;
        Ok(self)
    }

    /// Point the run (or normal sheet) at an existing FontSet entry.
    pub fn set_font_index(&mut self, index: usize) -> psd_core::Result<&mut Self> {
        self.set_typed("Font", &font_index_value(index)?)
    }

    /// Use the font with this PostScript name, adding it to the FontSet as an
    /// OpenType Roman font when it is not already present. All-or-nothing: a
    /// bad run index does not leave a newly added font behind.
    pub fn set_font(&mut self, postscript_name: &str) -> psd_core::Result<&mut Self> {
        let sheet = self.sheet;
        self.layer.transaction(|layer| {
            let index = layer.find_or_add_font(
                postscript_name,
                FontType::OpenType,
                FontScript::Roman,
                0,
            )?;
            layer.set_sheet_value(sheet, "Font", &font_index_value(index)?)
        })?;
        Ok(self)
    }

    /// Replace the fill color `Values`; the `FillColor` dictionary must exist.
    pub fn set_fill_color(&mut self, values: [f64; 4]) -> psd_core::Result<&mut Self> {
        self.layer
            .set_sheet_color(self.sheet, "FillColor", values)?;
        Ok(self)
    }

    /// Replace the stroke color `Values`; the `StrokeColor` dictionary must exist.
    pub fn set_stroke_color(&mut self, values: [f64; 4]) -> psd_core::Result<&mut Self> {
        self.layer
            .set_sheet_color(self.sheet, "StrokeColor", values)?;
        Ok(self)
    }

    fn set_typed<V: PropertyValue>(&mut self, key: &str, value: &V) -> psd_core::Result<&mut Self> {
        self.layer.set_sheet_value(self.sheet, key, value)?;
        Ok(self)
    }
}

impl<T: BitDepth> ParagraphStyleMut<'_, T> {
    /// Set any property from a raw EngineData value. Existing values must keep
    /// their token kind; absent properties are inserted.
    pub fn set_property(&mut self, key: &str, value: EngineValue) -> psd_core::Result<&mut Self> {
        self.layer.set_sheet_property(self.sheet, key, value)?;
        Ok(self)
    }

    fn set_typed<V: PropertyValue>(&mut self, key: &str, value: &V) -> psd_core::Result<&mut Self> {
        self.layer.set_sheet_value(self.sheet, key, value)?;
        Ok(self)
    }

    fn set_array(&mut self, key: &str, values: &[f64]) -> psd_core::Result<&mut Self> {
        self.layer.set_sheet_array(self.sheet, key, values)?;
        Ok(self)
    }
}

impl<T: BitDepth> CharacterStyleRange<'_, T> {
    /// Point every covered run at an existing FontSet entry.
    pub fn set_font_index(&mut self, index: usize) -> psd_core::Result<&mut Self> {
        let value = font_index_value(index)?;
        self.apply(|layer, run| layer.set_sheet_value(Sheet::StyleRun(run), "Font", &value))
    }

    /// Use the font with this PostScript name for every covered run, adding
    /// it to the FontSet when needed.
    pub fn set_font(&mut self, postscript_name: &str) -> psd_core::Result<&mut Self> {
        self.apply(|layer, run| {
            layer.style_run_mut(run).set_font(postscript_name)?;
            Ok(())
        })
    }

    /// Set the fill color `Values` of every covered run.
    pub fn set_fill_color(&mut self, values: [f64; 4]) -> psd_core::Result<&mut Self> {
        self.apply(|layer, run| layer.set_sheet_color(Sheet::StyleRun(run), "FillColor", values))
    }

    /// Set the stroke color `Values` of every covered run; each needs an
    /// existing `StrokeColor` dictionary.
    pub fn set_stroke_color(&mut self, values: [f64; 4]) -> psd_core::Result<&mut Self> {
        self.apply(|layer, run| layer.set_sheet_color(Sheet::StyleRun(run), "StrokeColor", values))
    }
}

fn font_index_value(index: usize) -> psd_core::Result<i32> {
    i32::try_from(index).map_err(|_| invalid("font index exceeds the EngineData i32 range"))
}

// ---------------------------------------------------------------------------
// Property tables
// ---------------------------------------------------------------------------

macro_rules! character_properties {
    ($(
        $(#[$doc:meta])*
        $get:ident / $set:ident : $ty:ty = $key:literal $(, check = $check:path)?;
    )+) => {
        impl CharacterStyle {
            $(
                #[doc = concat!("`", $key, "` of this character style.")]
                #[doc = ""]
                $(#[$doc])*
                pub fn $get(&self) -> Option<$ty> { self.value($key) }
            )+
        }

        impl<T: BitDepth> CharacterStyleMut<'_, T> {
            $(
                #[doc = concat!("Set `", $key, "`, inserting it when absent.")]
                #[doc = ""]
                $(#[$doc])*
                pub fn $set(&mut self, value: $ty) -> psd_core::Result<&mut Self> {
                    $( $check(value)?; )?
                    self.set_typed($key, &value)
                }
            )+
        }

        impl<T: BitDepth> CharacterStyleRange<'_, T> {
            $(
                #[doc = concat!("Set `", $key, "` on every run covered by the range.")]
                #[doc = ""]
                $(#[$doc])*
                pub fn $set(&mut self, value: $ty) -> psd_core::Result<&mut Self> {
                    $( $check(value)?; )?
                    self.apply(|layer, run| {
                        layer.set_sheet_value(Sheet::StyleRun(run), $key, &value)
                    })
                }
            )+
        }
    };
}

character_properties! {
    /// Font size in points (`FontSize`); must be positive.
    font_size / set_font_size: f64 = "FontSize", check = positive;
    /// Explicit leading in points (`Leading`), used when auto leading is off.
    leading / set_leading: f64 = "Leading";
    auto_leading / set_auto_leading: bool = "AutoLeading";
    kerning / set_kerning: i32 = "Kerning";
    faux_bold / set_faux_bold: bool = "FauxBold";
    faux_italic / set_faux_italic: bool = "FauxItalic";
    horizontal_scale / set_horizontal_scale: f64 = "HorizontalScale";
    vertical_scale / set_vertical_scale: f64 = "VerticalScale";
    tracking / set_tracking: i32 = "Tracking";
    auto_kerning / set_auto_kerning: bool = "AutoKerning";
    baseline_shift / set_baseline_shift: f64 = "BaselineShift";
    font_caps / set_font_caps: FontCaps = "FontCaps";
    no_break / set_no_break: bool = "NoBreak";
    font_baseline / set_font_baseline: FontBaseline = "FontBaseline";
    /// Photoshop language code (`Language`).
    language / set_language: i32 = "Language";
    character_direction / set_character_direction: CharacterDirection = "CharacterDirection";
    baseline_direction / set_baseline_direction: BaselineDirection = "BaselineDirection";
    /// CJK proportional spacing (`Tsume`).
    tsume / set_tsume: f64 = "Tsume";
    /// Arabic kashida justification (`Kashida`).
    kashida / set_kashida: i32 = "Kashida";
    diacritic_position / set_diacritic_position: DiacriticPosition = "DiacriticPos";
    ligatures / set_ligatures: bool = "Ligatures";
    discretionary_ligatures / set_discretionary_ligatures: bool = "DLigatures";
    underline / set_underline: bool = "Underline";
    strikethrough / set_strikethrough: bool = "Strikethrough";
    stroke_flag / set_stroke_flag: bool = "StrokeFlag";
    fill_flag / set_fill_flag: bool = "FillFlag";
    /// Draw the fill before the stroke (`FillFirst`).
    fill_first / set_fill_first: bool = "FillFirst";
    /// Stroke width in points (`OutlineWidth`).
    outline_width / set_outline_width: f64 = "OutlineWidth";
}

macro_rules! paragraph_properties {
    ($(
        $(#[$doc:meta])*
        $get:ident / $set:ident : $ty:ty = $key:literal;
    )+) => {
        impl ParagraphStyle {
            $(
                #[doc = concat!("`", $key, "` of this paragraph style.")]
                #[doc = ""]
                $(#[$doc])*
                pub fn $get(&self) -> Option<$ty> { self.value($key) }
            )+
        }

        impl<T: BitDepth> ParagraphStyleMut<'_, T> {
            $(
                #[doc = concat!("Set `", $key, "`, inserting it when absent.")]
                #[doc = ""]
                $(#[$doc])*
                pub fn $set(&mut self, value: $ty) -> psd_core::Result<&mut Self> {
                    self.set_typed($key, &value)
                }
            )+
        }

        impl<T: BitDepth> ParagraphStyleRange<'_, T> {
            $(
                #[doc = concat!("Set `", $key, "` on every paragraph the range touches.")]
                #[doc = ""]
                $(#[$doc])*
                pub fn $set(&mut self, value: $ty) -> psd_core::Result<&mut Self> {
                    self.apply(|layer, run| {
                        layer.set_sheet_value(Sheet::ParagraphRun(run), $key, &value)
                    })
                }
            )+
        }
    };
}

paragraph_properties! {
    justification / set_justification: Justification = "Justification";
    auto_hyphenate / set_auto_hyphenate: bool = "AutoHyphenate";
    hyphenated_word_size / set_hyphenated_word_size: i32 = "HyphenatedWordSize";
    pre_hyphen / set_pre_hyphen: i32 = "PreHyphen";
    post_hyphen / set_post_hyphen: i32 = "PostHyphen";
    consecutive_hyphens / set_consecutive_hyphens: i32 = "ConsecutiveHyphens";
    leading_type / set_leading_type: LeadingType = "LeadingType";
    /// Roman hanging punctuation (`Hanging`).
    hanging / set_hanging: bool = "Hanging";
    /// Japanese burasagari hanging punctuation (`Burasagari`).
    burasagari / set_burasagari: bool = "Burasagari";
    kinsoku_order / set_kinsoku_order: KinsokuOrder = "KinsokuOrder";
    /// Adobe every-line composer instead of the single-line composer.
    every_line_composer / set_every_line_composer: bool = "EveryLineComposer";
}

/// Float paragraph properties, whose set values always carry a decimal point
/// (see [`ParagraphFloat`]).
macro_rules! paragraph_float_properties {
    ($(
        $(#[$doc:meta])*
        $get:ident / $set:ident = $key:literal;
    )+) => {
        impl ParagraphStyle {
            $(
                #[doc = concat!("`", $key, "` of this paragraph style.")]
                #[doc = ""]
                $(#[$doc])*
                pub fn $get(&self) -> Option<f64> { self.value($key) }
            )+
        }

        impl<T: BitDepth> ParagraphStyleMut<'_, T> {
            $(
                #[doc = concat!("Set `", $key, "`, inserting it when absent.")]
                #[doc = ""]
                $(#[$doc])*
                pub fn $set(&mut self, value: f64) -> psd_core::Result<&mut Self> {
                    self.set_typed($key, &ParagraphFloat(value))
                }
            )+
        }

        impl<T: BitDepth> ParagraphStyleRange<'_, T> {
            $(
                #[doc = concat!("Set `", $key, "` on every paragraph the range touches.")]
                #[doc = ""]
                $(#[$doc])*
                pub fn $set(&mut self, value: f64) -> psd_core::Result<&mut Self> {
                    self.apply(|layer, run| {
                        layer.set_sheet_value(Sheet::ParagraphRun(run), $key, &ParagraphFloat(value))
                    })
                }
            )+
        }
    };
}

paragraph_float_properties! {
    first_line_indent / set_first_line_indent = "FirstLineIndent";
    start_indent / set_start_indent = "StartIndent";
    end_indent / set_end_indent = "EndIndent";
    space_before / set_space_before = "SpaceBefore";
    space_after / set_space_after = "SpaceAfter";
    /// Hyphenation zone in points (`Zone`).
    zone / set_zone = "Zone";
    /// Auto-leading multiplier (`AutoLeading`, e.g. `1.2`).
    auto_leading / set_auto_leading = "AutoLeading";
}

macro_rules! paragraph_array_properties {
    ($(
        $(#[$doc:meta])*
        $get:ident / $set:ident = $key:literal;
    )+) => {
        impl ParagraphStyle {
            $(
                #[doc = concat!("`", $key, "` of this paragraph style.")]
                #[doc = ""]
                $(#[$doc])*
                pub fn $get(&self) -> Option<Vec<f64>> {
                    self.data.get($key)?.as_double_vector()
                }
            )+
        }

        impl<T: BitDepth> ParagraphStyleMut<'_, T> {
            $(
                #[doc = concat!("Set `", $key, "` (non-empty, finite), inserting it when absent.")]
                #[doc = ""]
                $(#[$doc])*
                pub fn $set(&mut self, values: &[f64]) -> psd_core::Result<&mut Self> {
                    self.set_array($key, values)
                }
            )+
        }

        impl<T: BitDepth> ParagraphStyleRange<'_, T> {
            $(
                #[doc = concat!("Set `", $key, "` on every paragraph the range touches.")]
                #[doc = ""]
                $(#[$doc])*
                pub fn $set(&mut self, values: &[f64]) -> psd_core::Result<&mut Self> {
                    self.apply(|layer, run| {
                        layer.set_sheet_array(Sheet::ParagraphRun(run), $key, values)
                    })
                }
            )+
        }
    };
}

paragraph_array_properties! {
    /// Justification word spacing `[minimum, desired, maximum]`.
    word_spacing / set_word_spacing = "WordSpacing";
    /// Justification letter spacing `[minimum, desired, maximum]`.
    letter_spacing / set_letter_spacing = "LetterSpacing";
    /// Justification glyph scaling `[minimum, desired, maximum]`.
    glyph_spacing / set_glyph_spacing = "GlyphSpacing";
}

// ---------------------------------------------------------------------------
// Layer entry points
// ---------------------------------------------------------------------------

impl<T: BitDepth> Layer<T> {
    /// Snapshot of one character style run.
    pub fn style_run(&self, run: usize) -> Option<CharacterStyle> {
        self.character_style(Sheet::StyleRun(run))
    }

    /// Snapshots of every character style run, in text order.
    pub fn style_runs(&self) -> Vec<CharacterStyle> {
        (0..self.style_run_count())
            .map_while(|run| self.style_run(run))
            .collect()
    }

    /// Snapshot of the normal (default) character style sheet.
    pub fn style_normal(&self) -> Option<CharacterStyle> {
        self.character_style(Sheet::StyleNormal)
    }

    /// Snapshot of one paragraph run.
    pub fn paragraph_run(&self, run: usize) -> Option<ParagraphStyle> {
        self.paragraph_style(Sheet::ParagraphRun(run))
    }

    /// Snapshots of every paragraph run, in text order.
    pub fn paragraph_runs(&self) -> Vec<ParagraphStyle> {
        (0..self.paragraph_run_count())
            .map_while(|run| self.paragraph_run(run))
            .collect()
    }

    /// Snapshot of the normal (default) paragraph sheet.
    pub fn paragraph_normal(&self) -> Option<ParagraphStyle> {
        self.paragraph_style(Sheet::ParagraphNormal)
    }

    /// Mutable handle for one character style run. Setters fail when the run
    /// index is out of range.
    pub fn style_run_mut(&mut self, run: usize) -> CharacterStyleMut<'_, T> {
        CharacterStyleMut {
            layer: self,
            sheet: Sheet::StyleRun(run),
        }
    }

    /// Mutable handle for the normal character style sheet.
    pub fn style_normal_mut(&mut self) -> CharacterStyleMut<'_, T> {
        CharacterStyleMut {
            layer: self,
            sheet: Sheet::StyleNormal,
        }
    }

    /// Mutable handle for one paragraph run.
    pub fn paragraph_run_mut(&mut self, run: usize) -> ParagraphStyleMut<'_, T> {
        ParagraphStyleMut {
            layer: self,
            sheet: Sheet::ParagraphRun(run),
        }
    }

    /// Mutable handle for the normal paragraph sheet.
    pub fn paragraph_normal_mut(&mut self) -> ParagraphStyleMut<'_, T> {
        ParagraphStyleMut {
            layer: self,
            sheet: Sheet::ParagraphNormal,
        }
    }

    /// Index of the normal sheet in `ResourceDict/StyleSheetSet`.
    pub fn style_normal_sheet_index(&self) -> Option<usize> {
        self.normal_sheet_index(false)
    }

    /// Choose which `StyleSheetSet` entry is the normal character sheet.
    pub fn set_style_normal_sheet_index(&mut self, index: usize) -> psd_core::Result<()> {
        self.set_normal_sheet_index(false, index)
    }

    /// Index of the normal sheet in `ResourceDict/ParagraphSheetSet`.
    pub fn paragraph_normal_sheet_index(&self) -> Option<usize> {
        self.normal_sheet_index(true)
    }

    /// Choose which `ParagraphSheetSet` entry is the normal paragraph sheet.
    pub fn set_paragraph_normal_sheet_index(&mut self, index: usize) -> psd_core::Result<()> {
        self.set_normal_sheet_index(true, index)
    }

    fn character_style(&self, sheet: Sheet) -> Option<CharacterStyle> {
        let root = first_engine_data(self)?;
        let data = sheet.resolve(&root)?;
        data.as_dictionary()
            .is_some()
            .then(|| CharacterStyle { data: data.clone() })
    }

    fn paragraph_style(&self, sheet: Sheet) -> Option<ParagraphStyle> {
        let root = first_engine_data(self)?;
        let data = sheet.resolve(&root)?;
        data.as_dictionary()
            .is_some()
            .then(|| ParagraphStyle { data: data.clone() })
    }

    fn normal_sheet_index(&self, paragraph: bool) -> Option<usize> {
        let root = first_engine_data(self)?;
        let (index_key, set_key) = normal_sheet_keys(paragraph);
        let index =
            usize::try_from(integer_value(root.get_path(["ResourceDict", index_key])?)?).ok()?;
        let count = root.get_path(["ResourceDict", set_key])?.as_array()?.len();
        (index < count).then_some(index)
    }

    fn set_normal_sheet_index(&mut self, paragraph: bool, index: usize) -> psd_core::Result<()> {
        let (index_key, set_key) = normal_sheet_keys(paragraph);
        let value = i32::try_from(index).map_err(|_| invalid("sheet index exceeds i32"))?;
        self.patch_engine_data(|root, payload| {
            let count = root
                .get_path(["ResourceDict", set_key])
                .and_then(EngineValue::as_array)
                .ok_or_else(|| invalid("text sheet set is missing"))?
                .len();
            if index >= count {
                return Err(invalid("normal sheet index is out of range"));
            }
            let current = root
                .get_path(["ResourceDict", index_key])
                .ok_or_else(|| invalid("normal sheet index is missing"))?;
            let replacement = value.to_engine(Some(current))?;
            engine_data::apply_patches_checked(
                payload,
                &mut [PayloadPatch {
                    range: current.span.clone(),
                    new_bytes: engine_data::format_value_bytes(&replacement, 0),
                }],
            )?;
            Ok(true)
        })
    }

    /// Typed write of one property in `sheet` (inserting it when absent).
    pub(crate) fn set_sheet_value<V: PropertyValue>(
        &mut self,
        sheet: Sheet,
        key: &str,
        value: &V,
    ) -> psd_core::Result<()> {
        self.patch_engine_data(|root, payload| {
            let dictionary = sheet.resolve(root).ok_or_else(|| sheet.missing())?;
            let replacement = value.to_engine(dictionary.get(key))?;
            write_dictionary_property(payload, dictionary, key, &replacement)?;
            Ok(true)
        })
    }

    pub(crate) fn set_sheet_property(
        &mut self,
        sheet: Sheet,
        key: &str,
        value: EngineValue,
    ) -> psd_core::Result<()> {
        self.patch_engine_data(|root, payload| {
            let dictionary = sheet.resolve(root).ok_or_else(|| sheet.missing())?;
            write_dictionary_property(payload, dictionary, key, &value)?;
            Ok(true)
        })
    }

    pub(crate) fn set_sheet_array(
        &mut self,
        sheet: Sheet,
        key: &str,
        values: &[f64],
    ) -> psd_core::Result<()> {
        self.patch_engine_data(|root, payload| {
            let dictionary = sheet.resolve(root).ok_or_else(|| sheet.missing())?;
            let replacement = float_array(values, dictionary.get(key))?;
            write_dictionary_property(payload, dictionary, key, &replacement)?;
            Ok(true)
        })
    }

    /// Replace `<key>/Values` of an existing color dictionary with four floats.
    /// Like upstream, a missing color is an error rather than an insertion:
    /// a color also needs a `Type`, which this API does not choose.
    pub(crate) fn set_sheet_color(
        &mut self,
        sheet: Sheet,
        key: &str,
        values: [f64; 4],
    ) -> psd_core::Result<()> {
        self.patch_engine_data(|root, payload| {
            let dictionary = sheet.resolve(root).ok_or_else(|| sheet.missing())?;
            let current = dictionary
                .get(key)
                .and_then(|color| color.get("Values"))
                .ok_or_else(|| invalid("text color dictionary or its Values are missing"))?;
            let replacement = float_array(&values, Some(current))?;
            engine_data::apply_patches_checked(
                payload,
                &mut [PayloadPatch {
                    range: current.span.clone(),
                    new_bytes: engine_data::format_value_bytes(&replacement, 0),
                }],
            )?;
            Ok(true)
        })
    }
}

fn normal_sheet_keys(paragraph: bool) -> (&'static str, &'static str) {
    if paragraph {
        ("TheNormalParagraphSheet", "ParagraphSheetSet")
    } else {
        ("TheNormalStyleSheet", "StyleSheetSet")
    }
}
