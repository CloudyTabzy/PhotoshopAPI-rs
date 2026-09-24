//! Copies between NumPy channel arrays and the document's planar channels.
//!
//! Image data is accepted the ways upstream's bindings accept it: a
//! channel-first `(channels, height, width)` array, a flattened
//! `(channels, height * width)` array, or a dict mapping channel indices or
//! `ChannelID` members to `(height, width)` arrays. Color channels come first
//! in color-mode order with an optional trailing alpha channel; a dict may
//! also carry the pixel mask (`-2`), whose shape is its own.

use numpy::ndarray::{Array2, ArrayViewD, Axis};
use numpy::{Element, IntoPyArray, PyArray1, PyArray2, PyReadonlyArrayDyn};
use psd::core::ColorMode;
use psd::{color_channel_count, BitDepth, ChannelKey, ChannelStore, Layer};
use pyo3::exceptions::{PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::convert::is_enum_member;

/// The value as an array of `T`. Other dtypes (and nested sequences) are
/// converted with `numpy.asarray(value, dtype)`, like the force-casting
/// pybind11 `array_t<T>` arguments upstream takes.
fn numpy_array<'py, T: Element>(value: &Bound<'py, PyAny>) -> PyResult<PyReadonlyArrayDyn<'py, T>> {
    if let Ok(array) = value.extract() {
        return Ok(array);
    }
    let py = value.py();
    let error =
        || PyTypeError::new_err("image data must be a NumPy array (or array-like of numbers)");
    let converted = PyModule::import(py, "numpy")?
        .call_method1("asarray", (value, numpy::dtype::<T>(py)))
        .map_err(|_| error())?;
    converted.extract().map_err(|_| error())
}

/// Channel key for an upstream `ChannelID` value in `color_mode`.
pub fn channel_id_key(color_mode: ColorMode, channel_id: i64) -> PyResult<ChannelKey> {
    let index = match (color_mode, channel_id) {
        (ColorMode::Rgb, 0) | (ColorMode::Cmyk, 3) | (ColorMode::Grayscale, 7) => 0,
        (ColorMode::Rgb, 1) | (ColorMode::Cmyk, 4) => 1,
        (ColorMode::Rgb, 2) | (ColorMode::Cmyk, 5) => 2,
        // Upstream maps Black to index 2; the fourth CMYK channel is 3.
        (ColorMode::Cmyk, 6) => 3,
        (_, 9) => -1,
        (_, 10) => -2,
        _ => {
            return Err(PyRuntimeError::new_err(
                "channel ID is invalid for this color mode",
            ))
        }
    };
    Ok(ChannelKey(index))
}

/// A dict key or `__setitem__` key: a `ChannelID` member or a raw index.
pub fn channel_key_from_py(
    value: &Bound<'_, PyAny>,
    color_mode: ColorMode,
) -> PyResult<ChannelKey> {
    if is_enum_member(value, "ChannelID")? {
        return channel_id_key(color_mode, value.extract()?);
    }
    let index: i16 = value
        .extract()
        .map_err(|_| PyTypeError::new_err("channel keys are indices or ChannelID members"))?;
    validate_index(index, color_mode)?;
    Ok(ChannelKey(index))
}

/// Color indices must exist in the color mode; `-1` alpha, `-2`/`-3` masks.
pub fn validate_index(index: i16, color_mode: ColorMode) -> PyResult<()> {
    let color_count = color_channel_count(color_mode) as i16;
    if index < -3 || index >= color_count {
        return Err(PyRuntimeError::new_err(format!(
            "channel index {index} is invalid for this color mode"
        )));
    }
    Ok(())
}

fn supported_color_count(color_mode: ColorMode) -> PyResult<usize> {
    let count = usize::from(color_channel_count(color_mode));
    if matches!(count, 1 | 3 | 4) {
        Ok(count)
    } else {
        Err(PyValueError::new_err(
            "NumPy image data supports grayscale, RGB, and CMYK documents",
        ))
    }
}

fn plane<T: Element + Copy>(
    view: ArrayViewD<'_, T>,
    height: usize,
    width: usize,
) -> PyResult<Vec<T>> {
    let shape = view.shape();
    let matches = match shape.len() {
        2 => shape == [height, width],
        1 => shape[0] == height * width,
        _ => false,
    };
    if !matches {
        return Err(PyValueError::new_err(format!(
            "channel shape {shape:?} does not match the layer's ({height}, {width})"
        )));
    }
    Ok(view.iter().copied().collect())
}

/// One `(height, width)` (or flattened) channel.
pub fn channel_plane<T: Element + Copy>(
    value: &Bound<'_, PyAny>,
    height: usize,
    width: usize,
) -> PyResult<Vec<T>> {
    plane(numpy_array::<T>(value)?.as_array(), height, width)
}

/// A mask array: must be two-dimensional; returns `(pixels, height, width)`.
pub fn mask_array<T: Element + Copy>(value: &Bound<'_, PyAny>) -> PyResult<(Vec<T>, usize, usize)> {
    let array = numpy_array::<T>(value)?;
    let view = array.as_array();
    if view.ndim() != 2 {
        return Err(PyValueError::new_err(
            "a mask must be a 2-dimensional array of shape (height, width)",
        ));
    }
    let (height, width) = (view.shape()[0], view.shape()[1]);
    Ok((view.iter().copied().collect(), height, width))
}

/// Channel-first or dict image data, decoded and validated.
pub struct ParsedImage<T> {
    pub channels: ChannelStore<T>,
    /// A `-2` entry of a dict: `(pixels, height, width)`.
    pub mask: Option<(Vec<T>, usize, usize)>,
}

/// `(height, width)` of image data when the caller did not pass them.
pub fn infer_dimensions<T: Element>(
    value: &Bound<'_, PyAny>,
    color_mode: ColorMode,
) -> PyResult<(usize, usize)> {
    if let Ok(dictionary) = value.cast::<PyDict>() {
        for (key, array) in dictionary.iter() {
            // Color and alpha channels share the layer's size; masks do not.
            if !channel_key_from_py(&key, color_mode)?.is_mask() {
                let array = numpy_array::<T>(&array)?;
                let shape = array.as_array().shape().to_vec();
                if shape.len() == 2 {
                    return Ok((shape[0], shape[1]));
                }
                return Err(PyValueError::new_err(
                    "pass width and height for flattened channel arrays",
                ));
            }
        }
        return Err(PyValueError::new_err("image data has no color channels"));
    }
    let array = numpy_array::<T>(value)?;
    let shape = array.as_array().shape().to_vec();
    match shape.len() {
        3 => Ok((shape[1], shape[2])),
        _ => Err(PyValueError::new_err(
            "pass width and height, or image data of shape (channels, height, width)",
        )),
    }
}

/// Decode image data for a `height` × `width` layer in `color_mode`.
pub fn parse_image<T: BitDepth + Element>(
    value: &Bound<'_, PyAny>,
    height: usize,
    width: usize,
    color_mode: ColorMode,
) -> PyResult<ParsedImage<T>> {
    let color_count = supported_color_count(color_mode)?;
    let mut channels = ChannelStore::new();
    let mut mask = None;
    if let Ok(dictionary) = value.cast::<PyDict>() {
        for (key, array) in dictionary.iter() {
            let key = channel_key_from_py(&key, color_mode)?;
            if key == ChannelKey::USER_MASK {
                mask = Some(mask_array::<T>(&array)?);
            } else if key.is_mask() {
                return Err(PyValueError::new_err(
                    "set the real user mask (-3) through the channel APIs of a read layer",
                ));
            } else {
                channels.insert(key, channel_plane::<T>(&array, height, width)?);
            }
        }
    } else {
        let array = numpy_array::<T>(value)?;
        let view = array.as_array();
        let shape = view.shape().to_vec();
        let layout_ok = match shape.len() {
            3 => shape[1] == height && shape[2] == width,
            2 => shape[1] == height * width,
            _ => false,
        };
        if !layout_ok {
            return Err(PyValueError::new_err(format!(
                "image data of shape {shape:?} does not match (channels, {height}, {width})"
            )));
        }
        if shape[0] != color_count && shape[0] != color_count + 1 {
            return Err(PyRuntimeError::new_err(format!(
                "image data must have {color_count} color channels and an optional alpha channel"
            )));
        }
        for (index, plane) in view.axis_iter(Axis(0)).enumerate() {
            let key = if index == color_count {
                ChannelKey::ALPHA
            } else {
                ChannelKey::color(index as u8)
            };
            channels.insert(key, plane.iter().copied().collect());
        }
    }
    for index in 0..color_count {
        if !channels.contains(ChannelKey::color(index as u8)) {
            return Err(PyRuntimeError::new_err(format!(
                "image data is missing color channel {index}"
            )));
        }
    }
    Ok(ParsedImage { channels, mask })
}

pub fn channel_array<'py, T: Element + Copy>(
    py: Python<'py>,
    data: &[T],
    height: usize,
    width: usize,
) -> PyResult<Bound<'py, PyArray2<T>>> {
    let array = Array2::from_shape_vec((height, width), data.to_vec()).map_err(|_| {
        PyRuntimeError::new_err(
            "channel length does not match the layer bounds; set new image data after resizing",
        )
    })?;
    Ok(array.into_pyarray(py))
}

/// An empty 1-D array (upstream's value for "no mask").
pub fn empty_array<T: Element>(py: Python<'_>) -> Bound<'_, PyArray1<T>> {
    Vec::<T>::new().into_pyarray(py)
}

/// `(height, width)` of channel `key`: the matching mask record for mask
/// channels, the layer bounds otherwise.
pub fn channel_dimensions<T: BitDepth>(
    layer: &Layer<T>,
    key: ChannelKey,
) -> PyResult<(usize, usize)> {
    let rect = if key.is_mask() {
        layer
            .mask
            .as_ref()
            .and_then(|mask| mask.record_for_channel(key.index()))
            .map(|mask| psd::Rect::new(mask.top, mask.left, mask.bottom, mask.right))
            .unwrap_or(layer.bounds)
    } else {
        layer.bounds
    };
    let height = usize::try_from(rect.height().max(0)).expect("non-negative");
    let width = usize::try_from(rect.width().max(0)).expect("non-negative");
    Ok((height, width))
}

pub fn image_channels<T: BitDepth>(layer: &Layer<T>) -> PyResult<&ChannelStore<T>> {
    layer
        .channels()
        .ok_or_else(|| PyTypeError::new_err("layer has no image channels"))
}

pub fn image_channels_mut<T: BitDepth>(layer: &mut Layer<T>) -> PyResult<&mut ChannelStore<T>> {
    layer
        .channels_mut()
        .ok_or_else(|| PyTypeError::new_err("layer has no image channels"))
}
