//! Read-only views of vector paths, masks, and shape data.
//!
//! Photoshop stores Bézier paths as 26-byte path records (the Adobe PSD/PSB
//! specification's "Path resource format"). The same records appear in
//! layer vector masks (`vmsk`, or `vsms` from Photoshop CS6 on) and in the
//! document's saved-path image resources (IDs 2000–2997, and 1025 for the
//! work path). A shape layer pairs a vector mask with its fill: a `SoCo`,
//! `GdFl`, or `PtFl` block (see [`crate::adjustments`]), or, from CS6 on, a
//! `vscg` block. `vstk` holds its stroke style and `vogk` the live-shape
//! parameters. The document-level `pths` block lists Unicode names for the
//! saved paths.
//!
//! The specification leaves most of a subpath length record undocumented.
//! [`SubpathRecord`] keeps every byte and names the fields that the public
//! `psd-tools` (<https://github.com/psd-tools/psd-tools>) and `ag-psd-rs`
//! (<https://github.com/Vasyanator/ag-psd-rs>) readers interpret. ag-psd-rs
//! skips `pths` and the `vogk` key list; these views read both.
//!
//! Upstream (`LayeredFile/LayerTypes/ShapeLayer.h`) only detects shape layers
//! and round-trips them as opaque data. As with the other views, the original
//! tagged block is still what gets written.

use crate::adjustments::{AdjustmentKind, FillSettings};
use crate::descriptor::{Descriptor, DescriptorKey, DescriptorValue};
use crate::error::{PsdError, Result};
use crate::io::BeReader;
use crate::tagged_blocks::{TaggedBlock, TaggedBlockKey};
use crate::views::{enumerator, four_cc, number, read_versioned_descriptor, trailing, unsupported};

/// Size of one path record: a `u16` selector and 24 bytes of data.
pub const PATH_RECORD_SIZE: usize = 26;

/// A path point: signed 8.24 fixed-point components relative to the
/// document size, stored vertical first. `(0, 0)` is the top-left corner and
/// `(1, 1)` (`0x0100_0000`) the bottom-right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathPoint {
    pub vertical: i32,
    pub horizontal: i32,
}

impl PathPoint {
    fn read(reader: &mut BeReader) -> Result<Self> {
        Ok(Self {
            vertical: reader.i32()?,
            horizontal: reader.i32()?,
        })
    }

    /// Horizontal position as a fraction of the document width.
    pub fn x(&self) -> f64 {
        fixed_8_24(self.horizontal)
    }

    /// Vertical position as a fraction of the document height.
    pub fn y(&self) -> f64 {
        fixed_8_24(self.vertical)
    }

    /// Position in pixels for a `width` × `height` document.
    pub fn to_pixels(&self, width: u32, height: u32) -> (f64, f64) {
        (self.x() * f64::from(width), self.y() * f64::from(height))
    }
}

/// Convert a signed 8.24 fixed-point value.
pub fn fixed_8_24(value: i32) -> f64 {
    f64::from(value) / f64::from(1 << 24)
}

/// A subpath length record (selector 0 for closed, 3 for open).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubpathRecord {
    pub closed: bool,
    /// Number of knot records that follow.
    pub knot_count: u16,
    /// Shape operation: −1 none, 0 exclude, 1 combine, 2 subtract,
    /// 3 intersect (as ag-psd-rs and psd-tools read it).
    pub operation: i16,
    /// ag-psd-rs reads 2 as the non-zero winding fill rule and any other value
    /// as even-odd.
    pub flags: u16,
    /// The remaining 18 bytes, zero in most files.
    pub reserved: [u8; 18],
}

impl SubpathRecord {
    /// The index of the matching entry in the layer's `vogk` key list, which
    /// psd-tools reads from bytes 4–7 of [`reserved`](Self::reserved).
    pub fn origination_index(&self) -> u32 {
        u32::from_be_bytes([
            self.reserved[4],
            self.reserved[5],
            self.reserved[6],
            self.reserved[7],
        ])
    }
}

/// A Bézier knot record (selectors 1 and 2 closed, 4 and 5 open).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BezierKnot {
    pub closed: bool,
    /// Whether the two control points move together.
    pub linked: bool,
    /// Control point of the segment arriving at the knot.
    pub preceding: PathPoint,
    pub anchor: PathPoint,
    /// Control point of the segment leaving the knot.
    pub leaving: PathPoint,
}

/// One 26-byte path record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathRecord {
    Subpath(SubpathRecord),
    Knot(BezierKnot),
    /// Selector 6, which starts every path. Its data is normally zero.
    PathFillRule {
        reserved: [u8; 24],
    },
    /// Selector 7: the clipboard bounds (8.24 fixed point) and resolution.
    Clipboard {
        top: i32,
        left: i32,
        bottom: i32,
        right: i32,
        resolution: i32,
        reserved: [u8; 4],
    },
    /// Selector 8: 1 when filling starts with all pixels.
    InitialFillRule {
        value: u16,
        reserved: [u8; 22],
    },
    /// A selector this view does not know, kept verbatim.
    Unknown {
        selector: u16,
        data: [u8; 24],
    },
}

impl PathRecord {
    fn read(reader: &mut BeReader) -> Result<Self> {
        let selector = reader.u16()?;
        Ok(match selector {
            0 | 3 => Self::Subpath(SubpathRecord {
                closed: selector == 0,
                knot_count: reader.u16()?,
                operation: reader.i16()?,
                flags: reader.u16()?,
                reserved: array(reader)?,
            }),
            1 | 2 | 4 | 5 => Self::Knot(BezierKnot {
                closed: selector <= 2,
                linked: selector == 1 || selector == 4,
                preceding: PathPoint::read(reader)?,
                anchor: PathPoint::read(reader)?,
                leaving: PathPoint::read(reader)?,
            }),
            6 => Self::PathFillRule {
                reserved: array(reader)?,
            },
            7 => Self::Clipboard {
                top: reader.i32()?,
                left: reader.i32()?,
                bottom: reader.i32()?,
                right: reader.i32()?,
                resolution: reader.i32()?,
                reserved: array(reader)?,
            },
            8 => Self::InitialFillRule {
                value: reader.u16()?,
                reserved: array(reader)?,
            },
            _ => Self::Unknown {
                selector,
                data: array(reader)?,
            },
        })
    }
}

/// A sequence of path records, as stored in a vector mask or a path resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorPath {
    pub records: Vec<PathRecord>,
    /// Bytes after the last whole record, usually alignment padding.
    pub trailing_bytes: Vec<u8>,
}

/// A subpath and its knots, borrowed from a [`VectorPath`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subpath<'a> {
    pub record: &'a SubpathRecord,
    pub knots: Vec<&'a BezierKnot>,
}

impl VectorPath {
    /// Read whole 26-byte records until fewer than 26 bytes remain.
    pub fn read(data: &[u8]) -> Result<Self> {
        let mut reader = BeReader::new(data);
        Self::read_from(&mut reader)
    }

    fn read_from(reader: &mut BeReader) -> Result<Self> {
        let mut records = Vec::with_capacity(reader.remaining() / PATH_RECORD_SIZE);
        while reader.remaining() >= PATH_RECORD_SIZE {
            records.push(PathRecord::read(reader)?);
        }
        Ok(Self {
            records,
            trailing_bytes: trailing(reader)?,
        })
    }

    /// Subpaths with the knots that follow each length record. A knot before
    /// the first length record is an error. The declared
    /// [`knot_count`](SubpathRecord::knot_count) is not enforced.
    pub fn subpaths(&self) -> Result<Vec<Subpath<'_>>> {
        let mut subpaths: Vec<Subpath<'_>> = Vec::new();
        for record in &self.records {
            match record {
                PathRecord::Subpath(record) => subpaths.push(Subpath {
                    record,
                    knots: Vec::with_capacity(usize::from(record.knot_count).min(64)),
                }),
                PathRecord::Knot(knot) => match subpaths.last_mut() {
                    Some(subpath) => subpath.knots.push(knot),
                    None => {
                        return Err(PsdError::InvalidData {
                            offset: 0,
                            message: "path knot record before any subpath record",
                        })
                    }
                },
                _ => {}
            }
        }
        Ok(subpaths)
    }

    /// The initial fill rule record's value (1 = start with all pixels).
    pub fn initial_fill_rule(&self) -> Option<u16> {
        self.records.iter().find_map(|record| match record {
            PathRecord::InitialFillRule { value, .. } => Some(*value),
            _ => None,
        })
    }
}

/// `vmsk` / `vsms`: a layer vector mask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorMask {
    pub version: u32,
    /// Bit 0 invert, bit 1 not linked, bit 2 disabled; other bits kept.
    pub flags: u32,
    pub path: VectorPath,
}

impl VectorMask {
    /// Parse a mask payload (version 3, flags, path records).
    pub fn read(data: &[u8]) -> Result<Self> {
        let mut reader = BeReader::new(data);
        let version = reader.u32()?;
        if version != 3 {
            return Err(unsupported(&reader));
        }
        let flags = reader.u32()?;
        let path = VectorPath::read_from(&mut reader)?;
        Ok(Self {
            version,
            flags,
            path,
        })
    }

    pub fn inverted(&self) -> bool {
        self.flags & 1 != 0
    }

    pub fn not_linked(&self) -> bool {
        self.flags & 2 != 0
    }

    pub fn disabled(&self) -> bool {
        self.flags & 4 != 0
    }
}

/// `vstk`: a shape's stroke style (descriptor class `strokeStyle`).
#[derive(Debug, Clone, PartialEq)]
pub struct VectorStroke {
    pub descriptor: Descriptor,
    pub trailing_bytes: Vec<u8>,
}

impl VectorStroke {
    pub fn stroke_enabled(&self) -> Option<bool> {
        self.descriptor.get("strokeEnabled")?.as_bool()
    }

    pub fn fill_enabled(&self) -> Option<bool> {
        self.descriptor.get("fillEnabled")?.as_bool()
    }

    /// Unit (for example `#Pxl` or `#Pnt`) and width.
    pub fn line_width(&self) -> Option<([u8; 4], f64)> {
        self.descriptor
            .get("strokeStyleLineWidth")?
            .as_unit_float()
            .map(|(unit, value)| (*unit, value))
    }

    /// Cap enumerator, for example `strokeStyleButtCap`.
    pub fn line_cap(&self) -> Option<&DescriptorKey> {
        enumerator(&self.descriptor, "strokeStyleLineCapType")
    }

    /// Join enumerator, for example `strokeStyleMiterJoin`.
    pub fn line_join(&self) -> Option<&DescriptorKey> {
        enumerator(&self.descriptor, "strokeStyleLineJoinType")
    }

    /// Alignment enumerator, for example `strokeStyleAlignCenter`.
    pub fn line_alignment(&self) -> Option<&DescriptorKey> {
        enumerator(&self.descriptor, "strokeStyleLineAlignment")
    }

    pub fn miter_limit(&self) -> Option<f64> {
        number(&self.descriptor, "strokeStyleMiterLimit")
    }

    /// Dash pattern lengths; empty for a solid stroke.
    pub fn dash_set(&self) -> Option<Vec<f64>> {
        let list = self.descriptor.get("strokeStyleLineDashSet")?.as_list()?;
        list.iter()
            .map(|value| match value {
                DescriptorValue::UnitFloat { value, .. } | DescriptorValue::Double(value) => {
                    Some(*value)
                }
                DescriptorValue::Integer(value) => Some(f64::from(*value)),
                _ => None,
            })
            .collect()
    }

    /// Raw blend-mode enumerator (`BlnM`).
    pub fn blend_mode(&self) -> Option<&DescriptorKey> {
        enumerator(&self.descriptor, "strokeStyleBlendMode")
    }

    /// Opacity in percent.
    pub fn opacity_percent(&self) -> Option<f64> {
        number(&self.descriptor, "strokeStyleOpacity")
    }

    /// The stroke paint: a descriptor of class `solidColorLayer`,
    /// `gradientLayer`, or `patternLayer`.
    pub fn content(&self) -> Option<&Descriptor> {
        self.descriptor.get("strokeStyleContent")?.as_descriptor()
    }

    pub fn resolution(&self) -> Option<f64> {
        number(&self.descriptor, "strokeStyleResolution")
    }
}

/// `vscg`: a CS6+ shape layer's fill. The specification calls it "vector
/// stroke content"; Photoshop stores the shape's fill paint here, and the key
/// names the paint type.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorContent {
    /// `SoCo`, `GdFl`, or `PtFl`.
    pub key: [u8; 4],
    pub fill: FillSettings,
}

impl VectorContent {
    /// The fill kind the key names, when it is a known fill key.
    pub fn kind(&self) -> Option<AdjustmentKind> {
        AdjustmentKind::from_key(TaggedBlockKey::new(self.key)).filter(|kind| kind.is_fill())
    }
}

/// `vogk`: live-shape parameters, one entry per shape in `keyDescriptorList`.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorOrigination {
    pub version: u32,
    pub descriptor: Descriptor,
    pub trailing_bytes: Vec<u8>,
}

impl VectorOrigination {
    /// The shapes' origination entries in stored order.
    pub fn keys(&self) -> Vec<OriginationKey<'_>> {
        self.descriptor
            .get("keyDescriptorList")
            .and_then(DescriptorValue::as_list)
            .unwrap_or_default()
            .iter()
            .filter_map(DescriptorValue::as_descriptor)
            .map(|descriptor| OriginationKey { descriptor })
            .collect()
    }
}

/// Bounds in document pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapeBounds {
    pub top: f64,
    pub left: f64,
    pub bottom: f64,
    pub right: f64,
}

/// A rounded rectangle's corner radii in document pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CornerRadii {
    pub top_left: f64,
    pub top_right: f64,
    pub bottom_right: f64,
    pub bottom_left: f64,
}

/// One live-shape entry from a `vogk` key list.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OriginationKey<'a> {
    pub descriptor: &'a Descriptor,
}

impl OriginationKey<'_> {
    /// Photoshop's shape type (`keyOriginType`): for example 1 rectangle,
    /// 2 rounded rectangle, 4 line, 5 ellipse, and 8 polygon.
    pub fn origin_type(&self) -> Option<i32> {
        self.descriptor.get("keyOriginType")?.as_integer()
    }

    /// The index subpath records refer to (`keyOriginIndex`).
    pub fn index(&self) -> Option<i32> {
        self.descriptor.get("keyOriginIndex")?.as_integer()
    }

    pub fn resolution(&self) -> Option<f64> {
        number(self.descriptor, "keyOriginResolution")
    }

    /// The shape's bounding box (`keyOriginShapeBBox`).
    pub fn shape_bounds(&self) -> Option<ShapeBounds> {
        let bounds = self.descriptor.get("keyOriginShapeBBox")?.as_descriptor()?;
        Some(ShapeBounds {
            top: number(bounds, "Top ")?,
            left: number(bounds, "Left")?,
            bottom: number(bounds, "Btom")?,
            right: number(bounds, "Rght")?,
        })
    }

    /// Rounded-rectangle corner radii (`keyOriginRRectRadii`).
    pub fn corner_radii(&self) -> Option<CornerRadii> {
        let radii = self
            .descriptor
            .get("keyOriginRRectRadii")?
            .as_descriptor()?;
        Some(CornerRadii {
            top_left: number(radii, "topLeft")?,
            top_right: number(radii, "topRight")?,
            bottom_right: number(radii, "bottomRight")?,
            bottom_left: number(radii, "bottomLeft")?,
        })
    }

    /// The shape transform (`Trnf`): `[xx, xy, yx, yy, tx, ty]`.
    pub fn transform(&self) -> Option<[f64; 6]> {
        let transform = self.descriptor.get("Trnf")?.as_descriptor()?;
        Some([
            number(transform, "xx")?,
            number(transform, "xy")?,
            number(transform, "yx")?,
            number(transform, "yy")?,
            number(transform, "tx")?,
            number(transform, "ty")?,
        ])
    }
}

/// `pths`: the document's Unicode names for its saved paths, in resource
/// order.
#[derive(Debug, Clone, PartialEq)]
pub struct PathNames {
    pub descriptor: Descriptor,
    pub trailing_bytes: Vec<u8>,
}

impl PathNames {
    /// Parse a `pths` payload.
    pub fn read(data: &[u8]) -> Result<Self> {
        let mut reader = BeReader::new(data);
        let descriptor = read_versioned_descriptor(&mut reader)?;
        Ok(Self {
            descriptor,
            trailing_bytes: trailing(&mut reader)?,
        })
    }

    /// Each `pathInfoClass` entry's `pathUnicodeName`, in stored order.
    pub fn names(&self) -> Vec<Option<&str>> {
        self.descriptor
            .get("pathList")
            .and_then(DescriptorValue::as_list)
            .unwrap_or_default()
            .iter()
            .map(|entry| entry.as_descriptor()?.get("pathUnicodeName")?.as_str())
            .collect()
    }
}

/// A parsed view of one vector tagged block.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorBlock {
    pub key: TaggedBlockKey,
    pub data: VectorData,
}

#[derive(Debug, Clone, PartialEq)]
pub enum VectorData {
    /// `vmsk` or `vsms`.
    Mask(VectorMask),
    /// `vstk`
    Stroke(VectorStroke),
    /// `vscg`
    Content(VectorContent),
    /// `vogk`
    Origination(VectorOrigination),
    /// `pths` (document level)
    PathNames(PathNames),
}

impl VectorBlock {
    /// Parse a recognized vector block. Other tagged blocks return `None`.
    /// A malformed or unsupported payload is an error only when this view is
    /// requested; the raw block is never modified.
    pub fn read(block: &TaggedBlock) -> Result<Option<Self>> {
        let data = match &block.key.as_bytes() {
            b"vmsk" | b"vsms" => VectorData::Mask(VectorMask::read(&block.data)?),
            b"vstk" => {
                let mut reader = BeReader::new(&block.data);
                let descriptor = read_versioned_descriptor(&mut reader)?;
                VectorData::Stroke(VectorStroke {
                    descriptor,
                    trailing_bytes: trailing(&mut reader)?,
                })
            }
            b"vscg" => {
                let mut reader = BeReader::new(&block.data);
                let key = four_cc(&mut reader)?;
                let descriptor = read_versioned_descriptor(&mut reader)?;
                VectorData::Content(VectorContent {
                    key,
                    fill: FillSettings {
                        descriptor,
                        trailing_bytes: trailing(&mut reader)?,
                    },
                })
            }
            b"vogk" => {
                let mut reader = BeReader::new(&block.data);
                let version = reader.u32()?;
                if version != 1 {
                    return Err(unsupported(&reader));
                }
                let descriptor = read_versioned_descriptor(&mut reader)?;
                VectorData::Origination(VectorOrigination {
                    version,
                    descriptor,
                    trailing_bytes: trailing(&mut reader)?,
                })
            }
            b"pths" => VectorData::PathNames(PathNames::read(&block.data)?),
            _ => return Ok(None),
        };
        Ok(Some(Self {
            key: block.key,
            data,
        }))
    }
}

/// Image-resource ID of the work path.
pub const WORK_PATH_RESOURCE_ID: u16 = 1025;
/// Image-resource IDs of saved paths.
pub const SAVED_PATH_RESOURCE_IDS: std::ops::RangeInclusive<u16> = 2000..=2997;

/// A document-level path from the image resources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentPath {
    /// 1025 for the work path, 2000–2997 for saved paths.
    pub resource_id: u16,
    /// The resource's Pascal-string name (the saved path's name).
    pub name: String,
    /// The saved path's name from the document's `pths` block, when present.
    pub unicode_name: Option<String>,
    pub path: VectorPath,
}

impl DocumentPath {
    pub fn is_work_path(&self) -> bool {
        self.resource_id == WORK_PATH_RESOURCE_ID
    }
}

/// Whether `key` holds a layer vector mask (`vmsk` or `vsms`).
pub fn is_vector_mask_key(key: TaggedBlockKey) -> bool {
    matches!(&key.as_bytes(), b"vmsk" | b"vsms")
}

fn array<const N: usize>(reader: &mut BeReader) -> Result<[u8; N]> {
    let mut bytes = [0; N];
    bytes.copy_from_slice(reader.take(N)?);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::DescriptorItem;
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

    fn fixed(value: f64) -> i32 {
        (value * f64::from(1 << 24)).round() as i32
    }

    fn knot(writer: &mut BeWriter, selector: u16, y: f64, x: f64) {
        writer.u16(selector);
        for _ in 0..3 {
            writer.i32(fixed(y));
            writer.i32(fixed(x));
        }
    }

    fn subpath(writer: &mut BeWriter, selector: u16, count: u16, operation: i16, index: u32) {
        writer.u16(selector);
        writer.u16(count);
        writer.i16(operation);
        writer.u16(1);
        writer.u32(0);
        writer.u32(index);
        writer.bytes(&[0; 10]);
    }

    /// A rectangle and an open two-knot path, with the records Photoshop
    /// writes first, then two padding bytes.
    fn mask_payload(flags: u32) -> Vec<u8> {
        let mut writer = BeWriter::new();
        writer.u32(3);
        writer.u32(flags);
        writer.u16(6);
        writer.bytes(&[0; 24]);
        writer.u16(8);
        writer.u16(1);
        writer.bytes(&[0; 22]);
        subpath(&mut writer, 0, 4, 1, 0);
        for (y, x) in [(0.25, 0.25), (0.25, 0.75), (0.75, 0.75), (0.75, 0.25)] {
            knot(&mut writer, 2, y, x);
        }
        subpath(&mut writer, 3, 2, 2, 1);
        knot(&mut writer, 4, 0.1, 0.1);
        knot(&mut writer, 5, 0.9, -0.5);
        writer.u16(0);
        writer.into_inner()
    }

    #[test]
    fn vector_masks_expose_flags_records_and_subpaths() {
        for key in [b"vmsk", b"vsms"] {
            let block = TaggedBlock::new(TaggedBlockKey::new(*key), mask_payload(0b1101));
            let parsed = VectorBlock::read(&block).unwrap().unwrap();
            let VectorData::Mask(mask) = parsed.data else {
                panic!("mask expected")
            };
            assert!(mask.inverted() && !mask.not_linked() && mask.disabled());
            assert_eq!(mask.flags, 0b1101);
            assert_eq!(mask.path.records.len(), 10);
            assert_eq!(mask.path.initial_fill_rule(), Some(1));
            assert_eq!(mask.path.trailing_bytes, [0, 0]);
            let subpaths = mask.path.subpaths().unwrap();
            assert_eq!(subpaths.len(), 2);
            assert!(subpaths[0].record.closed);
            assert_eq!(subpaths[0].record.operation, 1);
            assert_eq!(subpaths[0].knots.len(), 4);
            assert!(!subpaths[0].knots[1].linked);
            assert_eq!(subpaths[0].knots[1].anchor.x(), 0.75);
            assert_eq!(subpaths[0].knots[1].anchor.to_pixels(64, 32), (48.0, 8.0));
            assert!(!subpaths[1].record.closed);
            assert_eq!(subpaths[1].record.origination_index(), 1);
            assert!(subpaths[1].knots[0].linked);
            assert!(!subpaths[1].knots[0].closed);
            assert_eq!(subpaths[1].knots[1].anchor.x(), -0.5);
        }
    }

    #[test]
    fn unknown_records_and_clipboards_are_kept() {
        let mut writer = BeWriter::new();
        writer.u16(7);
        for value in [0.0, 0.0, 1.0, 1.0, 300.0] {
            writer.i32(fixed(value / 256.0));
        }
        writer.bytes(&[0; 4]);
        writer.u16(42);
        writer.bytes(&[7; 24]);
        writer.bytes(&[1, 2, 3]);
        let path = VectorPath::read(writer.as_slice()).unwrap();
        assert!(matches!(
            path.records[0],
            PathRecord::Clipboard { bottom, .. } if fixed_8_24(bottom) == 1.0 / 256.0
        ));
        assert_eq!(
            path.records[1],
            PathRecord::Unknown {
                selector: 42,
                data: [7; 24]
            }
        );
        assert_eq!(path.trailing_bytes, [1, 2, 3]);
        assert!(path.subpaths().unwrap().is_empty());
    }

    #[test]
    fn malformed_vector_payloads_fail_without_changing_blocks() {
        let mut knot_first = BeWriter::new();
        knot(&mut knot_first, 1, 0.0, 0.0);
        assert!(VectorPath::read(knot_first.as_slice())
            .unwrap()
            .subpaths()
            .is_err());

        let mut wrong_version = mask_payload(0);
        wrong_version[3] = 2;
        for (key, data) in [
            (b"vmsk", wrong_version),
            (b"vsms", vec![0, 0, 0]),
            (b"vstk", vec![0, 0, 0, 16]),
            (b"vscg", b"SoCo".to_vec()),
            (b"vogk", vec![0, 0, 0, 2, 0, 0, 0, 16]),
            (b"pths", vec![0, 0, 0, 15]),
        ] {
            let block = TaggedBlock::new(TaggedBlockKey::new(*key), data.clone());
            assert!(VectorBlock::read(&block).is_err());
            assert_eq!(block.data, data);
        }
        let other = TaggedBlock::new(TaggedBlockKey::new(*b"vowv"), vec![0, 0, 0, 2]);
        assert!(VectorBlock::read(&other).unwrap().is_none());
    }

    #[test]
    fn descriptor_blocks_expose_stroke_content_origination_and_names() {
        let mut writer = BeWriter::new();
        writer.u32(16);
        descriptor(
            "strokeStyle",
            vec![
                ("strokeEnabled", DescriptorValue::Boolean(true)),
                ("fillEnabled", DescriptorValue::Boolean(false)),
                (
                    "strokeStyleLineWidth",
                    DescriptorValue::UnitFloat {
                        unit: *b"#Pxl",
                        value: 3.0,
                    },
                ),
                (
                    "strokeStyleLineCapType",
                    DescriptorValue::Enumerated {
                        type_id: key("strokeStyleLineCapType"),
                        value: key("strokeStyleRoundCap"),
                    },
                ),
                (
                    "strokeStyleLineDashSet",
                    DescriptorValue::List(vec![
                        DescriptorValue::UnitFloat {
                            unit: *b"#Nne",
                            value: 4.0,
                        },
                        DescriptorValue::UnitFloat {
                            unit: *b"#Nne",
                            value: 2.0,
                        },
                    ]),
                ),
                (
                    "strokeStyleContent",
                    DescriptorValue::Descriptor(descriptor("solidColorLayer", vec![])),
                ),
            ],
        )
        .write(&mut writer)
        .unwrap();
        let block = TaggedBlock::new(TaggedBlockKey::new(*b"vstk"), writer.into_inner());
        let VectorData::Stroke(stroke) = VectorBlock::read(&block).unwrap().unwrap().data else {
            panic!("stroke expected")
        };
        assert_eq!(stroke.stroke_enabled(), Some(true));
        assert_eq!(stroke.fill_enabled(), Some(false));
        assert_eq!(stroke.line_width(), Some((*b"#Pxl", 3.0)));
        assert_eq!(
            stroke.line_cap().unwrap().as_bytes(),
            b"strokeStyleRoundCap"
        );
        assert_eq!(stroke.dash_set(), Some(vec![4.0, 2.0]));
        assert_eq!(
            stroke.content().unwrap().class_id.as_bytes(),
            b"solidColorLayer"
        );

        let mut writer = BeWriter::new();
        writer.bytes(b"GdFl");
        writer.u32(16);
        descriptor("null", vec![("Angl", DescriptorValue::Double(30.0))])
            .write(&mut writer)
            .unwrap();
        let block = TaggedBlock::new(TaggedBlockKey::new(*b"vscg"), writer.into_inner());
        let VectorData::Content(content) = VectorBlock::read(&block).unwrap().unwrap().data else {
            panic!("content expected")
        };
        assert_eq!(content.kind(), Some(AdjustmentKind::GradientFill));
        assert_eq!(content.fill.angle(), Some(30.0));

        let bounds = descriptor(
            "classFloatRect",
            vec![
                ("Top ", DescriptorValue::Double(10.0)),
                ("Left", DescriptorValue::Double(20.0)),
                ("Btom", DescriptorValue::Double(30.0)),
                ("Rght", DescriptorValue::Double(40.0)),
            ],
        );
        let entry = descriptor(
            "null",
            vec![
                ("keyOriginType", DescriptorValue::Integer(5)),
                ("keyOriginShapeBBox", DescriptorValue::Descriptor(bounds)),
                ("keyOriginIndex", DescriptorValue::Integer(0)),
            ],
        );
        let mut writer = BeWriter::new();
        writer.u32(1);
        writer.u32(16);
        descriptor(
            "null",
            vec![(
                "keyDescriptorList",
                DescriptorValue::List(vec![DescriptorValue::Descriptor(entry)]),
            )],
        )
        .write(&mut writer)
        .unwrap();
        writer.bytes(&[0, 0, 0]);
        let block = TaggedBlock::new(TaggedBlockKey::new(*b"vogk"), writer.into_inner());
        let VectorData::Origination(origination) = VectorBlock::read(&block).unwrap().unwrap().data
        else {
            panic!("origination expected")
        };
        let keys = origination.keys();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].origin_type(), Some(5));
        assert_eq!(keys[0].index(), Some(0));
        assert_eq!(
            keys[0].shape_bounds(),
            Some(ShapeBounds {
                top: 10.0,
                left: 20.0,
                bottom: 30.0,
                right: 40.0
            })
        );
        assert_eq!(keys[0].corner_radii(), None);
        assert_eq!(origination.trailing_bytes, [0, 0, 0]);

        let info = |name: &str| {
            DescriptorValue::Descriptor(descriptor(
                "pathInfoClass",
                vec![(
                    "pathUnicodeName",
                    DescriptorValue::String(UnicodeString::new(name, 1).unwrap()),
                )],
            ))
        };
        let mut writer = BeWriter::new();
        writer.u32(16);
        descriptor(
            "pathsDataClass",
            vec![(
                "pathList",
                DescriptorValue::List(vec![info("Outline"), info("Ünïcode")]),
            )],
        )
        .write(&mut writer)
        .unwrap();
        let names = PathNames::read(writer.as_slice()).unwrap();
        assert_eq!(names.names(), [Some("Outline"), Some("Ünïcode")]);
    }
}
