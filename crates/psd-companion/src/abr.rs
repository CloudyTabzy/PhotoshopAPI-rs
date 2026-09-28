//! Photoshop brush presets (`.abr`).
//!
//! The file starts with two big-endian `i16`s: a major version and a minor
//! version. Major versions 6, 7, 9 and 10 (minor 1 or 2) are the 8BIM-block
//! layout Photoshop has written since CS2: a series of `8BIM` sections keyed
//! `samp`, `desc`, `patt` and `phry`, each length-prefixed and padded to four
//! bytes.
//!
//! - `samp` carries the sampled brush tips: one record per tip with a
//!   Pascal-string id, a rectangle, a bit depth (8 or 16) and a compression
//!   flag (raw or per-row PackBits), holding the tip's alpha mask.
//! - `desc` carries one action descriptor whose `Brsh` key lists the presets —
//!   the same descriptor machinery the port's adjustment and effect views read.
//! - `patt` carries texture patterns, parsed by [`crate::pattern`].
//! - `phry` carries a descriptor this reader parses and ignores.
//!
//! Major versions 1 and 2 are the pre-CS entry-stream layout. No fixture or
//! reference for them exists in this workspace, so they are rejected with a
//! named error rather than decoded from guesswork.
//!
//! The typed brush model and the descriptor key spellings follow the
//! workspace's ag-psd port, which reads real Photoshop output.

use psd_core::descriptor::{Descriptor, DescriptorValue};
use psd_core::io::BeReader;
use psd_core::strings::PascalString;

use crate::error::{invalid, Error, Result};
use crate::packbits::decode_packbits_row;
use crate::pattern::read_pattern;

// ===========================================================================
// Data model
// ===========================================================================

/// A parsed `.abr` file: its presets, sampled tips and texture patterns.
#[derive(Debug, Clone, Default)]
pub struct Abr {
    pub brushes: Vec<Brush>,
    pub samples: Vec<SampleInfo>,
    pub patterns: Vec<crate::Pattern>,
}

/// One sampled brush tip's alpha mask.
#[derive(Debug, Clone)]
pub struct SampleInfo {
    /// The Pascal-string id a `sampledBrush` preset refers to.
    pub id: String,
    pub bounds: SampleBounds,
    /// The tip's alpha, `w * h` bytes.
    pub alpha: Vec<u8>,
}

/// A sampled tip's rectangle, in tip coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SampleBounds {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// One dynamics control: what drives it and how far it travels.
#[derive(Debug, Clone, PartialEq)]
pub struct BrushDynamics {
    /// `off`, `fade`, `pen pressure`, `pen tilt`, `stylus wheel`,
    /// `initial direction`, `direction`, `initial rotation` or `rotation`.
    pub control: String,
    pub steps: f64,
    pub jitter: f64,
    pub minimum: f64,
}

/// A brush tip: computed from numbers, sampled from a `samp` record, or one
/// of the two live-tip kinds.
#[derive(Debug, Clone, PartialEq)]
pub enum BrushShape {
    /// A round or flat tip computed from diameter, angle, roundness, hardness.
    Computed {
        size: f64,
        angle: f64,
        roundness: f64,
        hardness: f64,
        spacing_on: bool,
        spacing: f64,
        flip_x: bool,
        flip_y: bool,
    },
    /// A tip sampled from a `samp` record, referred to by id and name.
    Sampled {
        name: String,
        size: f64,
        angle: f64,
        roundness: f64,
        spacing_on: bool,
        spacing: f64,
        flip_x: bool,
        flip_y: bool,
        sampled_data: String,
    },
    /// An erodible or custom tip made of one or more "tips".
    Tips {
        angle: f64,
        size: f64,
        shape: String,
        physics: bool,
        spacing: f64,
        spacing_on: bool,
        flip_x: bool,
        flip_y: bool,
        tips_type: String,
        tips_length_ratio: f64,
        tips_hardness: f64,
        tips_grid_size: Option<f64>,
        tips_erodible_tip_height_map: Option<Vec<u8>>,
        tips_airbrush_cutoff_angle: f64,
        tips_airbrush_granularity: f64,
        tips_airbrush_streakiness: f64,
        tips_airbrush_splat_size: f64,
        tips_airbrush_splat_count: f64,
    },
    /// A dynamic (hair) brush tip.
    Dynamic {
        size: f64,
        angle: f64,
        shape: String,
        density: f64,
        length: f64,
        clumping: f64,
        thickness: f64,
        stiffness: f64,
        physics: bool,
        spacing: f64,
        spacing_on: bool,
        flip_x: bool,
        flip_y: bool,
    },
}

/// How the tip's size, angle and roundness vary along a stroke.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapeDynamics {
    pub size_dynamics: BrushDynamics,
    pub minimum_diameter: f64,
    pub tilt_scale: f64,
    pub angle_dynamics: BrushDynamics,
    pub roundness_dynamics: BrushDynamics,
    pub minimum_roundness: f64,
    pub flip_x: bool,
    pub flip_y: bool,
    pub brush_projection: bool,
}

/// Where along the stroke the tip lands.
#[derive(Debug, Clone, PartialEq)]
pub struct Scatter {
    pub both_axes: bool,
    pub scatter_dynamics: BrushDynamics,
    pub count_dynamics: BrushDynamics,
    pub count: f64,
}

/// A texture mapped onto the tip.
#[derive(Debug, Clone, PartialEq)]
pub struct Texture {
    pub id: String,
    pub name: String,
    pub invert: bool,
    pub scale: f64,
    pub brightness: f64,
    pub contrast: f64,
    pub blend_mode: BrushBlendMode,
    pub depth: f64,
    pub depth_minimum: f64,
    pub depth_dynamics: BrushDynamics,
    pub texture_each_tip: bool,
}

/// Two tips painted alternately.
#[derive(Debug, Clone, PartialEq)]
pub struct DualBrush {
    pub flip: bool,
    pub shape: BrushShape,
    pub blend_mode: BrushBlendMode,
    pub use_scatter: bool,
    pub spacing: f64,
    pub count: f64,
    pub both_axes: bool,
    pub count_dynamics: BrushDynamics,
    pub scatter_dynamics: BrushDynamics,
}

/// How the stroke's colour varies.
#[derive(Debug, Clone, PartialEq)]
pub struct ColorDynamics {
    pub foreground_background: BrushDynamics,
    pub hue: f64,
    pub saturation: f64,
    pub brightness: f64,
    pub purity: f64,
    pub per_tip: bool,
}

/// How flow, opacity, wetness and mix vary along a stroke.
#[derive(Debug, Clone, PartialEq)]
pub struct Transfer {
    pub flow_dynamics: BrushDynamics,
    pub opacity_dynamics: BrushDynamics,
    pub wetness_dynamics: BrushDynamics,
    pub mix_dynamics: BrushDynamics,
}

/// A stylus pose that overrides the tip's angle and tilt.
#[derive(Debug, Clone, PartialEq)]
pub struct BrushPose {
    pub override_angle: bool,
    pub override_tilt_x: bool,
    pub override_tilt_y: bool,
    pub override_pressure: bool,
    pub pressure: f64,
    pub tilt_x: f64,
    pub tilt_y: f64,
    pub angle: f64,
}

/// Which tool a preset's tool options belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolType {
    Brush,
    MixerBrush,
    SmudgeBrush,
}

/// A preset's tool options: the brush-tool state Photoshop restores with it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOptions {
    pub tool_type: ToolType,
    pub brush_preset: bool,
    pub flow: f64,
    pub wetness: Option<f64>,
    pub dryness: Option<f64>,
    pub mix: Option<f64>,
    pub smooth: f64,
    pub mode: BrushBlendMode,
    pub opacity: f64,
    pub smoothing: bool,
    pub smoothing_value: f64,
    pub smoothing_radius_mode: bool,
    pub smoothing_catchup: bool,
    pub smoothing_catchup_at_end: bool,
    pub smoothing_zoom_compensation: bool,
    pub pressure_smoothing: bool,
    pub use_pressure_overrides_size: bool,
    pub use_pressure_overrides_opacity: bool,
    pub use_legacy: bool,
    pub auto_fill: Option<bool>,
    pub auto_clean: Option<bool>,
    pub load_solid_color_only: Option<bool>,
    pub sample_all_layers: Option<bool>,
    pub flow_dynamics: Option<BrushDynamics>,
    pub opacity_dynamics: Option<BrushDynamics>,
    pub size_dynamics: Option<BrushDynamics>,
    pub smudge_finger_painting: Option<bool>,
    pub smudge_sample_all_layers: Option<bool>,
    pub strength: Option<f64>,
}

/// One brush preset.
#[derive(Debug, Clone, PartialEq)]
pub struct Brush {
    pub name: String,
    pub shape: BrushShape,
    pub shape_dynamics: Option<ShapeDynamics>,
    pub scatter: Option<Scatter>,
    pub texture: Option<Texture>,
    pub dual_brush: Option<DualBrush>,
    pub color_dynamics: Option<ColorDynamics>,
    pub transfer: Option<Transfer>,
    pub brush_pose: Option<BrushPose>,
    pub noise: bool,
    pub wet_edges: bool,
    pub protect_texture: Option<bool>,
    pub spacing: f64,
    pub interpretation: Option<bool>,
    pub use_brush_size: bool,
    pub tool_options: Option<ToolOptions>,
}

/// A paint or blend mode as a brush preset spells it: the descriptor
/// enumerator's key, decoded where it is known. The last three are brush-only
/// modes the layer blend-mode set does not carry; anything else is kept
/// verbatim rather than silently mapped to normal.
#[derive(Debug, Clone, PartialEq)]
pub enum BrushBlendMode {
    Normal,
    Dissolve,
    Darken,
    Multiply,
    ColorBurn,
    LinearBurn,
    DarkerColor,
    Lighten,
    Screen,
    ColorDodge,
    LinearDodge,
    LighterColor,
    Overlay,
    SoftLight,
    HardLight,
    VividLight,
    LinearLight,
    PinLight,
    HardMix,
    Difference,
    Exclusion,
    Subtract,
    Divide,
    Hue,
    Saturation,
    Color,
    Luminosity,
    LinearHeight,
    Height,
    Subtraction,
    /// A mode this reader does not know, with its key as the file spells it.
    Other(String),
}

impl BrushBlendMode {
    /// Decodes a `BlnM` enumerator key: the historical char ID (`Nrml`), the
    /// long string ID (`linearBurn`), or the Photoshop 2026 camelCase form
    /// (`colorBurn`). Unknown keys are kept, not defaulted.
    #[must_use]
    pub fn from_key(key: &str) -> Self {
        match key {
            "Nrml" | "normal" => Self::Normal,
            "Dslv" | "dissolve" => Self::Dissolve,
            "Drkn" | "darken" => Self::Darken,
            "Mltp" | "multiply" => Self::Multiply,
            "CBrn" | "colorBurn" => Self::ColorBurn,
            "linearBurn" => Self::LinearBurn,
            "darkerColor" => Self::DarkerColor,
            "Lghn" | "lighten" => Self::Lighten,
            "Scrn" | "screen" => Self::Screen,
            "CDdg" | "colorDodge" => Self::ColorDodge,
            "linearDodge" => Self::LinearDodge,
            "lighterColor" => Self::LighterColor,
            "Ovrl" | "overlay" => Self::Overlay,
            "SftL" | "softLight" => Self::SoftLight,
            "HrdL" | "hardLight" => Self::HardLight,
            "vividLight" => Self::VividLight,
            "linearLight" => Self::LinearLight,
            "pinLight" => Self::PinLight,
            "hardMix" => Self::HardMix,
            "Dfrn" | "difference" => Self::Difference,
            "Xclu" | "exclusion" => Self::Exclusion,
            "blendSubtraction" => Self::Subtract,
            "blendDivide" => Self::Divide,
            "H   " | "hue" => Self::Hue,
            "Strt" | "saturation" => Self::Saturation,
            "Clr " | "color" => Self::Color,
            "Lmns" | "luminosity" => Self::Luminosity,
            "linearHeight" => Self::LinearHeight,
            "Hght" => Self::Height,
            "Sbtr" => Self::Subtraction,
            other => Self::Other(other.to_owned()),
        }
    }
}

// ===========================================================================
// Descriptor accessors
// ===========================================================================

fn as_bool(value: Option<&DescriptorValue>) -> bool {
    matches!(value, Some(DescriptorValue::Boolean(true)))
}

fn as_opt_bool(value: Option<&DescriptorValue>) -> Option<bool> {
    match value {
        Some(DescriptorValue::Boolean(b)) => Some(*b),
        _ => None,
    }
}

fn as_number(value: Option<&DescriptorValue>) -> f64 {
    match value {
        Some(DescriptorValue::Integer(i)) => f64::from(*i),
        Some(DescriptorValue::LargeInteger(i)) => *i as f64,
        Some(DescriptorValue::Double(d)) => *d,
        Some(DescriptorValue::UnitFloat { value, .. }) => *value,
        _ => 0.0,
    }
}

fn as_opt_number(value: Option<&DescriptorValue>) -> Option<f64> {
    match value {
        Some(DescriptorValue::Integer(i)) => Some(f64::from(*i)),
        Some(DescriptorValue::LargeInteger(i)) => Some(*i as f64),
        Some(DescriptorValue::Double(d)) => Some(*d),
        Some(DescriptorValue::UnitFloat { value, .. }) => Some(*value),
        _ => None,
    }
}

fn as_text(value: Option<&DescriptorValue>) -> String {
    match value {
        Some(DescriptorValue::String(s)) => s.value().to_owned(),
        Some(DescriptorValue::Enumerated { value, .. }) => value.as_str().into_owned(),
        _ => String::new(),
    }
}

fn as_descriptor(value: Option<&DescriptorValue>) -> Option<&Descriptor> {
    match value {
        Some(DescriptorValue::Descriptor(d)) => Some(d),
        _ => None,
    }
}

fn as_raw(value: Option<&DescriptorValue>) -> Option<&[u8]> {
    match value {
        Some(DescriptorValue::RawData { data, .. }) => Some(data),
        _ => None,
    }
}

/// Reads a `UntF` unit value and requires it to carry `expected` (`#Ang`,
/// `#Prc`, `#Pxl`).
fn unit_value(descriptor: &Descriptor, key: &str, expected: &[u8; 4]) -> Result<Option<f64>> {
    match descriptor.get(key) {
        Some(DescriptorValue::UnitFloat { unit, value }) => {
            if unit == expected {
                Ok(Some(*value))
            } else {
                Err(invalid_key(key, unit))
            }
        }
        _ => Ok(None),
    }
}

fn invalid_key(key: &str, unit: &[u8; 4]) -> Error {
    Error::Invalid {
        offset: 0,
        message: format!(
            "key {key:?} carries units {}, which this reader does not accept here",
            String::from_utf8_lossy(unit)
        ),
    }
}

/// A percent: 1.0 when absent, the value divided by 100 when present.
fn percent(descriptor: &Descriptor, key: &str) -> Result<f64> {
    Ok(unit_value(descriptor, key, b"#Prc")?.map_or(1.0, |v| v / 100.0))
}

/// An angle: 0.0 when absent.
fn angle(descriptor: &Descriptor, key: &str) -> Result<f64> {
    Ok(unit_value(descriptor, key, b"#Ang")?.unwrap_or(0.0))
}

/// A pixel length: required when the key is present at all.
fn pixels(descriptor: &Descriptor, key: &str) -> Result<f64> {
    unit_value(descriptor, key, b"#Pxl")?.ok_or_else(|| Error::Invalid {
        offset: 0,
        message: format!("missing pixel value {key:?}"),
    })
}

// ===========================================================================
// Brush model parsing
// ===========================================================================

/// The `bVTy` control values, in Photoshop's own order. Pinned against a
/// Photoshop 2026 export whose every dynamic was set to a distinct value, and
/// confirmed by two independent format references; a widely used parser
/// library carries a different order from index 5 on, which reads `rotation`
/// as `initial direction` and shifts everything after it.
const DYNAMICS_CONTROL: [&str; 8] = [
    "off",
    "fade",
    "pen pressure",
    "pen tilt",
    "stylus wheel",
    "rotation",
    "initial direction",
    "direction",
];

const DYNAMIC_BRUSH_SHAPES: [&str; 10] = [
    "round point",
    "round blunt",
    "round curve",
    "round angle",
    "round fan",
    "flat point",
    "flat blunt",
    "flat curve",
    "flat angle",
    "flat fan",
];

const TIPS_SHAPES: [&str; 6] = [
    "erodible point",
    "erodible flat",
    "erodible round",
    "erodible square",
    "erodible triangle",
    "custom",
];

fn shape_name(table: &[&str], index: f64) -> String {
    table.get(index as usize).copied().unwrap_or("").to_owned()
}

fn blend_mode(descriptor: &Descriptor, key: &str) -> BrushBlendMode {
    BrushBlendMode::from_key(&as_text(descriptor.get(key)))
}

/// The `bVTy` control name for a raw index; unknown values degrade to `off`,
/// as the reference does.
fn dynamics_control(index: usize) -> &'static str {
    DYNAMICS_CONTROL.get(index).copied().unwrap_or("off")
}

fn dynamics(descriptor: &Descriptor) -> Result<BrushDynamics> {
    let control_index = as_number(descriptor.get("bVTy")) as usize;
    Ok(BrushDynamics {
        control: dynamics_control(control_index).to_owned(),
        steps: as_number(descriptor.get("fStp")),
        jitter: percent(descriptor, "jitter")?,
        minimum: percent(descriptor, "Mnm ")?,
    })
}

fn dynamics_of(parent: &Descriptor, key: &str) -> Result<BrushDynamics> {
    match as_descriptor(parent.get(key)) {
        Some(d) => dynamics(d),
        None => Err(Error::Invalid {
            offset: 0,
            message: format!("missing dynamics descriptor {key:?}"),
        }),
    }
}

fn dynamics_opt(parent: &Descriptor, key: &str) -> Result<Option<BrushDynamics>> {
    match as_descriptor(parent.get(key)) {
        Some(d) => dynamics(d).map(Some),
        None => Ok(None),
    }
}

fn brush_shape(descriptor: &Descriptor) -> Result<BrushShape> {
    match descriptor.class_id.as_str().as_ref() {
        "computedBrush" => Ok(BrushShape::Computed {
            size: pixels(descriptor, "Dmtr")?,
            angle: angle(descriptor, "Angl")?,
            roundness: percent(descriptor, "Rndn")?,
            spacing_on: as_bool(descriptor.get("Intr")),
            spacing: percent(descriptor, "Spcn")?,
            flip_x: as_bool(descriptor.get("flipX")),
            flip_y: as_bool(descriptor.get("flipY")),
            hardness: percent(descriptor, "Hrdn")?,
        }),
        "sampledBrush" => Ok(BrushShape::Sampled {
            size: pixels(descriptor, "Dmtr")?,
            angle: angle(descriptor, "Angl")?,
            roundness: percent(descriptor, "Rndn")?,
            spacing_on: as_bool(descriptor.get("Intr")),
            spacing: percent(descriptor, "Spcn")?,
            flip_x: as_bool(descriptor.get("flipX")),
            flip_y: as_bool(descriptor.get("flipY")),
            name: as_text(descriptor.get("Nm  ")),
            sampled_data: as_text(descriptor.get("sampledData")),
        }),
        "dBrush" => Ok(BrushShape::Dynamic {
            shape: shape_name(&DYNAMIC_BRUSH_SHAPES, as_number(descriptor.get("Shp "))),
            angle: angle(descriptor, "Angl")?,
            size: pixels(descriptor, "Dmtr")?,
            density: percent(descriptor, "Dnst")?,
            length: percent(descriptor, "Lngt")?,
            clumping: percent(descriptor, "clumping")?,
            thickness: percent(descriptor, "thickness")?,
            stiffness: percent(descriptor, "stiffness")?,
            physics: as_bool(descriptor.get("physics")),
            spacing: percent(descriptor, "Spcn")?,
            spacing_on: as_bool(descriptor.get("Intr")),
            flip_x: as_bool(descriptor.get("flipX")),
            flip_y: as_bool(descriptor.get("flipY")),
        }),
        "dTips" => {
            let grid_size = as_number(descriptor.get("dtipsGridSize"));
            let height_map = as_raw(descriptor.get("dtipsErodibleTipHeightMap"));
            // Both fields are emitted only when the grid size is non-zero and
            // the height map is present, mirroring the reference's condition.
            let (tips_grid_size, tips_erodible_tip_height_map) = match height_map {
                Some(map) if grid_size != 0.0 => (Some(grid_size), Some(map.to_vec())),
                _ => (None, None),
            };
            Ok(BrushShape::Tips {
                angle: angle(descriptor, "Angl")?,
                size: pixels(descriptor, "Dmtr")?,
                shape: shape_name(&DYNAMIC_BRUSH_SHAPES, as_number(descriptor.get("Shp "))),
                physics: as_bool(descriptor.get("physics")),
                spacing: percent(descriptor, "Spcn")?,
                spacing_on: as_bool(descriptor.get("Intr")),
                flip_x: as_bool(descriptor.get("flipX")),
                flip_y: as_bool(descriptor.get("flipY")),
                tips_type: shape_name(&TIPS_SHAPES, as_number(descriptor.get("dtipsType"))),
                tips_length_ratio: percent(descriptor, "dtipsLengthRatio")?,
                tips_hardness: percent(descriptor, "dtipsHardness")?,
                tips_grid_size,
                tips_erodible_tip_height_map,
                tips_airbrush_cutoff_angle: as_number(descriptor.get("dtipsAirbrushCutoffAngle")),
                tips_airbrush_granularity: percent(descriptor, "dtipsAirbrushGranularity")?,
                tips_airbrush_streakiness: percent(descriptor, "dtipsAirbrushStreakiness")?,
                tips_airbrush_splat_size: percent(descriptor, "dtipsAirbrushSplatSize")?,
                tips_airbrush_splat_count: as_number(descriptor.get("dtipsAirbrushSplatCount")),
            })
        }
        other => Err(Error::Invalid {
            offset: 0,
            message: format!("unknown brush class id {other:?}"),
        }),
    }
}

fn tool_type(class_id: &str) -> ToolType {
    match class_id {
        "MixB" => ToolType::MixerBrush,
        "SmTl" => ToolType::SmudgeBrush,
        _ => ToolType::Brush,
    }
}

fn parse_brush(brush: &Descriptor) -> Result<Brush> {
    let shape = as_descriptor(brush.get("Brsh")).ok_or_else(|| Error::Invalid {
        offset: 0,
        message: "missing brush shape descriptor".to_owned(),
    })?;

    let mut parsed = Brush {
        name: as_text(brush.get("Nm  ")),
        shape: brush_shape(shape)?,
        spacing: percent(brush, "Spcn")?,
        wet_edges: as_bool(brush.get("Wtdg")),
        noise: as_bool(brush.get("Nose")),
        use_brush_size: as_bool(brush.get("useBrushSize")),
        shape_dynamics: None,
        scatter: None,
        texture: None,
        dual_brush: None,
        color_dynamics: None,
        transfer: None,
        brush_pose: None,
        protect_texture: None,
        interpretation: None,
        tool_options: None,
    };

    parsed.interpretation = as_opt_bool(brush.get("interpretation"));
    parsed.protect_texture = as_opt_bool(brush.get("protectTexture"));

    if as_bool(brush.get("useTipDynamics")) {
        parsed.shape_dynamics = Some(ShapeDynamics {
            tilt_scale: percent(brush, "tiltScale")?,
            size_dynamics: dynamics_of(brush, "szVr")?,
            angle_dynamics: dynamics_of(brush, "angleDynamics")?,
            roundness_dynamics: dynamics_of(brush, "roundnessDynamics")?,
            flip_x: as_bool(brush.get("flipX")),
            flip_y: as_bool(brush.get("flipY")),
            brush_projection: as_bool(brush.get("brushProjection")),
            minimum_diameter: percent(brush, "minimumDiameter")?,
            minimum_roundness: percent(brush, "minimumRoundness")?,
        });
    }

    if as_bool(brush.get("useScatter")) {
        parsed.scatter = Some(Scatter {
            count: as_number(brush.get("Cnt ")),
            both_axes: as_bool(brush.get("bothAxes")),
            count_dynamics: dynamics_of(brush, "countDynamics")?,
            scatter_dynamics: dynamics_of(brush, "scatterDynamics")?,
        });
    }

    if as_bool(brush.get("useTexture")) {
        if let Some(txtr) = as_descriptor(brush.get("Txtr")) {
            parsed.texture = Some(Texture {
                id: as_text(txtr.get("Idnt")),
                name: as_text(txtr.get("Nm  ")),
                blend_mode: blend_mode(brush, "textureBlendMode"),
                depth: percent(brush, "textureDepth")?,
                depth_minimum: percent(brush, "minimumDepth")?,
                depth_dynamics: dynamics_of(brush, "textureDepthDynamics")?,
                scale: percent(brush, "textureScale")?,
                invert: as_bool(brush.get("InvT")),
                brightness: as_number(brush.get("textureBrightness")),
                contrast: as_number(brush.get("textureContrast")),
                texture_each_tip: as_bool(brush.get("TxtC")),
            });
        }
    }

    if let Some(db) = as_descriptor(brush.get("dualBrush")) {
        if as_bool(db.get("useDualBrush")) {
            let db_shape = as_descriptor(db.get("Brsh")).ok_or_else(|| Error::Invalid {
                offset: 0,
                message: "missing dual brush shape".to_owned(),
            })?;
            parsed.dual_brush = Some(DualBrush {
                flip: as_bool(db.get("Flip")),
                shape: brush_shape(db_shape)?,
                blend_mode: blend_mode(db, "BlnM"),
                use_scatter: as_bool(db.get("useScatter")),
                spacing: percent(db, "Spcn")?,
                count: as_number(db.get("Cnt ")),
                both_axes: as_bool(db.get("bothAxes")),
                count_dynamics: dynamics_of(db, "countDynamics")?,
                scatter_dynamics: dynamics_of(db, "scatterDynamics")?,
            });
        }
    }

    if as_bool(brush.get("useColorDynamics")) {
        parsed.color_dynamics = Some(ColorDynamics {
            foreground_background: dynamics_of(brush, "clVr")?,
            hue: percent(brush, "H   ")?,
            saturation: percent(brush, "Strt")?,
            brightness: percent(brush, "Brgh")?,
            purity: percent(brush, "purity")?,
            per_tip: as_bool(brush.get("colorDynamicsPerTip")),
        });
    }

    if as_bool(brush.get("usePaintDynamics")) {
        parsed.transfer = Some(Transfer {
            flow_dynamics: dynamics_of(brush, "prVr")?,
            opacity_dynamics: dynamics_of(brush, "opVr")?,
            wetness_dynamics: dynamics_of(brush, "wtVr")?,
            mix_dynamics: dynamics_of(brush, "mxVr")?,
        });
    }

    if as_bool(brush.get("useBrushPose")) {
        parsed.brush_pose = Some(BrushPose {
            override_angle: as_bool(brush.get("overridePoseAngle")),
            override_tilt_x: as_bool(brush.get("overridePoseTiltX")),
            override_tilt_y: as_bool(brush.get("overridePoseTiltY")),
            override_pressure: as_bool(brush.get("overridePosePressure")),
            pressure: percent(brush, "brushPosePressure")?,
            tilt_x: as_number(brush.get("brushPoseTiltX")),
            tilt_y: as_number(brush.get("brushPoseTiltY")),
            angle: as_number(brush.get("brushPoseAngle")),
        });
    }

    if let Some(to) = as_descriptor(brush.get("toolOptions")) {
        let options = ToolOptions {
            tool_type: tool_type(to.class_id.as_str().as_ref()),
            brush_preset: as_bool(to.get("brushPreset")),
            flow: as_opt_number(to.get("flow")).unwrap_or(100.0),
            wetness: as_opt_number(to.get("wetness")),
            dryness: as_opt_number(to.get("dryness")),
            mix: as_opt_number(to.get("mix")),
            smooth: as_opt_number(to.get("Smoo")).unwrap_or(0.0),
            mode: {
                let mode = as_text(to.get("Md  "));
                BrushBlendMode::from_key(if mode.is_empty() { "Nrml" } else { &mode })
            },
            opacity: as_opt_number(to.get("Opct")).unwrap_or(100.0),
            smoothing: as_bool(to.get("smoothing")),
            smoothing_value: as_opt_number(to.get("smoothingValue")).unwrap_or(0.0),
            smoothing_radius_mode: as_bool(to.get("smoothingRadiusMode")),
            smoothing_catchup: as_bool(to.get("smoothingCatchup")),
            smoothing_catchup_at_end: as_bool(to.get("smoothingCatchupAtEnd")),
            smoothing_zoom_compensation: as_bool(to.get("smoothingZoomCompensation")),
            pressure_smoothing: as_bool(to.get("pressureSmoothing")),
            use_pressure_overrides_size: as_bool(to.get("usePressureOverridesSize")),
            use_pressure_overrides_opacity: as_bool(to.get("usePressureOverridesOpacity")),
            use_legacy: as_bool(to.get("useLegacy")),
            auto_fill: as_opt_bool(to.get("autoFill")),
            auto_clean: as_opt_bool(to.get("autoClean")),
            load_solid_color_only: as_opt_bool(to.get("loadSolidColorOnly")),
            sample_all_layers: as_opt_bool(to.get("sampleAllLayers")),
            flow_dynamics: dynamics_opt(to, "prVr")?,
            opacity_dynamics: dynamics_opt(to, "opVr")?,
            size_dynamics: dynamics_opt(to, "szVr")?,
            smudge_finger_painting: as_opt_bool(to.get("SmdF")),
            smudge_sample_all_layers: as_opt_bool(to.get("SmdS")),
            strength: as_opt_number(to.get("Prs ")),
        };
        parsed.tool_options = Some(options);
    }

    Ok(parsed)
}

// ===========================================================================
// Sample decoding
// ===========================================================================

/// Decodes one `samp` record's alpha channel. The reader sits after the
/// compression flag; `w * h` bytes are appended to `alpha`.
fn read_sample_alpha(
    reader: &mut BeReader<'_>,
    w: usize,
    h: usize,
    bit_depth: i16,
    compression: u8,
) -> Result<Vec<u8>> {
    let mut alpha = vec![0u8; w * h];
    match (bit_depth, compression) {
        (8, 0) => {
            let bytes = reader.take(alpha.len())?;
            alpha.copy_from_slice(bytes);
        }
        (8, 1) => {
            // One channel: a u16 length per row comes first as a table, then
            // the PackBits rows themselves. Both references read it this way,
            // and real Photoshop files confirm it.
            let mut lengths = vec![0usize; h];
            for length in &mut lengths {
                *length = reader.u16()? as usize;
            }
            for (row, length) in alpha.chunks_mut(w).zip(lengths) {
                let bytes = reader.take(length)?;
                decode_packbits_row(bytes, row);
            }
        }
        (16, 0) => {
            for sample in &mut alpha {
                *sample = (reader.u16()? >> 8) as u8;
            }
        }
        (16, 1) => {
            return Err(Error::Unsupported(
                "16-bit run-length-encoded brush samples are not decoded".to_owned(),
            ));
        }
        (depth, _) => {
            return Err(invalid(reader, format!("invalid sample bit depth {depth}")));
        }
    }
    Ok(alpha)
}

// ===========================================================================
// Main reader
// ===========================================================================

/// Parses a `.abr` brush file.
///
/// # Errors
///
/// [`Error::Unsupported`] for major versions 1 and 2, minor versions other
/// than 1 or 2, 16-bit run-length-encoded samples, or an unknown section key.
/// [`Error::Invalid`] for malformed sections, records or descriptors.
pub fn read_abr(bytes: &[u8]) -> Result<Abr> {
    let reader = &mut BeReader::new(bytes);
    let version = reader.i16()?;
    if !matches!(version, 6 | 7 | 9 | 10) {
        return Err(Error::Unsupported(format!(
            "ABR major version {version}; this reader knows 6, 7, 9 and 10 (1 and 2 are the pre-CS entry-stream layout)"
        )));
    }
    let minor_version = reader.i16()?;
    if minor_version != 1 && minor_version != 2 {
        return Err(Error::Unsupported(format!(
            "ABR minor version {minor_version}; this reader knows 1 and 2"
        )));
    }

    let mut abr = Abr::default();
    while reader.position() < bytes.len() {
        reader.signature("8BIM")?;
        let key = {
            let mut code = [0u8; 4];
            code.copy_from_slice(reader.take(4)?);
            code
        };
        let size = reader.u32()? as usize;
        let end = reader
            .position()
            .checked_add(size)
            .filter(|&end| end <= bytes.len())
            .ok_or_else(|| invalid(reader, "section length runs past the end of the file"))?;

        match &key {
            b"samp" => {
                while reader.position() < end {
                    let mut brush_length = reader.u32()? as usize;
                    brush_length = brush_length.div_ceil(4) * 4;
                    let brush_end = reader
                        .position()
                        .checked_add(brush_length)
                        .filter(|&end| end <= bytes.len())
                        .ok_or_else(|| invalid(reader, "sample length runs past the section"))?;

                    let id = PascalString::read(reader, 1)?.value().to_owned();
                    // Minor version 1 carries two `i16`s and a `u16` of
                    // unknowns before the rectangle; minor version 2 carries
                    // 264 bytes of unknowns instead.
                    reader.skip(if minor_version == 1 { 10 } else { 264 })?;
                    let (y, x, bottom, right) =
                        (reader.i32()?, reader.i32()?, reader.i32()?, reader.i32()?);
                    let (h, w) = (bottom - y, right - x);
                    if w <= 0 || h <= 0 {
                        return Err(invalid(reader, "invalid sample rectangle"));
                    }
                    let bit_depth = reader.i16()?;
                    let compression = reader.u8()?;
                    let alpha =
                        read_sample_alpha(reader, w as usize, h as usize, bit_depth, compression)?;

                    abr.samples.push(SampleInfo {
                        id,
                        bounds: SampleBounds { x, y, w, h },
                        alpha,
                    });
                    reader.seek(brush_end)?;
                }
            }
            b"desc" => {
                if reader.u32()? != 16 {
                    return Err(invalid(reader, "unsupported descriptor version"));
                }
                let descriptor = Descriptor::read(reader)?;
                if let Some(DescriptorValue::List(list)) = descriptor.get("Brsh") {
                    for item in list {
                        if let DescriptorValue::Descriptor(brush) = item {
                            abr.brushes.push(parse_brush(brush)?);
                        }
                    }
                }
            }
            b"patt" => {
                while reader.position() < end {
                    abr.patterns.push(read_pattern(reader)?);
                }
            }
            b"phry" => {
                // A descriptor whose meaning is not documented; parsed so the
                // framing stays honest, then ignored.
                if reader.u32()? != 16 {
                    return Err(invalid(reader, "unsupported descriptor version"));
                }
                Descriptor::read(reader)?;
            }
            other => {
                return Err(invalid(
                    reader,
                    format!("unknown brush section {}", String::from_utf8_lossy(other)),
                ));
            }
        }

        // Sections are padded up to a four-byte boundary; a size already on
        // the boundary has no padding at all.
        reader.skip(size.next_multiple_of(4) - size)?;
    }

    Ok(abr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use psd_core::descriptor::{DescriptorItem, DescriptorKey};
    use psd_core::io::BeWriter;
    use psd_core::strings::UnicodeString;

    fn item(key: &str, value: DescriptorValue) -> DescriptorItem {
        DescriptorItem {
            key: DescriptorKey::new(key),
            value,
        }
    }

    fn unit(key: &str, unit: &[u8; 4], value: f64) -> DescriptorItem {
        item(key, DescriptorValue::UnitFloat { unit: *unit, value })
    }

    /// A computed-brush preset descriptor: name, spacing, and the shape.
    fn computed_brush_descriptor() -> Descriptor {
        let shape = Descriptor {
            name: UnicodeString::new("", 1).expect("name"),
            class_id: DescriptorKey::new("computedBrush"),
            items: vec![
                unit("Dmtr", b"#Pxl", 30.0),
                unit("Angl", b"#Ang", 0.0),
                unit("Rndn", b"#Prc", 100.0),
                item("Intr", DescriptorValue::Boolean(true)),
                unit("Spcn", b"#Prc", 25.0),
                unit("Hrdn", b"#Prc", 0.0),
            ],
        };
        Descriptor {
            name: UnicodeString::new("", 1).expect("name"),
            class_id: DescriptorKey::new("preset"),
            items: vec![
                item(
                    "Nm  ",
                    DescriptorValue::String(UnicodeString::new("Soft Round", 1).expect("n")),
                ),
                unit("Spcn", b"#Prc", 100.0),
                item("Brsh", DescriptorValue::Descriptor(shape)),
            ],
        }
    }

    fn write_section(writer: &mut BeWriter, key: &[u8; 4], body: &[u8]) {
        writer.bytes(b"8BIM");
        writer.bytes(key);
        writer.u32(body.len() as u32);
        writer.bytes(body);
        while !writer.position().is_multiple_of(4) {
            writer.u8(0);
        }
    }

    /// A `samp` record: id, the version's unknowns, rectangle, depth,
    /// compression and pixels.
    fn sample_body(
        id: &str,
        minor: i16,
        w: i32,
        h: i32,
        depth: i16,
        compression: u8,
        pixels: &[u8],
    ) -> Vec<u8> {
        let mut body = BeWriter::new();
        PascalString::new(id, 1).write(&mut body).expect("write id");
        if minor == 1 {
            // Ten bytes of unknowns before the rectangle.
            body.u16(0);
            body.u16(0);
            body.u16(0);
            body.u16(0);
            body.u16(0);
        } else {
            body.bytes(&[0u8; 264]);
        }
        body.i32(0); // y
        body.i32(0); // x
        body.i32(h); // bottom
        body.i32(w); // right
        body.i16(depth);
        body.u8(compression);
        body.bytes(pixels);
        body.into_inner()
    }

    fn write_sample(writer: &mut BeWriter, body: &[u8]) {
        let mut length = body.len() as u32;
        while !length.is_multiple_of(4) {
            length += 1;
        }
        writer.u32(length);
        writer.bytes(body);
        while !writer.position().is_multiple_of(4) {
            writer.u8(0);
        }
    }

    fn abr(minor: i16, sections: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut writer = BeWriter::new();
        writer.i16(6);
        writer.i16(minor);
        for (key, body) in sections {
            write_section(&mut writer, key, body);
        }
        writer.into_inner()
    }

    #[test]
    fn rejects_versions_it_does_not_know() {
        for version in [1i16, 2, 5, 11] {
            let mut writer = BeWriter::new();
            writer.i16(version);
            writer.i16(1);
            assert!(
                matches!(read_abr(&writer.into_inner()), Err(Error::Unsupported(_))),
                "version {version}"
            );
        }
        let mut writer = BeWriter::new();
        writer.i16(6);
        writer.i16(3);
        assert!(matches!(
            read_abr(&writer.into_inner()),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn decodes_a_computed_brush_from_the_descriptor_section() {
        let mut desc_body = BeWriter::new();
        desc_body.u32(16);
        Descriptor {
            name: UnicodeString::new("", 1).expect("name"),
            class_id: DescriptorKey::char_id(*b"null"),
            items: vec![item(
                "Brsh",
                DescriptorValue::List(vec![DescriptorValue::Descriptor(
                    computed_brush_descriptor(),
                )]),
            )],
        }
        .write(&mut desc_body)
        .expect("write descriptor");

        let bytes = abr(1, &[(b"desc", desc_body.into_inner())]);
        let abr = read_abr(&bytes).expect("decode");
        assert_eq!(abr.brushes.len(), 1);
        let brush = &abr.brushes[0];
        assert_eq!(brush.name, "Soft Round");
        assert_eq!(brush.spacing, 1.0);
        assert!(!brush.wet_edges);
        assert!(!brush.noise);
        match &brush.shape {
            BrushShape::Computed {
                size,
                angle,
                roundness,
                hardness,
                spacing_on,
                spacing,
                ..
            } => {
                assert_eq!(
                    (*size, *angle, *roundness, *hardness),
                    (30.0, 0.0, 1.0, 0.0)
                );
                assert!(*spacing_on);
                assert_eq!(*spacing, 0.25);
            }
            other => panic!("expected a computed brush, got {other:?}"),
        }
    }

    #[test]
    fn decodes_raw_and_rle_samples() {
        let raw = sample_body("raw", 1, 2, 2, 8, 0, &[10, 20, 30, 40]);
        // One PackBits row per height, each preceded by its u16 length: a
        // literal packet (header 1) of two bytes, twice.
        // The length table comes first, then the rows: each row is a literal
        // packet (header 1) of two bytes, and its length covers the header.
        let mut rle_pixels = Vec::new();
        for _ in 0..2 {
            rle_pixels.extend_from_slice(&3u16.to_be_bytes());
        }
        for row in [[200u8, 201], [202, 203]] {
            rle_pixels.extend_from_slice(&[1, row[0], row[1]]);
        }
        let rle = sample_body("rle", 1, 2, 2, 8, 1, &rle_pixels);

        let mut samp_body = BeWriter::new();
        write_sample(&mut samp_body, &raw);
        write_sample(&mut samp_body, &rle);

        let bytes = abr(1, &[(b"samp", samp_body.into_inner())]);
        let abr = read_abr(&bytes).expect("decode");
        assert_eq!(abr.samples.len(), 2);
        assert_eq!(abr.samples[0].id, "raw");
        assert_eq!(
            abr.samples[0].bounds,
            SampleBounds {
                x: 0,
                y: 0,
                w: 2,
                h: 2
            }
        );
        assert_eq!(abr.samples[0].alpha, vec![10, 20, 30, 40]);
        assert_eq!(abr.samples[1].id, "rle");
        assert_eq!(abr.samples[1].alpha, vec![200, 201, 202, 203]);
    }

    #[test]
    fn decodes_a_16bit_raw_sample_to_its_high_bytes() {
        let mut samp_body = BeWriter::new();
        let mut body = BeWriter::new();
        PascalString::new("deep", 1)
            .write(&mut body)
            .expect("write id");
        body.u16(0);
        body.u16(0);
        body.u16(0);
        body.u16(0);
        body.u16(0);
        body.i32(0);
        body.i32(0);
        body.i32(1); // bottom
        body.i32(2); // right
        body.i16(16);
        body.u8(0);
        body.u16(0x1234);
        body.u16(0xABCD);
        let mut length = body.position() as u32;
        while !length.is_multiple_of(4) {
            length += 1;
        }
        samp_body.u32(length);
        samp_body.bytes(&body.into_inner());
        while !samp_body.position().is_multiple_of(4) {
            samp_body.u8(0);
        }

        let bytes = abr(1, &[(b"samp", samp_body.into_inner())]);
        let abr = read_abr(&bytes).expect("decode");
        assert_eq!(abr.samples[0].alpha, vec![0x12, 0xAB]);
    }

    #[test]
    fn rejects_an_unknown_section_and_a_bad_sample_rectangle() {
        let bytes = abr(1, &[(b"wibb", Vec::new())]);
        assert!(matches!(read_abr(&bytes), Err(Error::Invalid { .. })));

        let bad = sample_body("bad", 1, 0, 2, 8, 0, &[]);
        let mut samp_body = BeWriter::new();
        write_sample(&mut samp_body, &bad);
        let bytes = abr(1, &[(b"samp", samp_body.into_inner())]);
        assert!(matches!(read_abr(&bytes), Err(Error::Invalid { .. })));
    }

    #[test]
    fn a_full_file_decodes_every_section() {
        let mut desc_body = BeWriter::new();
        desc_body.u32(16);
        Descriptor {
            name: UnicodeString::new("", 1).expect("name"),
            class_id: DescriptorKey::char_id(*b"null"),
            items: vec![item(
                "Brsh",
                DescriptorValue::List(vec![DescriptorValue::Descriptor(
                    computed_brush_descriptor(),
                )]),
            )],
        }
        .write(&mut desc_body)
        .expect("write descriptor");

        let mut samp_body = BeWriter::new();
        write_sample(
            &mut samp_body,
            &sample_body("tip", 1, 2, 2, 8, 0, &[1, 2, 3, 4]),
        );

        let mut patt_body = BeWriter::new();
        patt_body.bytes(&crate::pattern::test_support::rgb_pattern_record());

        let mut phry_body = BeWriter::new();
        phry_body.u32(16);
        Descriptor {
            name: UnicodeString::new("", 1).expect("name"),
            class_id: DescriptorKey::char_id(*b"null"),
            items: vec![],
        }
        .write(&mut phry_body)
        .expect("write descriptor");

        let bytes = abr(
            1,
            &[
                (b"samp", samp_body.into_inner()),
                (b"desc", desc_body.into_inner()),
                (b"patt", patt_body.into_inner()),
                (b"phry", phry_body.into_inner()),
            ],
        );
        let abr = read_abr(&bytes).expect("decode");
        assert_eq!(abr.brushes.len(), 1);
        assert_eq!(abr.samples.len(), 1);
        assert_eq!(abr.samples[0].alpha, vec![1, 2, 3, 4]);
        assert_eq!(abr.patterns.len(), 1);
        assert_eq!(abr.patterns[0].id, "pat-id");
    }

    #[test]
    fn dynamics_controls_follow_photoshop_order() {
        // Pinned against a Photoshop 2026 export whose every dynamic was set
        // to a distinct value and confirmed by two independent format
        // references. A widely used parser library orders these differently
        // from index 5 on, which silently renames the angle controls.
        assert_eq!(dynamics_control(0), "off");
        assert_eq!(dynamics_control(1), "fade");
        assert_eq!(dynamics_control(2), "pen pressure");
        assert_eq!(dynamics_control(3), "pen tilt");
        assert_eq!(dynamics_control(4), "stylus wheel");
        assert_eq!(dynamics_control(5), "rotation");
        assert_eq!(dynamics_control(6), "initial direction");
        assert_eq!(dynamics_control(7), "direction");
        assert_eq!(dynamics_control(8), "off", "unknown values degrade to off");
    }

    #[test]
    fn blend_mode_keys_decode_including_the_brush_only_ones() {
        assert_eq!(BrushBlendMode::from_key("Nrml"), BrushBlendMode::Normal);
        assert_eq!(
            BrushBlendMode::from_key("colorBurn"),
            BrushBlendMode::ColorBurn
        );
        assert_eq!(
            BrushBlendMode::from_key("linearHeight"),
            BrushBlendMode::LinearHeight
        );
        assert_eq!(BrushBlendMode::from_key("Hght"), BrushBlendMode::Height);
        assert_eq!(
            BrushBlendMode::from_key("Sbtr"),
            BrushBlendMode::Subtraction
        );
        assert_eq!(
            BrushBlendMode::from_key("wibble"),
            BrushBlendMode::Other("wibble".to_owned())
        );
    }
}
