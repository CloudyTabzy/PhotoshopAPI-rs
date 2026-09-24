//! TySh affine transform access (`TextLayerTransformMixin.h` upstream).
//!
//! The six doubles `[xx, xy, yx, yy, tx, ty]` sit at a fixed offset right
//! after the TySh version, so every edit is an in-place 48-byte patch; neither
//! descriptor is re-serialized.

use psd_core::engine_data::{self, PayloadPatch};
use psd_core::TypeToolTaggedBlock;

use super::{invalid, tysh_spans, TYSH};
use crate::{BitDepth, Layer};

/// The transform of a TySh payload recognizable as text (its `Txt `/
/// EngineData payloads locate), read at its fixed offset so a descriptor
/// value this port cannot parse does not hide it.
fn tysh_transform(data: &[u8]) -> Option<[f64; 6]> {
    tysh_spans(data)?;
    TypeToolTaggedBlock::transform_of(data)
}

/// Scales below this are treated as degenerate when rescaling an axis.
const MIN_SCALE: f64 = 1e-15;

fn axis_scale(x: f64, y: f64) -> f64 {
    x.hypot(y)
}

impl<T: BitDepth> Layer<T> {
    /// Return the TySh affine transform `[xx, xy, yx, yy, tx, ty]` from the
    /// first parseable TySh block.
    pub fn text_transform(&self) -> Option<[f64; 6]> {
        self.blocks
            .blocks
            .iter()
            .filter(|block| block.key == TYSH)
            .find_map(|block| tysh_transform(&block.data))
    }

    /// One transform component by index (`0..6` = xx, xy, yx, yy, tx, ty).
    pub fn text_transform_component(&self, index: usize) -> Option<f64> {
        self.text_transform()?.get(index).copied()
    }

    /// Replace the whole TySh affine transform on every parseable TySh block.
    pub fn set_text_transform(&mut self, transform: [f64; 6]) -> psd_core::Result<()> {
        self.update_text_transforms(|_| Ok(transform))
    }

    /// Replace one transform component (`0..6` = xx, xy, yx, yy, tx, ty).
    pub fn set_text_transform_component(
        &mut self,
        index: usize,
        value: f64,
    ) -> psd_core::Result<()> {
        if index >= 6 {
            return Err(invalid(
                "text transform component index must be less than six",
            ));
        }
        self.update_text_transforms(|mut transform| {
            transform[index] = value;
            Ok(transform)
        })
    }

    /// Translation `(tx, ty)` of the text anchor.
    pub fn text_position(&self) -> Option<(f64, f64)> {
        let transform = self.text_transform()?;
        Some((transform[4], transform[5]))
    }

    /// Move the text anchor, keeping rotation and scale.
    pub fn set_text_position(&mut self, x: f64, y: f64) -> psd_core::Result<()> {
        self.update_text_transforms(|mut transform| {
            transform[4] = x;
            transform[5] = y;
            Ok(transform)
        })
    }

    /// Rotation of the x axis in degrees (`atan2(xy, xx)`).
    pub fn text_rotation_angle(&self) -> Option<f64> {
        let [xx, xy, ..] = self.text_transform()?;
        Some(xy.atan2(xx).to_degrees())
    }

    /// Length of the transformed x axis.
    pub fn text_scale_x(&self) -> Option<f64> {
        let [xx, xy, ..] = self.text_transform()?;
        Some(axis_scale(xx, xy))
    }

    /// Length of the transformed y axis.
    pub fn text_scale_y(&self) -> Option<f64> {
        let [_, _, yx, yy, ..] = self.text_transform()?;
        Some(axis_scale(yx, yy))
    }

    /// Set the rotation, keeping both axis scales and the translation. Any
    /// skew in the current matrix is replaced by a pure rotation.
    pub fn set_text_rotation_angle(&mut self, degrees: f64) -> psd_core::Result<()> {
        let (sin, cos) = degrees.to_radians().sin_cos();
        self.update_text_transforms(|[xx, xy, yx, yy, tx, ty]| {
            let sx = axis_scale(xx, xy);
            let sy = axis_scale(yx, yy);
            Ok([cos * sx, sin * sx, -sin * sy, cos * sy, tx, ty])
        })
    }

    /// Set the x-axis scale, keeping its direction.
    pub fn set_text_scale_x(&mut self, sx: f64) -> psd_core::Result<()> {
        self.update_text_transforms(|mut transform| {
            let ratio = rescale_ratio(sx, axis_scale(transform[0], transform[1]))?;
            transform[0] *= ratio;
            transform[1] *= ratio;
            Ok(transform)
        })
    }

    /// Set the y-axis scale, keeping its direction.
    pub fn set_text_scale_y(&mut self, sy: f64) -> psd_core::Result<()> {
        self.update_text_transforms(|mut transform| {
            let ratio = rescale_ratio(sy, axis_scale(transform[2], transform[3]))?;
            transform[2] *= ratio;
            transform[3] *= ratio;
            Ok(transform)
        })
    }

    /// Set both axis scales, keeping their directions.
    pub fn set_text_scale(&mut self, sx: f64, sy: f64) -> psd_core::Result<()> {
        self.update_text_transforms(|mut transform| {
            let rx = rescale_ratio(sx, axis_scale(transform[0], transform[1]))?;
            let ry = rescale_ratio(sy, axis_scale(transform[2], transform[3]))?;
            transform[0] *= rx;
            transform[1] *= rx;
            transform[2] *= ry;
            transform[3] *= ry;
            Ok(transform)
        })
    }

    /// Reset rotation, scale, and skew to identity, keeping the translation.
    pub fn reset_text_transform(&mut self) -> psd_core::Result<()> {
        self.update_text_transforms(|[_, _, _, _, tx, ty]| Ok([1.0, 0.0, 0.0, 1.0, tx, ty]))
    }

    /// Apply `update` to each parseable TySh block's own transform, staging
    /// all blocks so a rejected value leaves the layer unchanged.
    fn update_text_transforms(
        &mut self,
        mut update: impl FnMut([f64; 6]) -> psd_core::Result<[f64; 6]>,
    ) -> psd_core::Result<()> {
        let mut staged = self.blocks.clone();
        let mut found = false;
        for block in &mut staged.blocks {
            if block.key != TYSH {
                continue;
            }
            let Some(current) = tysh_transform(&block.data) else {
                continue;
            };
            found = true;
            let transform = update(current)?;
            if !transform.iter().all(|value| value.is_finite()) {
                return Err(invalid("text transform values must be finite"));
            }
            let mut bytes = Vec::with_capacity(TypeToolTaggedBlock::TRANSFORM_RANGE.len());
            for value in transform {
                bytes.extend_from_slice(&value.to_be_bytes());
            }
            engine_data::apply_patches_checked(
                &mut block.data,
                &mut [PayloadPatch {
                    range: TypeToolTaggedBlock::TRANSFORM_RANGE,
                    new_bytes: bytes,
                }],
            )?;
        }
        if !found {
            return Err(invalid("layer has no parseable TySh transform"));
        }
        self.blocks = staged;
        Ok(())
    }
}

fn rescale_ratio(target: f64, current: f64) -> psd_core::Result<f64> {
    if !target.is_finite() {
        return Err(invalid("text scale must be finite"));
    }
    if current < MIN_SCALE {
        return Err(invalid("current text axis scale is degenerate"));
    }
    Ok(target / current)
}
