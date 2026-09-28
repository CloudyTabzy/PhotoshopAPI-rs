//! Conversions between Python values and the Rust document types.
//!
//! The Python enums in `photoshopapi.enum` are `IntEnum`s whose values follow
//! upstream's pybind11 enums (declaration order), except `ColorMode` and
//! `Compression`, which use their on-disk values. Setters accept the enum
//! members or plain integers.

use std::path::PathBuf;

use psd::core::{BlendMode, ColorMode, Compression, LayerColor};
use psd::geometry::Point2;
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString};

use crate::state::psd_error;

/// Blend modes in upstream `Enum::BlendMode` order; the index is the
/// `photoshopapi.enum.BlendMode` value.
const BLEND_MODES: [BlendMode; 28] = [
    BlendMode::PASSTHROUGH,
    BlendMode::NORMAL,
    BlendMode::DISSOLVE,
    BlendMode::DARKEN,
    BlendMode::MULTIPLY,
    BlendMode::COLOR_BURN,
    BlendMode::LINEAR_BURN,
    BlendMode::DARKER_COLOR,
    BlendMode::LIGHTEN,
    BlendMode::SCREEN,
    BlendMode::COLOR_DODGE,
    BlendMode::LINEAR_DODGE,
    BlendMode::LIGHTER_COLOR,
    BlendMode::OVERLAY,
    BlendMode::SOFT_LIGHT,
    BlendMode::HARD_LIGHT,
    BlendMode::VIVID_LIGHT,
    BlendMode::LINEAR_LIGHT,
    BlendMode::PIN_LIGHT,
    BlendMode::HARD_MIX,
    BlendMode::DIFFERENCE,
    BlendMode::EXCLUSION,
    BlendMode::SUBTRACT,
    BlendMode::DIVIDE,
    BlendMode::HUE,
    BlendMode::SATURATION,
    BlendMode::COLOR,
    BlendMode::LUMINOSITY,
];

/// `photoshopapi.enum.<class_name>(raw)`.
pub fn py_enum(py: Python<'_>, class_name: &str, raw: i64) -> PyResult<Py<PyAny>> {
    PyModule::import(py, "photoshopapi.enum")?
        .getattr(class_name)?
        .call1((raw,))
        .map(Bound::unbind)
}

/// An enum member or integer as its integer value. Booleans are rejected.
pub fn enum_value(value: &Bound<'_, PyAny>) -> PyResult<i64> {
    if value.is_instance_of::<pyo3::types::PyBool>() {
        return Err(PyTypeError::new_err("expected an enum member or integer"));
    }
    value
        .extract::<i64>()
        .map_err(|_| PyTypeError::new_err("expected an enum member or integer"))
}

/// Whether `value` is a member of `photoshopapi.enum.<class_name>`.
pub fn is_enum_member(value: &Bound<'_, PyAny>, class_name: &str) -> PyResult<bool> {
    let class = PyModule::import(value.py(), "photoshopapi.enum")?.getattr(class_name)?;
    value.is_instance(&class)
}

pub fn blend_mode_from_py(value: &Bound<'_, PyAny>) -> PyResult<BlendMode> {
    if let Ok(key) = value.cast::<PyString>() {
        let key = key.to_str()?;
        let bytes: [u8; 4] = key
            .as_bytes()
            .try_into()
            .map_err(|_| PyValueError::new_err("blend mode keys are four characters"))?;
        return Ok(BlendMode::from_bytes(bytes));
    }
    let raw = enum_value(value)?;
    usize::try_from(raw)
        .ok()
        .and_then(|index| BLEND_MODES.get(index).copied())
        .ok_or_else(|| PyValueError::new_err(format!("unknown blend mode {raw}")))
}

/// The enum member for `mode`; modes outside upstream's enum are returned as
/// their four-character key so they are not lost.
pub fn blend_mode_to_py(py: Python<'_>, mode: BlendMode) -> PyResult<Py<PyAny>> {
    match BLEND_MODES.iter().position(|known| *known == mode) {
        Some(index) => py_enum(py, "BlendMode", index as i64),
        None => Ok(PyString::new(py, &mode.as_str()).into_any().unbind()),
    }
}

pub fn color_mode_from_py(value: &Bound<'_, PyAny>) -> PyResult<ColorMode> {
    let raw = u16::try_from(enum_value(value)?)
        .map_err(|_| PyValueError::new_err("unknown color mode"))?;
    ColorMode::from_raw(raw).map_err(psd_error)
}

pub fn compression_from_py(value: &Bound<'_, PyAny>) -> PyResult<Compression> {
    let raw = u16::try_from(enum_value(value)?)
        .map_err(|_| PyValueError::new_err("unknown compression"))?;
    Compression::from_raw(raw).map_err(psd_error)
}

pub fn layer_color_from_py(value: &Bound<'_, PyAny>) -> PyResult<LayerColor> {
    let raw = u16::try_from(enum_value(value)?)
        .map_err(|_| PyValueError::new_err("unknown layer color"))?;
    Ok(LayerColor::from_raw(raw))
}

pub fn layer_color_to_py(py: Python<'_>, color: LayerColor) -> PyResult<Py<PyAny>> {
    match color {
        LayerColor::Other(raw) => Ok(raw.into_pyobject(py)?.into_any().unbind()),
        known => py_enum(py, "LayerColor", i64::from(known.as_raw())),
    }
}

/// `photoshopapi.enum.LinkedLayerType` (`data` = 0, `external` = 1); the
/// default is embedded data.
pub fn linkage_from_py(value: Option<&Bound<'_, PyAny>>) -> PyResult<psd::LinkedStorage> {
    match value.map(enum_value).transpose()? {
        None | Some(0) => Ok(psd::LinkedStorage::Embedded),
        Some(1) => Ok(psd::LinkedStorage::External),
        Some(other) => Err(PyValueError::new_err(format!("unknown linkage {other}"))),
    }
}

pub fn linkage_number(storage: psd::LinkedStorage) -> i64 {
    match storage {
        psd::LinkedStorage::Embedded => 0,
        psd::LinkedStorage::External => 1,
    }
}

/// Cumulative decoded-channel budget for one read.
///
/// `None` keeps [`psd::ReadOptions::default`], `0` selects unlimited decoding,
/// and a positive value is a byte budget. A negative value is a caller bug, not
/// a request for unlimited decoding, so it is rejected.
pub fn read_options_from_py(memory_limit: Option<i64>) -> PyResult<psd::ReadOptions> {
    Ok(match memory_limit {
        None => psd::ReadOptions::default(),
        Some(0) => psd::ReadOptions::unlimited(),
        Some(bytes) => psd::ReadOptions {
            total_memory_limit: Some(usize::try_from(bytes).map_err(|_| {
                PyValueError::new_err(format!("memory_limit must not be negative, got {bytes}"))
            })?),
            ..psd::ReadOptions::default()
        },
    })
}

/// `photoshopapi.enum.BitDepth` for a sample depth in bits.
///
/// The raw values are upstream's: `BD_1 = 0`, `BD_8 = 1`, `BD_16 = 2`,
/// `BD_32 = 3`.
pub fn bit_depth_to_py(py: Python<'_>, depth: u16) -> PyResult<Py<PyAny>> {
    let raw = match depth {
        1 => 0,
        8 => 1,
        16 => 2,
        32 => 3,
        other => {
            return Err(PyValueError::new_err(format!(
                "unknown bit depth {other}; expected 1, 8, 16 or 32"
            )))
        }
    };
    py_enum(py, "BitDepth", raw)
}

/// Python truthiness, the way pybind11's converting `bool` caster reads
/// setter arguments (so `layer.is_visible = 0` works as it does upstream).
pub fn truthy(value: &Bound<'_, PyAny>) -> PyResult<bool> {
    value.is_truthy()
}

/// An integer in `0..=255`. Anything else is a `TypeError`, matching the
/// pybind11 `uint8_t` overload failure upstream's tests expect.
pub fn byte_value(value: &Bound<'_, PyAny>) -> PyResult<u8> {
    if value.is_instance_of::<pyo3::types::PyBool>() {
        return Err(PyTypeError::new_err("expected an integer from 0 to 255"));
    }
    value
        .extract::<i64>()
        .ok()
        .and_then(|raw| u8::try_from(raw).ok())
        .ok_or_else(|| PyTypeError::new_err("expected an integer from 0 to 255"))
}

/// A non-negative index; negative values are a `ValueError` like the other
/// out-of-range indices.
pub fn index_value(raw: i64) -> PyResult<usize> {
    usize::try_from(raw).map_err(|_| PyValueError::new_err("index must not be negative"))
}

/// A `Point2D` (anything with `x`/`y`) or an `(x, y)` pair.
pub fn point_from_py(value: &Bound<'_, PyAny>) -> PyResult<Point2> {
    if let Ok((x, y)) = value.extract::<(f64, f64)>() {
        return Ok(Point2::new(x, y));
    }
    Ok(Point2::new(
        value.getattr("x")?.extract()?,
        value.getattr("y")?.extract()?,
    ))
}

/// `photoshopapi.geometry.Point2D(x, y)`.
pub fn point_to_py(py: Python<'_>, point: Point2) -> PyResult<Py<PyAny>> {
    PyModule::import(py, "photoshopapi.geometry")?
        .getattr("Point2D")?
        .call1((point.x, point.y))
        .map(Bound::unbind)
}

/// ICC profile bytes from what upstream's `icc` setter accepts: a path (a
/// `str`, `os.PathLike`, or — as pybind11 converts it — `bytes`) to an
/// `.icc` file, or the profile itself as a NumPy `uint8` array, a byte
/// sequence, or a single integer.
pub fn icc_bytes(value: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    let py = value.py();
    let read_profile = |path: PathBuf| {
        std::fs::read(&path).map_err(|_| {
            pyo3::exceptions::PyRuntimeError::new_err(format!(
                "Must pass a valid .icc file into the ctor. Unable to read '{}'",
                path.display()
            ))
        })
    };
    if value.is_none() {
        return Err(PyTypeError::new_err(
            "the ICC profile must be a path or a numpy.ndarray of bytes",
        ));
    }
    if value.is_instance_of::<PyString>() || value.hasattr("__fspath__")? {
        return read_profile(value.extract()?);
    }
    if value.is_instance_of::<PyBytes>() {
        let path: String = PyModule::import(py, "os")?
            .call_method1("fsdecode", (value,))?
            .extract()?;
        return read_profile(PathBuf::from(path));
    }
    if let Ok(array) = value.extract::<numpy::PyReadonlyArrayDyn<'_, u8>>() {
        return Ok(array.as_array().iter().copied().collect());
    }
    if value.is_instance_of::<pyo3::types::PyInt>() {
        return Ok(vec![byte_value(value)?]);
    }
    if value.is_instance_of::<pyo3::types::PyByteArray>()
        || value.is_instance_of::<pyo3::types::PyMemoryView>()
        || value.is_instance_of::<pyo3::types::PyList>()
        || value.is_instance_of::<pyo3::types::PyTuple>()
    {
        let bytes = PyModule::import(py, "builtins")?
            .getattr("bytes")?
            .call1((value,))?;
        return Ok(bytes.cast::<PyBytes>()?.as_bytes().to_vec());
    }
    Err(PyTypeError::new_err(
        "the ICC profile must be a path or a numpy.ndarray of bytes",
    ))
}

/// Where a document is read from or written to.
pub enum Io<'py> {
    Path(PathBuf),
    /// A binary file-like object (`read()`/`write()`).
    Stream(Bound<'py, PyAny>),
}

impl<'py> Io<'py> {
    pub fn from_py(value: &Bound<'py, PyAny>) -> PyResult<Self> {
        if let Ok(path) = value.extract::<PathBuf>() {
            return Ok(Self::Path(path));
        }
        if value.hasattr("read")? || value.hasattr("write")? {
            return Ok(Self::Stream(value.clone()));
        }
        Err(PyTypeError::new_err(
            "expected a path or a binary file-like object",
        ))
    }

    /// The bytes to parse (reading a stream from its current position).
    pub fn read_bytes(&self) -> PyResult<Vec<u8>> {
        match self {
            Self::Path(path) => {
                std::fs::read(path).map_err(|error| psd_error(psd::core::PsdError::from(error)))
            }
            Self::Stream(stream) => stream.call_method0("read")?.extract::<Vec<u8>>(),
        }
    }

    pub fn write_bytes(&self, bytes: &[u8]) -> PyResult<()> {
        match self {
            Self::Path(path) => std::fs::write(path, bytes)
                .map_err(|error| psd_error(psd::core::PsdError::from(error))),
            Self::Stream(stream) => {
                stream.call_method1("write", (PyBytes::new(stream.py(), bytes),))?;
                Ok(())
            }
        }
    }
}
