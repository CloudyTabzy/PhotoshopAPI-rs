//! Layer and group masks resolved to a coverage plane over a canvas rect.
//!
//! A layer can carry a raster mask and a vector mask at once; both multiply.
//! Each has its own density (lerp toward white) and feather (Gaussian, sigma
//! equal to the feather radius in pixels) parameters that Photoshop applies at
//! render time. A raster mask's feather replicates the canvas edge; a vector
//! mask's feather blurs the path coverage unclamped, so a path ending on the
//! canvas edge fades there.

use crate::layer::{Layer, Rect};
use crate::BitDepth;

use super::paths::{gaussian_blur, rasterize_path};
use super::Compositor;

impl<T: BitDepth> Compositor<'_, T> {
    /// The combined mask coverage of `layer` over `rect` (canvas-clamped), or
    /// `None` when the layer has no active mask.
    pub(super) fn mask_plane(&self, layer: &Layer<T>, rect: Rect) -> Option<Vec<f32>> {
        let user = self.user_mask_plane(layer, rect);
        let vector = self.vector_mask_plane(layer, rect);
        match (user, vector) {
            (Some(mut user), Some(vector)) => {
                for (a, b) in user.iter_mut().zip(vector) {
                    *a *= b;
                }
                Some(user)
            }
            (user, vector) => user.or(vector),
        }
    }

    fn params(layer: &Layer<T>) -> psd_core::MaskParams {
        layer
            .mask
            .as_ref()
            .and_then(|data| {
                data.pixel_mask
                    .as_ref()
                    .or(data.vector_mask.as_ref())
                    .and_then(|record| record.params)
            })
            .unwrap_or_default()
    }

    fn user_mask_plane(&self, layer: &Layer<T>, rect: Rect) -> Option<Vec<f32>> {
        let record = layer.mask_record()?;
        if record.flags.disabled() {
            return None;
        }
        let params = Self::params(layer);
        let feather = params.user_mask_feather.unwrap_or(0.0);

        // Build over the rect plus the blur margin, clamped to the canvas, so
        // the feather sees real mask samples and replicates the canvas edge.
        let margin = if feather > 0.0 {
            (feather * 3.0).ceil() as i32 + 1
        } else {
            0
        };
        let working = clamp_rect(
            Rect::new(
                rect.top - margin,
                rect.left - margin,
                rect.bottom + margin,
                rect.right + margin,
            ),
            self.document.width,
            self.document.height,
        );
        let (width, height) = (
            working.width().max(0) as usize,
            working.height().max(0) as usize,
        );
        let default = f32::from(record.default_color) / 255.0;
        let mut plane = vec![default; width * height];
        if let Some(pixels) = layer.mask_pixels() {
            let mask_width = (record.right - record.left).max(0) as usize;
            let mask_height = (record.bottom - record.top).max(0) as usize;
            if pixels.len() >= mask_width * mask_height {
                for y in record.top.max(working.top)..record.bottom.min(working.bottom) {
                    let source_row = (y - record.top) as usize * mask_width;
                    let target_row = (y - working.top) as usize * width;
                    for x in record.left.max(working.left)..record.right.min(working.right) {
                        plane[target_row + (x - working.left) as usize] =
                            pixels[source_row + (x - record.left) as usize].to_f32();
                    }
                }
            }
        }
        if feather > 0.0 {
            gaussian_blur(&mut plane, width, height, feather);
        }
        if let Some(density) = params.user_mask_density {
            apply_density(&mut plane, density);
        }
        Some(crop(&plane, working, rect))
    }

    fn vector_mask_plane(&self, layer: &Layer<T>, rect: Rect) -> Option<Vec<f32>> {
        let mask = layer.vector_mask().ok()??;
        if mask.disabled() {
            return None;
        }
        let params = Self::params(layer);
        let feather = params.vector_mask_feather.unwrap_or(0.0);
        let margin = if feather > 0.0 {
            (feather * 3.0).ceil() as i32 + 1
        } else {
            0
        };
        let working = Rect::new(
            rect.top - margin,
            rect.left - margin,
            rect.bottom + margin,
            rect.right + margin,
        );
        let (width, height) = (
            working.width().max(0) as usize,
            working.height().max(0) as usize,
        );
        let mut plane = rasterize_path(
            &mask.path,
            self.document.width,
            self.document.height,
            working,
        );
        if mask.inverted() {
            for value in &mut plane {
                *value = 1.0 - *value;
            }
        }
        if feather > 0.0 {
            gaussian_blur(&mut plane, width, height, feather);
        }
        if let Some(density) = params.vector_mask_density {
            apply_density(&mut plane, density);
        }
        Some(crop(&plane, working, rect))
    }
}

/// Mask density: the mask shows through at `density`, white elsewhere.
fn apply_density(plane: &mut [f32], density: u8) {
    let density = f32::from(density) / 255.0;
    for value in plane {
        *value = 1.0 - density * (1.0 - *value);
    }
}

fn clamp_rect(rect: Rect, width: u32, height: u32) -> Rect {
    Rect::new(
        rect.top.max(0),
        rect.left.max(0),
        rect.bottom.min(height as i32),
        rect.right.min(width as i32),
    )
}

/// Copy the `inner` window out of a plane that covers `outer`.
fn crop(plane: &[f32], outer: Rect, inner: Rect) -> Vec<f32> {
    let outer_width = outer.width().max(0) as usize;
    let inner_width = inner.width().max(0) as usize;
    let inner_height = inner.height().max(0) as usize;
    let mut out = vec![0.0f32; inner_width * inner_height];
    for row in 0..inner_height {
        let source_y = inner.top + row as i32 - outer.top;
        if source_y < 0 || source_y >= outer.height() {
            continue;
        }
        let source = source_y as usize * outer_width + (inner.left - outer.left).max(0) as usize;
        let slice = &plane[source..source + inner_width];
        out[row * inner_width..(row + 1) * inner_width].copy_from_slice(slice);
    }
    out
}
