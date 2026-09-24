//! Text layers created from scratch (`TextLayerTyShBuilderUtils.h` upstream).
//!
//! The generated TySh mirrors what Photoshop writes for a new box-text layer:
//! the EngineData tree (runs, sheets, kinsoku/mojikumi sets, FontSet with the
//! `AdobeInvisFont` sentinel), the `TxLr` descriptor with Photoshop's
//! char-ID key encodings, a no-op `warp` descriptor, and the 19 zero bytes
//! Photoshop appends. Glyph metrics are estimated; Photoshop recomputes
//! them (and the raster preview) the first time the text is edited.

use psd_core::descriptor::OS_RAW_DATA;
use psd_core::engine_data::{self, EngineValue};
use psd_core::{
    BeWriter, Descriptor, DescriptorItem, DescriptorKey, DescriptorValue, TaggedBlock,
    TypeToolTaggedBlock, UnicodeString,
};

use super::{engine_array, engine_dictionary, invalid, SENTINEL_FONT, TYSH};
use crate::{BitDepth, Layer, LayerKind, Rect, TextLayer};

/// Builder for a new, editable text layer (`TextLayer::create` upstream).
///
/// ```
/// let layer = psd::TextLayerBuilder::new("Title", "Hello\nWorld")
///     .font("ArialMT")
///     .font_size(36.0)
///     .fill_color([1.0, 1.0, 0.0, 0.0])
///     .position(20.0, 50.0)
///     .build::<u8>()
///     .unwrap();
/// assert_eq!(layer.text().as_deref(), Some("Hello\rWorld"));
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct TextLayerBuilder {
    name: String,
    text: String,
    font: String,
    font_size: f64,
    fill_color: [f64; 4],
    position: (f64, f64),
    box_size: Option<(f64, f64)>,
}

impl TextLayerBuilder {
    /// A layer named `name` showing `text`; `\n` becomes Photoshop's `\r`.
    /// Defaults: ArialMT 24pt, opaque black, anchored at (20, 50), box size
    /// estimated from the text.
    pub fn new(name: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            text: text.into(),
            font: "ArialMT".to_owned(),
            font_size: 24.0,
            fill_color: [1.0, 0.0, 0.0, 0.0],
            position: (20.0, 50.0),
            box_size: None,
        }
    }

    /// PostScript name of the font (FontSet entry 0).
    pub fn font(mut self, postscript_name: impl Into<String>) -> Self {
        self.font = postscript_name.into();
        self
    }

    /// Font size in points.
    pub fn font_size(mut self, points: f64) -> Self {
        self.font_size = points;
        self
    }

    /// Fill color as EngineData `Values`: `[alpha, red, green, blue]` in `0..=1`.
    pub fn fill_color(mut self, argb: [f64; 4]) -> Self {
        self.fill_color = argb;
        self
    }

    /// Text anchor in document pixels (the TySh transform translation).
    pub fn position(mut self, x: f64, y: f64) -> Self {
        self.position = (x, y);
        self
    }

    /// Explicit text-box size in points instead of the estimate.
    pub fn box_size(mut self, width: f64, height: f64) -> Self {
        self.box_size = Some((width, height));
        self
    }

    /// Build the layer. Its preview channels start empty; Photoshop renders
    /// the text when the document is opened.
    pub fn build<T: BitDepth>(&self) -> psd_core::Result<Layer<T>> {
        let data = self.tysh_bytes()?;
        let mut layer = Layer::new_image(self.name.clone(), Rect::default());
        layer.kind = LayerKind::Text(TextLayer::new());
        layer.blocks.push(TaggedBlock::new(TYSH, data));
        Ok(layer)
    }

    fn validate(&self) -> psd_core::Result<()> {
        if !self.font_size.is_finite() || self.font_size <= 0.0 {
            return Err(invalid("text font size must be finite and positive"));
        }
        if !self.fill_color.iter().all(|value| value.is_finite()) {
            return Err(invalid("text fill color values must be finite"));
        }
        if !self.position.0.is_finite() || !self.position.1.is_finite() {
            return Err(invalid("text position must be finite"));
        }
        if let Some((width, height)) = self.box_size {
            if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
                return Err(invalid("text box size must be finite and positive"));
            }
        }
        Ok(())
    }

    fn tysh_bytes(&self) -> psd_core::Result<Vec<u8>> {
        self.validate()?;
        let units: Vec<u16> = self
            .text
            .encode_utf16()
            .map(|unit| {
                if unit == u16::from(b'\n') {
                    u16::from(b'\r')
                } else {
                    unit
                }
            })
            .collect();

        // Rough metrics (typical ascent 0.85em, average advance 0.55em).
        let size = self.font_size;
        let ascent = size * 0.85;
        let descent = size * 0.15;
        let lines: Vec<usize> = units
            .split(|&unit| unit == u16::from(b'\r'))
            .map(<[u16]>::len)
            .collect();
        let longest = lines.iter().copied().max().unwrap_or(0) as f64;
        let (width, height) = self
            .box_size
            .unwrap_or((longest * size * 0.55, lines.len() as f64 * size * 1.2));

        let engine = build_engine_data(&units, &self.font, size, self.fill_color, width, height)?;
        let text = build_text_descriptor(
            &units,
            engine,
            [0.0, -ascent, width, height - ascent],
            [2.0, -(ascent - descent), width - 2.0, 0.0],
        )?;
        let block = TypeToolTaggedBlock {
            tysh_version: 1,
            transform: [1.0, 0.0, 0.0, 1.0, self.position.0, self.position.1],
            text_version: 50,
            text_descriptor_version: 16,
            text,
            warp_version: 1,
            warp_descriptor_version: 16,
            warp: build_warp_descriptor()?,
            trailing: vec![0; 19],
        };
        let mut writer = BeWriter::new();
        block.write(&mut writer)?;
        Ok(writer.into_inner())
    }
}

impl<T: BitDepth> Layer<T> {
    /// A new text layer with [`TextLayerBuilder`] defaults.
    pub fn new_text(name: impl Into<String>, text: &str) -> psd_core::Result<Self> {
        TextLayerBuilder::new(name, text).build()
    }
}

// ---------------------------------------------------------------------------
// EngineData
// ---------------------------------------------------------------------------

fn float(value: f64) -> EngineValue {
    EngineValue::float(value)
}

fn int(value: i64) -> EngineValue {
    EngineValue::integer(value)
}

fn boolean(value: bool) -> EngineValue {
    EngineValue::boolean(value)
}

fn floats(values: &[f64]) -> EngineValue {
    engine_array(values.iter().copied().map(float))
}

/// A UTF-16BE (BOM-prefixed) EngineData literal string.
fn utf16_literal(text: &str) -> EngineValue {
    EngineValue::literal_string_bytes(engine_data::encode_utf16be_literal(text))
}

fn utf16_units_literal(units: &[u16]) -> EngineValue {
    EngineValue::literal_string_bytes(engine_data::encode_utf16be_literal_units(units, true))
}

fn color(values: [f64; 4]) -> EngineValue {
    engine_dictionary([("Type", int(1)), ("Values", floats(&values))])
}

fn adjustments() -> EngineValue {
    engine_dictionary([
        ("Axis", floats(&[1.0, 0.0, 1.0])),
        ("XY", floats(&[0.0, 0.0])),
    ])
}

fn paragraph_properties(auto_hyphenate: bool) -> EngineValue {
    engine_dictionary([
        ("Justification", int(0)),
        ("FirstLineIndent", float(0.0)),
        ("StartIndent", float(0.0)),
        ("EndIndent", float(0.0)),
        ("SpaceBefore", float(0.0)),
        ("SpaceAfter", float(0.0)),
        ("AutoHyphenate", boolean(auto_hyphenate)),
        ("HyphenatedWordSize", int(6)),
        ("PreHyphen", int(2)),
        ("PostHyphen", int(2)),
        ("ConsecutiveHyphens", int(8)),
        ("Zone", float(36.0)),
        ("WordSpacing", floats(&[0.8, 1.0, 1.33])),
        ("LetterSpacing", floats(&[0.0, 0.0, 0.0])),
        ("GlyphSpacing", floats(&[1.0, 1.0, 1.0])),
        ("AutoLeading", float(1.2)),
        ("LeadingType", int(0)),
        ("Hanging", boolean(false)),
        ("Burasagari", boolean(false)),
        ("KinsokuOrder", int(0)),
        ("EveryLineComposer", boolean(false)),
    ])
}

/// The `Normal RGB` character sheet Photoshop stores in both resource dicts.
fn normal_style_sheet(fill_color: [f64; 4]) -> EngineValue {
    let data = engine_dictionary([
        ("Font", int(0)),
        ("FontSize", float(12.0)),
        ("FauxBold", boolean(false)),
        ("FauxItalic", boolean(false)),
        ("AutoLeading", boolean(true)),
        ("Leading", float(0.0)),
        ("HorizontalScale", float(1.0)),
        ("VerticalScale", float(1.0)),
        ("Tracking", int(0)),
        ("BaselineShift", float(0.0)),
        ("AutoKerning", boolean(true)),
        ("Kerning", int(0)),
        ("FontCaps", int(0)),
        ("FontBaseline", int(0)),
        ("Underline", boolean(false)),
        ("Strikethrough", boolean(false)),
        ("Ligatures", boolean(true)),
        ("DLigatures", boolean(false)),
        ("BaselineDirection", int(2)),
        ("Tsume", float(0.0)),
        ("StyleRunAlignment", int(2)),
        ("Language", int(0)),
        ("NoBreak", boolean(false)),
        ("CharacterDirection", int(0)),
        ("HindiNumbers", boolean(false)),
        ("Kashida", int(1)),
        ("DiacriticPos", int(2)),
        ("FillColor", color(fill_color)),
        ("StrokeColor", color([1.0, 0.0, 0.0, 0.0])),
        ("FillFlag", boolean(true)),
        ("StrokeFlag", boolean(false)),
        ("FillFirst", boolean(false)),
        ("YUnderline", int(1)),
        ("OutlineWidth", float(1.0)),
    ]);
    engine_dictionary([
        ("Name", utf16_literal("Normal RGB")),
        ("StyleSheetData", data),
    ])
}

fn normal_paragraph_sheet() -> EngineValue {
    engine_dictionary([
        ("Name", utf16_literal("Normal RGB")),
        ("DefaultStyleSheet", int(0)),
        ("Properties", paragraph_properties(true)),
    ])
}

/// `PhotoshopKinsokuHard` / `PhotoshopKinsokuSoft` line-breaking sets.
fn kinsoku_sets() -> EngineValue {
    const HARD_NO_START: &[u16] = &[
        0x3001, 0x3002, 0xFF0C, 0xFF0E, 0x30FB, 0xFF1A, 0xFF1B, 0xFF1F, 0xFF01, 0x30FC, 0x2015,
        0x2019, 0x201D, 0xFF09, 0x3015, 0xFF3D, 0xFF5D, 0x3009, 0x300B, 0x300D, 0x300F, 0x3011,
        0x30FD, 0x30FE, 0x309D, 0x309E, 0x3005, 0x3041, 0x3043, 0x3045, 0x3047, 0x3049, 0x3063,
        0x3083, 0x3085, 0x3087, 0x308E, 0x30A1, 0x30A3, 0x30A5, 0x30A7, 0x30A9, 0x30C3, 0x30E3,
        0x30E5, 0x30E7, 0x30EE, 0x30F5, 0x30F6, 0x309B, 0x309C, 0x003F, 0x0021, 0x0029, 0x005D,
        0x007D, 0x002C, 0x002E, 0x003A, 0x003B, 0x2103, 0x2109, 0x00A2, 0xFF05, 0x2030,
    ];
    const HARD_NO_END: &[u16] = &[
        0x2018, 0x201C, 0xFF08, 0x3014, 0xFF3B, 0xFF5B, 0x3008, 0x300A, 0x300C, 0x300E, 0x3010,
        0x0028, 0x005B, 0x007B, 0xFFE5, 0xFF04, 0x00A3, 0xFF20, 0x00A7, 0x3012, 0xFF03,
    ];
    const SOFT_NO_START: &[u16] = &[
        0x3001, 0x3002, 0xFF0C, 0xFF0E, 0x30FB, 0xFF1A, 0xFF1B, 0xFF1F, 0xFF01, 0x2019, 0x201D,
        0xFF09, 0x3015, 0xFF3D, 0xFF5D, 0x3009, 0x300B, 0x300D, 0x300F, 0x3011, 0x30FD, 0x30FE,
        0x309D, 0x309E, 0x3005,
    ];
    const SOFT_NO_END: &[u16] = &[
        0x2018, 0x201C, 0xFF08, 0x3014, 0xFF3B, 0xFF5B, 0x3008, 0x300A, 0x300C, 0x300E, 0x3010,
    ];
    const KEEP: &[u16] = &[0x2015, 0x2025];
    const HANGING: &[u16] = &[0x3001, 0x3002, 0x002E, 0x002C];

    let set = |name: &str, no_start: &[u16], no_end: &[u16]| {
        engine_dictionary([
            ("Name", utf16_literal(name)),
            ("NoStart", utf16_units_literal(no_start)),
            ("NoEnd", utf16_units_literal(no_end)),
            ("Keep", utf16_units_literal(KEEP)),
            ("Hanging", utf16_units_literal(HANGING)),
        ])
    };
    engine_array([
        set("PhotoshopKinsokuHard", HARD_NO_START, HARD_NO_END),
        set("PhotoshopKinsokuSoft", SOFT_NO_START, SOFT_NO_END),
    ])
}

fn mojikumi_sets() -> EngineValue {
    engine_array((1..=4).map(|index| {
        engine_dictionary([(
            "InternalName",
            utf16_literal(&format!("Photoshop6MojiKumiSet{index}")),
        )])
    }))
}

fn font_set(font_name: &str) -> EngineValue {
    let font = |name: &str, font_type: i64| {
        engine_dictionary([
            ("Name", utf16_literal(name)),
            ("Script", int(0)),
            ("FontType", int(font_type)),
            ("Synthetic", int(0)),
        ])
    };
    engine_array([font(font_name, 1), font(SENTINEL_FONT, 0)])
}

/// `ResourceDict` and the root-level `DocumentResources` share one layout.
fn resources(font_name: &str, fill_color: [f64; 4]) -> EngineValue {
    engine_dictionary([
        ("KinsokuSet", kinsoku_sets()),
        ("MojiKumiSet", mojikumi_sets()),
        ("TheNormalStyleSheet", int(0)),
        ("TheNormalParagraphSheet", int(0)),
        (
            "ParagraphSheetSet",
            engine_array([normal_paragraph_sheet()]),
        ),
        (
            "StyleSheetSet",
            engine_array([normal_style_sheet(fill_color)]),
        ),
        ("FontSet", font_set(font_name)),
        ("SuperscriptSize", float(0.583)),
        ("SuperscriptPosition", float(0.333)),
        ("SubscriptSize", float(0.583)),
        ("SubscriptPosition", float(0.333)),
        ("SmallCapSize", float(0.7)),
    ])
}

fn build_engine_data(
    units: &[u16],
    font_name: &str,
    font_size: f64,
    fill_color: [f64; 4],
    box_width: f64,
    box_height: f64,
) -> psd_core::Result<Vec<u8>> {
    // EngineData text carries Photoshop's terminal carriage return.
    let mut engine_text = units.to_vec();
    engine_text.push(u16::from(b'\r'));
    let run_length = i64::try_from(engine_text.len())
        .ok()
        .filter(|&length| length <= i64::from(i32::MAX))
        .ok_or_else(|| invalid("text builder run exceeds the EngineData i32 range"))?;
    // Photoshop keeps one paragraph run per `\r`-terminated paragraph
    // (upstream writes a single run spanning every paragraph).
    let paragraph_lengths: Vec<i64> = engine_text
        .split_inclusive(|&unit| unit == u16::from(b'\r'))
        .map(|paragraph| paragraph.len() as i64)
        .collect();
    let leading = (font_size * 1.2 * 10.0).round() / 10.0;

    let paragraph_run = engine_dictionary([
        (
            "DefaultRunData",
            engine_dictionary([
                (
                    "ParagraphSheet",
                    engine_dictionary([
                        ("DefaultStyleSheet", int(0)),
                        ("Properties", EngineValue::dict()),
                    ]),
                ),
                ("Adjustments", adjustments()),
            ]),
        ),
        (
            "RunArray",
            engine_array(paragraph_lengths.iter().map(|_| {
                engine_dictionary([
                    (
                        "ParagraphSheet",
                        engine_dictionary([
                            ("DefaultStyleSheet", int(0)),
                            ("Properties", paragraph_properties(false)),
                        ]),
                    ),
                    ("Adjustments", adjustments()),
                ])
            })),
        ),
        (
            "RunLengthArray",
            engine_array(paragraph_lengths.iter().copied().map(int)),
        ),
        ("IsJoinable", int(1)),
    ]);

    // DefaultRunData carries the base font/size so Photoshop shows the right
    // size for an insertion point after the last character.
    let default_style = engine_dictionary([
        ("Font", int(0)),
        ("FontSize", float(font_size)),
        ("FauxBold", boolean(false)),
        ("FauxItalic", boolean(false)),
        ("AutoLeading", boolean(true)),
        ("Leading", float(font_size * 1.2)),
        ("HorizontalScale", int(1)),
        ("VerticalScale", int(1)),
        ("Tracking", int(0)),
        ("AutoKerning", boolean(true)),
        ("BaselineDirection", int(1)),
        ("FillColor", color(fill_color)),
    ]);
    let run_style = engine_dictionary([
        ("Font", int(0)),
        ("FontSize", float(font_size)),
        ("FauxBold", boolean(false)),
        ("FauxItalic", boolean(false)),
        ("AutoLeading", boolean(true)),
        ("Leading", float(leading)),
        ("HorizontalScale", float(1.0)),
        ("VerticalScale", float(1.0)),
        ("Tracking", int(0)),
        ("AutoKerning", boolean(true)),
        ("Kerning", int(0)),
        ("BaselineShift", float(0.0)),
        ("FontCaps", int(0)),
        ("FontBaseline", int(0)),
        ("Underline", boolean(false)),
        ("Strikethrough", boolean(false)),
        ("Ligatures", boolean(true)),
        ("DLigatures", boolean(false)),
        ("BaselineDirection", int(1)),
        ("Tsume", float(0.0)),
        ("StyleRunAlignment", int(2)),
        ("Language", int(14)),
        ("NoBreak", boolean(false)),
        ("FillColor", color(fill_color)),
        ("StrokeColor", color([1.0, 0.0, 0.0, 0.0])),
        ("FillFlag", boolean(true)),
        ("StrokeFlag", boolean(false)),
        ("FillFirst", boolean(false)),
        ("YUnderline", int(1)),
        ("OutlineWidth", float(1.0)),
        ("HindiNumbers", boolean(false)),
        ("Kashida", int(1)),
    ]);
    let style_sheet = |data: EngineValue| {
        engine_dictionary([("StyleSheet", engine_dictionary([("StyleSheetData", data)]))])
    };
    let style_run = engine_dictionary([
        ("DefaultRunData", style_sheet(default_style)),
        ("RunArray", engine_array([style_sheet(run_style)])),
        ("RunLengthArray", engine_array([int(run_length)])),
        ("IsJoinable", int(2)),
    ]);

    let grid_color = || color([0.0, 0.0, 0.0, 1.0]);
    let grid_info = engine_dictionary([
        ("GridIsOn", boolean(false)),
        ("ShowGrid", boolean(false)),
        ("GridSize", float(18.0)),
        ("GridLeading", float(22.0)),
        ("GridColor", grid_color()),
        ("GridLeadingFillColor", grid_color()),
        ("AlignLineHeightToGridFlags", boolean(false)),
    ]);

    // Box text at the origin: on-disk BoxBounds order is [top left right bottom].
    let shape = engine_dictionary([
        ("ShapeType", int(1)),
        ("Procession", int(0)),
        (
            "Lines",
            engine_dictionary([
                ("WritingDirection", int(0)),
                ("Children", EngineValue::array()),
            ]),
        ),
        (
            "Cookie",
            engine_dictionary([(
                "Photoshop",
                engine_dictionary([
                    ("ShapeType", int(1)),
                    ("BoxBounds", floats(&[0.0, 0.0, box_width, box_height])),
                    (
                        "Base",
                        engine_dictionary([
                            ("ShapeType", int(1)),
                            ("TransformPoint0", floats(&[1.0, 0.0])),
                            ("TransformPoint1", floats(&[0.0, 1.0])),
                            ("TransformPoint2", floats(&[0.0, 0.0])),
                        ]),
                    ),
                ]),
            )]),
        ),
    ]);
    let rendered = engine_dictionary([
        ("Version", int(1)),
        (
            "Shapes",
            engine_dictionary([
                ("WritingDirection", int(0)),
                ("Children", engine_array([shape])),
            ]),
        ),
    ]);

    let engine_dict = engine_dictionary([
        (
            "Editor",
            engine_dictionary([("Text", utf16_units_literal(&engine_text))]),
        ),
        ("ParagraphRun", paragraph_run),
        ("StyleRun", style_run),
        ("GridInfo", grid_info),
        ("AntiAlias", int(1)),
        ("UseFractionalGlyphWidths", boolean(true)),
        ("Rendered", rendered),
    ]);
    Ok(engine_data::serialize(&engine_dictionary([
        ("EngineDict", engine_dict),
        ("ResourceDict", resources(font_name, fill_color)),
        ("DocumentResources", resources(font_name, fill_color)),
    ])))
}

// ---------------------------------------------------------------------------
// Descriptors
// ---------------------------------------------------------------------------

/// Photoshop writes descriptor class names as a single NUL code unit.
fn descriptor_name() -> psd_core::Result<UnicodeString> {
    UnicodeString::new("\0", 1)
}

fn item(key: DescriptorKey, value: DescriptorValue) -> DescriptorItem {
    DescriptorItem { key, value }
}

fn char_id(code: &[u8; 4]) -> DescriptorKey {
    DescriptorKey::char_id(*code)
}

/// `{Left, Top, Right, Bottom}` in points, as a `bounds`-style descriptor.
fn points_rect(
    class: &str,
    [left, top, right, bottom]: [f64; 4],
) -> psd_core::Result<DescriptorValue> {
    let point = |value| DescriptorValue::UnitFloat {
        unit: *b"#Pnt",
        value,
    };
    Ok(DescriptorValue::Descriptor(Descriptor {
        name: descriptor_name()?,
        class_id: DescriptorKey::new(class),
        items: vec![
            item(char_id(b"Left"), point(left)),
            item(char_id(b"Top "), point(top)),
            item(char_id(b"Rght"), point(right)),
            item(char_id(b"Btom"), point(bottom)),
        ],
    }))
}

fn build_text_descriptor(
    units: &[u16],
    engine: Vec<u8>,
    bounds: [f64; 4],
    bounding_box: [f64; 4],
) -> psd_core::Result<Descriptor> {
    // `Txt ` ends with a NUL code unit, like Photoshop-authored layers.
    let text = String::from_utf16(units).map_err(|_| invalid("text is not valid UTF-16"))? + "\0";
    Ok(Descriptor {
        name: descriptor_name()?,
        class_id: char_id(b"TxLr"),
        items: vec![
            item(
                char_id(b"Txt "),
                DescriptorValue::String(UnicodeString::new(text, 1)?),
            ),
            item(
                DescriptorKey::new("textGridding"),
                DescriptorValue::Enumerated {
                    type_id: DescriptorKey::new("textGridding"),
                    value: char_id(b"None"),
                },
            ),
            item(
                char_id(b"Ornt"),
                DescriptorValue::Enumerated {
                    type_id: char_id(b"Ornt"),
                    value: char_id(b"Hrzn"),
                },
            ),
            item(
                char_id(b"AntA"),
                DescriptorValue::Enumerated {
                    type_id: char_id(b"Annt"),
                    value: char_id(b"AnCr"),
                },
            ),
            item(DescriptorKey::new("bounds"), points_rect("bounds", bounds)?),
            item(
                DescriptorKey::new("boundingBox"),
                points_rect("boundingBox", bounding_box)?,
            ),
            item(DescriptorKey::new("TextIndex"), DescriptorValue::Integer(1)),
            item(
                DescriptorKey::new("EngineData"),
                DescriptorValue::RawData {
                    os_key: OS_RAW_DATA,
                    data: engine,
                },
            ),
        ],
    })
}

fn build_warp_descriptor() -> psd_core::Result<Descriptor> {
    Ok(Descriptor {
        name: descriptor_name()?,
        class_id: DescriptorKey::new("warp"),
        items: vec![
            item(
                DescriptorKey::new("warpStyle"),
                DescriptorValue::Enumerated {
                    type_id: DescriptorKey::new("warpStyle"),
                    value: DescriptorKey::new("warpNone"),
                },
            ),
            item(
                DescriptorKey::new("warpValue"),
                DescriptorValue::Double(0.0),
            ),
            item(
                DescriptorKey::new("warpPerspective"),
                DescriptorValue::Double(0.0),
            ),
            item(
                DescriptorKey::new("warpPerspectiveOther"),
                DescriptorValue::Double(0.0),
            ),
            item(
                DescriptorKey::new("warpRotate"),
                DescriptorValue::Enumerated {
                    type_id: char_id(b"Ornt"),
                    value: char_id(b"Hrzn"),
                },
            ),
        ],
    })
}
