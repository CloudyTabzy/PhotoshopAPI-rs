//! Core on-disk enums and their raw representations.
//!
//! Mirrors `Util/Enum.h`. Every enum that round-trips through the file format
//! gets `from_raw`/`as_raw`; extend this file as more of the enum zoo is ported
//! (blend modes, channel IDs, tagged-block keys come with the sections that
//! need them).

use crate::descriptor::DescriptorKey;
use crate::error::{PsdError, Result};

/// Container version. Drives every variable length marker in the format:
/// PSD (1) uses 2/4-byte mixes, PSB (2) uses 8-byte lengths for layer/mask
/// info, layer info, and per-channel image data sizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Version {
    Psd,
    Psb,
}

impl Version {
    pub fn from_raw(raw: u16) -> Result<Self> {
        match raw {
            1 => Ok(Self::Psd),
            2 => Ok(Self::Psb),
            other => Err(PsdError::UnsupportedVersion(other)),
        }
    }

    pub const fn as_raw(self) -> u16 {
        match self {
            Self::Psd => 1,
            Self::Psb => 2,
        }
    }
}

/// Channel bit depth. The port supports 8/16/32-bit documents; the format
/// also defines 1-bit (bitmap) which we reject here for now, matching upstream's
/// supported-feature set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BitDepth {
    Eight,
    Sixteen,
    ThirtyTwo,
}

impl BitDepth {
    pub fn from_raw(raw: u16) -> Result<Self> {
        match raw {
            8 => Ok(Self::Eight),
            16 => Ok(Self::Sixteen),
            32 => Ok(Self::ThirtyTwo),
            other => Err(PsdError::UnsupportedBitDepth(other)),
        }
    }

    pub const fn as_raw(self) -> u16 {
        match self {
            Self::Eight => 8,
            Self::Sixteen => 16,
            Self::ThirtyTwo => 32,
        }
    }

    /// Bytes per channel sample on disk.
    pub const fn bytes_per_sample(self) -> u32 {
        match self {
            Self::Eight => 1,
            Self::Sixteen => 2,
            Self::ThirtyTwo => 4,
        }
    }
}

/// Document color mode (`colorModeMap` upstream; note the on-disk gaps at 5/6).
/// RGB/CMYK/Grayscale are first-class; Indexed round-trips; the rest are
/// recognized but unsupported for editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorMode {
    Bitmap,
    Grayscale,
    Indexed,
    Rgb,
    Cmyk,
    Multichannel,
    Duotone,
    Lab,
}

impl ColorMode {
    pub fn from_raw(raw: u16) -> Result<Self> {
        match raw {
            0 => Ok(Self::Bitmap),
            1 => Ok(Self::Grayscale),
            2 => Ok(Self::Indexed),
            3 => Ok(Self::Rgb),
            4 => Ok(Self::Cmyk),
            7 => Ok(Self::Multichannel),
            8 => Ok(Self::Duotone),
            9 => Ok(Self::Lab),
            other => Err(PsdError::UnsupportedColorMode(other)),
        }
    }

    pub const fn as_raw(self) -> u16 {
        match self {
            Self::Bitmap => 0,
            Self::Grayscale => 1,
            Self::Indexed => 2,
            Self::Rgb => 3,
            Self::Cmyk => 4,
            Self::Multichannel => 7,
            Self::Duotone => 8,
            Self::Lab => 9,
        }
    }
}

/// Per-layer/per-scanline compression codec (`Compression.h` upstream).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Compression {
    /// Uncompressed bytes.
    Raw,
    /// PackBits run-length encoding.
    Rle,
    /// zlib-wrapped deflate.
    Zip,
    /// Per-scanline delta (f32: byte-deinterleaved first) then deflate.
    ZipPrediction,
}

impl Compression {
    pub fn from_raw(raw: u16) -> Result<Self> {
        match raw {
            0 => Ok(Self::Raw),
            1 => Ok(Self::Rle),
            2 => Ok(Self::Zip),
            3 => Ok(Self::ZipPrediction),
            other => Err(PsdError::UnsupportedCompression(other)),
        }
    }

    pub const fn as_raw(self) -> u16 {
        match self {
            Self::Raw => 0,
            Self::Rle => 1,
            Self::Zip => 2,
            Self::ZipPrediction => 3,
        }
    }
}

/// Resolution unit of a `ResolutionInfo` image-resource block.
///
/// Upstream (`Enum::resolutionUnitMap`) tolerates unknown values by defaulting
/// to pixels-per-inch with a warning, so [`from_raw`](Self::from_raw) returns
/// an `Option` and lets the block parser decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ResolutionUnit {
    #[default]
    PixelsPerInch,
    PixelsPerCm,
}

impl ResolutionUnit {
    pub fn from_raw(raw: u16) -> Option<Self> {
        match raw {
            1 => Some(Self::PixelsPerInch),
            2 => Some(Self::PixelsPerCm),
            _ => None,
        }
    }

    pub const fn as_raw(self) -> u16 {
        match self {
            Self::PixelsPerInch => 1,
            Self::PixelsPerCm => 2,
        }
    }
}

/// Display unit of a `ResolutionInfo` image-resource block.
///
/// Unknown values default to inches upstream; see [`ResolutionUnit`] for why
/// [`from_raw`](Self::from_raw) is fallible here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DisplayUnit {
    #[default]
    Inches,
    Cm,
    Points,
    Picas,
    Columns,
}

impl DisplayUnit {
    pub fn from_raw(raw: u16) -> Option<Self> {
        match raw {
            1 => Some(Self::Inches),
            2 => Some(Self::Cm),
            3 => Some(Self::Points),
            4 => Some(Self::Picas),
            5 => Some(Self::Columns),
            _ => None,
        }
    }

    pub const fn as_raw(self) -> u16 {
        match self {
            Self::Inches => 1,
            Self::Cm => 2,
            Self::Points => 3,
            Self::Picas => 4,
            Self::Columns => 5,
        }
    }
}

/// Per-layer channel identifier (`Enum::ChannelID`).
///
/// The raw `i16` index on disk is the authoritative value; this enum is the
/// interpreted view for a given color mode (`toChannelIDInfo` upstream).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelId {
    Red,
    Green,
    Blue,
    Cyan,
    Magenta,
    Yellow,
    Black,
    Gray,
    /// User-defined channel with an index outside the mode's color channels.
    Custom,
    Alpha,
    UserSuppliedLayerMask,
    RealUserSuppliedLayerMask,
}

impl ChannelId {
    /// Interpret a layer-record channel index for the given color mode.
    ///
    /// `-1`/`-2`/`-3` are alpha / user mask / real user mask in every mode;
    /// unsupported color modes report every color channel as
    /// [`Custom`](Self::Custom) (upstream falls back to a defaulted struct
    /// that loses the index entirely).
    pub fn from_index(index: i16, color_mode: ColorMode) -> Self {
        match index {
            -1 => Self::Alpha,
            -2 => Self::UserSuppliedLayerMask,
            -3 => Self::RealUserSuppliedLayerMask,
            _ => match color_mode {
                ColorMode::Rgb => match index {
                    0 => Self::Red,
                    1 => Self::Green,
                    2 => Self::Blue,
                    _ => Self::Custom,
                },
                ColorMode::Cmyk => match index {
                    0 => Self::Cyan,
                    1 => Self::Magenta,
                    2 => Self::Yellow,
                    3 => Self::Black,
                    _ => Self::Custom,
                },
                ColorMode::Grayscale => match index {
                    0 => Self::Gray,
                    _ => Self::Custom,
                },
                _ => Self::Custom,
            },
        }
    }
}

/// Section divider type stored in an `lsct` tagged block
/// (`Enum::SectionDivider`): group start/end markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SectionDivider {
    /// `0` — any other type of layer.
    Any,
    /// `1` — start of an open group.
    OpenFolder,
    /// `2` — start of a closed group.
    ClosedFolder,
    /// `3` — bounding section divider (end of a group).
    BoundingSection,
}

impl SectionDivider {
    pub fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Any),
            1 => Some(Self::OpenFolder),
            2 => Some(Self::ClosedFolder),
            3 => Some(Self::BoundingSection),
            _ => None,
        }
    }

    pub const fn as_raw(self) -> u32 {
        match self {
            Self::Any => 0,
            Self::OpenFolder => 1,
            Self::ClosedFolder => 2,
            Self::BoundingSection => 3,
        }
    }
}

/// Layer blend mode as its 4-byte on-disk key (`norm`, `mul `, …).
///
/// A transparent wrapper rather than a closed enum: the format defines ~30
/// modes and unknown keys must round-trip. Constants for the full upstream
/// `blendModeMap` are added alongside the document API when it needs them.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlendMode([u8; 4]);

impl BlendMode {
    pub const NORMAL: Self = Self(*b"norm");
    /// Groups pass compositing through to their children.
    pub const PASSTHROUGH: Self = Self(*b"pass");
    pub const DISSOLVE: Self = Self(*b"diss");
    pub const DARKEN: Self = Self(*b"dark");
    pub const MULTIPLY: Self = Self(*b"mul ");
    pub const COLOR_BURN: Self = Self(*b"idiv");
    pub const LINEAR_BURN: Self = Self(*b"lbrn");
    pub const DARKER_COLOR: Self = Self(*b"dkCl");
    pub const LIGHTEN: Self = Self(*b"lite");
    pub const SCREEN: Self = Self(*b"scrn");
    pub const COLOR_DODGE: Self = Self(*b"div ");
    pub const LINEAR_DODGE: Self = Self(*b"lddg");
    pub const LIGHTER_COLOR: Self = Self(*b"lgCl");
    pub const OVERLAY: Self = Self(*b"over");
    pub const SOFT_LIGHT: Self = Self(*b"sLit");
    pub const HARD_LIGHT: Self = Self(*b"hLit");
    pub const VIVID_LIGHT: Self = Self(*b"vLit");
    pub const LINEAR_LIGHT: Self = Self(*b"lLit");
    pub const PIN_LIGHT: Self = Self(*b"pLit");
    pub const HARD_MIX: Self = Self(*b"hMix");
    pub const DIFFERENCE: Self = Self(*b"diff");
    pub const EXCLUSION: Self = Self(*b"smud");
    pub const SUBTRACT: Self = Self(*b"fsub");
    pub const DIVIDE: Self = Self(*b"fdiv");
    pub const HUE: Self = Self(*b"hue ");
    pub const SATURATION: Self = Self(*b"sat ");
    pub const COLOR: Self = Self(*b"colr");
    pub const LUMINOSITY: Self = Self(*b"lum ");

    pub const fn from_bytes(bytes: [u8; 4]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(self) -> [u8; 4] {
        self.0
    }

    /// The key as a (possibly lossy) string, e.g. for tracing.
    pub fn as_str(&self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }

    /// Decode a descriptor `BlnM` enumerator, as layer effects and vector
    /// strokes store it. Descriptors spell blend modes differently from layer
    /// records (`Mltp` rather than `mul `). Both the historical ID (a char ID
    /// such as `Mltp`, or a string ID such as `linearBurn` for newer modes)
    /// and the string ID Photoshop 2026 writes instead (`multiply`,
    /// `colorBurn`) are accepted. Other values return `None`.
    pub fn from_descriptor_enum(value: &[u8]) -> Option<Self> {
        DESCRIPTOR_BLEND_MODES
            .iter()
            .find(|(historical, current, _)| *historical == value || *current == value)
            .map(|(_, _, mode)| *mode)
    }

    /// This mode's historical descriptor enumerator, which every Photoshop
    /// version reads: a zero-length char ID (`Mltp`) or, for modes without
    /// one, an explicit string ID (`linearBurn`). `None` for keys that have
    /// no descriptor spelling.
    pub fn to_descriptor_enum(self) -> Option<DescriptorKey> {
        let (historical, _, _) = DESCRIPTOR_BLEND_MODES
            .iter()
            .find(|(_, _, mode)| *mode == self)?;
        Some(match <[u8; 4]>::try_from(*historical) {
            Ok(code) => DescriptorKey::char_id(code),
            Err(_) => DescriptorKey::from_bytes(*historical),
        })
    }
}

/// Descriptor blend modes as `(historical ID, Photoshop 2026 ID, record key)`.
/// The historical IDs follow the public ag-psd-rs table
/// (<https://github.com/Vasyanator/ag-psd-rs>); the 2026 IDs are the ones a
/// Photoshop 2026 document writes for every layer-record blend mode.
const DESCRIPTOR_BLEND_MODES: [(&[u8], &[u8], BlendMode); 28] = [
    (b"Nrml", b"normal", BlendMode::NORMAL),
    (b"Dslv", b"dissolve", BlendMode::DISSOLVE),
    (b"Drkn", b"darken", BlendMode::DARKEN),
    (b"Mltp", b"multiply", BlendMode::MULTIPLY),
    (b"CBrn", b"colorBurn", BlendMode::COLOR_BURN),
    (b"linearBurn", b"linearBurn", BlendMode::LINEAR_BURN),
    (b"darkerColor", b"darkerColor", BlendMode::DARKER_COLOR),
    (b"Lghn", b"lighten", BlendMode::LIGHTEN),
    (b"Scrn", b"screen", BlendMode::SCREEN),
    (b"CDdg", b"colorDodge", BlendMode::COLOR_DODGE),
    (b"linearDodge", b"linearDodge", BlendMode::LINEAR_DODGE),
    (b"lighterColor", b"lighterColor", BlendMode::LIGHTER_COLOR),
    (b"Ovrl", b"overlay", BlendMode::OVERLAY),
    (b"SftL", b"softLight", BlendMode::SOFT_LIGHT),
    (b"HrdL", b"hardLight", BlendMode::HARD_LIGHT),
    (b"vividLight", b"vividLight", BlendMode::VIVID_LIGHT),
    (b"linearLight", b"linearLight", BlendMode::LINEAR_LIGHT),
    (b"pinLight", b"pinLight", BlendMode::PIN_LIGHT),
    (b"hardMix", b"hardMix", BlendMode::HARD_MIX),
    (b"Dfrn", b"difference", BlendMode::DIFFERENCE),
    (b"Xclu", b"exclusion", BlendMode::EXCLUSION),
    (
        b"blendSubtraction",
        b"blendSubtraction",
        BlendMode::SUBTRACT,
    ),
    (b"blendDivide", b"blendDivide", BlendMode::DIVIDE),
    (b"H   ", b"hue", BlendMode::HUE),
    (b"Strt", b"saturation", BlendMode::SATURATION),
    (b"Clr ", b"color", BlendMode::COLOR),
    (b"Lmns", b"luminosity", BlendMode::LUMINOSITY),
    (b"passThrough", b"passThrough", BlendMode::PASSTHROUGH),
];

impl Default for BlendMode {
    fn default() -> Self {
        Self::NORMAL
    }
}

impl std::fmt::Debug for BlendMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

impl std::fmt::Display for BlendMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_str())
    }
}

/// Layer display color shown in Photoshop's layers panel (the `lclr` block's
/// leading `u16`, `Enum::LayerColor` upstream).
///
/// Unknown values are kept in [`LayerColor::Other`] so they round-trip; the
/// `fuschia` spelling of upstream's enum is corrected to `Fuchsia` here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LayerColor {
    #[default]
    None,
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Violet,
    Gray,
    Seafoam,
    Indigo,
    Magenta,
    Fuchsia,
    Other(u16),
}

impl LayerColor {
    pub const fn from_raw(raw: u16) -> Self {
        match raw {
            0 => Self::None,
            1 => Self::Red,
            2 => Self::Orange,
            3 => Self::Yellow,
            4 => Self::Green,
            5 => Self::Blue,
            6 => Self::Violet,
            7 => Self::Gray,
            8 => Self::Seafoam,
            9 => Self::Indigo,
            10 => Self::Magenta,
            11 => Self::Fuchsia,
            other => Self::Other(other),
        }
    }

    pub const fn as_raw(self) -> u16 {
        match self {
            Self::None => 0,
            Self::Red => 1,
            Self::Orange => 2,
            Self::Yellow => 3,
            Self::Green => 4,
            Self::Blue => 5,
            Self::Violet => 6,
            Self::Gray => 7,
            Self::Seafoam => 8,
            Self::Indigo => 9,
            Self::Magenta => 10,
            Self::Fuchsia => 11,
            Self::Other(raw) => raw,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_blend_modes_accept_historical_and_2026_ids() {
        // Pairs a Photoshop 2026 document writes for the same modes.
        for (historical, current, mode) in [
            (&b"Nrml"[..], &b"normal"[..], BlendMode::NORMAL),
            (b"Mltp", b"multiply", BlendMode::MULTIPLY),
            (b"CBrn", b"colorBurn", BlendMode::COLOR_BURN),
            (b"linearBurn", b"linearBurn", BlendMode::LINEAR_BURN),
            (b"CDdg", b"colorDodge", BlendMode::COLOR_DODGE),
            (b"SftL", b"softLight", BlendMode::SOFT_LIGHT),
            (b"Xclu", b"exclusion", BlendMode::EXCLUSION),
            (
                b"blendSubtraction",
                b"blendSubtraction",
                BlendMode::SUBTRACT,
            ),
            (b"H   ", b"hue", BlendMode::HUE),
            (b"Lmns", b"luminosity", BlendMode::LUMINOSITY),
        ] {
            assert_eq!(BlendMode::from_descriptor_enum(historical), Some(mode));
            assert_eq!(BlendMode::from_descriptor_enum(current), Some(mode));
            assert_eq!(mode.to_descriptor_enum().unwrap().as_bytes(), historical);
        }
        // Every record mode except the record-only keys has both spellings.
        for (historical, current, mode) in DESCRIPTOR_BLEND_MODES {
            assert_eq!(BlendMode::from_descriptor_enum(historical), Some(mode));
            assert_eq!(BlendMode::from_descriptor_enum(current), Some(mode));
        }
        let char_id = BlendMode::MULTIPLY.to_descriptor_enum().unwrap();
        assert!(char_id.uses_implicit_length());
        let string_id = BlendMode::VIVID_LIGHT.to_descriptor_enum().unwrap();
        assert!(!string_id.uses_implicit_length());
        assert_eq!(BlendMode::from_descriptor_enum(b"mul "), None);
        assert_eq!(BlendMode::from_descriptor_enum(b"Hght"), None);
        assert_eq!(BlendMode::from_bytes(*b"xxxx").to_descriptor_enum(), None);
    }

    #[test]
    fn layer_color_round_trips_known_and_unknown_values() {
        for raw in 0u16..=12 {
            assert_eq!(LayerColor::from_raw(raw).as_raw(), raw);
        }
        assert_eq!(LayerColor::from_raw(6), LayerColor::Violet);
        assert_eq!(LayerColor::from_raw(12), LayerColor::Other(12));
    }

    #[test]
    fn round_trip_through_raw_values() {
        for raw in [1u16, 2] {
            assert_eq!(Version::from_raw(raw).unwrap().as_raw(), raw);
        }
        for raw in [8u16, 16, 32] {
            assert_eq!(BitDepth::from_raw(raw).unwrap().as_raw(), raw);
        }
        for raw in [0u16, 1, 2, 3, 4, 7, 8, 9] {
            assert_eq!(ColorMode::from_raw(raw).unwrap().as_raw(), raw);
        }
        for raw in [0u16, 1, 2, 3] {
            assert_eq!(Compression::from_raw(raw).unwrap().as_raw(), raw);
        }
    }

    #[test]
    fn rejects_unknown_values() {
        assert!(matches!(
            Version::from_raw(3),
            Err(PsdError::UnsupportedVersion(3))
        ));
        assert!(matches!(
            BitDepth::from_raw(1),
            Err(PsdError::UnsupportedBitDepth(1))
        ));
        assert!(matches!(
            ColorMode::from_raw(5),
            Err(PsdError::UnsupportedColorMode(5))
        ));
        assert!(matches!(
            Compression::from_raw(4),
            Err(PsdError::UnsupportedCompression(4))
        ));
    }

    #[test]
    fn unit_enums_round_trip_known_values() {
        for raw in [1u16, 2] {
            assert_eq!(ResolutionUnit::from_raw(raw).unwrap().as_raw(), raw);
        }
        for raw in [1u16, 2, 3, 4, 5] {
            assert_eq!(DisplayUnit::from_raw(raw).unwrap().as_raw(), raw);
        }
        // Unknown values are tolerated by the parser, not rejected.
        assert!(ResolutionUnit::from_raw(0).is_none());
        assert!(DisplayUnit::from_raw(6).is_none());
    }

    #[test]
    fn channel_id_mapping_per_color_mode() {
        assert_eq!(ChannelId::from_index(0, ColorMode::Rgb), ChannelId::Red);
        assert_eq!(ChannelId::from_index(2, ColorMode::Rgb), ChannelId::Blue);
        assert_eq!(ChannelId::from_index(3, ColorMode::Rgb), ChannelId::Custom);
        assert_eq!(ChannelId::from_index(3, ColorMode::Cmyk), ChannelId::Black);
        assert_eq!(
            ChannelId::from_index(0, ColorMode::Grayscale),
            ChannelId::Gray
        );
        assert_eq!(
            ChannelId::from_index(7, ColorMode::Indexed),
            ChannelId::Custom
        );
        for mode in [ColorMode::Rgb, ColorMode::Cmyk, ColorMode::Grayscale] {
            assert_eq!(ChannelId::from_index(-1, mode), ChannelId::Alpha);
            assert_eq!(
                ChannelId::from_index(-2, mode),
                ChannelId::UserSuppliedLayerMask
            );
            assert_eq!(
                ChannelId::from_index(-3, mode),
                ChannelId::RealUserSuppliedLayerMask
            );
        }
    }

    #[test]
    fn section_divider_round_trips() {
        for raw in 0u32..=3 {
            assert_eq!(SectionDivider::from_raw(raw).unwrap().as_raw(), raw);
        }
        assert!(SectionDivider::from_raw(4).is_none());
    }

    #[test]
    fn blend_mode_preserves_unknown_keys() {
        assert_eq!(BlendMode::default(), BlendMode::NORMAL);
        assert_eq!(BlendMode::NORMAL.as_str(), "norm");
        let custom = BlendMode::from_bytes(*b"mul ");
        assert_eq!(custom.as_bytes(), *b"mul ");
        assert_eq!(format!("{custom:?}"), "\"mul \"");
    }

    #[test]
    fn blend_mode_constants_match_upstream_map() {
        // Every entry of upstream `blendModeMap`, pinned.
        let all = [
            (BlendMode::PASSTHROUGH, *b"pass"),
            (BlendMode::NORMAL, *b"norm"),
            (BlendMode::DISSOLVE, *b"diss"),
            (BlendMode::DARKEN, *b"dark"),
            (BlendMode::MULTIPLY, *b"mul "),
            (BlendMode::COLOR_BURN, *b"idiv"),
            (BlendMode::LINEAR_BURN, *b"lbrn"),
            (BlendMode::DARKER_COLOR, *b"dkCl"),
            (BlendMode::LIGHTEN, *b"lite"),
            (BlendMode::SCREEN, *b"scrn"),
            (BlendMode::COLOR_DODGE, *b"div "),
            (BlendMode::LINEAR_DODGE, *b"lddg"),
            (BlendMode::LIGHTER_COLOR, *b"lgCl"),
            (BlendMode::OVERLAY, *b"over"),
            (BlendMode::SOFT_LIGHT, *b"sLit"),
            (BlendMode::HARD_LIGHT, *b"hLit"),
            (BlendMode::VIVID_LIGHT, *b"vLit"),
            (BlendMode::LINEAR_LIGHT, *b"lLit"),
            (BlendMode::PIN_LIGHT, *b"pLit"),
            (BlendMode::HARD_MIX, *b"hMix"),
            (BlendMode::DIFFERENCE, *b"diff"),
            (BlendMode::EXCLUSION, *b"smud"),
            (BlendMode::SUBTRACT, *b"fsub"),
            (BlendMode::DIVIDE, *b"fdiv"),
            (BlendMode::HUE, *b"hue "),
            (BlendMode::SATURATION, *b"sat "),
            (BlendMode::COLOR, *b"colr"),
            (BlendMode::LUMINOSITY, *b"lum "),
        ];
        for (mode, key) in all {
            assert_eq!(mode.as_bytes(), key);
        }
        // All 27 constants are distinct keys.
        let mut keys: Vec<[u8; 4]> = all.iter().map(|(mode, _)| mode.as_bytes()).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), all.len());
    }
}
