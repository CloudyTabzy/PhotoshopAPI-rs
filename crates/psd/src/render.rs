//! Channel resampling and RGB compositing for smart-object warps.
//!
//! This module accepts already-decoded planar channels. Linked JPEG/PNG/PSD/PSB
//! decoding and Smart Object replacement live in `smart_object`, keeping warp
//! math independent of image codecs.

use rayon::prelude::*;

use crate::channels::{ChannelKey, ChannelStore};
use crate::geometry::{Point2, QuadMesh};
use crate::{core::PsdError, core::Result};
use crate::{BitDepth, Warp};

pub(crate) const MAX_RASTER_BYTES: usize = 512 * 1024 * 1024;

/// Memory cap for the cached UV field in [`render_warped`]: one `Point2`
/// (16 bytes) per output pixel per supersample. Past this, the renderer falls
/// back to computing mesh lookups per channel.
const MAX_UV_FIELD_BYTES: usize = 64 * 1024 * 1024;

/// Owned planar raster with an integer document-space origin.
#[derive(Debug, Clone, PartialEq)]
pub struct Raster<T: BitDepth> {
    width: usize,
    height: usize,
    origin_x: i32,
    origin_y: i32,
    channels: ChannelStore<T>,
}

/// In-memory interpolation methods supported by channel resampling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interpolation {
    Nearest,
    Bilinear,
    Bicubic,
}

impl<T: BitDepth> Raster<T> {
    pub fn new(width: usize, height: usize) -> Result<Self> {
        Self::with_origin(width, height, 0, 0)
    }

    pub fn with_origin(width: usize, height: usize, origin_x: i32, origin_y: i32) -> Result<Self> {
        if width == 0
            || height == 0
            || width > i32::MAX as usize
            || height > i32::MAX as usize
            || width.checked_mul(height).is_none()
            || origin_x.checked_add(width as i32).is_none()
            || origin_y.checked_add(height as i32).is_none()
        {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "raster dimensions must fit document coordinates",
            });
        }
        Ok(Self {
            width,
            height,
            origin_x,
            origin_y,
            channels: ChannelStore::new(),
        })
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn origin(&self) -> (i32, i32) {
        (self.origin_x, self.origin_y)
    }

    pub fn channels(&self) -> &ChannelStore<T> {
        &self.channels
    }

    pub(crate) fn into_channels(self) -> ChannelStore<T> {
        self.channels
    }

    pub fn channel(&self, key: ChannelKey) -> Option<&[T]> {
        self.channels.get(key)
    }

    pub fn set_channel(&mut self, key: ChannelKey, data: Vec<T>) -> Result<()> {
        if data.len() != self.width * self.height {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "raster channel length does not match its dimensions",
            });
        }
        self.channels.insert(key, data);
        Ok(())
    }

    /// Sample a channel at a pixel-space coordinate. Bilinear sampling fades
    /// out-of-image taps to zero; bicubic sampling clamps taps to the edge.
    pub fn sample_channel(
        &self,
        key: ChannelKey,
        point: Point2,
        method: Interpolation,
    ) -> Option<T> {
        let plane = self.channels.get(key)?;
        if !point.is_finite() {
            return None;
        }
        Some(sample_plane(
            plane,
            self.width,
            self.height,
            point.x,
            point.y,
            method,
        ))
    }

    /// Resize every planar channel, preserving the raster's top-left document
    /// origin. The source's alpha and auxiliary planes follow the same filter.
    pub fn rescale(&self, width: usize, height: usize, method: Interpolation) -> Result<Self> {
        let mut output = Self::with_origin(width, height, self.origin_x, self.origin_y)?;
        for (key, source) in self.channels.iter() {
            let mut plane = vec![T::ZERO; width * height];
            plane
                .par_chunks_mut(width)
                .enumerate()
                .for_each(|(y, row)| {
                    for (x, pixel) in row.iter_mut().enumerate() {
                        *pixel = match method {
                            // Upstream's `round(x / width · source_width)`
                            // mapping; the index clamp fixes its out-of-bounds
                            // read when upscaling by more than 2×.
                            Interpolation::Nearest => sample_plane(
                                source,
                                self.width,
                                self.height,
                                (x as f64 / width as f64 * self.width as f64).round(),
                                (y as f64 / height as f64 * self.height as f64).round(),
                                method,
                            ),
                            Interpolation::Bilinear => T::from_f32(sample_bilinear_at(
                                source,
                                self.width,
                                self.height,
                                rescaled_center(x, width, self.width),
                                rescaled_center(y, height, self.height),
                                Edge::Clamp,
                            )),
                            Interpolation::Bicubic => T::from_f32(sample_bicubic_at(
                                source,
                                self.width,
                                self.height,
                                rescaled_center(x, width, self.width),
                                rescaled_center(y, height, self.height),
                            )),
                        };
                    }
                });
            output.set_channel(key, plane)?;
        }
        Ok(output)
    }

    fn validate_color_raster(&self) -> Result<()> {
        if !self.channels.keys().any(|key| key.index() >= 0)
            || self.channels.keys().any(ChannelKey::is_mask)
        {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "raster requires color channels and cannot contain mask channels",
            });
        }
        if self
            .channels
            .iter()
            .any(|(_, data)| data.len() != self.width * self.height)
        {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "raster channel dimensions do not match",
            });
        }
        Ok(())
    }
}

/// Supersampling and tessellation settings for [`render_warped`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WarpRenderOptions {
    /// Number of subpixel samples along each axis. The upstream default is 4.
    pub supersample: usize,
    /// Approximate source pixels per mesh segment. The upstream default is 20.
    pub mesh_resolution: usize,
}

impl Default for WarpRenderOptions {
    fn default() -> Self {
        Self {
            supersample: 4,
            mesh_resolution: 20,
        }
    }
}

/// Resample all source planes through `warp`, returning a layer-sized raster
/// whose origin is the rounded document-space center of the transformed mesh.
pub fn render_warped<T: BitDepth>(
    warp: &Warp,
    source: &Raster<T>,
    options: WarpRenderOptions,
) -> Result<Raster<T>> {
    source.validate_color_raster()?;
    if options.supersample == 0 || options.supersample > 16 || options.mesh_resolution == 0 {
        return Err(PsdError::InvalidData {
            offset: 0,
            message: "warp render options are outside the supported range",
        });
    }

    let mesh = warp.tessellated_mesh(
        (source.width / options.mesh_resolution).max(2),
        (source.height / options.mesh_resolution).max(2),
    )?;
    let bounds = mesh.bounds();
    let output_width = rounded_extent(bounds.width())?;
    let output_height = rounded_extent(bounds.height())?;
    let mut keys: Vec<_> = source.channels.keys().collect();
    if !keys.contains(&ChannelKey::ALPHA) {
        keys.push(ChannelKey::ALPHA);
    }
    keys.sort_unstable();
    let output_bytes = output_width
        .checked_mul(output_height)
        .and_then(|samples| samples.checked_mul(keys.len()))
        .and_then(|samples| samples.checked_mul(std::mem::size_of::<T>()))
        .ok_or(PsdError::InvalidData {
            offset: 0,
            message: "warp output raster size overflows addressable memory",
        })?;
    if output_bytes > MAX_RASTER_BYTES {
        return Err(PsdError::InvalidData {
            offset: 0,
            message: "warp output raster exceeds the 512 MiB limit",
        });
    }
    let center = bounds.center();
    // Upstream places the output at `round(center − extent/2)`
    // (`Util/CoordinateUtil.h` via `evaluate_transforms`); rounding the center
    // first and subtracting a floored half extent instead would drift odd
    // output widths by one document pixel.
    let mut output = Raster::with_origin(
        output_width,
        output_height,
        rounded_coordinate(center.x - output_width as f64 / 2.0)?,
        rounded_coordinate(center.y - output_height as f64 / 2.0)?,
    )?;

    // The UV field is channel-independent; computing the mesh lookup once and
    // caching it is ~nkeys× cheaper than the per-channel walk. The field is
    // `ss²` entries per output pixel; past the memory cap the renderer falls
    // back to per-channel mesh lookups.
    let samples_per_pixel = options.supersample * options.supersample;
    let uv_field_bytes = output_width
        .checked_mul(output_height)
        .and_then(|pixels| pixels.checked_mul(samples_per_pixel))
        .and_then(|samples| samples.checked_mul(std::mem::size_of::<Option<Point2>>()))
        .ok_or(PsdError::InvalidData {
            offset: 0,
            message: "warp UV field size overflows",
        })?;
    // Sample in the coordinates of the pixels actually written: the output is
    // placed at its rounded origin, so sampling from `bounds.min` (upstream)
    // misregisters the image by up to ~0.75 px whenever the mesh bounds are
    // fractional.
    let sample_base = Point2::new(f64::from(output.origin_x), f64::from(output.origin_y));
    let uv_field = if uv_field_bytes <= MAX_UV_FIELD_BYTES {
        let mut field = vec![None; output_width * output_height * samples_per_pixel];
        field
            .par_chunks_mut(output_width * samples_per_pixel)
            .enumerate()
            .for_each(|(y, row)| {
                for (x, slot) in row.chunks_exact_mut(samples_per_pixel).enumerate() {
                    for (s, uv) in slot.iter_mut().enumerate() {
                        *uv = mesh.uv_at(sample_position(
                            sample_base,
                            x,
                            y,
                            s % options.supersample,
                            s / options.supersample,
                            options.supersample,
                        ));
                    }
                }
            });
        Some(field)
    } else {
        None
    };

    for key in keys {
        let source_plane = source.channel(key);
        let plane = render_channel(
            &mesh,
            sample_base,
            source_plane,
            uv_field.as_deref(),
            RenderSpec {
                source_width: source.width,
                source_height: source.height,
                output_width,
                output_height,
                supersample: options.supersample,
            },
        );
        output.set_channel(key, plane)?;
    }
    Ok(output)
}

/// Composite `layer` over `canvas` using Photoshop's normal RGB kernel.
///
/// Color uses the same channel-domain kernel as upstream PhotoshopAPI
/// (`source + destination × (1 − source_alpha)`); alpha uses source-over.
/// Callers that need a transparent destination should provide an explicit
/// zero-filled alpha plane. A destination without alpha is treated as opaque.
///
/// Deviation from upstream: upstream iterates the *canvas's* channels and
/// skips channels the layer lacks; this port iterates the *layer's* color
/// channels and inserts a zero-filled canvas channel for any the canvas lacks,
/// so compositing onto a canvas without a layer's channel acquires it.
pub fn composite_rgb<T: BitDepth>(canvas: &mut Raster<T>, layer: &Raster<T>) -> Result<()> {
    canvas.validate_color_raster()?;
    layer.validate_color_raster()?;
    let pixel_count = canvas.width * canvas.height;

    if !canvas.channels.contains(ChannelKey::ALPHA) {
        canvas
            .channels
            .insert(ChannelKey::ALPHA, vec![T::from_f32(1.0); pixel_count]);
    }
    let layer_alpha = layer.channel(ChannelKey::ALPHA);

    let canvas_right = canvas.origin_x.saturating_add(canvas.width as i32);
    let canvas_bottom = canvas.origin_y.saturating_add(canvas.height as i32);
    let layer_right = layer.origin_x.saturating_add(layer.width as i32);
    let layer_bottom = layer.origin_y.saturating_add(layer.height as i32);
    let left = canvas.origin_x.max(layer.origin_x);
    let top = canvas.origin_y.max(layer.origin_y);
    let right = canvas_right.min(layer_right);
    let bottom = canvas_bottom.min(layer_bottom);
    if left >= right || top >= bottom {
        return Ok(());
    }

    let source_color_keys: Vec<_> = layer
        .channels
        .keys()
        .filter(|key| key.index() >= 0)
        .collect();
    for key in &source_color_keys {
        if !canvas.channels.contains(*key) {
            canvas.channels.insert(*key, vec![T::ZERO; pixel_count]);
        }
    }

    // Gather the planes once (one mutable walk over the canvas's channel map)
    // instead of a BTreeMap lookup per channel per pixel. The color kernel
    // (`source + destination × (1 − source_alpha)`) never reads destination
    // alpha, so each plane blends independently.
    let mut alpha_plane: Option<&mut [T]> = None;
    let mut color_planes: Vec<(&[T], &mut [T])> = Vec::with_capacity(source_color_keys.len());
    for (key, channel) in canvas.channels.iter_mut() {
        if key == ChannelKey::ALPHA {
            alpha_plane = Some(channel);
        } else if key.index() >= 0 {
            if let Some(source) = layer.channels.get(key) {
                color_planes.push((source, channel));
            }
        }
    }

    let overlap_pixels = (right - left) as usize * (bottom - top) as usize;
    let parallel = overlap_pixels >= 4096;
    let canvas_geometry = PlaneGeometry {
        origin_x: canvas.origin_x,
        origin_y: canvas.origin_y,
        width: canvas.width,
    };
    let layer_geometry = PlaneGeometry {
        origin_x: layer.origin_x,
        origin_y: layer.origin_y,
        width: layer.width,
    };

    for (source, destination) in color_planes {
        blend_rows(
            canvas_geometry,
            layer_geometry,
            Some(source),
            layer_alpha,
            destination,
            (left, top, right, bottom),
            parallel,
        );
    }
    blend_rows(
        canvas_geometry,
        layer_geometry,
        None,
        layer_alpha,
        alpha_plane.expect("alpha inserted above"),
        (left, top, right, bottom),
        parallel,
    );
    Ok(())
}

/// Origin and row stride of one raster plane, captured before the channel
/// map is mutably borrowed.
#[derive(Clone, Copy)]
struct PlaneGeometry {
    origin_x: i32,
    origin_y: i32,
    width: usize,
}

/// Blend the overlap region of one channel plane row-by-row. `source: None`
/// selects the source-over alpha kernel, which reads and writes the
/// destination alpha plane itself; `Some` selects the color kernel.
fn blend_rows<T: BitDepth>(
    canvas: PlaneGeometry,
    layer: PlaneGeometry,
    source: Option<&[T]>,
    layer_alpha: Option<&[T]>,
    destination: &mut [T],
    overlap: (i32, i32, i32, i32),
    parallel: bool,
) {
    let (left, top, right, bottom) = overlap;
    let blend_row = |(y, row): (i32, &mut [T])| {
        let layer_y = (y - layer.origin_y) as usize;
        for x in left..right {
            let canvas_x = (x - canvas.origin_x) as usize;
            let layer_x = (x - layer.origin_x) as usize;
            let layer_index = layer_y * layer.width + layer_x;
            let source_alpha = layer_alpha
                .map(|alpha| alpha[layer_index].to_f32().clamp(0.0, 1.0))
                .unwrap_or(1.0);
            match source {
                Some(source) => {
                    let result = source[layer_index].to_f32()
                        + row[canvas_x].to_f32() * (1.0 - source_alpha);
                    row[canvas_x] = T::from_f32(result);
                }
                None => {
                    let destination_alpha = row[canvas_x].to_f32().clamp(0.0, 1.0);
                    let alpha = source_alpha + destination_alpha * (1.0 - source_alpha);
                    row[canvas_x] = T::from_f32(alpha);
                }
            }
        }
    };

    if parallel {
        destination
            .par_chunks_mut(canvas.width)
            .enumerate()
            .for_each(|(canvas_y, row)| {
                let y = canvas_y as i32 + canvas.origin_y;
                if y < top || y >= bottom {
                    return;
                }
                blend_row((y, row));
            });
    } else {
        for (canvas_y, row) in destination.chunks_mut(canvas.width).enumerate() {
            let y = canvas_y as i32 + canvas.origin_y;
            if y < top || y >= bottom {
                continue;
            }
            blend_row((y, row));
        }
    }
}

#[derive(Clone, Copy)]
struct RenderSpec {
    source_width: usize,
    source_height: usize,
    output_width: usize,
    output_height: usize,
    supersample: usize,
}

fn render_channel<T: BitDepth>(
    mesh: &QuadMesh,
    sample_base: Point2,
    source: Option<&[T]>,
    uv_field: Option<&[Option<Point2>]>,
    spec: RenderSpec,
) -> Vec<T> {
    let mut output = vec![T::ZERO; spec.output_width * spec.output_height];
    let sample_count = (spec.supersample * spec.supersample) as f32;
    output
        .par_chunks_mut(spec.output_width)
        .enumerate()
        .for_each(|(y, row)| {
            for (x, pixel) in row.iter_mut().enumerate() {
                let mut accumulated = 0.0f32;
                for sample_y in 0..spec.supersample {
                    for sample_x in 0..spec.supersample {
                        let uv = match uv_field {
                            // Cached field: `ss²` entries per pixel.
                            Some(field) => {
                                field[(y * spec.output_width + x)
                                    * spec.supersample
                                    * spec.supersample
                                    + sample_y * spec.supersample
                                    + sample_x]
                            }
                            None => mesh.uv_at(sample_position(
                                sample_base,
                                x,
                                y,
                                sample_x,
                                sample_y,
                                spec.supersample,
                            )),
                        };
                        if let Some(uv) = uv {
                            accumulated += match source {
                                Some(plane) => sample_bilinear_uv(
                                    plane,
                                    spec.source_width,
                                    spec.source_height,
                                    uv,
                                ),
                                // A missing source alpha plane is fully opaque
                                // (deviation: upstream warps a
                                // synthetic max-alpha plane whose bilinear taps
                                // fade at the source border; treating it as a
                                // constant 1 is both cheaper and closer to what
                                // Photoshop does with an opaque object).
                                None => 1.0,
                            };
                        }
                    }
                }
                *pixel = T::from_f32((accumulated / sample_count).clamp(0.0, 1.0));
            }
        });
    output
}

/// Document-space position of subsample (`sample_x`, `sample_y`) of output
/// pixel (`x`, `y`), where `base` is the output raster's origin.
///
/// Subsamples sit at the centers of an `supersample`² grid inside the pixel.
/// Upstream anchors the grid at the pixel's top-left corner, which biases every
/// render by `1/(2·supersample)` px up and left and makes edge coverage
/// asymmetric; against the Photoshop reference renders the
/// centered grid plus origin-aligned sampling lowers the summed mean error by
/// about 10%.
fn sample_position(
    base: Point2,
    x: usize,
    y: usize,
    sample_x: usize,
    sample_y: usize,
    supersample: usize,
) -> Point2 {
    let step = 1.0 / supersample as f64;
    Point2::new(
        base.x + x as f64 + (sample_x as f64 + 0.5) * step,
        base.y + y as f64 + (sample_y as f64 + 0.5) * step,
    )
}

/// Bilinear sample at normalized source coordinates; taps outside the
/// source fade to zero so warped edges antialias against transparency.
fn sample_bilinear_uv<T: BitDepth>(plane: &[T], width: usize, height: usize, uv: Point2) -> f32 {
    sample_bilinear_at(
        plane,
        width,
        height,
        uv.x * width as f64 - 0.5,
        uv.y * height as f64 - 0.5,
        Edge::Zero,
    )
}

/// Source pixel-center coordinate of output pixel `index` when resizing
/// `source` samples to `target`.
///
/// Upstream's `rescale_bilinear`/`rescale_bicubic` use `index / target ·
/// source − 0.5`, which drops the output pixel's own half-pixel center: every
/// resize is shifted half an output pixel up and left, and even a same-size
/// resize blurs.
fn rescaled_center(index: usize, target: usize, source: usize) -> f64 {
    (index as f64 + 0.5) * source as f64 / target as f64 - 0.5
}

/// How bilinear taps outside the plane are resolved.
#[derive(Clone, Copy)]
enum Edge {
    /// Out-of-plane taps read zero (warps and `Raster::sample_channel`, as
    /// upstream `sample_bilinear`).
    Zero,
    /// Out-of-plane taps repeat the nearest edge pixel (resizing, as upstream
    /// `rescale_bilinear`'s edge-clamped `get_matrix`).
    Clamp,
}

fn sample_plane<T: BitDepth>(
    plane: &[T],
    width: usize,
    height: usize,
    x: f64,
    y: f64,
    method: Interpolation,
) -> T {
    match method {
        Interpolation::Nearest => {
            let x = x.round().clamp(0.0, (width - 1) as f64) as usize;
            let y = y.round().clamp(0.0, (height - 1) as f64) as usize;
            plane[y * width + x]
        }
        Interpolation::Bilinear => {
            T::from_f32(sample_bilinear_at(plane, width, height, x, y, Edge::Zero))
        }
        Interpolation::Bicubic => T::from_f32(sample_bicubic_at(plane, width, height, x, y)),
    }
}

fn sample_bilinear_at<T: BitDepth>(
    plane: &[T],
    width: usize,
    height: usize,
    x: f64,
    y: f64,
    edge: Edge,
) -> f32 {
    let x0 = x.floor() as isize;
    let y0 = y.floor() as isize;
    let dx = (x - x0 as f64) as f32;
    let dy = (y - y0 as f64) as f32;
    let sample = |sx: isize, sy: isize| -> f32 {
        let inside = sx >= 0 && sy >= 0 && sx < width as isize && sy < height as isize;
        match edge {
            _ if inside => plane[sy as usize * width + sx as usize].to_f32(),
            Edge::Zero => 0.0,
            Edge::Clamp => {
                let sx = sx.clamp(0, width as isize - 1) as usize;
                let sy = sy.clamp(0, height as isize - 1) as usize;
                plane[sy * width + sx].to_f32()
            }
        }
    };
    let p00 = sample(x0, y0);
    let p10 = sample(x0 + 1, y0);
    let p01 = sample(x0, y0 + 1);
    let p11 = sample(x0 + 1, y0 + 1);
    let top = p00 + dx * (p10 - p00);
    let bottom = p01 + dx * (p11 - p01);
    (top + dy * (bottom - top)).clamp(0.0, 1.0)
}

fn sample_bicubic_at<T: BitDepth>(plane: &[T], width: usize, height: usize, x: f64, y: f64) -> f32 {
    let x_floor = x.floor();
    let y_floor = y.floor();
    let x0 = x_floor as isize;
    let y0 = y_floor as isize;
    let tx = (x - x_floor) as f32;
    let ty = (y - y_floor) as f32;
    let sample = |sx: isize, sy: isize| -> f32 {
        let sx = sx.clamp(0, width as isize - 1) as usize;
        let sy = sy.clamp(0, height as isize - 1) as usize;
        plane[sy * width + sx].to_f32()
    };
    let mut rows = [0.0f32; 4];
    for (row, value) in rows.iter_mut().enumerate() {
        let sy = y0 + row as isize - 1;
        *value = cubic_hermite(
            sample(x0 - 1, sy),
            sample(x0, sy),
            sample(x0 + 1, sy),
            sample(x0 + 2, sy),
            tx,
        );
    }
    cubic_hermite(rows[0], rows[1], rows[2], rows[3], ty).clamp(0.0, 1.0)
}

fn cubic_hermite(a: f32, b: f32, c: f32, d: f32, t: f32) -> f32 {
    let coefficient_a = -a / 2.0 + 3.0 * b / 2.0 - 3.0 * c / 2.0 + d / 2.0;
    let coefficient_b = a - 5.0 * b / 2.0 + 2.0 * c - d / 2.0;
    let coefficient_c = -a / 2.0 + c / 2.0;
    coefficient_a * t * t * t + coefficient_b * t * t + coefficient_c * t + b
}

fn rounded_extent(value: f64) -> Result<usize> {
    if !value.is_finite() || value <= 0.0 || value.round() > usize::MAX as f64 {
        return Err(PsdError::InvalidData {
            offset: 0,
            message: "warp output extent is invalid",
        });
    }
    Ok(value.round() as usize)
}

fn rounded_coordinate(value: f64) -> Result<i32> {
    if !value.is_finite() || value.round() < i32::MIN as f64 || value.round() > i32::MAX as f64 {
        return Err(PsdError::InvalidData {
            offset: 0,
            message: "warp output center does not fit document coordinates",
        });
    }
    Ok(value.round() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_warp_resamples_all_planes_and_adds_opaque_alpha() {
        let mut source = Raster::<u8>::new(8, 8).unwrap();
        source
            .set_channel(ChannelKey::color(0), vec![128; 64])
            .unwrap();
        source
            .set_channel(ChannelKey::color(1), vec![64; 64])
            .unwrap();
        source
            .set_channel(ChannelKey::color(2), vec![32; 64])
            .unwrap();

        let warp = Warp::identity(8, 8).unwrap();
        let output = render_warped(&warp, &source, WarpRenderOptions::default()).unwrap();
        assert_eq!(
            warp.apply(&source, WarpRenderOptions::default()).unwrap(),
            output
        );
        assert_eq!(
            (output.width(), output.height(), output.origin()),
            (8, 8, (0, 0))
        );
        assert_eq!(output.channel(ChannelKey::ALPHA).unwrap().len(), 64);
        assert!(output.channel(ChannelKey::color(0)).unwrap()[3 * 8 + 3] >= 127);
        assert!(output.channel(ChannelKey::ALPHA).unwrap()[3 * 8 + 3] >= 254);
    }

    #[test]
    fn oversized_warp_output_is_rejected_before_channel_allocation() {
        let mut source = Raster::<u8>::new(1, 1).unwrap();
        for key in [
            ChannelKey::color(0),
            ChannelKey::color(1),
            ChannelKey::color(2),
        ] {
            source.set_channel(key, vec![0]).unwrap();
        }

        let mut warp = Warp::generate_default(1, 1).unwrap();
        let large_quad = [
            Point2::new(0.0, 0.0),
            Point2::new(30_000.0, 0.0),
            Point2::new(0.0, 30_000.0),
            Point2::new(30_000.0, 30_000.0),
        ];
        warp.set_affine_transform(large_quad).unwrap();
        warp.set_non_affine_transform(large_quad).unwrap();

        assert!(matches!(
            render_warped(&warp, &source, WarpRenderOptions::default()),
            Err(PsdError::InvalidData {
                message: "warp output raster exceeds the 512 MiB limit",
                ..
            })
        ));
    }

    fn assert_identity_depth<T: BitDepth>(sample: T) {
        let mut source = Raster::<T>::new(8, 8).unwrap();
        for index in 0..3 {
            source
                .set_channel(ChannelKey::color(index), vec![sample; 64])
                .unwrap();
        }
        let output = render_warped(
            &Warp::identity(8, 8).unwrap(),
            &source,
            WarpRenderOptions::default(),
        )
        .unwrap();
        let center = output.channel(ChannelKey::color(0)).unwrap()[3 * 8 + 3].to_f32();
        assert!((center - sample.to_f32()).abs() < 0.005);
        assert!(output.channel(ChannelKey::ALPHA).unwrap()[3 * 8 + 3].to_f32() > 0.995);
    }

    #[test]
    fn warp_resampling_supports_all_photoshop_sample_depths() {
        assert_identity_depth::<u8>(128);
        assert_identity_depth::<u16>(32768);
        assert_identity_depth::<f32>(0.5);
    }

    #[test]
    fn composite_respects_document_origins_and_source_alpha() {
        let mut canvas = Raster::<u8>::with_origin(3, 3, 0, 0).unwrap();
        for key in [
            ChannelKey::color(0),
            ChannelKey::color(1),
            ChannelKey::color(2),
        ] {
            canvas.set_channel(key, vec![0; 9]).unwrap();
        }
        canvas.set_channel(ChannelKey::ALPHA, vec![0; 9]).unwrap();

        let mut layer = Raster::<u8>::with_origin(2, 2, 1, 1).unwrap();
        layer
            .set_channel(ChannelKey::color(0), vec![200; 4])
            .unwrap();
        layer
            .set_channel(ChannelKey::color(1), vec![80; 4])
            .unwrap();
        layer
            .set_channel(ChannelKey::color(2), vec![40; 4])
            .unwrap();
        layer.set_channel(ChannelKey::ALPHA, vec![128; 4]).unwrap();

        composite_rgb(&mut canvas, &layer).unwrap();
        let covered = 1 + 3;
        assert_eq!(canvas.channel(ChannelKey::color(0)).unwrap()[covered], 200);
        assert_eq!(canvas.channel(ChannelKey::ALPHA).unwrap()[covered], 128);
        assert_eq!(canvas.channel(ChannelKey::ALPHA).unwrap()[0], 0);
    }

    #[test]
    fn raster_sampling_and_rescale_support_nearest_bilinear_and_bicubic() {
        let mut raster = Raster::<u8>::with_origin(4, 4, -2, 3).unwrap();
        let values = (0..4)
            .flat_map(|y| (0..4).map(move |x| (x + 4 * y) as u8))
            .collect::<Vec<_>>();
        raster
            .set_channel(ChannelKey::color(0), values.clone())
            .unwrap();
        assert_eq!(
            raster.sample_channel(
                ChannelKey::color(0),
                Point2::new(1.5, 1.5),
                Interpolation::Nearest
            ),
            Some(10)
        );
        assert_eq!(
            raster.sample_channel(
                ChannelKey::color(0),
                Point2::new(1.5, 1.5),
                Interpolation::Bilinear
            ),
            Some(8)
        );
        assert_eq!(
            raster.sample_channel(
                ChannelKey::color(0),
                Point2::new(1.5, 1.5),
                Interpolation::Bicubic
            ),
            Some(8)
        );

        let resized = raster.rescale(2, 2, Interpolation::Nearest).unwrap();
        assert_eq!(resized.origin(), (-2, 3));
        assert_eq!(
            resized.channel(ChannelKey::color(0)).unwrap(),
            [0, 2, 8, 10]
        );
        let smooth = raster.rescale(7, 7, Interpolation::Bicubic).unwrap();
        assert_eq!((smooth.width(), smooth.height()), (7, 7));
        assert!(smooth
            .channel(ChannelKey::color(0))
            .unwrap()
            .iter()
            .all(|v| *v <= 15));

        // Nearest upscaling by more than 2x must not read past the source
        // (upstream's unclamped `round(u * width)` index).
        let wide = raster.rescale(9, 9, Interpolation::Nearest).unwrap();
        assert_eq!(wide.channel(ChannelKey::color(0)).unwrap()[80], 15);
    }

    #[test]
    fn filtered_rescale_is_centered_and_keeps_edges() {
        let mut ramp = Raster::<u8>::new(4, 4).unwrap();
        let values = (0..16).map(|value| value * 16).collect::<Vec<u8>>();
        ramp.set_channel(ChannelKey::color(0), values.clone())
            .unwrap();
        for method in [Interpolation::Bilinear, Interpolation::Bicubic] {
            // A same-size resize is the identity: no half-pixel shift, no blur.
            let same = ramp.rescale(4, 4, method).unwrap();
            assert_eq!(same.channel(ChannelKey::color(0)).unwrap(), values);
        }

        // Edges repeat instead of fading to black when enlarging.
        let mut flat = Raster::<u8>::new(3, 2).unwrap();
        flat.set_channel(ChannelKey::color(0), vec![200; 6])
            .unwrap();
        for method in [Interpolation::Bilinear, Interpolation::Bicubic] {
            let large = flat.rescale(9, 7, method).unwrap();
            assert!(large
                .channel(ChannelKey::color(0))
                .unwrap()
                .iter()
                .all(|value| *value == 200));
        }

        // Enlarging a two-pixel edge stays symmetric about the image center.
        let mut edge = Raster::<u8>::new(2, 1).unwrap();
        edge.set_channel(ChannelKey::color(0), vec![0, 255])
            .unwrap();
        let wide = edge.rescale(4, 1, Interpolation::Bilinear).unwrap();
        let wide = wide.channel(ChannelKey::color(0)).unwrap();
        assert_eq!((wide[0], wide[3]), (0, 255));
        for index in 0..4 {
            assert!(
                (i32::from(wide[index]) + i32::from(wide[3 - index]) - 255).abs() <= 1,
                "{wide:?}"
            );
        }
    }

    /// Brightness-weighted mean document x of row `row`, using pixel centers.
    fn document_centroid_x(raster: &Raster<u8>, row: usize) -> f64 {
        let plane = raster.channel(ChannelKey::color(0)).unwrap();
        let line = &plane[row * raster.width()..(row + 1) * raster.width()];
        let total: f64 = line.iter().map(|value| f64::from(*value)).sum();
        let weighted: f64 = line
            .iter()
            .enumerate()
            .map(|(x, value)| (x as f64 + 0.5) * f64::from(*value))
            .sum();
        f64::from(raster.origin().0) + weighted / total
    }

    #[test]
    fn warp_render_is_registered_to_its_output_origin() {
        // A one-pixel bright column centered at source x = 3.5.
        let mut source = Raster::<u8>::new(8, 8).unwrap();
        let column = (0..64)
            .map(|index| if index % 8 == 3 { 255 } else { 0 })
            .collect::<Vec<u8>>();
        for index in 0..3 {
            source
                .set_channel(ChannelKey::color(index), column.clone())
                .unwrap();
        }

        for offset in [0.0, 0.5, 2.25, -1.75] {
            let mut warp = Warp::identity(8, 8).unwrap();
            warp.translate(Point2::new(offset, 0.0)).unwrap();
            let output = render_warped(&warp, &source, WarpRenderOptions::default()).unwrap();
            // The supersampled render is a symmetric filter: the column's
            // centroid lands exactly where the placement puts it, even when
            // the placement is fractional and the output origin is rounded.
            let centroid = document_centroid_x(&output, 4);
            assert!(
                (centroid - (3.5 + offset)).abs() < 1e-3,
                "offset {offset}: centroid {centroid}, origin {:?}",
                output.origin()
            );
        }
    }
}
