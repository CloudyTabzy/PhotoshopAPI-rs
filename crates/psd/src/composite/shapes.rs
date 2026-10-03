//! Shape layers: a fill layer shaped by its vector path, with an optional
//! vector stroke.
//!
//! Photoshop stores a shape as a fill layer (`SoCo`, `GdFl` or `PtFl`) plus a
//! vector mask, and a `vstk` block for the stroke; it carries no pixels. The
//! path's coverage (with its inversion) shapes the fill, the stroke is a
//! second plane above it, and the path's feather blurs the *whole* rendered
//! shape (stroke included) without clamping at the canvas edge, while its
//! density shows the fill everywhere at `1 − density` outside the path.

use psd_core::vector::{VectorData, VectorStroke};
use psd_core::{AdjustmentKind, BlendMode, MaskParams};

use super::adjustments::paint_content;
use super::paths::{feather_blur, rasterize_path};
use super::stroke::{stroke_coverage, Alignment, Cap, Join, StrokeStyle};
use super::{blend, Compositor, Content, Rect};
use crate::layer::Layer;
use crate::BitDepth;

/// The half pixel by which CS6 draws a centred stroke right and down of its path.
const CS6_CENTER_OFFSET: f64 = 0.5;

impl<T: BitDepth> Compositor<'_, T> {
    /// Whether the layer has an active vector path.
    pub(super) fn has_vector_path(&self, layer: &Layer<T>) -> bool {
        layer
            .vector_mask()
            .ok()
            .flatten()
            .is_some_and(|mask| !mask.disabled())
    }

    /// How far a centred vector stroke sits right and down of its path.
    ///
    /// Approximation: a document last saved by Photoshop CS6 draws its centred
    /// strokes half a pixel right and down (a 3 px stroke on a path at integer
    /// coordinates covers whole pixel rows, and a dash pattern starts at the
    /// first knot); two such documents match to within a few levels with the
    /// offset and miss by up to a whole pixel without it. Documents from later
    /// versions draw the stroke on the path. The offset is only known for CS6;
    /// other releases of that era are not told apart.
    fn center_stroke_offset(&self) -> f64 {
        if last_saved_by(&self.document.image_resources).is_some_and(|name| name.contains("CS6")) {
            CS6_CENTER_OFFSET
        } else {
            0.0
        }
    }

    /// The vector stroke of a shape layer, resolved to pixels.
    fn vector_stroke(&self, layer: &Layer<T>) -> Option<(VectorStroke, StrokeStyle)> {
        let blocks = layer.vector_blocks().ok()?;
        let stroke = blocks.into_iter().find_map(|block| match block.data {
            VectorData::Stroke(stroke) => Some(stroke),
            _ => None,
        })?;
        if stroke.stroke_enabled() == Some(false) {
            return None;
        }
        let resolution = stroke.resolution().unwrap_or(72.0);
        let (unit, raw_width) = stroke.line_width()?;
        let width = if &unit == b"#Pnt" {
            raw_width * resolution / 72.0
        } else {
            raw_width
        };
        if width <= 0.0 {
            return None;
        }
        let name = |key: Option<&psd_core::DescriptorKey>| {
            key.map(|key| String::from_utf8_lossy(key.as_bytes()).to_ascii_lowercase())
                .unwrap_or_default()
        };
        let cap = name(stroke.line_cap());
        let join = name(stroke.line_join());
        let alignment = name(stroke.line_alignment());
        let dashes: Vec<f64> = stroke
            .dash_set()
            .unwrap_or_default()
            .into_iter()
            .map(|length| length * width)
            .collect();
        let style = StrokeStyle {
            width,
            cap: if cap.contains("round") {
                Cap::Round
            } else if cap.contains("square") {
                Cap::Square
            } else {
                Cap::Butt
            },
            join: if join.contains("round") {
                Join::Round
            } else if join.contains("bevel") {
                Join::Bevel
            } else {
                Join::Miter
            },
            miter_limit: stroke.miter_limit().unwrap_or(10.0).max(1.0),
            alignment: if alignment.contains("inside") {
                Alignment::Inside
            } else if alignment.contains("outside") {
                Alignment::Outside
            } else {
                Alignment::Center
            },
            dashes,
            dash_offset: 0.0,
            center_offset: self.center_stroke_offset(),
        };
        Some((stroke, style))
    }

    /// Whether the pixels stored beside a stroked shape hold clearly less ink
    /// than its stroke should. Some writers keep the ring at an unscaled
    /// width in a document whose resolution is not 72 ppi; the stored ring is
    /// then thinner than the stroke the document asks for, and the live
    /// stroke is the better source.
    pub(super) fn stored_pixels_miss_stroke(&self, layer: &Layer<T>) -> bool {
        let Some((_, style)) = self.vector_stroke(layer) else {
            return false;
        };
        let Some(mask) = layer.vector_mask().ok().flatten() else {
            return false;
        };
        let Some(alpha) = layer
            .channels()
            .and_then(|channels| channels.get(crate::ChannelKey::ALPHA))
        else {
            return false;
        };
        let stored: f64 = alpha.iter().map(|sample| f64::from(sample.to_f32())).sum();
        let (doc_w, doc_h) = (self.document.width, self.document.height);
        let rect = Rect::new(0, 0, doc_h as i32, doc_w as i32);
        let shape = rasterize_path(&mask.path, doc_w, doc_h, rect);
        let live: f64 = stroke_coverage(&mask.path, (doc_w, doc_h), rect, &style, &shape)
            .iter()
            .map(|value| f64::from(*value))
            .sum();
        // A quarter of slack, and a few pixels for rounding on tiny shapes.
        live > stored * 1.25 + 4.0
    }

    /// Shape a fill with its layer's vector path and stroke. `fill` covers the
    /// canvas with the fill's colours; the result does too, with the path, the
    /// stroke, the feather and the density folded into its alpha.
    pub(super) fn shape_content(&self, layer: &Layer<T>, fill: Content) -> Content {
        let Some(mask) = layer.vector_mask().ok().flatten() else {
            return fill;
        };
        let rect = fill.rect;
        let (doc_w, doc_h) = (self.document.width, self.document.height);
        let params: MaskParams = layer
            .mask
            .as_ref()
            .and_then(|data| {
                data.pixel_mask
                    .as_ref()
                    .or(data.vector_mask.as_ref())
                    .and_then(|record| record.params)
            })
            .unwrap_or_default();
        let feather = params.vector_mask_feather.unwrap_or(0.0).max(0.0);
        let density = params
            .vector_mask_density
            .map(|density| f32::from(density) / 255.0);
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

        // The path, and the stroke above it.
        let mut shape = rasterize_path(&mask.path, doc_w, doc_h, working);
        if mask.inverted() {
            for value in &mut shape {
                *value = 1.0 - *value;
            }
        }
        let stroke = self.vector_stroke(layer);
        let stroke_plane = stroke
            .as_ref()
            .map(|(_, style)| stroke_coverage(&mask.path, (doc_w, doc_h), working, style, &shape));
        let stroke_paint = stroke.as_ref().and_then(|(stroke, _)| {
            let content = stroke.content()?;
            let kind = if content.contains("Grad") {
                AdjustmentKind::GradientFill
            } else if content.contains("Ptrn") {
                AdjustmentKind::PatternFill
            } else {
                AdjustmentKind::SolidColor
            };
            paint_content(self, layer, content, kind, rect)
        });
        let stroke_opacity = stroke
            .as_ref()
            .and_then(|(stroke, _)| stroke.opacity_percent())
            .map_or(1.0, |percent| (percent / 100.0).clamp(0.0, 1.0) as f32);
        let fill_enabled = stroke
            .as_ref()
            .and_then(|(stroke, _)| stroke.fill_enabled())
            .unwrap_or(true)
            || stroke.is_none();
        // `strokeStyleBlendMode` paints the stroke onto the fill with its
        // own mode; Normal (what every authored file carries) skips the
        // blend entirely.
        let stroke_blend_mode = stroke
            .as_ref()
            .and_then(|(stroke, _)| stroke.blend_mode_value())
            .unwrap_or(BlendMode::NORMAL);

        // Composite fill and stroke into premultiplied planes over `working`.
        let clamp_index = |x: i32, y: i32| -> Option<usize> {
            if rect.width() <= 0 || rect.height() <= 0 {
                return None;
            }
            let cx = x.clamp(rect.left, rect.right - 1);
            let cy = y.clamp(rect.top, rect.bottom - 1);
            fill.index(i64::from(cx), i64::from(cy))
        };
        let mut premult = [
            vec![0.0f32; width * height],
            vec![0.0f32; width * height],
            vec![0.0f32; width * height],
        ];
        let mut alpha = vec![0.0f32; width * height];
        // The shape's own silhouette, without the fill's transparency: the
        // matte layer effects follow.
        let mut silhouette = vec![0.0f32; width * height];
        for y in 0..height {
            for x in 0..width {
                let index = y * width + x;
                let (dx, dy) = (working.left + x as i32, working.top + y as i32);
                let Some(source) = clamp_index(dx, dy) else {
                    continue;
                };
                let fill_alpha = if fill_enabled {
                    shape[index] * fill.alpha[source]
                } else {
                    0.0
                };
                let (mut stroke_alpha, mut stroke_color) = (0.0f32, [0.0f32; 3]);
                if let (Some(plane), Some(paint)) = (&stroke_plane, &stroke_paint) {
                    stroke_alpha = plane[index] * paint.alpha[source] * stroke_opacity;
                    stroke_color = [
                        paint.color[0][source],
                        paint.color[1][source],
                        paint.color[2][source],
                    ];
                }
                if stroke_blend_mode != BlendMode::NORMAL && stroke_alpha > 0.0 {
                    let backdrop = [
                        fill.color[0][source],
                        fill.color[1][source],
                        fill.color[2][source],
                    ];
                    let blended =
                        blend::blend(stroke_blend_mode, backdrop, stroke_color, self.byte_domain);
                    // The same translucent-backdrop rule the layer compositor
                    // applies: under a partly transparent fill the stroke
                    // keeps its own colour in proportion.
                    for (channel, value) in stroke_color.iter_mut().enumerate() {
                        *value = *value * (1.0 - fill_alpha) + blended[channel] * fill_alpha;
                    }
                }
                let out_alpha = stroke_alpha + fill_alpha * (1.0 - stroke_alpha);
                alpha[index] = out_alpha;
                let stroke_cover = stroke_plane
                    .as_ref()
                    .map_or(0.0, |plane| plane[index] * stroke_opacity);
                let fill_cover = if fill_enabled { shape[index] } else { 0.0 };
                silhouette[index] = stroke_cover + fill_cover * (1.0 - stroke_cover);
                for channel in 0..3 {
                    premult[channel][index] = stroke_color[channel] * stroke_alpha
                        + fill.color[channel][source] * fill_alpha * (1.0 - stroke_alpha);
                }
            }
        }
        if feather > 0.0 {
            // Shape (vector) feather is the same three-box approximation
            // Photoshop applies to mask feather.
            feather_blur(&mut alpha, width, height, feather);
            feather_blur(&mut silhouette, width, height, feather);
            for plane in &mut premult {
                feather_blur(plane, width, height, feather);
            }
        }

        // Crop back to the canvas, un-premultiply, apply density.
        let mut out = Content::new(rect);
        let mut out_silhouette = vec![0.0f32; out.alpha.len()];
        let (out_width, out_height) = (out.width(), out.height());
        for y in 0..out_height {
            for x in 0..out_width {
                let from = (y + margin as usize) * width + x + margin as usize;
                let to = y * out_width + x;
                let a = alpha[from];
                let mut color = if a > 1e-6 {
                    [
                        premult[0][from] / a,
                        premult[1][from] / a,
                        premult[2][from] / a,
                    ]
                } else {
                    [fill.color[0][to], fill.color[1][to], fill.color[2][to]]
                };
                let mut a = a;
                // Outside the path the fill still shows at `1 − density`,
                // inside the layer's own bounds: a layer's mask can claim
                // a larger rect than its content, but the fill's floor is
                // part of the content and stays in the authored bounds.
                let dx = rect.left + x as i32;
                let dy = rect.top + y as i32;
                let in_bounds = dx >= layer.bounds.left
                    && dx < layer.bounds.right
                    && dy >= layer.bounds.top
                    && dy < layer.bounds.bottom;
                if let Some(density) = density {
                    let raised = if in_bounds {
                        1.0 - density + density * a
                    } else {
                        a
                    };
                    if raised > 1e-6 {
                        for (channel, value) in color.iter_mut().enumerate() {
                            let base = fill.color[channel][to];
                            *value = (*value * a * density + base * (1.0 - density)) / raised;
                        }
                    }
                    a = raised * fill.alpha[to].max(if fill_enabled { 0.0 } else { 1.0 });
                }
                for (channel, value) in color.iter().enumerate() {
                    out.color[channel][to] = value.clamp(0.0, 1.0);
                }
                out.alpha[to] = a.clamp(0.0, 1.0);
                let cover = silhouette[from];
                out_silhouette[to] = match density {
                    Some(density) if in_bounds => 1.0 - density + density * cover,
                    _ => cover,
                }
                .clamp(0.0, 1.0);
            }
        }
        out.shape = Some(out_silhouette);
        out.mask = fill.mask;
        out
    }
}

/// The reader name of a document's version info resource (`1057`): the
/// Photoshop release that saved the file last, e.g. "Adobe Photoshop CS6".
fn last_saved_by(resources: &psd_core::ImageResources) -> Option<String> {
    let data = resources.blocks().iter().find_map(|block| match block {
        psd_core::ResourceBlock::Raw(raw) if raw.id == 1057 => Some(raw.data.as_slice()),
        _ => None,
    })?;
    // Version (4 bytes), a has-real-merged-data flag, then two Unicode
    // strings: the writer's name and the reader's.
    let mut rest = data.get(5..)?;
    let mut read_string = || -> Option<String> {
        let count = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let bytes = rest.get(4..4 + count * 2)?;
        rest = &rest[4 + count * 2..];
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect();
        Some(String::from_utf16_lossy(&units))
    };
    read_string()?;
    read_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use psd_core::{ImageResources, PascalString, RawResourceBlock, ResourceBlock};

    fn unicode(text: &str) -> Vec<u8> {
        let units: Vec<u16> = text.encode_utf16().collect();
        let mut bytes = (units.len() as u32).to_be_bytes().to_vec();
        for unit in units {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
        bytes
    }

    #[test]
    fn the_reader_name_comes_from_the_version_info_resource() {
        let mut data = vec![0, 0, 0, 1, 1];
        data.extend(unicode("Adobe Photoshop"));
        data.extend(unicode("Adobe Photoshop CS6"));
        data.extend([0, 0, 0, 1]);
        let mut resources = ImageResources::new();
        assert_eq!(last_saved_by(&resources), None);
        resources.push(ResourceBlock::Raw(RawResourceBlock {
            id: 1057,
            name: PascalString::new("", 2),
            data,
        }));
        assert_eq!(
            last_saved_by(&resources).as_deref(),
            Some("Adobe Photoshop CS6")
        );
    }

    #[test]
    fn a_truncated_version_info_resource_has_no_reader_name() {
        let mut resources = ImageResources::new();
        resources.push(ResourceBlock::Raw(RawResourceBlock {
            id: 1057,
            name: PascalString::new("", 2),
            data: vec![0, 0, 0, 1, 1, 0, 0, 0, 9, 0, 65],
        }));
        assert_eq!(last_saved_by(&resources), None);
    }
}
