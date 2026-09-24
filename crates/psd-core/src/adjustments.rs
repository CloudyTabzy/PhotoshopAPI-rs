//! Read-only views of adjustment- and fill-layer settings blocks.
//!
//! An adjustment layer stores its settings in one tagged block whose key names
//! the adjustment (`levl`, `curv`, `hue2`, ...); a fill layer uses `SoCo`,
//! `GdFl`, or `PtFl`. Photoshop may add a `CgEd` block with preset names or
//! the modern brightness/contrast values. The key list matches the Adobe
//! PSD/PSB specification's "Adjustment layer" section and upstream's detection
//! list in `LayeredFile/Impl/LayeredFileImpl.cpp`.
//!
//! Binary layouts follow the specification's "Additional File Formats"
//! chapter; the block payload is the matching preset file. The specification
//! does not describe the `curv` map flag and `Crv ` extension, the `blnc`
//! layout, or the 20-byte gradient-map color stop. Those layouts follow the
//! public `psd-tools` (<https://github.com/psd-tools/psd-tools>) and `ag-psd-rs`
//! (<https://github.com/Vasyanator/ag-psd-rs>) readers. Payload sizes match
//! Photoshop-saved documents such as ag-psd's public
//! `test/read/adjustment-layers/src.psd`.
//!
//! Upstream (`LayeredFile/LayerTypes/AdjustmentLayer.h`) only detects these
//! keys and round-trips the layer as opaque data. These views add typed read
//! access, but the original tagged block is still what gets written, so reading
//! a view changes no bytes. Bytes after a known layout, such as alignment
//! padding or newer data, remain in `trailing_bytes`.

use crate::descriptor::{Descriptor, DescriptorKey, DescriptorValue};
use crate::error::Result;
use crate::io::BeReader;
use crate::strings::UnicodeString;
use crate::tagged_blocks::{TaggedBlock, TaggedBlockKey};
use crate::types::RawColor;
use crate::views::{
    enumerator, four_cc, invalid, next_is, number, raw_data, read_versioned_descriptor, trailing,
    unsupported,
};

/// The adjustment or fill a tagged block describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdjustmentKind {
    /// `SoCo`: solid-color fill layer.
    SolidColor,
    /// `GdFl`: gradient fill layer.
    GradientFill,
    /// `PtFl`: pattern fill layer.
    PatternFill,
    /// `brit`
    BrightnessContrast,
    /// `levl`
    Levels,
    /// `curv`
    Curves,
    /// `expA`
    Exposure,
    /// `vibA`
    Vibrance,
    /// `hue2`: the Photoshop 5.0+ hue/saturation block.
    HueSaturation,
    /// `hue `: the Photoshop 4.0 hue/saturation block (same layout as `hue2`).
    LegacyHueSaturation,
    /// `blnc`
    ColorBalance,
    /// `blwh`
    BlackAndWhite,
    /// `phfl`
    PhotoFilter,
    /// `mixr`
    ChannelMixer,
    /// `clrL`
    ColorLookup,
    /// `nvrt`
    Invert,
    /// `post`
    Posterize,
    /// `thrs`
    Threshold,
    /// `grdm`
    GradientMap,
    /// `selc`
    SelectiveColor,
    /// `CgEd`: content-generator extra data that accompanies another
    /// adjustment block (preset names, modern brightness/contrast values).
    ContentGenerator,
}

const KIND_KEYS: [(AdjustmentKind, [u8; 4]); 21] = [
    (AdjustmentKind::SolidColor, *b"SoCo"),
    (AdjustmentKind::GradientFill, *b"GdFl"),
    (AdjustmentKind::PatternFill, *b"PtFl"),
    (AdjustmentKind::BrightnessContrast, *b"brit"),
    (AdjustmentKind::Levels, *b"levl"),
    (AdjustmentKind::Curves, *b"curv"),
    (AdjustmentKind::Exposure, *b"expA"),
    (AdjustmentKind::Vibrance, *b"vibA"),
    (AdjustmentKind::HueSaturation, *b"hue2"),
    (AdjustmentKind::LegacyHueSaturation, *b"hue "),
    (AdjustmentKind::ColorBalance, *b"blnc"),
    (AdjustmentKind::BlackAndWhite, *b"blwh"),
    (AdjustmentKind::PhotoFilter, *b"phfl"),
    (AdjustmentKind::ChannelMixer, *b"mixr"),
    (AdjustmentKind::ColorLookup, *b"clrL"),
    (AdjustmentKind::Invert, *b"nvrt"),
    (AdjustmentKind::Posterize, *b"post"),
    (AdjustmentKind::Threshold, *b"thrs"),
    (AdjustmentKind::GradientMap, *b"grdm"),
    (AdjustmentKind::SelectiveColor, *b"selc"),
    (AdjustmentKind::ContentGenerator, *b"CgEd"),
];

impl AdjustmentKind {
    /// The kind stored under `key`, or `None` for other tagged blocks.
    pub fn from_key(key: TaggedBlockKey) -> Option<Self> {
        KIND_KEYS
            .iter()
            .find(|(_, bytes)| *bytes == key.as_bytes())
            .map(|(kind, _)| *kind)
    }

    /// The tagged-block key that stores this kind.
    pub fn key(self) -> TaggedBlockKey {
        let (_, bytes) = KIND_KEYS
            .iter()
            .find(|(kind, _)| *kind == self)
            .expect("every kind has a key");
        TaggedBlockKey::new(*bytes)
    }

    /// Whether this is a fill-layer block (`SoCo`, `GdFl`, `PtFl`).
    pub fn is_fill(self) -> bool {
        matches!(
            self,
            Self::SolidColor | Self::GradientFill | Self::PatternFill
        )
    }

    /// Whether this block on its own marks a layer as an adjustment or fill
    /// layer. `CgEd` only accompanies another adjustment block.
    pub fn marks_layer(self) -> bool {
        self != Self::ContentGenerator
    }
}

/// A parsed view of one adjustment- or fill-layer tagged block.
#[derive(Debug, Clone, PartialEq)]
pub struct AdjustmentBlock {
    pub key: TaggedBlockKey,
    pub kind: AdjustmentKind,
    pub data: AdjustmentData,
}

impl AdjustmentBlock {
    /// Parse a recognized adjustment block. Other tagged blocks return `None`.
    /// A malformed or unsupported payload is an error only when this view is
    /// requested; the raw block is never modified.
    pub fn read(block: &TaggedBlock) -> Result<Option<Self>> {
        let Some(kind) = AdjustmentKind::from_key(block.key) else {
            return Ok(None);
        };
        let data = AdjustmentData::read(kind, &block.data)?;
        Ok(Some(Self {
            key: block.key,
            kind,
            data,
        }))
    }
}

/// Typed settings, one variant per payload layout.
#[derive(Debug, Clone, PartialEq)]
pub enum AdjustmentData {
    /// `SoCo`, `GdFl`, and `PtFl`.
    Fill(FillSettings),
    BrightnessContrast(BrightnessContrast),
    Levels(Levels),
    Curves(Curves),
    Exposure(Exposure),
    Vibrance(Vibrance),
    /// `hue2` and `hue `.
    HueSaturation(HueSaturation),
    ColorBalance(ColorBalance),
    BlackAndWhite(BlackAndWhite),
    PhotoFilter(PhotoFilter),
    ChannelMixer(ChannelMixer),
    ColorLookup(ColorLookup),
    /// `nvrt` has no settings; Photoshop writes an empty payload.
    Invert {
        trailing_bytes: Vec<u8>,
    },
    Posterize(Posterize),
    Threshold(Threshold),
    GradientMap(GradientMap),
    SelectiveColor(SelectiveColor),
    ContentGenerator(ContentGenerator),
}

impl AdjustmentData {
    fn read(kind: AdjustmentKind, data: &[u8]) -> Result<Self> {
        let mut reader = BeReader::new(data);
        let reader = &mut reader;
        Ok(match kind {
            AdjustmentKind::SolidColor
            | AdjustmentKind::GradientFill
            | AdjustmentKind::PatternFill => {
                let descriptor = read_versioned_descriptor(reader)?;
                Self::Fill(FillSettings {
                    descriptor,
                    trailing_bytes: trailing(reader)?,
                })
            }
            AdjustmentKind::BrightnessContrast => {
                Self::BrightnessContrast(BrightnessContrast::read(reader)?)
            }
            AdjustmentKind::Levels => Self::Levels(Levels::read(reader)?),
            AdjustmentKind::Curves => Self::Curves(Curves::read(reader)?),
            AdjustmentKind::Exposure => Self::Exposure(Exposure::read(reader)?),
            AdjustmentKind::Vibrance => {
                let descriptor = read_versioned_descriptor(reader)?;
                Self::Vibrance(Vibrance {
                    descriptor,
                    trailing_bytes: trailing(reader)?,
                })
            }
            AdjustmentKind::HueSaturation | AdjustmentKind::LegacyHueSaturation => {
                Self::HueSaturation(HueSaturation::read(reader)?)
            }
            AdjustmentKind::ColorBalance => Self::ColorBalance(ColorBalance::read(reader)?),
            AdjustmentKind::BlackAndWhite => {
                let descriptor = read_versioned_descriptor(reader)?;
                Self::BlackAndWhite(BlackAndWhite {
                    descriptor,
                    trailing_bytes: trailing(reader)?,
                })
            }
            AdjustmentKind::PhotoFilter => Self::PhotoFilter(PhotoFilter::read(reader)?),
            AdjustmentKind::ChannelMixer => Self::ChannelMixer(ChannelMixer::read(reader)?),
            AdjustmentKind::ColorLookup => {
                let version = reader.u16()?;
                if version != 1 {
                    return Err(unsupported(reader));
                }
                let descriptor = read_versioned_descriptor(reader)?;
                Self::ColorLookup(ColorLookup {
                    version,
                    descriptor,
                    trailing_bytes: trailing(reader)?,
                })
            }
            AdjustmentKind::Invert => Self::Invert {
                trailing_bytes: trailing(reader)?,
            },
            AdjustmentKind::Posterize => Self::Posterize(Posterize {
                levels: reader.u16()?,
                trailing_bytes: trailing(reader)?,
            }),
            AdjustmentKind::Threshold => Self::Threshold(Threshold {
                level: reader.u16()?,
                trailing_bytes: trailing(reader)?,
            }),
            AdjustmentKind::GradientMap => Self::GradientMap(GradientMap::read(reader)?),
            AdjustmentKind::SelectiveColor => Self::SelectiveColor(SelectiveColor::read(reader)?),
            AdjustmentKind::ContentGenerator => {
                let descriptor = read_versioned_descriptor(reader)?;
                Self::ContentGenerator(ContentGenerator {
                    descriptor,
                    trailing_bytes: trailing(reader)?,
                })
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Binary layouts
// ---------------------------------------------------------------------------

/// `brit`: legacy brightness/contrast. Photoshop CS3 and later keep the
/// current (non-legacy) values in the layer's `CgEd` block instead.
#[derive(Debug, Clone, PartialEq)]
pub struct BrightnessContrast {
    pub brightness: i16,
    pub contrast: i16,
    pub mean_value: i16,
    pub lab_only: bool,
    pub trailing_bytes: Vec<u8>,
}

impl BrightnessContrast {
    fn read(reader: &mut BeReader) -> Result<Self> {
        Ok(Self {
            brightness: reader.i16()?,
            contrast: reader.i16()?,
            mean_value: reader.i16()?,
            lab_only: reader.u8()? != 0,
            trailing_bytes: trailing(reader)?,
        })
    }
}

/// One levels record (the specification's "Level record structure").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LevelsRecord {
    pub input_floor: u16,
    pub input_ceiling: u16,
    pub output_floor: u16,
    pub output_ceiling: u16,
    /// Gamma × 100 (10–999 for 0.10–9.99).
    pub gamma: u16,
}

impl LevelsRecord {
    fn read(reader: &mut BeReader) -> Result<Self> {
        Ok(Self {
            input_floor: reader.u16()?,
            input_ceiling: reader.u16()?,
            output_floor: reader.u16()?,
            output_ceiling: reader.u16()?,
            gamma: reader.u16()?,
        })
    }

    /// The gamma as a float (the stored value / 100).
    pub fn gamma_value(&self) -> f64 {
        f64::from(self.gamma) / 100.0
    }
}

/// Number of level records every version-2 `levl` payload starts with.
const LEGACY_LEVELS_RECORDS: usize = 29;

/// `levl`: 29 legacy records, optionally followed by a `Lvls` extension that
/// raises the total record count (Photoshop CS and later write 62).
#[derive(Debug, Clone, PartialEq)]
pub struct Levels {
    pub version: u16,
    /// All records in stored order: record 0 is the composite (master)
    /// record and record `n` applies to channel `n`. Indexed-color documents
    /// use a different order; see the specification.
    pub records: Vec<LevelsRecord>,
    /// The `Lvls` extension version (3), when the extension is present.
    pub extension_version: Option<u16>,
    pub trailing_bytes: Vec<u8>,
}

impl Levels {
    fn read(reader: &mut BeReader) -> Result<Self> {
        let version = reader.u16()?;
        if version != 2 {
            return Err(unsupported(reader));
        }
        let mut records = Vec::with_capacity(LEGACY_LEVELS_RECORDS);
        for _ in 0..LEGACY_LEVELS_RECORDS {
            records.push(LevelsRecord::read(reader)?);
        }
        // Anything other than a `Lvls` marker stays in `trailing_bytes`.
        // psd-tools notes that Clip Studio Paint writes a damaged marker here.
        let mut extension_version = None;
        if next_is(reader, b"Lvls") && reader.remaining() >= 8 {
            reader.skip(4)?;
            let extension = reader.u16()?;
            if extension != 3 {
                return Err(unsupported(reader));
            }
            let total = usize::from(reader.u16()?);
            if total < LEGACY_LEVELS_RECORDS {
                return Err(invalid(reader, "levels record count below 29"));
            }
            let extra = total - LEGACY_LEVELS_RECORDS;
            records.reserve(extra.min(reader.remaining() / 10));
            for _ in 0..extra {
                records.push(LevelsRecord::read(reader)?);
            }
            extension_version = Some(extension);
        }
        Ok(Self {
            version,
            records,
            extension_version,
            trailing_bytes: trailing(reader)?,
        })
    }

    /// The composite (master) record.
    pub fn composite(&self) -> &LevelsRecord {
        &self.records[0]
    }

    /// The record for channel `index` (0 = first color channel).
    pub fn channel(&self, index: usize) -> Option<&LevelsRecord> {
        self.records.get(index.checked_add(1)?)
    }
}

/// One curve point. Both values are 0–255.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurvePoint {
    pub output: u16,
    pub input: u16,
}

/// A curve's stored shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurveData {
    /// Control points in stored order.
    Points(Vec<CurvePoint>),
    /// A freehand (pencil) curve: 256 output values, one per input level.
    Map(Vec<u8>),
}

/// One curve and the channel it applies to (0 = composite).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Curve {
    pub channel: u16,
    pub data: CurveData,
}

/// The `Crv ` extension after the legacy curves, with explicit channel ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurvesExtension {
    pub version: u16,
    pub curves: Vec<Curve>,
}

/// `curv`.
#[derive(Debug, Clone, PartialEq)]
pub struct Curves {
    /// Whether the curves are freehand maps rather than control points. The
    /// flag is the payload's first byte.
    pub is_map: bool,
    /// 1: the selector is a channel bitmap. 4: it is a curve count.
    pub version: u16,
    /// Curves from the legacy section. For version 1 each curve's channel is
    /// its bitmap bit; for version 4 it is the curve's position.
    pub curves: Vec<Curve>,
    pub extension: Option<CurvesExtension>,
    pub trailing_bytes: Vec<u8>,
}

impl Curves {
    fn read(reader: &mut BeReader) -> Result<Self> {
        let is_map = reader.u8()? != 0;
        let version = reader.u16()?;
        if !matches!(version, 1 | 4) {
            return Err(unsupported(reader));
        }
        let selector = reader.u32()?;
        let mut curves = Vec::new();
        if version == 1 {
            for channel in 0..32u16 {
                if selector & (1 << channel) != 0 {
                    let data = read_curve_data(reader, is_map)?;
                    curves.push(Curve { channel, data });
                }
            }
        } else {
            curves.reserve((selector as usize).min(reader.remaining() / 2));
            for index in 0..selector {
                let channel = u16::try_from(index)
                    .map_err(|_| invalid(reader, "curves count exceeds the channel range"))?;
                let data = read_curve_data(reader, is_map)?;
                curves.push(Curve { channel, data });
            }
        }
        let mut extension = None;
        if next_is(reader, b"Crv ") {
            reader.skip(4)?;
            let extension_version = reader.u16()?;
            let count = reader.u32()?;
            let mut extended = Vec::with_capacity((count as usize).min(reader.remaining() / 4));
            for _ in 0..count {
                let channel = reader.u16()?;
                let data = read_curve_data(reader, is_map)?;
                extended.push(Curve { channel, data });
            }
            extension = Some(CurvesExtension {
                version: extension_version,
                curves: extended,
            });
        }
        Ok(Self {
            is_map,
            version,
            curves,
            extension,
            trailing_bytes: trailing(reader)?,
        })
    }

    /// The `Crv ` extension's curves when present, else the legacy curves.
    pub fn effective_curves(&self) -> &[Curve] {
        match &self.extension {
            Some(extension) => &extension.curves,
            None => &self.curves,
        }
    }
}

fn read_curve_data(reader: &mut BeReader, is_map: bool) -> Result<CurveData> {
    if is_map {
        return Ok(CurveData::Map(reader.take(256)?.to_vec()));
    }
    let count = usize::from(reader.u16()?);
    let mut points = Vec::with_capacity(count.min(reader.remaining() / 4));
    for _ in 0..count {
        points.push(CurvePoint {
            output: reader.u16()?,
            input: reader.u16()?,
        });
    }
    Ok(CurveData::Points(points))
}

/// `expA`.
#[derive(Debug, Clone, PartialEq)]
pub struct Exposure {
    pub version: u16,
    pub exposure: f32,
    pub offset: f32,
    pub gamma: f32,
    pub trailing_bytes: Vec<u8>,
}

impl Exposure {
    fn read(reader: &mut BeReader) -> Result<Self> {
        let version = reader.u16()?;
        if version != 1 {
            return Err(unsupported(reader));
        }
        Ok(Self {
            version,
            exposure: reader.f32()?,
            offset: reader.f32()?,
            gamma: reader.f32()?,
            trailing_bytes: trailing(reader)?,
        })
    }
}

/// Hue, saturation, and lightness as stored (Photoshop 5.0 and later: hue
/// −180…180, saturation 0…100 for colorization or −100…100 otherwise,
/// lightness −100…100).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HueSaturationValues {
    pub hue: i16,
    pub saturation: i16,
    pub lightness: i16,
}

impl HueSaturationValues {
    fn read(reader: &mut BeReader) -> Result<Self> {
        Ok(Self {
            hue: reader.i16()?,
            saturation: reader.i16()?,
            lightness: reader.i16()?,
        })
    }
}

/// One of the six hue ranges: four range bounds, then its settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HueRange {
    pub range: [i16; 4],
    pub values: HueSaturationValues,
}

/// `hue2` and `hue `.
#[derive(Debug, Clone, PartialEq)]
pub struct HueSaturation {
    pub version: u16,
    pub colorize: bool,
    pub colorization: HueSaturationValues,
    pub master: HueSaturationValues,
    /// Reds, yellows, greens, cyans, blues, magentas (for Lab documents the
    /// first four apply to the Lab quadrants).
    pub ranges: [HueRange; 6],
    pub trailing_bytes: Vec<u8>,
}

impl HueSaturation {
    fn read(reader: &mut BeReader) -> Result<Self> {
        let version = reader.u16()?;
        if version != 2 {
            return Err(unsupported(reader));
        }
        let colorize = reader.u8()? != 0;
        reader.skip(1)?; // padding
        let colorization = HueSaturationValues::read(reader)?;
        let master = HueSaturationValues::read(reader)?;
        let mut ranges = [HueRange {
            range: [0; 4],
            values: HueSaturationValues {
                hue: 0,
                saturation: 0,
                lightness: 0,
            },
        }; 6];
        for range in &mut ranges {
            range.range = [reader.i16()?, reader.i16()?, reader.i16()?, reader.i16()?];
            range.values = HueSaturationValues::read(reader)?;
        }
        Ok(Self {
            version,
            colorize,
            colorization,
            master,
            ranges,
            trailing_bytes: trailing(reader)?,
        })
    }
}

/// Cyan–red, magenta–green, and yellow–blue shifts (−100…100).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorBalanceValues {
    pub cyan_red: i16,
    pub magenta_green: i16,
    pub yellow_blue: i16,
}

impl ColorBalanceValues {
    fn read(reader: &mut BeReader) -> Result<Self> {
        Ok(Self {
            cyan_red: reader.i16()?,
            magenta_green: reader.i16()?,
            yellow_blue: reader.i16()?,
        })
    }
}

/// `blnc`.
#[derive(Debug, Clone, PartialEq)]
pub struct ColorBalance {
    pub shadows: ColorBalanceValues,
    pub midtones: ColorBalanceValues,
    pub highlights: ColorBalanceValues,
    pub preserve_luminosity: bool,
    pub trailing_bytes: Vec<u8>,
}

impl ColorBalance {
    fn read(reader: &mut BeReader) -> Result<Self> {
        Ok(Self {
            shadows: ColorBalanceValues::read(reader)?,
            midtones: ColorBalanceValues::read(reader)?,
            highlights: ColorBalanceValues::read(reader)?,
            preserve_luminosity: reader.u8()? != 0,
            trailing_bytes: trailing(reader)?,
        })
    }
}

/// A photo filter's color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhotoFilterColor {
    /// Version 3: three 4-byte components (the specification says XYZ).
    Xyz([i32; 3]),
    /// Version 2: a 10-byte color structure.
    Color(RawColor),
}

/// `phfl`.
#[derive(Debug, Clone, PartialEq)]
pub struct PhotoFilter {
    pub version: u16,
    pub color: PhotoFilterColor,
    /// Density in percent.
    pub density: u32,
    pub preserve_luminosity: bool,
    pub trailing_bytes: Vec<u8>,
}

impl PhotoFilter {
    fn read(reader: &mut BeReader) -> Result<Self> {
        let version = reader.u16()?;
        let color = match version {
            2 => PhotoFilterColor::Color(RawColor::read(reader)?),
            3 => PhotoFilterColor::Xyz([reader.i32()?, reader.i32()?, reader.i32()?]),
            _ => return Err(unsupported(reader)),
        };
        Ok(Self {
            version,
            color,
            density: reader.u32()?,
            preserve_luminosity: reader.u8()? != 0,
            trailing_bytes: trailing(reader)?,
        })
    }
}

/// One output channel's mix: four source-channel percentages, then the
/// constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelMix {
    pub sources: [i16; 4],
    pub constant: i16,
}

/// `mixr`.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelMixer {
    pub version: u16,
    pub monochrome: bool,
    /// Every complete 10-byte mix in stored order. RGB documents store four
    /// (red, green, blue, gray). In monochrome mode, ag-psd-rs reads the gray
    /// mix from the first record.
    pub mixes: Vec<ChannelMix>,
    pub trailing_bytes: Vec<u8>,
}

impl ChannelMixer {
    fn read(reader: &mut BeReader) -> Result<Self> {
        let version = reader.u16()?;
        if version != 1 {
            return Err(unsupported(reader));
        }
        let monochrome = reader.u16()? != 0;
        let mut mixes = Vec::with_capacity(reader.remaining() / 10);
        while reader.remaining() >= 10 {
            mixes.push(ChannelMix {
                sources: [reader.i16()?, reader.i16()?, reader.i16()?, reader.i16()?],
                constant: reader.i16()?,
            });
        }
        Ok(Self {
            version,
            monochrome,
            mixes,
            trailing_bytes: trailing(reader)?,
        })
    }
}

/// `post`.
#[derive(Debug, Clone, PartialEq)]
pub struct Posterize {
    pub levels: u16,
    pub trailing_bytes: Vec<u8>,
}

/// `thrs`.
#[derive(Debug, Clone, PartialEq)]
pub struct Threshold {
    pub level: u16,
    pub trailing_bytes: Vec<u8>,
}

/// A gradient color stop. Location uses Photoshop's 0–4096 scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GradientColorStop {
    pub location: u32,
    /// Midpoint in percent.
    pub midpoint: u32,
    pub color: RawColor,
}

/// A gradient transparency stop. Location uses Photoshop's 0–4096 scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GradientTransparencyStop {
    pub location: u32,
    /// Midpoint in percent.
    pub midpoint: u32,
    /// Opacity, 0–255.
    pub opacity: u16,
}

/// `grdm`.
#[derive(Debug, Clone, PartialEq)]
pub struct GradientMap {
    /// 1, or 3 when an interpolation method follows the flags.
    pub version: u16,
    pub reversed: bool,
    pub dithered: bool,
    /// Version 3's interpolation method (for example `Gcls`, `Perc`, `Lnr `).
    pub method: Option<[u8; 4]>,
    pub name: String,
    pub color_stops: Vec<GradientColorStop>,
    pub transparency_stops: Vec<GradientTransparencyStop>,
    /// Smoothness × 4096.
    pub interpolation: u16,
    /// 0 = solid gradient, 1 = noise gradient.
    pub mode: u16,
    pub random_seed: u32,
    pub show_transparency: bool,
    pub use_vector_color: bool,
    /// Roughness × 4096.
    pub roughness: u32,
    pub color_model: u16,
    pub minimum_color: [u16; 4],
    pub maximum_color: [u16; 4],
    pub trailing_bytes: Vec<u8>,
}

impl GradientMap {
    fn read(reader: &mut BeReader) -> Result<Self> {
        let version = reader.u16()?;
        if !matches!(version, 1 | 3) {
            return Err(unsupported(reader));
        }
        let reversed = reader.u8()? != 0;
        let dithered = reader.u8()? != 0;
        let method = if version == 3 {
            Some(four_cc(reader)?)
        } else {
            None
        };
        let name = UnicodeString::read(reader, 1)?.value().to_owned();

        let count = usize::from(reader.u16()?);
        let mut color_stops = Vec::with_capacity(count.min(reader.remaining() / 20));
        for _ in 0..count {
            let location = reader.u32()?;
            let midpoint = reader.u32()?;
            let color = RawColor::read(reader)?;
            reader.skip(2)?; // unused
            color_stops.push(GradientColorStop {
                location,
                midpoint,
                color,
            });
        }
        let count = usize::from(reader.u16()?);
        let mut transparency_stops = Vec::with_capacity(count.min(reader.remaining() / 10));
        for _ in 0..count {
            transparency_stops.push(GradientTransparencyStop {
                location: reader.u32()?,
                midpoint: reader.u32()?,
                opacity: reader.u16()?,
            });
        }

        if reader.u16()? != 2 {
            return Err(invalid(reader, "gradient map expansion count is not 2"));
        }
        let interpolation = reader.u16()?;
        if reader.u16()? != 32 {
            return Err(invalid(reader, "gradient map noise length is not 32"));
        }
        Ok(Self {
            version,
            reversed,
            dithered,
            method,
            name,
            color_stops,
            transparency_stops,
            interpolation,
            mode: reader.u16()?,
            random_seed: reader.u32()?,
            show_transparency: reader.u16()? != 0,
            use_vector_color: reader.u16()? != 0,
            roughness: reader.u32()?,
            color_model: reader.u16()?,
            minimum_color: [reader.u16()?, reader.u16()?, reader.u16()?, reader.u16()?],
            maximum_color: [reader.u16()?, reader.u16()?, reader.u16()?, reader.u16()?],
            trailing_bytes: trailing(reader)?,
        })
    }

    /// Smoothness in the range 0–1.
    pub fn smoothness(&self) -> f64 {
        f64::from(self.interpolation) / 4096.0
    }
}

/// Cyan, magenta, yellow, and black corrections (−100…100).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CmykCorrection {
    pub cyan: i16,
    pub magenta: i16,
    pub yellow: i16,
    pub black: i16,
}

impl CmykCorrection {
    fn read(reader: &mut BeReader) -> Result<Self> {
        Ok(Self {
            cyan: reader.i16()?,
            magenta: reader.i16()?,
            yellow: reader.i16()?,
            black: reader.i16()?,
        })
    }
}

/// `selc`.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectiveColor {
    pub version: u16,
    /// `true` for absolute correction, `false` for relative.
    pub absolute: bool,
    /// The first plate record, reserved by Photoshop (normally zero).
    pub reserved: CmykCorrection,
    pub reds: CmykCorrection,
    pub yellows: CmykCorrection,
    pub greens: CmykCorrection,
    pub cyans: CmykCorrection,
    pub blues: CmykCorrection,
    pub magentas: CmykCorrection,
    pub whites: CmykCorrection,
    pub neutrals: CmykCorrection,
    pub blacks: CmykCorrection,
    pub trailing_bytes: Vec<u8>,
}

impl SelectiveColor {
    fn read(reader: &mut BeReader) -> Result<Self> {
        let version = reader.u16()?;
        if version != 1 {
            return Err(unsupported(reader));
        }
        Ok(Self {
            version,
            absolute: reader.u16()? != 0,
            reserved: CmykCorrection::read(reader)?,
            reds: CmykCorrection::read(reader)?,
            yellows: CmykCorrection::read(reader)?,
            greens: CmykCorrection::read(reader)?,
            cyans: CmykCorrection::read(reader)?,
            blues: CmykCorrection::read(reader)?,
            magentas: CmykCorrection::read(reader)?,
            whites: CmykCorrection::read(reader)?,
            neutrals: CmykCorrection::read(reader)?,
            blacks: CmykCorrection::read(reader)?,
            trailing_bytes: trailing(reader)?,
        })
    }
}

// ---------------------------------------------------------------------------
// Descriptor layouts
// ---------------------------------------------------------------------------

/// A preset reference: `kind` is Photoshop's preset-kind number (commonly 1
/// for the default and 5 for a custom file).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdjustmentPreset<'a> {
    pub kind: Option<f64>,
    pub file_name: Option<&'a str>,
}

/// `SoCo`, `GdFl`, `PtFl`: fill settings descriptors.
#[derive(Debug, Clone, PartialEq)]
pub struct FillSettings {
    pub descriptor: Descriptor,
    pub trailing_bytes: Vec<u8>,
}

impl FillSettings {
    /// A solid fill's color descriptor (`Clr `).
    pub fn color(&self) -> Option<&Descriptor> {
        self.descriptor.get("Clr ")?.as_descriptor()
    }

    /// A gradient fill's gradient descriptor (`Grad`).
    pub fn gradient(&self) -> Option<&Descriptor> {
        self.descriptor.get("Grad")?.as_descriptor()
    }

    /// A gradient fill's type enumerator (`Type`, for example `Lnr `).
    pub fn gradient_type(&self) -> Option<&DescriptorKey> {
        enumerator(&self.descriptor, "Type")
    }

    /// A gradient fill's angle in degrees (`Angl`).
    pub fn angle(&self) -> Option<f64> {
        number(&self.descriptor, "Angl")
    }

    /// A pattern fill's pattern descriptor (`Ptrn`).
    pub fn pattern(&self) -> Option<&Descriptor> {
        self.descriptor.get("Ptrn")?.as_descriptor()
    }

    /// A gradient or pattern fill's scale in percent (`Scl `).
    pub fn scale(&self) -> Option<f64> {
        number(&self.descriptor, "Scl ")
    }
}

/// `vibA`.
#[derive(Debug, Clone, PartialEq)]
pub struct Vibrance {
    pub descriptor: Descriptor,
    pub trailing_bytes: Vec<u8>,
}

impl Vibrance {
    /// Vibrance, −100…100 (`vibrance`).
    pub fn vibrance(&self) -> Option<f64> {
        number(&self.descriptor, "vibrance")
    }

    /// Saturation, −100…100 (`Strt`).
    pub fn saturation(&self) -> Option<f64> {
        number(&self.descriptor, "Strt")
    }
}

/// `blwh`.
#[derive(Debug, Clone, PartialEq)]
pub struct BlackAndWhite {
    pub descriptor: Descriptor,
    pub trailing_bytes: Vec<u8>,
}

impl BlackAndWhite {
    /// The six color weights in percent: reds, yellows, greens, cyans, blues,
    /// magentas. Missing weights are `None`.
    pub fn weights(&self) -> [Option<f64>; 6] {
        ["Rd  ", "Yllw", "Grn ", "Cyn ", "Bl  ", "Mgnt"].map(|key| number(&self.descriptor, key))
    }

    pub fn use_tint(&self) -> Option<bool> {
        self.descriptor.get("useTint")?.as_bool()
    }

    /// The tint color descriptor (`tintColor`).
    pub fn tint_color(&self) -> Option<&Descriptor> {
        self.descriptor.get("tintColor")?.as_descriptor()
    }

    pub fn preset(&self) -> Option<AdjustmentPreset<'_>> {
        preset(
            &self.descriptor,
            "bwPresetKind",
            "blackAndWhitePresetFileName",
        )
    }
}

/// `clrL`: a `u16` version, then a versioned descriptor.
#[derive(Debug, Clone, PartialEq)]
pub struct ColorLookup {
    pub version: u16,
    pub descriptor: Descriptor,
    pub trailing_bytes: Vec<u8>,
}

impl ColorLookup {
    /// The lookup type enumerator (`lookupType`, for example `3DLUT`).
    pub fn lookup_type(&self) -> Option<&DescriptorKey> {
        enumerator(&self.descriptor, "lookupType")
    }

    /// The display name (`Nm  `).
    pub fn name(&self) -> Option<&str> {
        self.descriptor.get("Nm  ")?.as_str()
    }

    pub fn dither(&self) -> Option<bool> {
        self.descriptor.get("Dthr")?.as_bool()
    }

    /// Embedded ICC profile bytes for profile lookups (`profile`).
    pub fn profile(&self) -> Option<&[u8]> {
        raw_data(&self.descriptor, "profile")
    }

    /// The LUT file format enumerator (`LUTFormat`).
    pub fn lut_format(&self) -> Option<&DescriptorKey> {
        enumerator(&self.descriptor, "LUTFormat")
    }

    /// The embedded LUT file (`LUT3DFileData`).
    pub fn lut_file_data(&self) -> Option<&[u8]> {
        raw_data(&self.descriptor, "LUT3DFileData")
    }

    /// The LUT file name (`LUT3DFileName`).
    pub fn lut_file_name(&self) -> Option<&str> {
        self.descriptor.get("LUT3DFileName")?.as_str()
    }
}

/// `CgEd`: content-generator extra data.
#[derive(Debug, Clone, PartialEq)]
pub struct ContentGenerator {
    pub descriptor: Descriptor,
    pub trailing_bytes: Vec<u8>,
}

impl ContentGenerator {
    /// The descriptor's own version field (`Vrsn`).
    pub fn version(&self) -> Option<f64> {
        number(&self.descriptor, "Vrsn")
    }

    /// Brightness (`Brgh`), present for brightness/contrast layers.
    pub fn brightness(&self) -> Option<f64> {
        number(&self.descriptor, "Brgh")
    }

    /// Contrast (`Cntr`).
    pub fn contrast(&self) -> Option<f64> {
        number(&self.descriptor, "Cntr")
    }

    /// Mean value (`means`).
    pub fn mean_value(&self) -> Option<f64> {
        number(&self.descriptor, "means")
    }

    /// Whether the adjustment applies to the Lab lightness only (`Lab `).
    pub fn lab_only(&self) -> Option<bool> {
        self.descriptor.get("Lab ")?.as_bool()
    }

    /// Whether brightness/contrast uses the legacy algorithm (`useLegacy`).
    pub fn use_legacy(&self) -> Option<bool> {
        self.descriptor.get("useLegacy")?.as_bool()
    }

    /// Whether brightness/contrast was set automatically (`Auto`).
    pub fn auto(&self) -> Option<bool> {
        self.descriptor.get("Auto")?.as_bool()
    }

    /// The preset reference for levels, exposure, and hue/saturation
    /// (`presetKind`/`presetFileName`), curves (`curvesPreset…`), or the
    /// channel mixer (`mixerPreset…`), whichever is present.
    pub fn preset(&self) -> Option<AdjustmentPreset<'_>> {
        [
            ("presetKind", "presetFileName"),
            ("curvesPresetKind", "curvesPresetFileName"),
            ("mixerPresetKind", "mixerPresetFileName"),
        ]
        .into_iter()
        .find_map(|(kind, name)| preset(&self.descriptor, kind, name))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn preset<'a>(
    descriptor: &'a Descriptor,
    kind_key: &str,
    name_key: &str,
) -> Option<AdjustmentPreset<'a>> {
    let kind = number(descriptor, kind_key);
    let file_name = descriptor.get(name_key).and_then(DescriptorValue::as_str);
    (kind.is_some() || file_name.is_some()).then_some(AdjustmentPreset { kind, file_name })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::DescriptorItem;
    use crate::io::BeWriter;

    fn block(key: &[u8; 4], data: Vec<u8>) -> TaggedBlock {
        TaggedBlock::new(TaggedBlockKey::new(*key), data)
    }

    fn parse(key: &[u8; 4], data: Vec<u8>) -> AdjustmentBlock {
        AdjustmentBlock::read(&block(key, data)).unwrap().unwrap()
    }

    fn descriptor(items: Vec<(&str, DescriptorValue)>) -> Descriptor {
        Descriptor {
            name: UnicodeString::new("", 1).unwrap(),
            class_id: DescriptorKey::char_id(*b"null"),
            items: items
                .into_iter()
                .map(|(key, value)| DescriptorItem {
                    key: DescriptorKey::new(key),
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

    fn levels_record(writer: &mut BeWriter, record: [u16; 5]) {
        for value in record {
            writer.u16(value);
        }
    }

    #[test]
    fn every_key_maps_to_one_kind_and_back() {
        for (kind, key) in KIND_KEYS {
            let key = TaggedBlockKey::new(key);
            assert_eq!(AdjustmentKind::from_key(key), Some(kind));
            assert_eq!(kind.key(), key);
        }
        assert_eq!(
            AdjustmentKind::from_key(TaggedBlockKey::new(*b"lfx2")),
            None
        );
        assert!(AdjustmentBlock::read(&block(b"luni", vec![1, 2, 3]))
            .unwrap()
            .is_none());
        assert!(AdjustmentKind::SolidColor.is_fill());
        assert!(!AdjustmentKind::Levels.is_fill());
        assert!(!AdjustmentKind::ContentGenerator.marks_layer());
        assert!(AdjustmentKind::Invert.marks_layer());
    }

    #[test]
    fn levels_read_legacy_records_and_the_lvls_extension() {
        // Photoshop layout: 29 records, `Lvls` v3 raising the total to 62,
        // then two bytes of padding (632 bytes in all).
        let mut writer = BeWriter::new();
        writer.u16(2);
        levels_record(&mut writer, [10, 245, 0, 255, 100]);
        levels_record(&mut writer, [0, 255, 5, 250, 150]);
        for _ in 2..29 {
            levels_record(&mut writer, [0, 255, 0, 255, 100]);
        }
        writer.bytes(b"Lvls");
        writer.u16(3);
        writer.u16(62);
        for index in 29..62 {
            levels_record(&mut writer, [index, 255, 0, 255, 100]);
        }
        writer.u16(0);
        assert_eq!(writer.position(), 632);
        let parsed = parse(b"levl", writer.into_inner());
        assert_eq!(parsed.kind, AdjustmentKind::Levels);
        let AdjustmentData::Levels(levels) = parsed.data else {
            panic!("levels expected")
        };
        assert_eq!(levels.records.len(), 62);
        assert_eq!(levels.extension_version, Some(3));
        assert_eq!(levels.composite().input_floor, 10);
        assert_eq!(levels.channel(0).unwrap().output_floor, 5);
        assert_eq!(levels.channel(0).unwrap().gamma_value(), 1.5);
        assert_eq!(levels.records[61].input_floor, 61);
        assert_eq!(levels.trailing_bytes, [0, 0]);
    }

    #[test]
    fn levels_without_a_valid_extension_marker_keep_the_bytes() {
        let mut writer = BeWriter::new();
        writer.u16(2);
        for _ in 0..29 {
            levels_record(&mut writer, [0, 255, 0, 255, 100]);
        }
        writer.bytes(b"ls\0\x03\0\x10");
        let AdjustmentData::Levels(levels) = parse(b"levl", writer.into_inner()).data else {
            panic!("levels expected")
        };
        assert_eq!(levels.records.len(), 29);
        assert_eq!(levels.extension_version, None);
        assert_eq!(levels.trailing_bytes, b"ls\0\x03\0\x10");
    }

    #[test]
    fn curves_read_bitmap_curves_and_the_crv_extension() {
        let mut writer = BeWriter::new();
        writer.u8(0);
        writer.u16(1);
        writer.u32(0b1001); // composite and channel 3
        for points in [
            &[(0, 0), (255, 255)][..],
            &[(0, 10), (128, 100), (250, 255)],
        ] {
            writer.u16(points.len() as u16);
            for &(output, input) in points {
                writer.u16(output);
                writer.u16(input);
            }
        }
        writer.bytes(b"Crv ");
        writer.u16(4);
        writer.u32(1);
        writer.u16(3);
        writer.u16(2);
        for value in [0, 5, 255, 250] {
            writer.u16(value);
        }
        writer.u8(0);
        let AdjustmentData::Curves(curves) = parse(b"curv", writer.into_inner()).data else {
            panic!("curves expected")
        };
        assert!(!curves.is_map);
        assert_eq!(curves.curves.len(), 2);
        assert_eq!(curves.curves[0].channel, 0);
        assert_eq!(curves.curves[1].channel, 3);
        assert_eq!(
            curves.curves[1].data,
            CurveData::Points(vec![
                CurvePoint {
                    output: 0,
                    input: 10
                },
                CurvePoint {
                    output: 128,
                    input: 100
                },
                CurvePoint {
                    output: 250,
                    input: 255
                },
            ])
        );
        let extension = curves.extension.as_ref().unwrap();
        assert_eq!(extension.version, 4);
        assert_eq!(curves.effective_curves().len(), 1);
        assert_eq!(curves.effective_curves()[0].channel, 3);
        assert_eq!(curves.trailing_bytes, [0]);
    }

    #[test]
    fn curves_read_freehand_maps_and_count_selectors() {
        let mut writer = BeWriter::new();
        writer.u8(1);
        writer.u16(4);
        writer.u32(2);
        let ramp: Vec<u8> = (0..=255).collect();
        writer.bytes(&ramp);
        writer.bytes(&ramp);
        let AdjustmentData::Curves(curves) = parse(b"curv", writer.into_inner()).data else {
            panic!("curves expected")
        };
        assert!(curves.is_map);
        assert_eq!(curves.curves.len(), 2);
        assert_eq!(curves.curves[1].channel, 1);
        assert_eq!(curves.curves[1].data, CurveData::Map(ramp));
        assert!(curves.extension.is_none());
    }

    #[test]
    fn fixed_binary_layouts_expose_every_field() {
        let brit = parse(b"brit", vec![0, 20, 0xff, 0xf6, 0, 127, 1, 0]);
        assert_eq!(
            brit.data,
            AdjustmentData::BrightnessContrast(BrightnessContrast {
                brightness: 20,
                contrast: -10,
                mean_value: 127,
                lab_only: true,
                trailing_bytes: vec![0],
            })
        );

        let mut writer = BeWriter::new();
        writer.u16(1);
        writer.f32(-1.0);
        writer.f32(0.25);
        writer.f32(1.0);
        writer.u16(0);
        let AdjustmentData::Exposure(exposure) = parse(b"expA", writer.into_inner()).data else {
            panic!("exposure expected")
        };
        assert_eq!(
            (exposure.exposure, exposure.offset, exposure.gamma),
            (-1.0, 0.25, 1.0)
        );
        assert_eq!(exposure.trailing_bytes, [0, 0]);

        let mut writer = BeWriter::new();
        for value in [0i16, 0, 16, -21, 33, 38, 35, 38] {
            writer.i16(value);
        }
        writer.i16(-5);
        writer.u8(1);
        writer.u8(0);
        let AdjustmentData::ColorBalance(balance) = parse(b"blnc", writer.into_inner()).data else {
            panic!("color balance expected")
        };
        assert_eq!(balance.shadows.yellow_blue, 16);
        assert_eq!(balance.midtones.cyan_red, -21);
        assert_eq!(balance.highlights.yellow_blue, -5);
        assert!(balance.preserve_luminosity);

        let post = parse(b"post", vec![0, 16, 0, 0]);
        assert!(matches!(
            post.data,
            AdjustmentData::Posterize(Posterize { levels: 16, .. })
        ));
        let thrs = parse(b"thrs", vec![0, 182, 0, 0]);
        assert!(matches!(
            thrs.data,
            AdjustmentData::Threshold(Threshold { level: 182, .. })
        ));
        let invert = parse(b"nvrt", Vec::new());
        assert_eq!(
            invert.data,
            AdjustmentData::Invert {
                trailing_bytes: Vec::new()
            }
        );
    }

    #[test]
    fn hue_saturation_reads_both_keys() {
        let mut writer = BeWriter::new();
        writer.u16(2);
        writer.u8(1);
        writer.u8(0);
        for value in [66i16, 25, 0, 0, -40, 5] {
            writer.i16(value);
        }
        for index in 0..6i16 {
            for value in [
                index * 60 - 45,
                index * 60 - 15,
                index * 60 + 15,
                index * 60 + 45,
            ] {
                writer.i16(value);
            }
            for value in [index, -index, 0] {
                writer.i16(value);
            }
        }
        let bytes = writer.into_inner();
        assert_eq!(bytes.len(), 100);
        for key in [b"hue2", b"hue "] {
            let AdjustmentData::HueSaturation(hue) = parse(key, bytes.clone()).data else {
                panic!("hue/saturation expected")
            };
            assert!(hue.colorize);
            assert_eq!(hue.colorization.hue, 66);
            assert_eq!(hue.master.saturation, -40);
            assert_eq!(hue.ranges[5].range, [255, 285, 315, 345]);
            assert_eq!(hue.ranges[5].values.saturation, -5);
            assert!(hue.trailing_bytes.is_empty());
        }
        assert_eq!(
            parse(b"hue ", bytes).kind,
            AdjustmentKind::LegacyHueSaturation
        );
    }

    #[test]
    fn photo_filter_mixer_and_selective_color_layouts() {
        let mut writer = BeWriter::new();
        writer.u16(2);
        for value in [7, 0x1194, 0x0708, 0xcff4, 0] {
            writer.u16(value);
        }
        writer.u32(25);
        writer.u8(1);
        writer.bytes(&[0, 0, 0]);
        let AdjustmentData::PhotoFilter(filter) = parse(b"phfl", writer.into_inner()).data else {
            panic!("photo filter expected")
        };
        assert_eq!(
            filter.color,
            PhotoFilterColor::Color(RawColor {
                color_space: 7,
                components: [0x1194, 0x0708, 0xcff4, 0],
            })
        );
        assert_eq!(filter.density, 25);
        assert!(filter.preserve_luminosity);

        let mut writer = BeWriter::new();
        writer.u16(3);
        for value in [5000, -200, 300] {
            writer.i32(value);
        }
        writer.u32(80);
        writer.u8(0);
        let AdjustmentData::PhotoFilter(filter) = parse(b"phfl", writer.into_inner()).data else {
            panic!("photo filter expected")
        };
        assert_eq!(filter.color, PhotoFilterColor::Xyz([5000, -200, 300]));

        let mut writer = BeWriter::new();
        writer.u16(1);
        writer.u16(1);
        for mix in 0..4i16 {
            for value in [mix * 10, 100 - mix, 0, 0, -mix] {
                writer.i16(value);
            }
        }
        writer.u8(9);
        let AdjustmentData::ChannelMixer(mixer) = parse(b"mixr", writer.into_inner()).data else {
            panic!("channel mixer expected")
        };
        assert!(mixer.monochrome);
        assert_eq!(mixer.mixes.len(), 4);
        assert_eq!(
            mixer.mixes[3],
            ChannelMix {
                sources: [30, 97, 0, 0],
                constant: -3
            }
        );
        assert_eq!(mixer.trailing_bytes, [9]);

        let mut writer = BeWriter::new();
        writer.u16(1);
        writer.u16(1);
        for plate in 0..10i16 {
            for channel in 0..4i16 {
                writer.i16(plate * 4 + channel);
            }
        }
        let AdjustmentData::SelectiveColor(selective) = parse(b"selc", writer.into_inner()).data
        else {
            panic!("selective color expected")
        };
        assert!(selective.absolute);
        assert_eq!(selective.reserved.cyan, 0);
        assert_eq!(selective.reds.cyan, 4);
        assert_eq!(selective.blacks.black, 39);
    }

    #[test]
    fn gradient_map_reads_stops_and_noise_settings() {
        let mut writer = BeWriter::new();
        writer.u16(3);
        writer.u8(1);
        writer.u8(0);
        writer.bytes(b"Perc");
        UnicodeString::new("Violet, Orange", 1)
            .unwrap()
            .write(&mut writer)
            .unwrap();
        writer.u16(2);
        for (location, color) in [
            (0u32, [0x2929, 0x0a0a, 0x5959]),
            (4096, [0xffff, 0x7c7c, 0]),
        ] {
            writer.u32(location);
            writer.u32(50);
            writer.u16(0);
            for value in color {
                writer.u16(value);
            }
            writer.u16(0);
            writer.u16(0);
        }
        writer.u16(1);
        writer.u32(4096);
        writer.u32(50);
        writer.u16(255);
        writer.u16(2);
        writer.u16(2048);
        writer.u16(32);
        writer.u16(1);
        writer.u32(0x0182_71af);
        writer.u16(1);
        writer.u16(0);
        writer.u32(2048);
        writer.u16(3);
        for value in [0, 0, 0, 0, 0x8000, 0x8000, 0x8000, 0x8000] {
            writer.u16(value);
        }
        writer.bytes(&[0, 0, 0, 0]);
        let AdjustmentData::GradientMap(map) = parse(b"grdm", writer.into_inner()).data else {
            panic!("gradient map expected")
        };
        assert!(map.reversed);
        assert!(!map.dithered);
        assert_eq!(map.method, Some(*b"Perc"));
        assert_eq!(map.name, "Violet, Orange");
        assert_eq!(map.color_stops.len(), 2);
        assert_eq!(map.color_stops[1].location, 4096);
        assert_eq!(map.color_stops[1].color.components[1], 0x7c7c);
        assert_eq!(map.transparency_stops[0].opacity, 255);
        assert_eq!(map.smoothness(), 0.5);
        assert_eq!(map.mode, 1);
        assert_eq!(map.random_seed, 0x0182_71af);
        assert!(map.show_transparency);
        assert_eq!(map.maximum_color, [0x8000; 4]);
        assert_eq!(map.trailing_bytes, [0, 0, 0, 0]);
    }

    #[test]
    fn descriptor_layouts_expose_known_fields_and_keep_unknown_ones() {
        let vibrance = descriptor(vec![
            ("vibrance", DescriptorValue::Integer(21)),
            ("Strt", DescriptorValue::Integer(-4)),
            ("futureField", DescriptorValue::Boolean(true)),
        ]);
        let AdjustmentData::Vibrance(parsed) = parse(b"vibA", versioned(&vibrance, &[0, 0])).data
        else {
            panic!("vibrance expected")
        };
        assert_eq!(parsed.vibrance(), Some(21.0));
        assert_eq!(parsed.saturation(), Some(-4.0));
        assert_eq!(
            parsed.descriptor.get("futureField").unwrap().as_bool(),
            Some(true)
        );
        assert_eq!(parsed.trailing_bytes, [0, 0]);

        let tint = descriptor(vec![("Rd  ", DescriptorValue::Double(225.0))]);
        let black_and_white = descriptor(vec![
            ("Rd  ", DescriptorValue::Integer(-40)),
            ("Yllw", DescriptorValue::Integer(235)),
            ("Mgnt", DescriptorValue::Double(80.0)),
            ("useTint", DescriptorValue::Boolean(true)),
            ("tintColor", DescriptorValue::Descriptor(tint)),
            ("bwPresetKind", DescriptorValue::Integer(1)),
            (
                "blackAndWhitePresetFileName",
                DescriptorValue::String(UnicodeString::new("Default", 2).unwrap()),
            ),
        ]);
        let AdjustmentData::BlackAndWhite(parsed) =
            parse(b"blwh", versioned(&black_and_white, &[])).data
        else {
            panic!("black and white expected")
        };
        assert_eq!(
            parsed.weights(),
            [Some(-40.0), Some(235.0), None, None, None, Some(80.0)]
        );
        assert_eq!(parsed.use_tint(), Some(true));
        let tint_red = parsed.tint_color().unwrap().get("Rd  ").unwrap();
        assert_eq!(tint_red.as_double(), Some(225.0));
        assert_eq!(
            parsed.preset(),
            Some(AdjustmentPreset {
                kind: Some(1.0),
                file_name: Some("Default")
            })
        );

        let lookup = descriptor(vec![
            (
                "lookupType",
                DescriptorValue::Enumerated {
                    type_id: DescriptorKey::new("colorLookupType"),
                    value: DescriptorKey::new("3DLUT"),
                },
            ),
            (
                "Nm  ",
                DescriptorValue::String(UnicodeString::new("Film.cube", 2).unwrap()),
            ),
            ("Dthr", DescriptorValue::Boolean(true)),
            (
                "LUT3DFileData",
                DescriptorValue::RawData {
                    os_key: *b"tdta",
                    data: vec![1, 2, 3],
                },
            ),
        ]);
        let mut bytes = vec![0, 1];
        bytes.extend(versioned(&lookup, &[]));
        let AdjustmentData::ColorLookup(parsed) = parse(b"clrL", bytes).data else {
            panic!("color lookup expected")
        };
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.lookup_type().unwrap().as_bytes(), b"3DLUT");
        assert_eq!(parsed.name(), Some("Film.cube"));
        assert_eq!(parsed.dither(), Some(true));
        assert_eq!(parsed.lut_file_data(), Some(&[1, 2, 3][..]));
        assert_eq!(parsed.profile(), None);

        let generator = descriptor(vec![
            ("Vrsn", DescriptorValue::Integer(1)),
            ("Brgh", DescriptorValue::Integer(22)),
            ("Cntr", DescriptorValue::Integer(-13)),
            ("means", DescriptorValue::Integer(127)),
            ("Lab ", DescriptorValue::Boolean(false)),
            ("useLegacy", DescriptorValue::Boolean(false)),
            ("Auto", DescriptorValue::Boolean(true)),
        ]);
        let AdjustmentData::ContentGenerator(parsed) =
            parse(b"CgEd", versioned(&generator, &[])).data
        else {
            panic!("content generator expected")
        };
        assert_eq!(parsed.version(), Some(1.0));
        assert_eq!(parsed.brightness(), Some(22.0));
        assert_eq!(parsed.contrast(), Some(-13.0));
        assert_eq!(parsed.mean_value(), Some(127.0));
        assert_eq!(parsed.use_legacy(), Some(false));
        assert_eq!(parsed.auto(), Some(true));
        assert_eq!(parsed.preset(), None);

        let curves_preset = descriptor(vec![
            ("Vrsn", DescriptorValue::Integer(1)),
            ("curvesPresetKind", DescriptorValue::Integer(5)),
            (
                "curvesPresetFileName",
                DescriptorValue::String(UnicodeString::new("Custom", 2).unwrap()),
            ),
        ]);
        let AdjustmentData::ContentGenerator(parsed) =
            parse(b"CgEd", versioned(&curves_preset, &[])).data
        else {
            panic!("content generator expected")
        };
        assert_eq!(
            parsed.preset(),
            Some(AdjustmentPreset {
                kind: Some(5.0),
                file_name: Some("Custom")
            })
        );

        let color = descriptor(vec![("Rd  ", DescriptorValue::Double(255.0))]);
        let fill = descriptor(vec![("Clr ", DescriptorValue::Descriptor(color))]);
        let parsed = parse(b"SoCo", versioned(&fill, &[]));
        assert_eq!(parsed.kind, AdjustmentKind::SolidColor);
        let AdjustmentData::Fill(settings) = parsed.data else {
            panic!("fill expected")
        };
        let red = settings.color().unwrap().get("Rd  ").unwrap();
        assert_eq!(red.as_double(), Some(255.0));
        assert_eq!(settings.gradient(), None);
    }

    #[test]
    fn malformed_and_unsupported_payloads_fail_without_changing_blocks() {
        let cases: Vec<(&[u8; 4], Vec<u8>)> = vec![
            (b"levl", vec![0, 3]),                      // unknown version
            (b"levl", vec![0, 2, 0, 0]),                // truncated records
            (b"curv", vec![0, 0, 2, 0, 0, 0, 1]),       // unknown version
            (b"curv", vec![0, 0, 1, 0, 0, 0, 1, 0, 9]), // points past the end
            (b"expA", vec![0, 2]),
            (b"hue2", vec![0, 1]),
            (b"phfl", vec![0, 4]),
            (b"mixr", vec![0, 2, 0, 0]),
            (b"selc", vec![0, 1, 0, 0, 0, 0]),
            (b"grdm", vec![0, 2]),
            (b"brit", vec![0, 1]),
            (b"blnc", vec![0; 5]),
            (b"post", vec![0]),
            (b"vibA", vec![0, 0, 0, 15]),
            (b"clrL", vec![0, 2, 0, 0, 0, 16]),
            (b"CgEd", vec![0, 0, 0, 16, 0]),
            (b"SoCo", Vec::new()),
        ];
        for (key, data) in cases {
            let raw = block(key, data.clone());
            assert!(
                AdjustmentBlock::read(&raw).is_err(),
                "{} should fail",
                String::from_utf8_lossy(key)
            );
            assert_eq!(raw.data, data);
        }

        // A Lvls marker with an unknown version or a count below 29.
        for (version, count) in [(4u16, 62u16), (3, 12)] {
            let mut writer = BeWriter::new();
            writer.u16(2);
            for _ in 0..29 {
                levels_record(&mut writer, [0, 255, 0, 255, 100]);
            }
            writer.bytes(b"Lvls");
            writer.u16(version);
            writer.u16(count);
            assert!(AdjustmentBlock::read(&block(b"levl", writer.into_inner())).is_err());
        }

        // A huge declared curve count fails on the payload bound, not on
        // allocation.
        let mut writer = BeWriter::new();
        writer.u8(0);
        writer.u16(4);
        writer.u32(u32::MAX);
        writer.u16(0);
        assert!(AdjustmentBlock::read(&block(b"curv", writer.into_inner())).is_err());
    }
}
