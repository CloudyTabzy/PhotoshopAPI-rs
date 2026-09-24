//! Python view of the owned smart-object warp geometry.

use psd::{Point2, Warp};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use crate::state::psd_error;

fn point_from_python(value: &Bound<'_, PyAny>) -> PyResult<Point2> {
    if let Ok((x, y)) = value.extract::<(f64, f64)>() {
        return Ok(Point2::new(x, y));
    }
    Ok(Point2::new(
        value.getattr("x")?.extract()?,
        value.getattr("y")?.extract()?,
    ))
}

fn points_from_python(value: &Bound<'_, PyAny>) -> PyResult<Vec<Point2>> {
    value
        .try_iter()?
        .map(|item| point_from_python(&item?))
        .collect()
}

fn points_to_python(py: Python<'_>, points: &[Point2]) -> PyResult<Vec<Py<PyAny>>> {
    let point_class = PyModule::import(py, "photoshopapi.geometry")?.getattr("Point2D")?;
    points
        .iter()
        .map(|point| point_class.call1((point.x, point.y)).map(Bound::unbind))
        .collect()
}

fn quad_from_python(value: &Bound<'_, PyAny>) -> PyResult<[Point2; 4]> {
    points_from_python(value)?
        .try_into()
        .map_err(|_| PyValueError::new_err("warp transform must contain four points"))
}

#[pyclass(name = "SmartObjectWarp", module = "photoshopapi")]
pub struct PySmartWarp {
    pub(crate) warp: Warp,
}

impl PySmartWarp {
    pub(crate) fn from_warp(warp: Warp) -> Self {
        Self { warp }
    }
}

#[pymethods]
impl PySmartWarp {
    #[new]
    fn new(points: &Bound<'_, PyAny>, u_dims: usize, v_dims: usize) -> PyResult<Self> {
        Warp::from_control_points(points_from_python(points)?, u_dims, v_dims)
            .map(Self::from_warp)
            .map_err(psd_error)
    }

    #[staticmethod]
    #[pyo3(signature = (width, height, u_dims=4, v_dims=4))]
    fn generate_default(
        width: usize,
        height: usize,
        u_dims: usize,
        v_dims: usize,
    ) -> PyResult<Self> {
        Warp::generate_default_with_grid(width, height, u_dims, v_dims)
            .map(Self::from_warp)
            .map_err(psd_error)
    }

    #[getter]
    fn points(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        points_to_python(py, self.warp.control_points())
    }

    #[setter]
    fn set_points(&mut self, points: &Bound<'_, PyAny>) -> PyResult<()> {
        self.warp
            .set_control_points(points_from_python(points)?)
            .map_err(psd_error)
    }

    #[getter]
    fn u_dims(&self) -> usize {
        self.warp.u_dimensions()
    }

    #[getter]
    fn v_dims(&self) -> usize {
        self.warp.v_dimensions()
    }

    #[getter]
    fn affine_transform(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        points_to_python(py, &self.warp.affine_transform())
    }

    #[setter]
    fn set_affine_transform(&mut self, points: &Bound<'_, PyAny>) -> PyResult<()> {
        self.warp
            .set_affine_transform(quad_from_python(points)?)
            .map_err(psd_error)
    }

    #[getter]
    fn non_affine_transform(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        points_to_python(py, &self.warp.non_affine_transform())
    }

    #[setter]
    fn set_non_affine_transform(&mut self, points: &Bound<'_, PyAny>) -> PyResult<()> {
        self.warp
            .set_non_affine_transform(quad_from_python(points)?)
            .map_err(psd_error)
    }

    fn no_op(&self) -> bool {
        self.warp.no_op()
    }

    fn valid(&self) -> bool {
        true
    }
}
