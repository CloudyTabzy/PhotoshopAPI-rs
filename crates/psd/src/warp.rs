//! Typed smart-object warp descriptors and geometry transforms.
//!
//! The raw `PlLd`/`SoLd` tagged-block bytes remain authoritative for
//! serialization. This module only creates an on-demand typed view, keeping
//! parsing and rendering separate from the byte-exact format path.

use psd_core::{
    AdditionalLayerInfo, BeReader, BeWriter, Descriptor, DescriptorItem, DescriptorKey,
    DescriptorValue, ObjectArray, PlacedLayer, PlacedLayerData, PsdError, Result, TaggedBlockKey,
    UnicodeString,
};

use crate::geometry::{BezierSurface, BoundingBox, Homography, Point2, QuadMesh};
use crate::layer::Layer;
use crate::BitDepth;

/// Normal Photoshop warp (one 4×4 cubic surface) or quilt warp (one or more
/// adjoining cubic patches).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarpKind {
    Normal,
    Quilt,
}

/// Typed view of a Photoshop smart-object warp.
#[derive(Debug, Clone, PartialEq)]
pub struct Warp {
    kind: WarpKind,
    style: String,
    bounds: BoundingBox,
    control_points: Vec<Point2>,
    grid_width: usize,
    grid_height: usize,
    custom_envelope: bool,
    /// Control-corner order is top-left, top-right, bottom-left, bottom-right.
    affine_transform: [Point2; 4],
    non_affine_transform: [Point2; 4],
    quilt_slices_x: Vec<f64>,
    quilt_slices_y: Vec<f64>,
    warp_value: f64,
    warp_perspective: f64,
    warp_perspective_other: f64,
    warp_rotate: String,
    u_order: i32,
    v_order: i32,
}

impl Warp {
    /// Create a flat normal warp for the source dimensions.
    pub fn generate_default(width: usize, height: usize) -> Result<Self> {
        Self::generate_default_with_grid(width, height, 4, 4)
    }

    /// Create a flat normal warp or quilt warp with `4 + 3n` control points
    /// along each axis. Quilt slice placement follows Photoshop's default
    /// `-0.6` / `extent + 0.6` endpoint convention.
    pub fn generate_default_with_grid(
        width: usize,
        height: usize,
        grid_width: usize,
        grid_height: usize,
    ) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "warp dimensions must be non-zero",
            });
        }
        validate_grid_dimensions(grid_width, grid_height)?;
        let points = default_control_points(width as f64, height as f64, grid_width, grid_height);
        let bounds = BoundingBox::new(Point2::ZERO, Point2::new(width as f64, height as f64));
        let kind = if grid_width == 4 && grid_height == 4 {
            WarpKind::Normal
        } else {
            WarpKind::Quilt
        };
        let quad = bounds_quad(bounds);
        let mut warp = Self {
            kind,
            style: "warpCustom".to_owned(),
            bounds,
            control_points: points,
            grid_width,
            grid_height,
            custom_envelope: true,
            affine_transform: quad,
            non_affine_transform: quad,
            quilt_slices_x: Vec::new(),
            quilt_slices_y: Vec::new(),
            warp_value: 0.0,
            warp_perspective: 0.0,
            warp_perspective_other: 0.0,
            warp_rotate: "Hrzn".to_owned(),
            u_order: 4,
            v_order: 4,
        };
        if kind == WarpKind::Quilt {
            warp.quilt_slices_x = default_quilt_slices(width as f64, grid_width);
            warp.quilt_slices_y = default_quilt_slices(height as f64, grid_height);
        }
        Ok(warp)
    }

    /// Build a warp from a caller-supplied scanline control net. Its initial
    /// affine/non-affine quads are the net's axis-aligned source bounds.
    pub fn from_control_points(
        points: Vec<Point2>,
        grid_width: usize,
        grid_height: usize,
    ) -> Result<Self> {
        validate_grid(grid_width, grid_height, &points)?;
        let bounds = BoundingBox::from_points(&points).ok_or(PsdError::InvalidData {
            offset: 0,
            message: "warp control points must be finite",
        })?;
        if bounds.width() <= 0.0 || bounds.height() <= 0.0 {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "warp control point bounds must have positive area",
            });
        }
        let kind = if grid_width == 4 && grid_height == 4 {
            WarpKind::Normal
        } else {
            WarpKind::Quilt
        };
        let quad = bounds_quad(bounds);
        Ok(Self {
            kind,
            style: "warpCustom".to_owned(),
            bounds,
            control_points: points,
            grid_width,
            grid_height,
            custom_envelope: true,
            affine_transform: quad,
            non_affine_transform: quad,
            quilt_slices_x: if kind == WarpKind::Quilt {
                default_quilt_slices(bounds.width(), grid_width)
                    .into_iter()
                    .map(|slice| slice + bounds.min.x)
                    .collect()
            } else {
                Vec::new()
            },
            quilt_slices_y: if kind == WarpKind::Quilt {
                default_quilt_slices(bounds.height(), grid_height)
                    .into_iter()
                    .map(|slice| slice + bounds.min.y)
                    .collect()
            } else {
                Vec::new()
            },
            warp_value: 0.0,
            warp_perspective: 0.0,
            warp_perspective_other: 0.0,
            warp_rotate: "Hrzn".to_owned(),
            u_order: 4,
            v_order: 4,
        })
    }

    /// Build a no-op warp over a source image of the given size.
    pub fn identity(width: usize, height: usize) -> Result<Self> {
        Self::generate_default(width, height)
    }

    /// Parse the full descriptor carried by `SoLd`/`SoLE`.
    pub fn from_placed_data(data: &PlacedLayerData) -> Result<Self> {
        Self::from_descriptor(&data.descriptor)
    }

    /// Parse a full `SoLd` descriptor, or a direct warp descriptor when the
    /// caller has already selected its affine placement.
    pub fn from_descriptor(descriptor: &Descriptor) -> Result<Self> {
        Self::from_descriptor_with_fallback(descriptor, None)
    }

    fn from_descriptor_with_fallback(
        descriptor: &Descriptor,
        fallback_transform: Option<[Point2; 4]>,
    ) -> Result<Self> {
        let (warp_descriptor, kind) =
            if let Some(quilt) = nested_descriptor(descriptor, "quiltWarp")? {
                (quilt, WarpKind::Quilt)
            } else if let Some(normal) = nested_descriptor(descriptor, "warp")? {
                (normal, WarpKind::Normal)
            } else {
                // PlLd stores the selected warp descriptor directly, unlike SoLd.
                let kind = if descriptor.class_id.as_str() == "quiltWarp" {
                    WarpKind::Quilt
                } else {
                    WarpKind::Normal
                };
                (descriptor, kind)
            };

        let bounds = parse_bounds(warp_descriptor)?;
        let affine_transform = descriptor
            .get("Trnf")
            .map(parse_transform_list)
            .transpose()?
            .or(fallback_transform)
            .unwrap_or_else(|| default_quad(bounds.width(), bounds.height()));
        let non_affine_transform = descriptor
            .get("nonAffineTransform")
            .map(parse_transform_list)
            .transpose()?
            .unwrap_or(affine_transform);

        let style =
            parse_enum(warp_descriptor, "warpStyle")?.unwrap_or_else(|| "warpCustom".to_owned());
        let warp_value = parse_optional_number(warp_descriptor, "warpValue")?.unwrap_or(0.0);
        let warp_perspective =
            parse_optional_number(warp_descriptor, "warpPerspective")?.unwrap_or(0.0);
        let warp_perspective_other =
            parse_optional_number(warp_descriptor, "warpPerspectiveOther")?.unwrap_or(0.0);
        let warp_rotate =
            parse_enum(warp_descriptor, "warpRotate")?.unwrap_or_else(|| "Hrzn".to_owned());
        let u_order = parse_optional_integer(warp_descriptor, "uOrder")?.unwrap_or(4);
        let v_order = parse_optional_integer(warp_descriptor, "vOrder")?.unwrap_or(4);

        let envelope = nested_descriptor(warp_descriptor, "customEnvelopeWarp")?;
        let custom_envelope = envelope.is_some();
        let mut grid_width = 4;
        let mut grid_height = 4;
        let control_points: Vec<Point2>;
        let mut quilt_slices_x = Vec::new();
        let mut quilt_slices_y = Vec::new();

        if let Some(envelope) = envelope {
            let mesh_points = envelope
                .get("meshPoints")
                .and_then(|value| match value {
                    DescriptorValue::ObjectArray(array) => Some(array),
                    _ => None,
                })
                .ok_or(PsdError::InvalidData {
                    offset: 0,
                    message: "warp custom envelope has no mesh point array",
                })?;
            let horizontal = parse_unit_floats(mesh_points.items.as_slice(), "Hrzn")?;
            let vertical = parse_unit_floats(mesh_points.items.as_slice(), "Vrtc")?;
            if horizontal.len() != vertical.len() || horizontal.is_empty() {
                return Err(PsdError::InvalidData {
                    offset: 0,
                    message: "warp horizontal and vertical control points differ in length",
                });
            }
            control_points = horizontal
                .into_iter()
                .zip(vertical)
                .map(|(x, y)| Point2::new(x, y))
                .collect();

            if kind == WarpKind::Quilt {
                grid_width = warp_descriptor
                    .get("deformNumCols")
                    .and_then(DescriptorValue::as_integer)
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or(PsdError::InvalidData {
                        offset: 0,
                        message: "quilt warp has invalid deformNumCols",
                    })?;
                grid_height = warp_descriptor
                    .get("deformNumRows")
                    .and_then(DescriptorValue::as_integer)
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or(PsdError::InvalidData {
                        offset: 0,
                        message: "quilt warp has invalid deformNumRows",
                    })?;
                quilt_slices_x = parse_nested_slices(envelope, "quiltSliceX")?;
                quilt_slices_y = parse_nested_slices(envelope, "quiltSliceY")?;
            } else if control_points.len() != 16 {
                return Err(PsdError::InvalidData {
                    offset: 0,
                    message: "normal warp requires sixteen Bézier control points",
                });
            }
        } else {
            // Upstream defaults a missing customEnvelopeWarp to a flat dummy
            // grid; this covers Photoshop's partial/default PlLd descriptors.
            if kind == WarpKind::Quilt {
                grid_width = warp_descriptor
                    .get("deformNumCols")
                    .and_then(DescriptorValue::as_integer)
                    .and_then(|value| usize::try_from(value).ok())
                    .unwrap_or(4);
                grid_height = warp_descriptor
                    .get("deformNumRows")
                    .and_then(DescriptorValue::as_integer)
                    .and_then(|value| usize::try_from(value).ok())
                    .unwrap_or(4);
            }
            control_points =
                default_control_points(bounds.width(), bounds.height(), grid_width, grid_height);
        }

        validate_grid(grid_width, grid_height, &control_points)?;
        Ok(Self {
            kind,
            style,
            bounds,
            control_points,
            grid_width,
            grid_height,
            custom_envelope,
            affine_transform,
            non_affine_transform,
            quilt_slices_x,
            quilt_slices_y,
            warp_value,
            warp_perspective,
            warp_perspective_other,
            warp_rotate,
            u_order,
            v_order,
        })
    }

    /// Parse the legacy PlLd warp descriptor using its four placed transform
    /// corners for the affine placement stage.
    pub fn from_placed_layer(placed: &PlacedLayer) -> Result<Self> {
        let transform = placed.transform;
        let affine = [
            point(transform.top_left.x, transform.top_left.y),
            point(transform.top_right.x, transform.top_right.y),
            point(transform.bottom_left.x, transform.bottom_left.y),
            point(transform.bottom_right.x, transform.bottom_right.y),
        ];
        Self::from_descriptor_with_fallback(&placed.warp, Some(affine))
    }

    /// Serialize the selected `warp` or `quiltWarp` descriptor.
    pub fn to_descriptor(&self) -> Result<Descriptor> {
        let name = match self.kind {
            WarpKind::Normal => "warp",
            WarpKind::Quilt => "quiltWarp",
        };
        let mut descriptor = empty_descriptor(name)?;
        self.update_warp_descriptor(&mut descriptor)?;
        Ok(descriptor)
    }

    /// Update a selected PlLd warp descriptor while preserving unrelated
    /// descriptor items and their original key encodings.
    pub fn update_warp_descriptor(&self, descriptor: &mut Descriptor) -> Result<()> {
        let name = match self.kind {
            WarpKind::Normal => "warp",
            WarpKind::Quilt => "quiltWarp",
        };
        descriptor.class_id = rekey_preserving_form(&descriptor.class_id, name)?;
        let (style_type, style_value) = match descriptor.get("warpStyle") {
            Some(DescriptorValue::Enumerated { type_id, value }) => (
                rekey_preserving_form(type_id, "warpStyle")?,
                rekey_preserving_form(value, &self.style)?,
            ),
            _ => (
                DescriptorKey::new("warpStyle"),
                DescriptorKey::new(self.style.clone()),
            ),
        };
        descriptor.insert(
            "warpStyle",
            DescriptorValue::Enumerated {
                type_id: style_type,
                value: style_value,
            },
        );
        descriptor.insert("warpValue", DescriptorValue::Double(self.warp_value));
        descriptor.insert(
            "warpPerspective",
            DescriptorValue::Double(self.warp_perspective),
        );
        descriptor.insert(
            "warpPerspectiveOther",
            DescriptorValue::Double(self.warp_perspective_other),
        );
        let (rotate_type, rotate_value) = match descriptor.get("warpRotate") {
            Some(DescriptorValue::Enumerated { type_id, value }) => (
                rekey_preserving_form(type_id, "Ornt")?,
                rotation_key(Some(value), &self.warp_rotate),
            ),
            // Upstream writes `Ornt` and `Hrzn`/`Vrtc` as zero-length char IDs.
            _ => (
                DescriptorKey::char_id(*b"Ornt"),
                rotation_key(None, &self.warp_rotate),
            ),
        };
        descriptor.insert(
            "warpRotate",
            DescriptorValue::Enumerated {
                type_id: rotate_type,
                value: rotate_value,
            },
        );
        self.update_bounds_descriptor(descriptor)?;
        descriptor.insert("uOrder", DescriptorValue::Integer(self.u_order));
        descriptor.insert("vOrder", DescriptorValue::Integer(self.v_order));

        if self.kind == WarpKind::Quilt {
            descriptor.insert(
                "deformNumRows",
                DescriptorValue::Integer(i32::try_from(self.grid_height).map_err(|_| {
                    PsdError::InvalidData {
                        offset: 0,
                        message: "quilt row count exceeds the descriptor integer limit",
                    }
                })?),
            );
            descriptor.insert(
                "deformNumCols",
                DescriptorValue::Integer(i32::try_from(self.grid_width).map_err(|_| {
                    PsdError::InvalidData {
                        offset: 0,
                        message: "quilt column count exceeds the descriptor integer limit",
                    }
                })?),
            );
        } else {
            remove_item(descriptor, "deformNumRows");
            remove_item(descriptor, "deformNumCols");
        }

        if self.custom_envelope || self.kind == WarpKind::Quilt {
            let mut envelope = match descriptor.get("customEnvelopeWarp") {
                Some(DescriptorValue::Descriptor(envelope)) => envelope.clone(),
                _ => empty_descriptor("customEnvelopeWarp")?,
            };
            self.update_mesh_points(&mut envelope)?;
            if self.kind == WarpKind::Quilt {
                self.update_quilt_slice(&mut envelope, "quiltSliceX", &self.quilt_slices_x)?;
                self.update_quilt_slice(&mut envelope, "quiltSliceY", &self.quilt_slices_y)?;
            } else {
                remove_item(&mut envelope, "quiltSliceX");
                remove_item(&mut envelope, "quiltSliceY");
            }
            descriptor.insert("customEnvelopeWarp", DescriptorValue::Descriptor(envelope));
        } else {
            remove_item(descriptor, "customEnvelopeWarp");
        }
        Ok(())
    }

    /// Apply this warp to the full SoLd descriptor. All unrelated placed-layer
    /// metadata and unknown descriptor items are retained.
    pub fn update_placed_layer_data(&self, data: &mut PlacedLayerData) -> Result<()> {
        let descriptor = &mut data.descriptor;
        descriptor.insert(
            "Trnf",
            DescriptorValue::List(transform_values(self.affine_transform)),
        );
        descriptor.insert(
            "nonAffineTransform",
            DescriptorValue::List(transform_values(self.non_affine_transform)),
        );

        let mut normal = nested_descriptor(descriptor, "warp")?
            .cloned()
            .unwrap_or(empty_descriptor("warp")?);
        if self.kind == WarpKind::Normal {
            self.update_warp_descriptor(&mut normal)?;
            descriptor.insert("warp", DescriptorValue::Descriptor(normal));
            remove_item(descriptor, "quiltWarp");
            return Ok(());
        }

        let mut no_warp = self.clone();
        no_warp.kind = WarpKind::Normal;
        no_warp.style = "warpNone".to_owned();
        no_warp.custom_envelope = false;
        no_warp.control_points.clear();
        no_warp.grid_width = 4;
        no_warp.grid_height = 4;
        no_warp.quilt_slices_x.clear();
        no_warp.quilt_slices_y.clear();
        no_warp.update_warp_descriptor(&mut normal)?;
        descriptor.insert("warp", DescriptorValue::Descriptor(normal));

        let mut quilt = nested_descriptor(descriptor, "quiltWarp")?
            .cloned()
            .unwrap_or(empty_descriptor("quiltWarp")?);
        self.update_warp_descriptor(&mut quilt)?;
        descriptor.insert("quiltWarp", DescriptorValue::Descriptor(quilt));
        Ok(())
    }

    /// Apply this warp to the legacy PlLd record, including its on-disk corner
    /// order and direct warp descriptor. PlLd has a single corner transform;
    /// reject a distinct non-affine stage rather than silently dropping it.
    pub fn update_placed_layer(&self, placed: &mut PlacedLayer) -> Result<()> {
        if !quads_approximately_equal(self.affine_transform, self.non_affine_transform, 1e-9) {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "legacy PlLd cannot represent a separate non-affine transform; use SoLd",
            });
        }
        let [top_left, top_right, bottom_left, bottom_right] = self.affine_transform;
        placed.transform = psd_core::Transform {
            top_left: psd_core::Point {
                x: top_left.x,
                y: top_left.y,
            },
            top_right: psd_core::Point {
                x: top_right.x,
                y: top_right.y,
            },
            bottom_right: psd_core::Point {
                x: bottom_right.x,
                y: bottom_right.y,
            },
            bottom_left: psd_core::Point {
                x: bottom_left.x,
                y: bottom_left.y,
            },
        };
        self.update_warp_descriptor(&mut placed.warp)
    }

    /// Persist this warp into the authoritative placed-layer payload of a
    /// layer. SoLd/SoLE is preferred because it stores affine and non-affine
    /// transforms separately; PlLd is updated only when no modern block exists.
    /// Untouched descriptor fields, tagged-block signatures, and trailing
    /// payload bytes remain intact. Layer pixel channels/bounds are not
    /// regenerated; render with [`crate::render_warped`] after the update.
    pub fn set_on_layer<T: BitDepth>(&self, layer: &mut Layer<T>) -> Result<()> {
        let mut blocks = layer.blocks.clone();
        self.write_to_blocks(&mut blocks)?;
        layer.blocks = blocks;
        Ok(())
    }

    /// Write this warp into a caller-owned staged block list. Callers should
    /// pass a clone when they need transactionality.
    pub(crate) fn write_to_blocks(&self, blocks: &mut AdditionalLayerInfo) -> Result<()> {
        let mut updated_modern = false;
        for key in [*b"SoLd", *b"SoLE"] {
            let key = TaggedBlockKey::new(key);
            if let Some(block) = blocks.get_mut(key) {
                block.data = serialize_placed_layer_data(&block.data, self)?;
                updated_modern = true;
            }
        }
        if !updated_modern {
            for key in [*b"PlLd", *b"plLd"] {
                let key = TaggedBlockKey::new(key);
                if let Some(block) = blocks.get_mut(key) {
                    block.data = serialize_placed_layer(&block.data, self)?;
                    updated_modern = true;
                }
            }
        }
        if !updated_modern {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "layer has no placed-layer warp block to update",
            });
        }
        Ok(())
    }

    fn update_bounds_descriptor(&self, warp_descriptor: &mut Descriptor) -> Result<()> {
        let mut bounds = match warp_descriptor.get("bounds") {
            Some(DescriptorValue::Descriptor(bounds)) => bounds.clone(),
            _ => empty_descriptor("classFloatRect")?,
        };
        bounds.class_id = rekey_preserving_form(&bounds.class_id, "classFloatRect")?;
        bounds.insert("Top ", DescriptorValue::Double(self.bounds.min.y));
        bounds.insert("Left", DescriptorValue::Double(self.bounds.min.x));
        bounds.insert("Btom", DescriptorValue::Double(self.bounds.max.y));
        bounds.insert("Rght", DescriptorValue::Double(self.bounds.max.x));
        warp_descriptor.insert("bounds", DescriptorValue::Descriptor(bounds));
        Ok(())
    }

    fn update_mesh_points(&self, envelope: &mut Descriptor) -> Result<()> {
        let mut mesh = match envelope.get("meshPoints") {
            Some(DescriptorValue::ObjectArray(mesh)) => mesh.clone(),
            _ => ObjectArray {
                items_count: 0,
                name: UnicodeString::new("\0", 2)?,
                class_id: DescriptorKey::new("rationalPoint"),
                items: Vec::new(),
            },
        };
        mesh.items_count =
            u32::try_from(self.control_points.len()).map_err(|_| PsdError::LengthOverflow {
                actual: self.control_points.len() as u64,
                width: 4,
            })?;
        mesh.class_id = rekey_preserving_form(&mesh.class_id, "rationalPoint")?;
        upsert_item(
            &mut mesh.items,
            "Hrzn",
            DescriptorValue::UnitFloats {
                unit: *b"#Pxl",
                values: self.control_points.iter().map(|point| point.x).collect(),
            },
        );
        upsert_item(
            &mut mesh.items,
            "Vrtc",
            DescriptorValue::UnitFloats {
                unit: *b"#Pxl",
                values: self.control_points.iter().map(|point| point.y).collect(),
            },
        );
        envelope.insert("meshPoints", DescriptorValue::ObjectArray(mesh));
        Ok(())
    }

    fn update_quilt_slice(
        &self,
        envelope: &mut Descriptor,
        key: &str,
        values: &[f64],
    ) -> Result<()> {
        if !valid_slices(values) {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "cannot serialize invalid quilt slice positions",
            });
        }
        let mut array = match envelope.get(key) {
            Some(DescriptorValue::ObjectArray(array)) => array.clone(),
            _ => ObjectArray {
                items_count: 0,
                name: UnicodeString::new("\0", 2)?,
                class_id: DescriptorKey::new("UntF"),
                items: Vec::new(),
            },
        };
        array.items_count = u32::try_from(values.len()).map_err(|_| PsdError::LengthOverflow {
            actual: values.len() as u64,
            width: 4,
        })?;
        array.class_id = rekey_preserving_form(&array.class_id, "UntF")?;
        upsert_item(
            &mut array.items,
            key,
            DescriptorValue::UnitFloats {
                unit: *b"#Pxl",
                values: values.to_vec(),
            },
        );
        envelope.insert(key.to_owned(), DescriptorValue::ObjectArray(array));
        Ok(())
    }

    /// Warp metadata exposed without changing or regenerating its raw block.
    pub fn kind(&self) -> WarpKind {
        self.kind
    }

    pub fn style(&self) -> &str {
        &self.style
    }

    /// Original source-space bounds from the descriptor.
    pub fn source_bounds(&self) -> BoundingBox {
        self.bounds
    }

    pub fn control_points(&self) -> &[Point2] {
        &self.control_points
    }

    /// Control-point slice in scanline order.
    pub fn points(&self) -> &[Point2] {
        &self.control_points
    }

    pub fn grid_dimensions(&self) -> (usize, usize) {
        (self.grid_width, self.grid_height)
    }

    pub fn u_dimensions(&self) -> usize {
        self.grid_width
    }

    pub fn v_dimensions(&self) -> usize {
        self.grid_height
    }

    /// Whether the descriptor carried explicit Bézier control points rather
    /// than relying on Photoshop's default flat mesh.
    pub fn has_custom_envelope(&self) -> bool {
        self.custom_envelope
    }

    pub fn affine_transform(&self) -> [Point2; 4] {
        self.affine_transform
    }

    pub fn non_affine_transform(&self) -> [Point2; 4] {
        self.non_affine_transform
    }

    pub fn quilt_slices(&self) -> (&[f64], &[f64]) {
        (&self.quilt_slices_x, &self.quilt_slices_y)
    }

    pub fn warp_value(&self) -> f64 {
        self.warp_value
    }

    pub fn perspective(&self) -> (f64, f64) {
        (self.warp_perspective, self.warp_perspective_other)
    }

    pub fn warp_rotate(&self) -> &str {
        &self.warp_rotate
    }

    pub fn order(&self) -> (i32, i32) {
        (self.u_order, self.v_order)
    }

    pub fn set_style(&mut self, style: impl Into<String>) -> Result<()> {
        let style = style.into();
        if style != "warpCustom" && style != "warpNone" {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "warp style must be warpCustom or warpNone",
            });
        }
        self.style = style;
        Ok(())
    }

    pub fn set_warp_value(&mut self, value: f64) -> Result<()> {
        if !value.is_finite() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "warp value must be finite",
            });
        }
        self.warp_value = value;
        Ok(())
    }

    pub fn set_perspective(&mut self, value: f64, other: f64) -> Result<()> {
        if !value.is_finite() || !other.is_finite() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "warp perspective values must be finite",
            });
        }
        self.warp_perspective = value;
        self.warp_perspective_other = other;
        Ok(())
    }

    /// Set the warp orientation: the char IDs `Hrzn`/`Vrtc`, or the string
    /// IDs `horizontal`/`vertical` that newer Photoshop versions can write in
    /// their place.
    pub fn set_warp_rotate(&mut self, rotate: impl Into<String>) -> Result<()> {
        let rotate = rotate.into();
        if !matches!(rotate.as_str(), "Hrzn" | "Vrtc" | "horizontal" | "vertical") {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "warp rotation must be Hrzn, Vrtc, horizontal, or vertical",
            });
        }
        self.warp_rotate = rotate;
        Ok(())
    }

    /// Whether the warp is oriented vertically, in either spelling of the
    /// rotation (`Vrtc` or `vertical`).
    pub fn is_vertical(&self) -> bool {
        matches!(self.warp_rotate.as_str(), "Vrtc" | "vertical")
    }

    pub fn set_source_bounds(&mut self, bounds: BoundingBox) -> Result<()> {
        if !bounds.min.is_finite()
            || !bounds.max.is_finite()
            || bounds.width() <= 0.0
            || bounds.height() <= 0.0
        {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "warp source bounds must be finite and have positive area",
            });
        }
        self.bounds = bounds;
        if !self.custom_envelope {
            self.control_points = default_control_points(
                bounds.width(),
                bounds.height(),
                self.grid_width,
                self.grid_height,
            );
        }
        Ok(())
    }

    /// Rebase source-space geometry after replacing a linked image.
    ///
    /// Upstream `SmartObjectLayer::replace` scales the control net but leaves
    /// its stored warp bounds unchanged. Updating both here makes a repeated
    /// replacement at the same new dimensions idempotent instead of scaling
    /// the points against stale source bounds.
    pub fn rescale_source(&mut self, width: usize, height: usize) -> Result<()> {
        if width == 0 || height == 0 {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "replacement source dimensions must be non-zero",
            });
        }
        let old = self.bounds;
        if old.width() <= 0.0 || old.height() <= 0.0 {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "warp source bounds must have positive area before replacement",
            });
        }
        let new_width = width as f64;
        let new_height = height as f64;
        let scale_x = new_width / old.width();
        let scale_y = new_height / old.height();
        let rebase_x = |x: f64| old.min.x + (x - old.min.x) * scale_x;
        let rebase_y = |y: f64| old.min.y + (y - old.min.y) * scale_y;
        let new_bounds = BoundingBox::new(
            old.min,
            Point2::new(old.min.x + new_width, old.min.y + new_height),
        );
        let points = if self.custom_envelope {
            Some(
                self.control_points
                    .iter()
                    .map(|point| Point2::new(rebase_x(point.x), rebase_y(point.y)))
                    .collect(),
            )
        } else {
            None
        };
        let slices = if self.kind == WarpKind::Quilt {
            Some((
                self.quilt_slices_x
                    .iter()
                    .map(|&slice| rebase_x(slice))
                    .collect(),
                self.quilt_slices_y
                    .iter()
                    .map(|&slice| rebase_y(slice))
                    .collect(),
            ))
        } else {
            None
        };

        self.set_source_bounds(new_bounds)?;
        if let Some(points) = points {
            self.set_control_points(points)?;
        }
        if let Some((slices_x, slices_y)) = slices {
            self.set_quilt_slices(slices_x, slices_y)?;
        }
        Ok(())
    }

    pub fn point(&self, u: usize, v: usize) -> Option<Point2> {
        if u >= self.grid_width || v >= self.grid_height {
            None
        } else {
            self.control_points.get(v * self.grid_width + u).copied()
        }
    }

    pub fn set_point(&mut self, u: usize, v: usize, point: Point2) -> Result<()> {
        if u >= self.grid_width || v >= self.grid_height || !point.is_finite() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "warp point index or coordinate is invalid",
            });
        }
        self.control_points[v * self.grid_width + u] = point;
        self.custom_envelope = true;
        Ok(())
    }

    /// Replace the control net without changing its dimensions or source bounds.
    pub fn set_control_points(&mut self, points: Vec<Point2>) -> Result<()> {
        validate_grid(self.grid_width, self.grid_height, &points)?;
        self.control_points = points;
        self.custom_envelope = true;
        Ok(())
    }

    pub fn set_affine_transform(&mut self, transform: [Point2; 4]) -> Result<()> {
        if !transform.iter().all(|point| point.is_finite()) {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "affine warp transform must contain finite coordinates",
            });
        }
        let expected_bottom_right = transform[1] + transform[2] - transform[0];
        let scale = transform
            .iter()
            .map(|point| point.x.abs().max(point.y.abs()))
            .fold(1.0, f64::max);
        if !expected_bottom_right.approx_eq(transform[3], scale * 1e-6) {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "affine warp corners must form a parallelogram",
            });
        }
        self.affine_transform = transform;
        Ok(())
    }

    pub fn set_non_affine_transform(&mut self, transform: [Point2; 4]) -> Result<()> {
        if !transform.iter().all(|point| point.is_finite()) {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "non-affine warp transform must contain finite coordinates",
            });
        }
        self.non_affine_transform = transform;
        Ok(())
    }

    pub fn set_quilt_slices(&mut self, x: Vec<f64>, y: Vec<f64>) -> Result<()> {
        if self.kind != WarpKind::Quilt || !valid_slices(&x) || !valid_slices(&y) {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "quilt warp requires finite, strictly increasing slices on both axes",
            });
        }
        self.quilt_slices_x = x;
        self.quilt_slices_y = y;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Placement transforms (upstream `SmartObjectLayer::move`/`rotate`/
    // `scale`/`transform`/`reset_transform`/`reset_warp`). They edit the
    // affine and non-affine corner quads; the Bézier net stays in source
    // space.
    // ------------------------------------------------------------------

    /// Move the placement by `offset` canvas pixels.
    pub fn translate(&mut self, offset: Point2) -> Result<()> {
        let homography = Homography::translation(offset)?;
        self.apply_placement(&homography, &homography)
    }

    /// Rotate the placement by `degrees` around `center`. Positive angles
    /// turn clockwise on the y-down canvas.
    ///
    /// Upstream documents degrees but feeds the value to `cos`/`sin` as
    /// radians, so `rotate(45)` turns by ~58°; the port converts.
    pub fn rotate(&mut self, degrees: f64, center: Point2) -> Result<()> {
        let homography = Homography::rotation(degrees.to_radians(), center)?;
        self.apply_placement(&homography, &homography)
    }

    /// Scale the placement by `factors` around `center`.
    pub fn scale(&mut self, factors: Point2, center: Point2) -> Result<()> {
        let homography = Homography::scale(factors, center)?;
        self.apply_placement(&homography, &homography)
    }

    /// Apply a 3×3 transform: its affine part to the affine quad (which must
    /// stay a parallelogram) and the whole matrix to the non-affine quad,
    /// exactly as upstream splits it.
    pub fn transform(&mut self, homography: &Homography) -> Result<()> {
        let mut affine = *homography.matrix();
        affine[(2, 0)] = 0.0;
        affine[(2, 1)] = 0.0;
        let scale = affine[(2, 2)];
        if scale == 0.0 || !scale.is_finite() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "transform matrix has a degenerate homogeneous scale",
            });
        }
        affine /= scale;
        let affine = Homography::from_matrix(affine)?;
        self.apply_placement(&affine, homography)
    }

    /// Map the placement back onto the source rectangle, keeping the Bézier
    /// net (upstream `reset_transform`).
    ///
    /// Upstream applies a homography that maps the non-affine quad onto the
    /// source rectangle, which leaves the affine quad skewed whenever the
    /// placement had perspective; both quads are reset exactly here.
    pub fn reset_transform(&mut self) -> Result<()> {
        let quad = bounds_quad(self.bounds);
        self.affine_transform = quad;
        self.non_affine_transform = quad;
        Ok(())
    }

    /// Replace the control net with a flat 4×4 warp over the source bounds,
    /// keeping the placement (upstream `reset_warp`, which also switches
    /// quilt warps back to a normal warp).
    pub fn reset_warp(&mut self) -> Result<()> {
        let (affine, non_affine) = (self.affine_transform, self.non_affine_transform);
        let bounds = self.bounds;
        let mut reset = Self::generate_default_with_grid(1, 1, 4, 4)?;
        reset.set_source_bounds(bounds)?;
        reset.control_points = default_control_points(bounds.width(), bounds.height(), 4, 4)
            .into_iter()
            .map(|point| point + bounds.min)
            .collect();
        reset.affine_transform = affine;
        reset.non_affine_transform = non_affine;
        *self = reset;
        Ok(())
    }

    /// Scale around the center of the warped bounds so they get the given
    /// width and/or height (upstream's smart-object `width`/`height`
    /// setters, which measure the evaluated mesh).
    pub fn resize(&mut self, width: Option<f64>, height: Option<f64>) -> Result<()> {
        let bounds = self.bounds()?;
        let factor = |target: Option<f64>, current: f64| match target {
            None => Ok(1.0),
            Some(target) if target.is_finite() && target > 0.0 && current > 0.0 => {
                Ok(target / current)
            }
            Some(_) => Err(PsdError::InvalidData {
                offset: 0,
                message: "warp size must be finite and positive",
            }),
        };
        let factors = Point2::new(
            factor(width, bounds.width())?,
            factor(height, bounds.height())?,
        );
        self.scale(factors, bounds.center())
    }

    /// Translate so the center of the warped bounds lands on `x` and/or `y`
    /// (upstream's smart-object `center_x`/`center_y` setters).
    pub fn recenter(&mut self, x: Option<f64>, y: Option<f64>) -> Result<()> {
        let center = self.bounds()?.center();
        self.translate(Point2::new(
            x.map_or(0.0, |x| x - center.x),
            y.map_or(0.0, |y| y - center.y),
        ))
    }

    fn apply_placement(&mut self, affine: &Homography, non_affine: &Homography) -> Result<()> {
        let mut affine_quad = self.affine_transform;
        let mut non_affine_quad = self.non_affine_transform;
        affine.transform_points(&mut affine_quad)?;
        non_affine.transform_points(&mut non_affine_quad)?;
        self.set_affine_transform(affine_quad)?;
        self.set_non_affine_transform(non_affine_quad)
    }

    pub fn no_op(&self) -> bool {
        let transforms_match = self
            .affine_transform
            .iter()
            .zip(self.non_affine_transform)
            .all(|(left, right)| {
                (left.x - right.x).abs() < 1e-9 && (left.y - right.y).abs() < 1e-9
            });
        if !transforms_match {
            return false;
        }
        if self.kind == WarpKind::Quilt
            && (!flat_quilt_slices(
                &self.quilt_slices_x,
                self.bounds.min.x,
                self.bounds.width(),
                self.grid_width,
            ) || !flat_quilt_slices(
                &self.quilt_slices_y,
                self.bounds.min.y,
                self.bounds.height(),
                self.grid_height,
            ))
        {
            return false;
        }

        // Upstream only compares the affine/non-affine corner quads, which
        // reports true for a curved custom mesh. Check the Bézier net too so
        // callers cannot skip a real deformation.
        let Some(bounds) = BoundingBox::from_points(&self.control_points) else {
            return false;
        };
        self.control_points
            .iter()
            .enumerate()
            .all(|(index, point)| {
                let x = index % self.grid_width;
                let y = index / self.grid_width;
                let expected = Point2::new(
                    bounds.min.x + bounds.width() * x as f64 / (self.grid_width - 1) as f64,
                    bounds.min.y + bounds.height() * y as f64 / (self.grid_height - 1) as f64,
                );
                (point.x - expected.x).abs() < 1e-9 && (point.y - expected.y).abs() < 1e-9
            })
    }

    pub fn approximately_eq(&self, other: &Self, epsilon: f64) -> bool {
        self.kind == other.kind
            && self.grid_dimensions() == other.grid_dimensions()
            && self.control_points.len() == other.control_points.len()
            && self
                .control_points
                .iter()
                .zip(&other.control_points)
                .all(|(left, right)| left.approx_eq(*right, epsilon))
            && self
                .affine_transform
                .iter()
                .zip(other.affine_transform)
                .all(|(left, right)| left.approx_eq(right, epsilon))
            && self
                .non_affine_transform
                .iter()
                .zip(other.non_affine_transform)
                .all(|(left, right)| left.approx_eq(right, epsilon))
            && self.quilt_slices_x.len() == other.quilt_slices_x.len()
            && self
                .quilt_slices_x
                .iter()
                .zip(&other.quilt_slices_x)
                .all(|(left, right)| (left - right).abs() <= epsilon)
            && self.quilt_slices_y.len() == other.quilt_slices_y.len()
            && self
                .quilt_slices_y
                .iter()
                .zip(&other.quilt_slices_y)
                .all(|(left, right)| (left - right).abs() <= epsilon)
    }

    /// Build the transformed cubic surface in document coordinates.
    pub fn surface(&self) -> Result<BezierSurface> {
        let points = self.transformed_control_points()?;

        if self.kind == WarpKind::Quilt
            && !self.quilt_slices_x.is_empty()
            && !self.quilt_slices_y.is_empty()
        {
            BezierSurface::with_quilt_slices(
                &points,
                self.grid_width,
                self.grid_height,
                &self.quilt_slices_x,
                &self.quilt_slices_y,
            )
        } else {
            BezierSurface::new(&points, self.grid_width, self.grid_height)
        }
    }

    fn transformed_control_points(&self) -> Result<Vec<Point2>> {
        let mut points = self.control_points.clone();
        let source_bounds = BoundingBox::from_points(&points).ok_or(PsdError::InvalidData {
            offset: 0,
            message: "warp has no finite control points",
        })?;
        let affine = Homography::between(bounds_quad(source_bounds), self.affine_transform)?;
        affine.transform_points(&mut points)?;
        let non_affine = Homography::between(self.affine_transform, self.non_affine_transform)?;
        non_affine.transform_points(&mut points)?;
        Ok(points)
    }

    /// Build a mesh directly from the transformed Bézier control net.
    pub fn mesh(&self) -> Result<QuadMesh> {
        QuadMesh::from_points(
            &self.transformed_control_points()?,
            self.grid_width,
            self.grid_height,
        )
    }

    /// Tessellate this warp for querying and image resampling.
    pub fn tessellated_mesh(&self, divisions_x: usize, divisions_y: usize) -> Result<QuadMesh> {
        self.surface()?.mesh(divisions_x, divisions_y)
    }

    /// Apply the warp to caller-decoded planar channels using the standard
    /// supersampled renderer.
    pub fn apply<T: BitDepth>(
        &self,
        source: &crate::render::Raster<T>,
        options: crate::render::WarpRenderOptions,
    ) -> Result<crate::render::Raster<T>> {
        crate::render::render_warped(self, source, options)
    }

    /// Tight sampled bounds of the transformed Bézier surface.
    pub fn bounds(&self) -> Result<BoundingBox> {
        self.mesh_bounds(true)
    }

    /// Bounds of the transformed control net (`false`) or a tessellated
    /// Bézier surface (`true`), matching upstream `Warp::bounds` semantics.
    pub fn mesh_bounds(&self, consider_bezier: bool) -> Result<BoundingBox> {
        if consider_bezier {
            Ok(self.surface()?.mesh(25, 25)?.bounds())
        } else {
            BoundingBox::from_points(&self.transformed_control_points()?).ok_or(
                PsdError::InvalidData {
                    offset: 0,
                    message: "warp has no finite transformed control points",
                },
            )
        }
    }

    pub fn control_bounds(&self) -> Result<BoundingBox> {
        self.mesh_bounds(false)
    }
}

impl<T: BitDepth> Layer<T> {
    /// Parse this layer's smart-object warp on demand. SoLd/SoLE is preferred;
    /// PlLd remains a fallback for older files.
    pub fn warp(&self) -> Result<Option<Warp>> {
        if let Some(data) = self.smart_object_data()? {
            let fallback = self
                .placed_layer()?
                .map(|placed| placed_transform_quad(placed.transform));
            return Warp::from_descriptor_with_fallback(&data.descriptor, fallback).map(Some);
        }
        self.placed_layer()?
            .map(|placed| Warp::from_placed_layer(&placed))
            .transpose()
    }

    /// Replace typed warp metadata and persist it into the authoritative
    /// SoLd/SoLE block, or PlLd when no modern descriptor exists. Legacy PlLd
    /// cannot store a distinct non-affine transform. Preview channels and
    /// bounds remain unchanged.
    pub fn set_warp(&mut self, warp: &Warp) -> Result<()> {
        warp.set_on_layer(self)
    }

    /// Persist a warp and install its rendered channels/bounds using an
    /// already-decoded source raster. This is transactional: the layer is
    /// replaced only if both descriptor update and rendering succeed. Linked
    /// source decoding and document-level link mutation are provided by
    /// `smart_object`. Layers with masks, target channels omitted from the
    /// source, or non-alpha source channels absent from the layer are rejected
    /// rather than silently losing or adding channel planes.
    pub fn set_warp_and_render(
        &mut self,
        warp: &Warp,
        source: &crate::render::Raster<T>,
        options: crate::render::WarpRenderOptions,
    ) -> Result<()> {
        let image = match &self.kind {
            crate::layer::LayerKind::Image(image) => image,
            _ => {
                return Err(PsdError::InvalidData {
                    offset: 0,
                    message: "warp rendering requires an image-kind layer",
                });
            }
        };
        if self.mask.is_some()
            || image
                .channels
                .keys()
                .any(crate::channels::ChannelKey::is_mask)
        {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "warp preview rendering does not support layer mask data",
            });
        }
        if source
            .channels()
            .keys()
            .any(|key| key != crate::channels::ChannelKey::ALPHA && !image.channels.contains(key))
        {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "warp source contains a channel absent from the target layer",
            });
        }
        if image.channels.contains(crate::channels::ChannelKey::ALPHA)
            && !source
                .channels()
                .contains(crate::channels::ChannelKey::ALPHA)
        {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "warp source omits the target layer's alpha channel",
            });
        }
        let rendered = warp.apply(source, options)?;
        if image
            .channels
            .keys()
            .any(|key| !rendered.channels().contains(key))
        {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "warp render would discard an existing layer channel",
            });
        }
        self.install_rendered_warp(warp, rendered, Vec::new())
    }

    /// Prepare preview, bounds, and warp-block changes without touching the
    /// target layer. The replacement flow adds linked UUID metadata to this
    /// staged block list before committing all three values together.
    pub(crate) fn prepare_rendered_warp(
        &self,
        warp: &Warp,
        rendered: crate::render::Raster<T>,
        preserved_masks: Vec<(crate::channels::ChannelKey, Vec<T>)>,
    ) -> Result<(
        AdditionalLayerInfo,
        crate::channels::ChannelStore<T>,
        crate::layer::Rect,
    )> {
        if self.image().is_none() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "warp rendering requires an image-kind layer",
            });
        }
        let mut blocks = self.blocks.clone();
        warp.write_to_blocks(&mut blocks)?;

        let (left, top) = rendered.origin();
        let right = left
            .checked_add(
                i32::try_from(rendered.width()).map_err(|_| PsdError::InvalidData {
                    offset: 0,
                    message: "rendered layer width does not fit document coordinates",
                })?,
            )
            .ok_or(PsdError::InvalidData {
                offset: 0,
                message: "rendered layer right edge overflows document coordinates",
            })?;
        let bottom = top
            .checked_add(
                i32::try_from(rendered.height()).map_err(|_| PsdError::InvalidData {
                    offset: 0,
                    message: "rendered layer height does not fit document coordinates",
                })?,
            )
            .ok_or(PsdError::InvalidData {
                offset: 0,
                message: "rendered layer bottom edge overflows document coordinates",
            })?;
        let mut channels = rendered.into_channels();
        for (key, samples) in preserved_masks {
            channels.insert(key, samples);
        }
        Ok((
            blocks,
            channels,
            crate::layer::Rect::new(top, left, bottom, right),
        ))
    }

    /// Install a previously rendered source preview, updating the authoritative
    /// descriptor and output bounds in one transaction. The document-level
    /// smart-object replacement path uses this after preserving mask planes.
    pub(crate) fn install_rendered_warp(
        &mut self,
        warp: &Warp,
        rendered: crate::render::Raster<T>,
        preserved_masks: Vec<(crate::channels::ChannelKey, Vec<T>)>,
    ) -> Result<()> {
        let (blocks, channels, bounds) =
            self.prepare_rendered_warp(warp, rendered, preserved_masks)?;
        self.blocks = blocks;
        self.bounds = bounds;
        self.image_mut()
            .expect("image kind was validated before staging")
            .channels = channels;
        Ok(())
    }
}

fn serialize_placed_layer_data(bytes: &[u8], warp: &Warp) -> Result<Vec<u8>> {
    let (mut data, trailing) = PlacedLayerData::read_with_trailing(&mut BeReader::new(bytes))?;
    warp.update_placed_layer_data(&mut data)?;
    let mut writer = BeWriter::new();
    data.write(&mut writer)?;
    writer.bytes(&trailing);
    Ok(writer.into_inner())
}

fn serialize_placed_layer(bytes: &[u8], warp: &Warp) -> Result<Vec<u8>> {
    let (mut placed, trailing) = PlacedLayer::read_with_trailing(&mut BeReader::new(bytes))?;
    warp.update_placed_layer(&mut placed)?;
    let mut writer = BeWriter::new();
    placed.write(&mut writer)?;
    writer.bytes(&trailing);
    Ok(writer.into_inner())
}

fn empty_descriptor(class_id: &str) -> Result<Descriptor> {
    Ok(Descriptor {
        // The NUL unit represents the empty descriptor name while preserving
        // Photoshop's null-terminated UTF-16 spelling for newly built trees.
        name: UnicodeString::new("\0", 2)?,
        class_id: DescriptorKey::new(class_id),
        items: Vec::new(),
    })
}

fn remove_item(descriptor: &mut Descriptor, key: &str) {
    descriptor
        .items
        .retain(|item| item.key.as_bytes() != key.as_bytes());
}

/// A `warpRotate` value: an unchanged value keeps its original encoding, and
/// a new char ID (`Hrzn`/`Vrtc`) is always written with the zero-length
/// encoding. Keeping the encoding of a replaced long-form value
/// (`horizontal`) would instead write `Vrtc` as an explicit-length string
/// ID, which Photoshop does not define.
fn rotation_key(existing: Option<&DescriptorKey>, rotate: &str) -> DescriptorKey {
    match existing {
        Some(existing) if existing.as_bytes() == rotate.as_bytes() => existing.clone(),
        _ => match <[u8; 4]>::try_from(rotate.as_bytes()) {
            Ok(code) => DescriptorKey::char_id(code),
            Err(_) => DescriptorKey::new(rotate),
        },
    }
}

fn rekey_preserving_form(existing: &DescriptorKey, text: &str) -> Result<DescriptorKey> {
    if existing.as_bytes() == text.as_bytes() {
        Ok(existing.clone())
    } else if existing.uses_implicit_length() && text.len() == 4 {
        existing.replace_text_preserving_encoding(text)
    } else {
        Ok(DescriptorKey::new(text))
    }
}

fn upsert_item(items: &mut Vec<DescriptorItem>, key: &str, value: DescriptorValue) {
    if let Some(item) = items
        .iter_mut()
        .find(|item| item.key.as_bytes() == key.as_bytes())
    {
        item.value = value;
    } else {
        items.push(DescriptorItem {
            key: DescriptorKey::new(key),
            value,
        });
    }
}

fn transform_values(transform: [Point2; 4]) -> Vec<DescriptorValue> {
    [transform[0], transform[1], transform[3], transform[2]]
        .into_iter()
        .flat_map(|point| {
            [
                DescriptorValue::Double(point.x),
                DescriptorValue::Double(point.y),
            ]
        })
        .collect()
}

fn quads_approximately_equal(left: [Point2; 4], right: [Point2; 4], epsilon: f64) -> bool {
    left.iter()
        .zip(right)
        .all(|(left, right)| left.approx_eq(right, epsilon))
}

fn placed_transform_quad(transform: psd_core::Transform) -> [Point2; 4] {
    [
        point(transform.top_left.x, transform.top_left.y),
        point(transform.top_right.x, transform.top_right.y),
        point(transform.bottom_left.x, transform.bottom_left.y),
        point(transform.bottom_right.x, transform.bottom_right.y),
    ]
}

fn point(x: f64, y: f64) -> Point2 {
    Point2::new(x, y)
}

fn default_quad(width: f64, height: f64) -> [Point2; 4] {
    [
        Point2::ZERO,
        Point2::new(width, 0.0),
        Point2::new(0.0, height),
        Point2::new(width, height),
    ]
}

fn bounds_quad(bounds: BoundingBox) -> [Point2; 4] {
    [
        bounds.min,
        Point2::new(bounds.max.x, bounds.min.y),
        Point2::new(bounds.min.x, bounds.max.y),
        bounds.max,
    ]
}

fn default_control_points(
    width: f64,
    height: f64,
    grid_width: usize,
    grid_height: usize,
) -> Vec<Point2> {
    (0..grid_height)
        .flat_map(|y| {
            (0..grid_width).map(move |x| {
                Point2::new(
                    width * x as f64 / (grid_width - 1) as f64,
                    height * y as f64 / (grid_height - 1) as f64,
                )
            })
        })
        .collect()
}

fn default_quilt_slices(extent: f64, grid_dimensions: usize) -> Vec<f64> {
    let patches = 1 + (grid_dimensions - 4) / 3;
    let count = patches + 2;
    let increment = extent / (count - 1) as f64;
    let mut slices = Vec::with_capacity(count);
    slices.push(-0.6);
    for index in 1..count - 1 {
        slices.push(increment * index as f64);
    }
    slices.push(extent + 0.6);
    slices
}

fn valid_slices(slices: &[f64]) -> bool {
    slices.len() >= 2
        && slices.iter().all(|value| value.is_finite())
        && slices[0] < *slices.last().expect("two values checked")
        && slices.windows(2).all(|pair| pair[0] < pair[1])
}

fn validate_grid(width: usize, height: usize, points: &[Point2]) -> Result<()> {
    validate_grid_dimensions(width, height)?;
    if width.checked_mul(height) != Some(points.len())
        || !points.iter().all(|point| point.is_finite())
    {
        return Err(PsdError::InvalidData {
            offset: 0,
            message: "warp control points do not form a valid cubic grid",
        });
    }
    Ok(())
}

fn validate_grid_dimensions(width: usize, height: usize) -> Result<()> {
    if width < 4 || height < 4 || !(width - 4).is_multiple_of(3) || !(height - 4).is_multiple_of(3)
    {
        return Err(PsdError::InvalidData {
            offset: 0,
            message: "warp grid dimensions must be at least four and follow 4 + 3n",
        });
    }
    Ok(())
}

/// Whether quilt slices leave the patch spacing undeformed: evenly spaced, or
/// Photoshop's default layout for a flat quilt (evenly spaced interior slices
/// with the `-0.6` / `extent + 0.6` end pads that [`default_quilt_slices`]
/// generates). Treating only exact uniform spacing as flat reported every
/// freshly generated quilt warp as a deformation.
fn flat_quilt_slices(slices: &[f64], origin: f64, extent: f64, grid_dimensions: usize) -> bool {
    if slices_are_uniform(slices) {
        return true;
    }
    let defaults = default_quilt_slices(extent, grid_dimensions);
    let tolerance = extent.abs().max(1.0) * 1e-9;
    slices.len() == defaults.len()
        && slices
            .iter()
            .zip(defaults)
            .all(|(slice, default)| (slice - (default + origin)).abs() <= tolerance)
}

fn slices_are_uniform(slices: &[f64]) -> bool {
    if slices.len() <= 2 {
        return true;
    }
    let first = slices[0];
    let span = slices[slices.len() - 1] - first;
    span > 0.0
        && slices.iter().enumerate().all(|(index, value)| {
            let expected = first + span * index as f64 / (slices.len() - 1) as f64;
            (value - expected).abs() <= span.abs() * 1e-9
        })
}

fn parse_bounds(descriptor: &Descriptor) -> Result<BoundingBox> {
    let bounds = descriptor
        .get("bounds")
        .and_then(DescriptorValue::as_descriptor)
        .ok_or(PsdError::InvalidData {
            offset: 0,
            message: "warp descriptor is missing bounds",
        })?;
    let top = parse_number(bounds, "Top ")?;
    let left = parse_number(bounds, "Left")?;
    let bottom = parse_number(bounds, "Btom")?;
    let right = parse_number(bounds, "Rght")?;
    let result = BoundingBox::new(Point2::new(left, top), Point2::new(right, bottom));
    if !result.min.is_finite()
        || !result.max.is_finite()
        || result.width() <= 0.0
        || result.height() <= 0.0
    {
        return Err(PsdError::InvalidData {
            offset: 0,
            message: "warp bounds are degenerate or non-finite",
        });
    }
    Ok(result)
}

fn parse_number(descriptor: &Descriptor, key: &str) -> Result<f64> {
    let value = descriptor.get(key).ok_or(PsdError::InvalidData {
        offset: 0,
        message: "warp descriptor is missing a numeric field",
    })?;
    let number = match value {
        DescriptorValue::Double(value) => *value,
        DescriptorValue::UnitFloat {
            unit: [b'#', b'P', b'x', b'l'],
            value,
        } => *value,
        _ => {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "warp bounds must be doubles or pixel unit floats",
            })
        }
    };
    if number.is_finite() {
        Ok(number)
    } else {
        Err(PsdError::InvalidData {
            offset: 0,
            message: "warp descriptor contains a non-finite number",
        })
    }
}

fn parse_optional_number(descriptor: &Descriptor, key: &str) -> Result<Option<f64>> {
    descriptor
        .get(key)
        .map(|_| parse_number(descriptor, key))
        .transpose()
}

fn nested_descriptor<'a>(descriptor: &'a Descriptor, key: &str) -> Result<Option<&'a Descriptor>> {
    match descriptor.get(key) {
        None => Ok(None),
        Some(DescriptorValue::Descriptor(nested)) => Ok(Some(nested)),
        Some(_) => Err(PsdError::InvalidData {
            offset: 0,
            message: "warp descriptor child has an invalid type",
        }),
    }
}

fn parse_enum(descriptor: &Descriptor, key: &str) -> Result<Option<String>> {
    match descriptor.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_enum()
            .map(|(_, value)| Some(value.as_str().into_owned()))
            .ok_or(PsdError::InvalidData {
                offset: 0,
                message: "warp enumerated field has an invalid type",
            }),
    }
}

fn parse_optional_integer(descriptor: &Descriptor, key: &str) -> Result<Option<i32>> {
    match descriptor.get(key) {
        None => Ok(None),
        Some(value) => value.as_integer().map(Some).ok_or(PsdError::InvalidData {
            offset: 0,
            message: "warp integer field has an invalid type",
        }),
    }
}

fn parse_transform_list(value: &DescriptorValue) -> Result<[Point2; 4]> {
    let values = value.as_list().ok_or(PsdError::InvalidData {
        offset: 0,
        message: "warp transform must be a descriptor list",
    })?;
    if values.len() != 8 {
        return Err(PsdError::InvalidData {
            offset: 0,
            message: "warp transform list must contain eight coordinates",
        });
    }
    let mut points = [Point2::ZERO; 4];
    for (index, pair) in values.chunks_exact(2).enumerate() {
        let x = pair[0].as_double().ok_or(PsdError::InvalidData {
            offset: 0,
            message: "warp transform coordinates must be doubles",
        })?;
        let y = pair[1].as_double().ok_or(PsdError::InvalidData {
            offset: 0,
            message: "warp transform coordinates must be doubles",
        })?;
        points[index] = Point2::new(x, y);
    }
    if !points.iter().all(|point| point.is_finite()) {
        return Err(PsdError::InvalidData {
            offset: 0,
            message: "warp transform contains non-finite coordinates",
        });
    }
    // Photoshop's lists are TL, TR, BR, BL; internal geometry is TL, TR, BL, BR.
    Ok([points[0], points[1], points[3], points[2]])
}

fn parse_unit_floats(items: &[psd_core::DescriptorItem], key: &str) -> Result<Vec<f64>> {
    let value = items
        .iter()
        .find(|item| item.key.as_bytes() == key.as_bytes())
        .map(|item| &item.value)
        .ok_or(PsdError::InvalidData {
            offset: 0,
            message: "warp descriptor is missing control point coordinates",
        })?;
    match value {
        DescriptorValue::UnitFloats { values, .. } if values.iter().all(|v| v.is_finite()) => {
            Ok(values.clone())
        }
        _ => Err(PsdError::InvalidData {
            offset: 0,
            message: "warp coordinates must be unit-float arrays",
        }),
    }
}

fn parse_nested_slices(envelope: &Descriptor, key: &str) -> Result<Vec<f64>> {
    let array = envelope
        .get(key)
        .and_then(|value| match value {
            DescriptorValue::ObjectArray(array) => Some(array),
            _ => None,
        })
        .ok_or(PsdError::InvalidData {
            offset: 0,
            message: "quilt descriptor is missing slice positions",
        })?;
    parse_unit_floats(&array.items, key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use psd_core::{DescriptorKey, DescriptorValue, ObjectArray, UnicodeString};

    fn descriptor(items: Vec<(&str, DescriptorValue)>) -> Descriptor {
        Descriptor {
            name: UnicodeString::new("warp", 1).unwrap(),
            class_id: DescriptorKey::new("warp"),
            items: items
                .into_iter()
                .map(|(key, value)| psd_core::DescriptorItem {
                    key: DescriptorKey::new(key),
                    value,
                })
                .collect(),
        }
    }

    fn bounds() -> DescriptorValue {
        DescriptorValue::Descriptor(descriptor(vec![
            ("Top ", DescriptorValue::Double(0.0)),
            ("Left", DescriptorValue::Double(0.0)),
            ("Btom", DescriptorValue::Double(32.0)),
            ("Rght", DescriptorValue::Double(32.0)),
        ]))
    }

    fn unit_array(key: &str, values: Vec<f64>) -> DescriptorValue {
        DescriptorValue::ObjectArray(ObjectArray {
            items_count: values.len() as u32,
            name: UnicodeString::new(key, 1).unwrap(),
            class_id: DescriptorKey::new("UntF"),
            items: vec![psd_core::DescriptorItem {
                key: DescriptorKey::new(key),
                value: DescriptorValue::UnitFloats {
                    unit: *b"#Pxl",
                    values,
                },
            }],
        })
    }

    #[test]
    fn missing_custom_envelope_builds_default_mesh() {
        let warp_desc = descriptor(vec![("bounds", bounds())]);
        let full = descriptor(vec![("warp", DescriptorValue::Descriptor(warp_desc))]);
        let warp = Warp::from_descriptor(&full).unwrap();
        assert_eq!(warp.grid_dimensions(), (4, 4));
        assert_eq!(warp.control_points().len(), 16);
        assert!(!warp.has_custom_envelope());
        assert!(warp.no_op());

        let mut curved = Warp::identity(32, 32).unwrap();
        curved.control_points[5].y += 2.0;
        assert!(!curved.no_op());
    }

    #[test]
    fn generated_normal_and_quilt_warps_have_expected_defaults() {
        let normal = Warp::generate_default(200, 108).unwrap();
        assert_eq!(normal.kind(), WarpKind::Normal);
        assert_eq!(normal.grid_dimensions(), (4, 4));
        assert_eq!(normal.point(0, 0), Some(Point2::ZERO));
        assert_eq!(normal.point(3, 3), Some(Point2::new(200.0, 108.0)));
        assert!(normal.no_op());

        let quilt = Warp::generate_default_with_grid(600, 300, 7, 4).unwrap();
        assert_eq!(quilt.kind(), WarpKind::Quilt);
        assert_eq!(quilt.grid_dimensions(), (7, 4));
        assert_eq!(quilt.mesh().unwrap().width(), 7);
        assert_eq!(quilt.quilt_slices().0, &[-0.6, 200.0, 400.0, 600.6]);
        assert_eq!(quilt.quilt_slices().1, &[-0.6, 150.0, 300.6]);
        // Photoshop's padded default slices describe an unwarped quilt.
        assert!(quilt.no_op());
        let offset_quilt = Warp::from_control_points(
            quilt
                .control_points()
                .iter()
                .map(|point| *point + Point2::new(-40.0, 25.0))
                .collect(),
            7,
            4,
        )
        .unwrap();
        assert!(offset_quilt.no_op());
        let mut moved_slice = quilt.clone();
        moved_slice
            .set_quilt_slices(vec![-0.6, 260.0, 400.0, 600.6], vec![-0.6, 150.0, 300.6])
            .unwrap();
        assert!(!moved_slice.no_op());
        assert!(Warp::generate_default_with_grid(20, 20, 5, 4).is_err());
    }

    #[test]
    fn warp_metadata_setters_roundtrip_through_a_direct_descriptor() {
        let mut warp = Warp::generate_default(40, 30).unwrap();
        warp.set_style("warpNone").unwrap();
        warp.set_warp_value(0.25).unwrap();
        warp.set_perspective(-0.5, 0.75).unwrap();
        warp.set_warp_rotate("Vrtc").unwrap();
        warp.set_source_bounds(BoundingBox::new(Point2::ZERO, Point2::new(80.0, 60.0)))
            .unwrap();
        assert!(warp.set_style("unknown").is_err());
        assert!(warp.set_warp_value(f64::NAN).is_err());
        assert!(warp.set_perspective(0.0, f64::INFINITY).is_err());
        assert!(warp
            .set_source_bounds(BoundingBox::new(Point2::ZERO, Point2::ZERO))
            .is_err());

        let parsed = Warp::from_descriptor(&warp.to_descriptor().unwrap()).unwrap();
        assert_eq!(parsed.style(), "warpNone");
        assert_eq!(parsed.warp_value(), 0.25);
        assert_eq!(parsed.perspective(), (-0.5, 0.75));
        assert_eq!(parsed.warp_rotate(), "Vrtc");
        assert_eq!(parsed.source_bounds(), warp.source_bounds());
        assert_eq!(parsed.points(), warp.points());
        let identity = Warp::identity(40, 30).unwrap();
        let identity_back = Warp::from_descriptor(&identity.to_descriptor().unwrap()).unwrap();
        assert!(identity.approximately_eq(&identity_back, 1e-9));
    }

    #[test]
    fn warp_rotation_reads_long_form_ids_and_writes_char_ids() {
        // A descriptor built from scratch uses zero-length char IDs, as
        // upstream writes them.
        let warp = Warp::generate_default(40, 30).unwrap();
        let written = warp.to_descriptor().unwrap();
        let (type_id, value) = written.get("warpRotate").unwrap().as_enum().unwrap();
        assert_eq!(
            (type_id.as_bytes(), value.as_bytes()),
            (&b"Ornt"[..], &b"Hrzn"[..])
        );
        assert!(type_id.uses_implicit_length() && value.uses_implicit_length());

        // The long-form string ID newer Photoshop versions can write instead.
        let mut long_form = written.clone();
        long_form.insert(
            "warpRotate",
            DescriptorValue::Enumerated {
                type_id: DescriptorKey::char_id(*b"Ornt"),
                value: DescriptorKey::new("vertical"),
            },
        );
        let mut parsed = Warp::from_descriptor(&long_form).unwrap();
        assert_eq!(parsed.warp_rotate(), "vertical");
        assert!(parsed.is_vertical());

        // Unchanged, the long form keeps its encoding.
        let mut unchanged = long_form.clone();
        parsed.update_warp_descriptor(&mut unchanged).unwrap();
        assert_eq!(unchanged.get("warpRotate"), long_form.get("warpRotate"));

        // A char ID replacing it is written with the zero-length encoding,
        // never as an explicit-length `Hrzn` string ID.
        parsed.set_warp_rotate("Hrzn").unwrap();
        assert!(!parsed.is_vertical());
        let mut replaced = long_form.clone();
        parsed.update_warp_descriptor(&mut replaced).unwrap();
        let (_, value) = replaced.get("warpRotate").unwrap().as_enum().unwrap();
        assert_eq!(value.as_bytes(), b"Hrzn");
        assert!(value.uses_implicit_length());

        parsed.set_warp_rotate("horizontal").unwrap();
        assert!(parsed.set_warp_rotate("sideways").is_err());
    }

    #[test]
    fn point_mutation_is_bounds_checked_and_transform_size_stays_constrained() {
        let mut warp = Warp::generate_default(200, 108).unwrap();
        let affine = warp.affine_transform();
        warp.set_point(0, 0, Point2::new(-500.0, 0.0)).unwrap();
        assert_eq!(warp.point(0, 0), Some(Point2::new(-500.0, 0.0)));
        assert!(warp.set_point(4, 0, Point2::ZERO).is_err());
        let output_bounds = warp.mesh_bounds(false).unwrap();
        assert!((output_bounds.width() - 200.0).abs() < 1e-8);
        assert!((output_bounds.height() - 108.0).abs() < 1e-8);
        assert_eq!(warp.affine_transform(), affine);
    }

    #[test]
    fn control_net_transform_and_quilt_setters_validate_and_affect_geometry() {
        let mut warp = Warp::generate_default(100, 50).unwrap();
        let mut points = warp.control_points().to_vec();
        points[5].y += 8.0;
        warp.set_control_points(points.clone()).unwrap();
        let mesh = warp.mesh().unwrap();
        assert_eq!(mesh.vertex(5).unwrap().point, points[5]);

        let moved = [
            Point2::new(10.0, 0.0),
            Point2::new(110.0, 0.0),
            Point2::new(10.0, 50.0),
            Point2::new(110.0, 50.0),
        ];
        warp.set_affine_transform(moved).unwrap();
        warp.set_non_affine_transform(moved).unwrap();
        let moved_bounds = warp.mesh().unwrap().bounds();
        assert!(moved_bounds.min.approx_eq(moved[0], 1e-8));
        assert!(moved_bounds.max.approx_eq(moved[3], 1e-8));
        assert!(warp
            .set_affine_transform([
                Point2::new(0.0, 0.0),
                Point2::new(10.0, 0.0),
                Point2::new(2.0, 10.0),
                Point2::new(10.0, 10.0),
            ])
            .is_err());
        assert!(warp.set_control_points(vec![Point2::ZERO; 15]).is_err());
        assert!(warp
            .set_control_points(vec![Point2::new(f64::NAN, 0.0); 16])
            .is_err());

        let mut quilt = Warp::generate_default_with_grid(60, 30, 7, 4).unwrap();
        quilt
            .set_quilt_slices(vec![-0.6, 20.0, 44.0, 60.6], vec![-0.6, 30.6])
            .unwrap();
        assert!(quilt
            .set_quilt_slices(vec![0.0, 0.0], vec![0.0, 1.0])
            .is_err());
        let non_affine = [
            Point2::new(-10.0, 2.0),
            Point2::new(70.0, -3.0),
            Point2::new(4.0, 36.0),
            Point2::new(65.0, 31.0),
        ];
        quilt.set_non_affine_transform(non_affine).unwrap();
        assert!(quilt.mesh().unwrap().bounds().min.x < 0.0);
        assert!(quilt.set_warp_rotate("diagonal").is_err());
        let mut invalid_non_affine = non_affine;
        invalid_non_affine[0].x = f64::NAN;
        assert!(quilt.set_non_affine_transform(invalid_non_affine).is_err());
        assert!(quilt
            .set_quilt_slices(vec![0.0, 10.0, f64::INFINITY], vec![0.0, 1.0])
            .is_err());
    }

    #[test]
    fn source_replacement_rebases_bounds_points_and_quilt_slices_idempotently() {
        let mut normal = Warp::generate_default(100, 80).unwrap();
        let mut point = normal.point(1, 1).unwrap();
        point.x += 12.0;
        point.y += 9.0;
        normal.set_point(1, 1, point).unwrap();
        let old_point = normal.point(1, 1).unwrap();

        normal.rescale_source(200, 40).unwrap();
        assert_eq!(normal.source_bounds().width(), 200.0);
        assert_eq!(normal.source_bounds().height(), 40.0);
        assert_eq!(
            normal.point(1, 1),
            Some(Point2::new(old_point.x * 2.0, old_point.y * 0.5))
        );
        let once = normal.clone();
        normal.rescale_source(200, 40).unwrap();
        assert_eq!(
            normal, once,
            "same-size replacement must not compound warp scaling"
        );

        let mut quilt = Warp::generate_default_with_grid(100, 80, 7, 4).unwrap();
        let old_slices_x = quilt.quilt_slices().0.to_vec();
        let old_slices_y = quilt.quilt_slices().1.to_vec();
        quilt.rescale_source(200, 40).unwrap();
        assert_eq!(quilt.quilt_slices().0[1], old_slices_x[1] * 2.0);
        assert_eq!(quilt.quilt_slices().1[1], old_slices_y[1] * 0.5);
        let once = quilt.clone();
        quilt.rescale_source(200, 40).unwrap();
        assert_eq!(
            quilt, once,
            "same-size replacement must not compound quilt slices"
        );
    }

    #[test]
    fn accurate_bounds_follow_the_surface_while_control_bounds_include_handles() {
        let mut points: Vec<_> = (0..4)
            .flat_map(|y| (0..4).map(move |x| Point2::new(x as f64 / 3.0, y as f64 / 3.0)))
            .collect();
        points[5].x = 4.0;
        let warp = Warp::from_control_points(points, 4, 4).unwrap();
        let control_bounds = warp.control_bounds().unwrap();
        let surface_bounds = warp.bounds().unwrap();
        assert_eq!(control_bounds.max.x, 4.0);
        assert!(surface_bounds.max.x < control_bounds.max.x - 0.5);
    }

    #[test]
    fn malformed_nested_warp_descriptor_is_rejected() {
        let full = descriptor(vec![("warp", DescriptorValue::Double(1.0))]);
        assert!(Warp::from_descriptor(&full).is_err());
    }

    #[test]
    fn normal_and_quilt_descriptors_serialize_and_parse_mutations() {
        for mut warp in [
            Warp::generate_default(40, 30).unwrap(),
            Warp::generate_default_with_grid(60, 45, 7, 4).unwrap(),
        ] {
            warp.set_point(1, 1, Point2::new(8.0, 11.0)).unwrap();
            let mut descriptor = warp.to_descriptor().unwrap();
            descriptor.insert("unknownWarpKey", DescriptorValue::Integer(123));
            warp.update_warp_descriptor(&mut descriptor).unwrap();
            assert_eq!(
                descriptor.get("unknownWarpKey").unwrap().as_integer(),
                Some(123)
            );

            let mut writer = BeWriter::new();
            descriptor.write(&mut writer).unwrap();
            let mut reader = BeReader::new(writer.as_slice());
            let parsed_descriptor = Descriptor::read(&mut reader).unwrap();
            assert!(reader.is_empty());
            let parsed = Warp::from_descriptor(&parsed_descriptor).unwrap();
            assert_eq!(parsed.kind(), warp.kind());
            assert_eq!(parsed.grid_dimensions(), warp.grid_dimensions());
            assert_eq!(parsed.point(1, 1), Some(Point2::new(8.0, 11.0)));
            assert_eq!(parsed.affine_transform(), warp.affine_transform());
            if warp.kind() == WarpKind::Quilt {
                assert_eq!(parsed.quilt_slices(), warp.quilt_slices());
            }
        }
    }

    #[test]
    fn full_placed_data_update_preserves_unrelated_descriptor_fields() {
        let root = descriptor(vec![
            (
                "Trnf",
                DescriptorValue::List(vec![DescriptorValue::Double(0.0); 8]),
            ),
            (
                "nonAffineTransform",
                DescriptorValue::List(vec![DescriptorValue::Double(0.0); 8]),
            ),
            (
                "warp",
                DescriptorValue::Descriptor(descriptor(vec![("bounds", bounds())])),
            ),
            (
                "Idnt",
                DescriptorValue::String(UnicodeString::new("uuid", 1).unwrap()),
            ),
        ]);
        let mut placed = PlacedLayerData {
            version: 4,
            descriptor_version: 16,
            descriptor: root.clone(),
        };
        let quilt = Warp::generate_default_with_grid(100, 50, 7, 4).unwrap();
        quilt.update_placed_layer_data(&mut placed).unwrap();
        assert_eq!(
            placed.descriptor.get("Idnt").unwrap().as_str(),
            Some("uuid")
        );
        assert!(placed.descriptor.get("quiltWarp").is_some());
        assert_eq!(
            placed
                .descriptor
                .get("warp")
                .unwrap()
                .as_descriptor()
                .unwrap()
                .get("warpStyle")
                .unwrap()
                .as_enum()
                .unwrap()
                .1
                .as_str(),
            "warpNone"
        );
        let parsed = Warp::from_placed_data(&placed).unwrap();
        assert_eq!(parsed.grid_dimensions(), (7, 4));
        assert_eq!(parsed.kind(), WarpKind::Quilt);
        assert_eq!(placed.version, 4);
        assert_eq!(placed.descriptor_version, 16);
    }

    #[test]
    fn quilt_descriptor_reads_dimensions_mesh_points_and_slices() {
        let row = [0.0, 10.0, 20.0, 30.0, 40.0, 50.0, 60.0];
        let horizontal = row
            .into_iter()
            .chain(row)
            .chain(row)
            .chain(row)
            .collect::<Vec<_>>();
        let vertical = vec![0.0, 10.0, 20.0, 30.0];
        let envelope = descriptor(vec![
            (
                "meshPoints",
                DescriptorValue::ObjectArray(ObjectArray {
                    items_count: 28,
                    name: UnicodeString::new("meshPoints", 1).unwrap(),
                    class_id: DescriptorKey::new("rationalPoint"),
                    items: vec![
                        psd_core::DescriptorItem {
                            key: DescriptorKey::new("Hrzn"),
                            value: DescriptorValue::UnitFloats {
                                unit: *b"#Pxl",
                                values: horizontal.clone(),
                            },
                        },
                        psd_core::DescriptorItem {
                            key: DescriptorKey::new("Vrtc"),
                            value: DescriptorValue::UnitFloats {
                                unit: *b"#Pxl",
                                values: vertical.into_iter().flat_map(|y| [y; 7]).collect(),
                            },
                        },
                    ],
                }),
            ),
            (
                "quiltSliceX",
                unit_array("quiltSliceX", vec![-0.6, 30.0, 60.6]),
            ),
            ("quiltSliceY", unit_array("quiltSliceY", vec![-0.6, 30.6])),
        ]);
        let quilt = descriptor(vec![
            ("bounds", bounds()),
            ("deformNumRows", DescriptorValue::Integer(4)),
            ("deformNumCols", DescriptorValue::Integer(7)),
            ("customEnvelopeWarp", DescriptorValue::Descriptor(envelope)),
        ]);
        let full = descriptor(vec![("quiltWarp", DescriptorValue::Descriptor(quilt))]);
        let warp = Warp::from_descriptor(&full).unwrap();
        assert_eq!(warp.kind(), WarpKind::Quilt);
        assert_eq!(warp.grid_dimensions(), (7, 4));
        assert_eq!(warp.control_points().len(), 28);
        assert_eq!(warp.quilt_slices().0.len(), 3);
        assert!(warp.tessellated_mesh(8, 5).is_ok());
    }
}
