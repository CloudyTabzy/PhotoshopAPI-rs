//! Name-based access to the typed text style properties.
//!
//! Upstream binds every character/paragraph property four times (flat
//! `style_run_*`/`style_normal_*`/`paragraph_run_*`/`paragraph_normal_*`
//! methods) plus proxy classes and range setters. Here one table per sheet
//! kind maps the upstream Python names onto the typed Rust accessors, and the
//! Python package generates the flat methods and proxies from
//! [`property_names`].

use std::ops::Range;

use psd::{
    BaselineDirection, BitDepth, CharacterDirection, CharacterStyle, DiacriticPosition,
    FontBaseline, FontCaps, Justification, KinsokuOrder, Layer, LeadingType, Occurrence,
    ParagraphStyle, TextWarpStyle,
};
use pyo3::exceptions::{PyAttributeError, PyValueError};
use pyo3::prelude::*;

use crate::convert::{enum_value, index_value, py_enum, truthy};
use crate::state::psd_error;

/// Which sheet a flat accessor targets.
#[derive(Debug, Clone, Copy)]
pub enum Sheet {
    Run(usize),
    Normal,
}

impl Sheet {
    pub fn from_py(kind: &str, index: i64) -> PyResult<Self> {
        match kind {
            "run" => Ok(Self::Run(index_value(index)?)),
            "normal" => Ok(Self::Normal),
            _ => Err(PyValueError::new_err(
                "sheet kind must be 'run' or 'normal'",
            )),
        }
    }
}

/// Which characters a range proxy covers; recomputed on every call so the
/// proxy stays valid while earlier setters split runs.
#[derive(Debug, Clone)]
pub enum RangeSpec {
    Span(Range<usize>),
    Text(String, Occurrence),
    All,
}

impl RangeSpec {
    /// `("range", start, end)`, `("text", needle, occurrence)` with
    /// upstream's occurrence numbering (0 = all, n = the n-th), or `("all",)`.
    pub fn from_py(spec: &Bound<'_, PyAny>) -> PyResult<Self> {
        let kind: String = spec.get_item(0)?.extract()?;
        match kind.as_str() {
            "range" => {
                let start = index_value(spec.get_item(1)?.extract()?)?;
                let end = index_value(spec.get_item(2)?.extract()?)?;
                if end < start {
                    return Err(PyValueError::new_err(
                        "range end must not precede its start",
                    ));
                }
                Ok(Self::Span(start..end))
            }
            "text" => {
                let needle: String = spec.get_item(1)?.extract()?;
                let occurrence = match index_value(spec.get_item(2)?.extract()?)? {
                    0 => Occurrence::All,
                    n => Occurrence::Nth(n - 1),
                };
                Ok(Self::Text(needle, occurrence))
            }
            "all" => Ok(Self::All),
            _ => Err(PyValueError::new_err("unknown text range kind")),
        }
    }
}

fn unknown(name: &str) -> PyErr {
    PyAttributeError::new_err(format!("unknown text property '{name}'"))
}

fn enum_or_int(py: Python<'_>, class: &str, raw: i32) -> PyResult<Py<PyAny>> {
    // Values outside the Python enum (kept by the Rust enums) stay ints.
    py_enum(py, class, i64::from(raw)).or_else(|_| Ok(raw.into_pyobject(py)?.into_any().unbind()))
}

fn option<T: for<'py> IntoPyObject<'py>>(py: Python<'_>, value: Option<T>) -> PyResult<Py<PyAny>> {
    use pyo3::IntoPyObjectExt;
    match value {
        Some(value) => value.into_py_any(py),
        None => Ok(py.None()),
    }
}

fn raw_enum(value: &Bound<'_, PyAny>) -> PyResult<i32> {
    i32::try_from(enum_value(value)?).map_err(|_| PyValueError::new_err("enum value out of range"))
}

fn color(value: &Bound<'_, PyAny>) -> PyResult<[f64; 4]> {
    let values: Vec<f64> = value.extract()?;
    values
        .try_into()
        .map_err(|_| PyValueError::new_err("colors need exactly four values [A, R, G, B]"))
}

// Getter/setter conversions per property kind. `get!` reads a snapshot,
// `set!` writes through any receiver with the typed setter (mutable handles
// and range handles share the setter names).
macro_rules! get {
    ($py:ident, $style:ident, f64, $getter:ident) => {
        option($py, $style.$getter())
    };
    ($py:ident, $style:ident, i32, $getter:ident) => {
        option($py, $style.$getter())
    };
    ($py:ident, $style:ident, bool, $getter:ident) => {
        option($py, $style.$getter())
    };
    ($py:ident, $style:ident, color, $getter:ident) => {
        option($py, $style.$getter())
    };
    ($py:ident, $style:ident, array, $getter:ident) => {
        option($py, $style.$getter())
    };
    ($py:ident, $style:ident, font, $getter:ident) => {
        option($py, $style.$getter())
    };
    ($py:ident, $style:ident, ($enum:ident, $class:literal), $getter:ident) => {
        match $style.$getter() {
            Some(value) => enum_or_int($py, $class, value.raw()),
            None => Ok($py.None()),
        }
    };
}

macro_rules! set {
    ($recv:expr, $value:ident, f64, $setter:ident) => {
        $recv.$setter($value.extract::<f64>()?).map(|_| ())
    };
    ($recv:expr, $value:ident, i32, $setter:ident) => {
        $recv.$setter($value.extract::<i32>()?).map(|_| ())
    };
    ($recv:expr, $value:ident, bool, $setter:ident) => {
        $recv.$setter(truthy($value)?).map(|_| ())
    };
    ($recv:expr, $value:ident, color, $setter:ident) => {
        $recv.$setter(color($value)?).map(|_| ())
    };
    ($recv:expr, $value:ident, array, $setter:ident) => {
        $recv.$setter(&$value.extract::<Vec<f64>>()?).map(|_| ())
    };
    ($recv:expr, $value:ident, font, $setter:ident) => {
        $recv
            .$setter(index_value($value.extract::<i64>()?)?)
            .map(|_| ())
    };
    ($recv:expr, $value:ident, ($enum:ident, $class:literal), $setter:ident) => {
        $recv
            .$setter($enum::from_raw(raw_enum($value)?))
            .map(|_| ())
    };
}

macro_rules! property_table {
    (
        $names:ident, $style:ty,
        $get_fn:ident, $set_fn:ident: $handle:ident, $range_fn:ident: $range:ident,
        [$( $name:literal => $getter:ident / $setter:ident : $kind:tt ),+ $(,)?]
    ) => {
        /// Upstream Python names of the properties in this table.
        pub const $names: &[&str] = &[$($name),+];

        fn $get_fn(py: Python<'_>, style: &$style, name: &str) -> PyResult<Py<PyAny>> {
            match name {
                $( $name => get!(py, style, $kind, $getter), )+
                _ => Err(unknown(name)),
            }
        }

        fn $set_fn<T: BitDepth>(
            handle: &mut psd::$handle<'_, T>,
            name: &str,
            value: &Bound<'_, PyAny>,
        ) -> PyResult<()> {
            match name {
                $( $name => set!(handle, value, $kind, $setter).map_err(psd_error), )+
                other => Err(unknown(other)),
            }
        }

        fn $range_fn<T: BitDepth>(
            range: &mut psd::$range<'_, T>,
            name: &str,
            value: &Bound<'_, PyAny>,
        ) -> PyResult<()> {
            match name {
                $( $name => set!(range, value, $kind, $setter).map_err(psd_error), )+
                other => Err(unknown(other)),
            }
        }
    };
}

property_table!(
    CHARACTER_PROPERTIES, CharacterStyle,
    character_get, character_set: CharacterStyleMut, character_range_set: CharacterStyleRange,
    [
    "font_size" => font_size / set_font_size: f64,
    "leading" => leading / set_leading: f64,
    "auto_leading" => auto_leading / set_auto_leading: bool,
    "kerning" => kerning / set_kerning: i32,
    "fill_color" => fill_color / set_fill_color: color,
    "stroke_color" => stroke_color / set_stroke_color: color,
    "font" => font_index / set_font_index: font,
    "faux_bold" => faux_bold / set_faux_bold: bool,
    "faux_italic" => faux_italic / set_faux_italic: bool,
    "horizontal_scale" => horizontal_scale / set_horizontal_scale: f64,
    "vertical_scale" => vertical_scale / set_vertical_scale: f64,
    "tracking" => tracking / set_tracking: i32,
    "auto_kerning" => auto_kerning / set_auto_kerning: bool,
    "baseline_shift" => baseline_shift / set_baseline_shift: f64,
    "font_caps" => font_caps / set_font_caps: (FontCaps, "FontCaps"),
    "font_baseline" => font_baseline / set_font_baseline: (FontBaseline, "FontBaseline"),
    "no_break" => no_break / set_no_break: bool,
    "language" => language / set_language: i32,
    "character_direction" => character_direction / set_character_direction: (CharacterDirection, "CharacterDirection"),
    "baseline_direction" => baseline_direction / set_baseline_direction: (BaselineDirection, "BaselineDirection"),
    "tsume" => tsume / set_tsume: f64,
    "kashida" => kashida / set_kashida: i32,
    "diacritic_pos" => diacritic_position / set_diacritic_position: (DiacriticPosition, "DiacriticPosition"),
    "ligatures" => ligatures / set_ligatures: bool,
    "dligatures" => discretionary_ligatures / set_discretionary_ligatures: bool,
    "underline" => underline / set_underline: bool,
    "strikethrough" => strikethrough / set_strikethrough: bool,
    "stroke_flag" => stroke_flag / set_stroke_flag: bool,
    "fill_flag" => fill_flag / set_fill_flag: bool,
    "fill_first" => fill_first / set_fill_first: bool,
    "outline_width" => outline_width / set_outline_width: f64,
    ]
);

property_table!(
    PARAGRAPH_PROPERTIES, ParagraphStyle,
    paragraph_get, paragraph_set: ParagraphStyleMut, paragraph_range_set: ParagraphStyleRange,
    [
    "justification" => justification / set_justification: (Justification, "Justification"),
    "first_line_indent" => first_line_indent / set_first_line_indent: f64,
    "start_indent" => start_indent / set_start_indent: f64,
    "end_indent" => end_indent / set_end_indent: f64,
    "space_before" => space_before / set_space_before: f64,
    "space_after" => space_after / set_space_after: f64,
    "auto_hyphenate" => auto_hyphenate / set_auto_hyphenate: bool,
    "hyphenated_word_size" => hyphenated_word_size / set_hyphenated_word_size: i32,
    "pre_hyphen" => pre_hyphen / set_pre_hyphen: i32,
    "post_hyphen" => post_hyphen / set_post_hyphen: i32,
    "consecutive_hyphens" => consecutive_hyphens / set_consecutive_hyphens: i32,
    "zone" => zone / set_zone: f64,
    "word_spacing" => word_spacing / set_word_spacing: array,
    "letter_spacing" => letter_spacing / set_letter_spacing: array,
    "glyph_spacing" => glyph_spacing / set_glyph_spacing: array,
    "auto_leading" => auto_leading / set_auto_leading: f64,
    "leading_type" => leading_type / set_leading_type: (LeadingType, "LeadingType"),
    "hanging" => hanging / set_hanging: bool,
    "burasagari" => burasagari / set_burasagari: bool,
    "kinsoku_order" => kinsoku_order / set_kinsoku_order: (KinsokuOrder, "KinsokuOrder"),
    "every_line_composer" => every_line_composer / set_every_line_composer: bool,
    ]
);

/// A character property of one run or the normal sheet; `None` when the
/// sheet or the property is missing.
pub fn get_character<T: BitDepth>(
    py: Python<'_>,
    layer: &Layer<T>,
    sheet: Sheet,
    name: &str,
) -> PyResult<Py<PyAny>> {
    let style = match sheet {
        Sheet::Run(run) => layer.style_run(run),
        Sheet::Normal => layer.style_normal(),
    };
    match style {
        Some(style) => character_get(py, &style, name),
        None if CHARACTER_PROPERTIES.contains(&name) => Ok(py.None()),
        None => Err(unknown(name)),
    }
}

pub fn set_character<T: BitDepth>(
    layer: &mut Layer<T>,
    sheet: Sheet,
    name: &str,
    value: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let mut handle = match sheet {
        Sheet::Run(run) => layer.style_run_mut(run),
        Sheet::Normal => layer.style_normal_mut(),
    };
    character_set(&mut handle, name, value)
}

pub fn get_paragraph<T: BitDepth>(
    py: Python<'_>,
    layer: &Layer<T>,
    sheet: Sheet,
    name: &str,
) -> PyResult<Py<PyAny>> {
    let style = match sheet {
        Sheet::Run(run) => layer.paragraph_run(run),
        Sheet::Normal => layer.paragraph_normal(),
    };
    match style {
        Some(style) => paragraph_get(py, &style, name),
        None if PARAGRAPH_PROPERTIES.contains(&name) => Ok(py.None()),
        None => Err(unknown(name)),
    }
}

pub fn set_paragraph<T: BitDepth>(
    layer: &mut Layer<T>,
    sheet: Sheet,
    name: &str,
    value: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let mut handle = match sheet {
        Sheet::Run(run) => layer.paragraph_run_mut(run),
        Sheet::Normal => layer.paragraph_normal_mut(),
    };
    paragraph_set(&mut handle, name, value)
}

/// Set a character property on every run a range covers. Upstream's range
/// proxies also accept `bold`/`italic` for the faux styles.
pub fn set_character_range<T: BitDepth>(
    layer: &mut Layer<T>,
    spec: &RangeSpec,
    name: &str,
    value: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let name = match name {
        "bold" => "faux_bold",
        "italic" => "faux_italic",
        other => other,
    };
    let mut range = match spec {
        RangeSpec::Span(span) => layer.style_range(span.clone()),
        RangeSpec::Text(needle, occurrence) => layer.style_text(needle, *occurrence),
        RangeSpec::All => layer.style_all(),
    };
    if name == "font_name" {
        let postscript_name: String = value.extract()?;
        return range
            .set_font(&postscript_name)
            .map(|_| ())
            .map_err(psd_error);
    }
    character_range_set(&mut range, name, value)
}

pub fn set_paragraph_range<T: BitDepth>(
    layer: &mut Layer<T>,
    spec: &RangeSpec,
    name: &str,
    value: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let mut range = match spec {
        RangeSpec::Span(span) => layer.paragraph_range(span.clone()),
        RangeSpec::Text(needle, occurrence) => layer.paragraph_text(needle, *occurrence),
        RangeSpec::All => layer.paragraph_all(),
    };
    paragraph_range_set(&mut range, name, value)
}

/// How many spans a range covers.
pub fn range_count<T: BitDepth>(layer: &mut Layer<T>, spec: &RangeSpec, paragraph: bool) -> usize {
    match (spec, paragraph) {
        (RangeSpec::Span(span), false) => layer.style_range(span.clone()).spans().len(),
        (RangeSpec::Text(needle, occurrence), false) => {
            layer.style_text(needle, *occurrence).spans().len()
        }
        (RangeSpec::All, false) => layer.style_all().spans().len(),
        (RangeSpec::Span(span), true) => layer.paragraph_range(span.clone()).spans().len(),
        (RangeSpec::Text(needle, occurrence), true) => {
            layer.paragraph_text(needle, *occurrence).spans().len()
        }
        (RangeSpec::All, true) => layer.paragraph_all().spans().len(),
    }
}

/// The `photoshopapi.enum.WarpStyle` value of a text warp style.
pub fn warp_style_number(style: &TextWarpStyle) -> Option<i32> {
    WARP_STYLES
        .iter()
        .position(|known| known == style)
        .map(|index| index as i32)
}

pub fn warp_style_from_number(value: i64) -> PyResult<TextWarpStyle> {
    usize::try_from(value)
        .ok()
        .and_then(|index| WARP_STYLES.get(index).cloned())
        .ok_or_else(|| PyValueError::new_err("unknown text warp style"))
}

/// Warp styles in `photoshopapi.enum.WarpStyle` order.
const WARP_STYLES: [TextWarpStyle; 17] = [
    TextWarpStyle::NoWarp,
    TextWarpStyle::Arc,
    TextWarpStyle::ArcLower,
    TextWarpStyle::ArcUpper,
    TextWarpStyle::Arch,
    TextWarpStyle::Bulge,
    TextWarpStyle::ShellLower,
    TextWarpStyle::ShellUpper,
    TextWarpStyle::Flag,
    TextWarpStyle::Wave,
    TextWarpStyle::Fish,
    TextWarpStyle::Rise,
    TextWarpStyle::FishEye,
    TextWarpStyle::Inflate,
    TextWarpStyle::Squeeze,
    TextWarpStyle::Twist,
    TextWarpStyle::Custom,
];

/// Both property-name tables, for the Python package.
pub fn property_names() -> (Vec<&'static str>, Vec<&'static str>) {
    (CHARACTER_PROPERTIES.to_vec(), PARAGRAPH_PROPERTIES.to_vec())
}
