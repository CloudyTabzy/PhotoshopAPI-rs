//! Depth-generic implementations of the `Layer_*bit` base API and the image
//! data accessors. The per-depth classes in `classes/` forward here.

use numpy::{Element, PyArray2};
use psd::core::{ColorMode, LayerMask};
use psd::geometry::Point2;
use psd::{BitDepth, ChannelKey, Layer, LayerKind, Rect};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::convert::{
    blend_mode_from_py, blend_mode_to_py, byte_value, compression_from_py, layer_color_from_py,
    layer_color_to_py, point_from_py, point_to_py, truthy,
};
use crate::numpy_io::{
    channel_array, channel_dimensions, channel_key_from_py, channel_plane, empty_array,
    image_channels, image_channels_mut, mask_array, parse_image,
};
use crate::state::{psd_error, LayerHandle};

/// What a Python wrapper class a layer gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Image,
    Group,
    Text,
    SmartObject,
    Divider,
}

pub fn kind_of<T: BitDepth>(layer: &Layer<T>) -> Kind {
    if layer.is_smart_object() {
        Kind::SmartObject
    } else if layer.is_text_layer() {
        Kind::Text
    } else {
        match layer.kind {
            LayerKind::Image(_) => Kind::Image,
            LayerKind::Text(_) => Kind::Text,
            LayerKind::Group(_) => Kind::Group,
            LayerKind::SectionDivider(_) => Kind::Divider,
        }
    }
}

/// Photoshop layer names are limited to 255 characters (upstream checks
/// this in its constructors; the port checks renames too).
pub fn check_name(name: &str) -> PyResult<()> {
    if name.chars().count() > 255 {
        return Err(PyValueError::new_err(
            "layer names cannot exceed 255 characters",
        ));
    }
    Ok(())
}

/// Settings every layer constructor accepts.
pub struct CommonOptions<'py> {
    pub opacity: f32,
    pub blend_mode: Option<Bound<'py, PyAny>>,
    pub compression: Option<Bound<'py, PyAny>>,
    pub is_visible: bool,
    pub is_locked: bool,
}

impl CommonOptions<'_> {
    /// Apply the options to a new layer (upstream `Layer::Params`).
    pub fn apply<T: BitDepth>(&self, py: Python<'_>, layer: &mut Layer<T>) -> PyResult<()> {
        if !self.opacity.is_finite() || !(0.0..=1.0).contains(&self.opacity) {
            return Err(PyValueError::new_err(format!(
                "opacity must be between 0-1, got {}",
                self.opacity
            )));
        }
        layer.opacity = (self.opacity * 255.0).round() as u8;
        if let Some(mode) = &self.blend_mode {
            let mode = blend_mode_from_py(mode)?;
            if mode == psd::core::BlendMode::PASSTHROUGH && layer.group().is_none() {
                warn(
                    py,
                    "the Passthrough blend mode is reserved for groups; using Normal instead",
                )?;
            } else {
                layer.blend_mode = mode;
            }
        }
        if let Some(compression) = &self.compression {
            layer.compression = Some(compression_from_py(compression)?);
        }
        layer.set_visible(self.is_visible);
        if self.is_locked {
            layer.set_locked(true);
        }
        Ok(())
    }
}

/// Python `warnings.warn(message)`.
pub fn warn(py: Python<'_>, message: &str) -> PyResult<()> {
    PyModule::import(py, "warnings")?.call_method1("warn", (message,))?;
    Ok(())
}

/// A `0..=1` fraction as a byte; out-of-range values are clamped with a
/// warning like upstream's `opacity`/`fill` setters.
fn unit_to_byte(py: Python<'_>, value: f32, what: &str) -> PyResult<u8> {
    if !value.is_finite() {
        return Err(PyValueError::new_err(format!("{what} must be finite")));
    }
    if !(0.0..=1.0).contains(&value) {
        warn(
            py,
            &format!("{what} {value} is outside 0-1; clamping it to that range"),
        )?;
    }
    Ok((value.clamp(0.0, 1.0) * 255.0).round() as u8)
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

pub fn blend_mode<T: BitDepth>(py: Python<'_>, handle: &LayerHandle<T>) -> PyResult<Py<PyAny>> {
    let mode = handle.with_layer(|layer| Ok(layer.blend_mode))?;
    blend_mode_to_py(py, mode)
}

pub fn set_blend_mode<T: BitDepth>(
    py: Python<'_>,
    handle: &LayerHandle<T>,
    value: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let mode = blend_mode_from_py(value)?;
    let is_group = handle.with_layer(|layer| Ok(layer.group().is_some()))?;
    if mode == psd::core::BlendMode::PASSTHROUGH && !is_group {
        warn(py, "the Passthrough blend mode is reserved for groups")?;
    }
    handle.with_layer_mut(|layer| {
        layer.blend_mode = mode;
        Ok(())
    })
}

pub fn opacity<T: BitDepth>(handle: &LayerHandle<T>) -> PyResult<f32> {
    handle.with_layer(|layer| Ok(f32::from(layer.opacity) / 255.0))
}

pub fn set_opacity<T: BitDepth>(
    py: Python<'_>,
    handle: &LayerHandle<T>,
    value: f32,
) -> PyResult<()> {
    let opacity = unit_to_byte(py, value, "opacity")?;
    handle.with_layer_mut(|layer| {
        layer.opacity = opacity;
        Ok(())
    })
}

pub fn fill<T: BitDepth>(handle: &LayerHandle<T>) -> PyResult<f32> {
    handle.with_layer(|layer| Ok(f32::from(layer.fill()) / 255.0))
}

pub fn set_fill<T: BitDepth>(py: Python<'_>, handle: &LayerHandle<T>, value: f32) -> PyResult<()> {
    let fill = unit_to_byte(py, value, "fill")?;
    handle.with_layer_mut(|layer| {
        layer.set_fill(fill);
        Ok(())
    })
}

pub fn set_flag<T: BitDepth>(
    handle: &LayerHandle<T>,
    value: &Bound<'_, PyAny>,
    apply: impl FnOnce(&mut Layer<T>, bool),
) -> PyResult<()> {
    let value = truthy(value)?;
    handle.with_layer_mut(|layer| {
        apply(layer, value);
        Ok(())
    })
}

pub fn display_color<T: BitDepth>(py: Python<'_>, handle: &LayerHandle<T>) -> PyResult<Py<PyAny>> {
    let color = handle.with_layer(|layer| Ok(layer.display_color()))?;
    layer_color_to_py(py, color)
}

pub fn set_display_color<T: BitDepth>(
    handle: &LayerHandle<T>,
    value: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let color = layer_color_from_py(value)?;
    handle.with_layer_mut(|layer| {
        layer.set_display_color(color);
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Geometry. Pixel layers keep upstream's semantics: `width`/`height` resize
// the bounds around the center and the channels must be replaced afterwards
// (upstream's RescaleCanvas example relies on this); smart objects scale and
// move their warp instead.
// ---------------------------------------------------------------------------

pub fn width<T: BitDepth>(handle: &LayerHandle<T>) -> PyResult<i64> {
    handle.with_layer(|layer| Ok(i64::from(layer_rect(layer).width().max(0))))
}

pub fn height<T: BitDepth>(handle: &LayerHandle<T>) -> PyResult<i64> {
    handle.with_layer(|layer| Ok(i64::from(layer_rect(layer).height().max(0))))
}

/// A group's size and position come from its mask when its record is empty
/// (upstream derives group extents the same way).
fn layer_rect<T: BitDepth>(layer: &Layer<T>) -> Rect {
    match (
        layer.group(),
        layer.bounds.sample_count(),
        layer.mask_rect(),
    ) {
        (Some(_), 0, Some(mask)) => mask,
        _ => layer.bounds,
    }
}

fn to_u32(value: i64, what: &str) -> PyResult<u32> {
    u32::try_from(value)
        .ok()
        .filter(|&value| value <= 300_000)
        .ok_or_else(|| PyValueError::new_err(format!("{what} must be between 0 and 300,000")))
}

pub fn set_size<T: BitDepth>(
    handle: &LayerHandle<T>,
    width: Option<i64>,
    height: Option<i64>,
) -> PyResult<()> {
    if handle.with_layer(|layer| Ok(layer.is_smart_object()))? {
        let width = width.map(|value| to_u32(value, "width")).transpose()?;
        let height = height.map(|value| to_u32(value, "height")).transpose()?;
        let (width, height) = (width.map(f64::from), height.map(f64::from));
        return handle.with_smart_object(
            |file, id| {
                file.resize_smart_object(id, width, height)
                    .map_err(psd_error)
            },
            |file, layer| {
                file.edit_smart_object_layer_warp(layer, |warp| warp.resize(width, height))
                    .map_err(psd_error)
            },
        );
    }
    handle.with_layer_mut(|layer| {
        let current = layer.bounds;
        let (center_x, center_y) = current.center();
        let width = match width {
            Some(value) => to_u32(value, "width")?,
            None => current.width().max(0) as u32,
        };
        let height = match height {
            Some(value) => to_u32(value, "height")?,
            None => current.height().max(0) as u32,
        };
        layer.bounds = Rect::centered(center_x, center_y, width, height).map_err(psd_error)?;
        Ok(())
    })
}

pub fn center<T: BitDepth>(handle: &LayerHandle<T>) -> PyResult<(f64, f64)> {
    handle.with_layer(|layer| Ok(layer.center()))
}

pub fn set_center<T: BitDepth>(
    handle: &LayerHandle<T>,
    x: Option<f64>,
    y: Option<f64>,
) -> PyResult<()> {
    for value in [x, y].into_iter().flatten() {
        if !value.is_finite() {
            return Err(PyValueError::new_err("layer centers must be finite"));
        }
    }
    if handle.with_layer(|layer| Ok(layer.is_smart_object()))? {
        return handle.with_smart_object(
            |file, id| file.center_smart_object(id, x, y).map_err(psd_error),
            |file, layer| {
                file.edit_smart_object_layer_warp(layer, |warp| warp.recenter(x, y))
                    .map_err(psd_error)
            },
        );
    }
    handle.with_layer_mut(|layer| {
        let (current_x, current_y) = layer.center();
        let dx = x.map_or(0.0, |x| x - current_x).round();
        let dy = y.map_or(0.0, |y| y - current_y).round();
        if dx.abs() > f64::from(i32::MAX) || dy.abs() > f64::from(i32::MAX) {
            return Err(PyValueError::new_err("layer center is out of range"));
        }
        layer.translate(dx as i32, dy as i32).map_err(psd_error)
    })
}

// ---------------------------------------------------------------------------
// Pixel mask (upstream `MaskMixin`)
// ---------------------------------------------------------------------------

pub fn mask<'py, T: BitDepth + Element>(
    py: Python<'py>,
    handle: &LayerHandle<T>,
) -> PyResult<Bound<'py, PyAny>> {
    handle.with_layer(|layer| match (layer.mask_pixels(), layer.mask_rect()) {
        (Some(pixels), Some(rect)) => Ok(channel_array(
            py,
            pixels,
            rect.height().max(0) as usize,
            rect.width().max(0) as usize,
        )?
        .into_any()),
        _ => Ok(empty_array::<T>(py).into_any()),
    })
}

/// Set the pixel mask from a 2-D array. A new mask sits at the canvas
/// origin; replacing a mask keeps its center (upstream `set_mask`).
pub fn set_mask<T: BitDepth + Element>(
    handle: &LayerHandle<T>,
    value: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let (pixels, height, width) = mask_array::<T>(value)?;
    handle.with_layer_mut(|layer| install_mask(layer, pixels, height, width))
}

fn install_mask<T: BitDepth>(
    layer: &mut Layer<T>,
    pixels: Vec<T>,
    height: usize,
    width: usize,
) -> PyResult<()> {
    let (center_x, center_y) = match layer.mask_rect() {
        Some(rect) if layer.has_mask() => rect.center(),
        _ => (width as f64 / 2.0, height as f64 / 2.0),
    };
    let size = |value: usize| {
        u32::try_from(value).map_err(|_| PyValueError::new_err("mask dimensions are too large"))
    };
    let rect =
        Rect::centered(center_x, center_y, size(width)?, size(height)?).map_err(psd_error)?;
    layer.set_mask(pixels, rect).map_err(psd_error)
}

fn mask_record<T: BitDepth>(layer: &Layer<T>) -> Option<LayerMask> {
    layer
        .has_mask()
        .then(|| layer.mask_record().copied())
        .flatten()
}

pub fn mask_flags<T: BitDepth>(handle: &LayerHandle<T>) -> PyResult<(bool, bool, u8)> {
    handle.with_layer(|layer| {
        Ok(mask_record(layer).map_or((false, false, 255), |record| {
            (
                record.flags.disabled(),
                record.flags.position_relative_to_layer(),
                record.default_color,
            )
        }))
    })
}

pub fn edit_mask<T: BitDepth>(
    handle: &LayerHandle<T>,
    edit: impl FnOnce(&mut Layer<T>) -> psd::core::Result<()>,
) -> PyResult<()> {
    handle.with_layer_mut(|layer| {
        if !layer.has_mask() {
            return Err(PyValueError::new_err(
                "the layer has no mask; assign `mask` first",
            ));
        }
        edit(layer).map_err(psd_error)
    })
}

pub fn set_mask_flag<T: BitDepth>(
    handle: &LayerHandle<T>,
    value: &Bound<'_, PyAny>,
    disabled: bool,
) -> PyResult<()> {
    let value = truthy(value)?;
    edit_mask(handle, |layer| {
        if disabled {
            layer.set_mask_disabled(value)
        } else {
            layer.set_mask_relative_to_layer(value)
        }
    })
}

pub fn set_mask_default_color<T: BitDepth>(
    handle: &LayerHandle<T>,
    value: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let color = byte_value(value)?;
    edit_mask(handle, |layer| layer.set_mask_default_color(color))
}

pub fn mask_density<T: BitDepth>(handle: &LayerHandle<T>) -> PyResult<Option<u8>> {
    handle.with_layer(|layer| Ok(layer.mask_density()))
}

pub fn set_mask_density<T: BitDepth>(
    handle: &LayerHandle<T>,
    value: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let density = if value.is_none() {
        None
    } else {
        Some(byte_value(value)?)
    };
    edit_mask(handle, |layer| layer.set_mask_density(density))
}

pub fn mask_feather<T: BitDepth>(handle: &LayerHandle<T>) -> PyResult<Option<f64>> {
    handle.with_layer(|layer| Ok(layer.mask_feather()))
}

pub fn set_mask_feather<T: BitDepth>(handle: &LayerHandle<T>, value: Option<f64>) -> PyResult<()> {
    edit_mask(handle, |layer| layer.set_mask_feather(value))
}

/// The mask's center, or `(-1, -1)` without a mask (upstream's value).
pub fn mask_position<T: BitDepth>(py: Python<'_>, handle: &LayerHandle<T>) -> PyResult<Py<PyAny>> {
    let center = handle.with_layer(|layer| {
        Ok(match (layer.has_mask(), layer.mask_rect()) {
            (true, Some(rect)) => rect.center(),
            _ => (-1.0, -1.0),
        })
    })?;
    point_to_py(py, Point2::new(center.0, center.1))
}

pub fn set_mask_position<T: BitDepth>(
    handle: &LayerHandle<T>,
    value: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let point = point_from_py(value)?;
    edit_mask(handle, |layer| layer.set_mask_center(point.x, point.y))
}

pub fn mask_size<T: BitDepth>(handle: &LayerHandle<T>) -> PyResult<(i64, i64)> {
    handle.with_layer(|layer| {
        Ok(match (layer.has_mask(), layer.mask_rect()) {
            (true, Some(rect)) => (
                i64::from(rect.width().max(0)),
                i64::from(rect.height().max(0)),
            ),
            _ => (0, 0),
        })
    })
}

pub fn set_mask_compression<T: BitDepth>(
    handle: &LayerHandle<T>,
    value: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let compression = compression_from_py(value)?;
    handle.with_layer_mut(|layer| {
        layer.mask_compression = Some(compression);
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Image data (upstream `ImageDataMixin` / `WritableImageDataMixin`)
// ---------------------------------------------------------------------------

pub fn channel_indices<T: BitDepth>(
    handle: &LayerHandle<T>,
    include_mask: bool,
) -> PyResult<Vec<i16>> {
    handle.with_layer(|layer| {
        Ok(image_channels(layer)?
            .keys()
            .filter(|key| include_mask || !key.is_mask())
            .map(ChannelKey::index)
            .collect())
    })
}

pub fn get_image_data<'py, T: BitDepth + Element>(
    py: Python<'py>,
    handle: &LayerHandle<T>,
) -> PyResult<Bound<'py, PyDict>> {
    let result = PyDict::new(py);
    handle.with_layer(|layer| {
        for (key, data) in image_channels(layer)?.iter() {
            let (height, width) = channel_dimensions(layer, key)?;
            result.set_item(key.index(), channel_array(py, data, height, width)?)?;
        }
        Ok(())
    })?;
    Ok(result)
}

pub fn get_channel<'py, T: BitDepth + Element>(
    py: Python<'py>,
    handle: &LayerHandle<T>,
    key: ChannelKey,
) -> PyResult<Bound<'py, PyArray2<T>>> {
    handle.with_layer(|layer| {
        // Upstream raises `ValueError` for a channel the layer lacks.
        let data = image_channels(layer)?.get(key).ok_or_else(|| {
            PyValueError::new_err(format!("the layer has no channel {}", key.index()))
        })?;
        let (height, width) = channel_dimensions(layer, key)?;
        channel_array(py, data, height, width)
    })
}

/// Replace one channel; the mask (`-2`) takes its size from the array.
pub fn set_channel<T: BitDepth + Element>(
    handle: &LayerHandle<T>,
    key: ChannelKey,
    value: &Bound<'_, PyAny>,
) -> PyResult<()> {
    if key == ChannelKey::USER_MASK {
        return set_mask(handle, value);
    }
    let (height, width) = handle.with_layer(|layer| channel_dimensions(layer, key))?;
    let pixels = channel_plane::<T>(value, height, width)?;
    handle.with_layer_mut(|layer| {
        image_channels_mut(layer)?.insert(key, pixels);
        Ok(())
    })
}

/// Replace every channel (upstream `set_image_data`); with `width` and
/// `height` the layer is resized around its center first.
pub fn set_image_data<T: BitDepth + Element>(
    handle: &LayerHandle<T>,
    color_mode: ColorMode,
    value: &Bound<'_, PyAny>,
    width: Option<i64>,
    height: Option<i64>,
) -> PyResult<()> {
    match (width, height) {
        (Some(width), Some(height)) => set_size(handle, Some(width), Some(height))?,
        (None, None) => {}
        _ => {
            warn(
                value.py(),
                "set_image_data expects both width and height or neither; ignoring the one given",
            )?;
        }
    }
    let (height, width) =
        handle.with_layer(|layer| channel_dimensions(layer, ChannelKey::color(0)))?;
    let parsed = parse_image::<T>(value, height, width, color_mode)?;
    handle.with_layer_mut(|layer| {
        let channels = image_channels_mut(layer)?;
        // Mask channels are only replaced when the new data carries one.
        let masks: Vec<_> = channels
            .iter()
            .filter(|(key, _)| key.is_mask())
            .map(|(key, samples)| (key, samples.to_vec()))
            .collect();
        *channels = parsed.channels;
        for (key, samples) in masks {
            channels.insert(key, samples);
        }
        match parsed.mask {
            Some((pixels, height, width)) => install_mask(layer, pixels, height, width),
            None => Ok(()),
        }
    })
}

/// A channel key from Python for a layer in `color_mode`.
pub fn channel_key(color_mode: ColorMode, value: &Bound<'_, PyAny>) -> PyResult<ChannelKey> {
    channel_key_from_py(value, color_mode)
}
