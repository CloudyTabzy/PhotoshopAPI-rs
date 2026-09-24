//! Python bindings for the Rust PhotoshopAPI port.
//!
//! The extension module `photoshopapi._native` holds the classes; the
//! `photoshopapi` package (in `python/`) re-exports them, adds the enum,
//! geometry, and util modules, and generates the text-layer flat methods and
//! proxies from the property tables in `text_ops`.

#[macro_use]
mod classes;
mod convert;
mod depth;
mod layer_ops;
mod numpy_io;
mod photoshop_file;
mod smart_warp;
mod state;
mod text_ops;
mod tree_ops;

#[pyo3::pymodule]
mod _native {
    use numpy::ndarray::Array2;
    use numpy::{IntoPyArray, PyArray2};
    use psd::core::BitDepth;
    use psd::{Homography, LayeredFile, Point2};
    use pyo3::prelude::*;

    use crate::convert::{read_options_from_py, Io};
    use crate::depth::{depth16, depth32, depth8};
    use crate::photoshop_file::{header_of, PyPhotoshopFile};
    use crate::smart_warp::PySmartWarp;
    use crate::state::psd_error;

    /// Version of the Rust package backing this Python module.
    #[pyfunction]
    fn rust_version() -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// Compute the projective transform between two four-point quads.
    #[pyfunction]
    fn homography_matrix<'py>(
        py: Python<'py>,
        source: Vec<(f64, f64)>,
        destination: Vec<(f64, f64)>,
    ) -> PyResult<Bound<'py, PyArray2<f64>>> {
        let source: [(f64, f64); 4] = source.try_into().map_err(|_| {
            pyo3::exceptions::PyValueError::new_err("source quad must contain four points")
        })?;
        let destination: [(f64, f64); 4] = destination.try_into().map_err(|_| {
            pyo3::exceptions::PyValueError::new_err("destination quad must contain four points")
        })?;
        let source = source.map(|(x, y)| Point2::new(x, y));
        let destination = destination.map(|(x, y)| Point2::new(x, y));
        let homography = Homography::between(source, destination).map_err(psd_error)?;
        let values = homography.to_rows().concat();
        let matrix = Array2::from_shape_vec((3, 3), values)
            .expect("the homography matrix contains exactly nine values");
        Ok(matrix.into_pyarray(py))
    }

    /// Read a PSD/PSB (path or binary file-like object) into the document
    /// class of its bit depth.
    ///
    /// `memory_limit` caps the cumulative decoded channel memory: `None` (the
    /// default) uses the library default, `0` means unlimited, and a positive
    /// value is a byte budget.
    #[pyfunction]
    #[pyo3(signature = (path, memory_limit=None))]
    fn read(
        py: Python<'_>,
        path: &Bound<'_, PyAny>,
        memory_limit: Option<i64>,
    ) -> PyResult<Py<PyAny>> {
        let options = read_options_from_py(memory_limit)?;
        let depth = match Io::from_py(path)? {
            Io::Path(file) => header_of(&file)?.depth,
            stream => {
                // Buffer the stream once and dispatch on its header.
                let bytes = stream.read_bytes()?;
                let header = psd::core::FileHeader::read(&mut psd::core::BeReader::new(&bytes))
                    .map_err(psd_error)?;
                return Ok(match header.depth {
                    BitDepth::Eight => Py::new(
                        py,
                        depth8::PyDocument::from_rust(
                            LayeredFile::from_bytes_with_options(&bytes, options)
                                .map_err(psd_error)?,
                        ),
                    )?
                    .into_any(),
                    BitDepth::Sixteen => Py::new(
                        py,
                        depth16::PyDocument::from_rust(
                            LayeredFile::from_bytes_with_options(&bytes, options)
                                .map_err(psd_error)?,
                        ),
                    )?
                    .into_any(),
                    BitDepth::ThirtyTwo => Py::new(
                        py,
                        depth32::PyDocument::from_rust(
                            LayeredFile::from_bytes_with_options(&bytes, options)
                                .map_err(psd_error)?,
                        ),
                    )?
                    .into_any(),
                });
            }
        };
        Ok(match depth {
            BitDepth::Eight => Py::new(py, depth8::PyDocument::open(path, options)?)?.into_any(),
            BitDepth::Sixteen => Py::new(py, depth16::PyDocument::open(path, options)?)?.into_any(),
            BitDepth::ThirtyTwo => {
                Py::new(py, depth32::PyDocument::open(path, options)?)?.into_any()
            }
        })
    }

    /// `(character, paragraph)` text property names for the Python package.
    #[pyfunction]
    fn text_property_names() -> (Vec<&'static str>, Vec<&'static str>) {
        crate::text_ops::property_names()
    }

    #[pymodule_init]
    fn init(m: &Bound<'_, PyModule>) -> PyResult<()> {
        m.add_class::<PySmartWarp>()?;
        m.add_class::<PyPhotoshopFile>()?;
        macro_rules! add_depth {
            ($module:ident) => {
                m.add_class::<$module::PyLayer>()?;
                m.add_class::<$module::PyImageLayer>()?;
                m.add_class::<$module::PyGroupLayer>()?;
                m.add_class::<$module::PyTextLayer>()?;
                m.add_class::<$module::PySmartObjectLayer>()?;
                m.add_class::<$module::PyDocument>()?;
            };
        }
        add_depth!(depth8);
        add_depth!(depth16);
        add_depth!(depth32);
        Ok(())
    }
}
