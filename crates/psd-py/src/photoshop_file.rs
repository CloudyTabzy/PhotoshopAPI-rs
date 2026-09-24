//! `PhotoshopFile`: upstream's low-level file structure.
//!
//! Upstream exposes it only to read, re-write, and probe files ("the
//! implementation details are not meant to be accessed"); so does this port.
//! The bytes are validated by parsing and written back re-serialized.

use std::path::PathBuf;

use psd::core::{BeReader, BeWriter, FileHeader, PhotoshopFile};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use crate::convert::bit_depth_to_py;
use crate::state::psd_error;

fn file_path(document: &Bound<'_, PyAny>) -> PyResult<PathBuf> {
    match document.getattr("path") {
        Ok(path) => path.extract(),
        Err(_) => document.extract(),
    }
}

fn read_bytes(path: &PathBuf) -> PyResult<Vec<u8>> {
    std::fs::read(path).map_err(|error| psd_error(psd::core::PsdError::from(error)))
}

#[pyclass(name = "PhotoshopFile", module = "photoshopapi")]
#[derive(Default)]
pub struct PyPhotoshopFile {
    bytes: Option<Vec<u8>>,
}

#[pymethods]
impl PyPhotoshopFile {
    #[new]
    fn new() -> Self {
        Self::default()
    }

    /// Parse a PSD/PSB from a `photoshopapi.util.File` (or a path).
    fn read(&mut self, document: &Bound<'_, PyAny>) -> PyResult<()> {
        let bytes = read_bytes(&file_path(document)?)?;
        PhotoshopFile::read(&mut BeReader::new(&bytes)).map_err(psd_error)?;
        self.bytes = Some(bytes);
        Ok(())
    }

    /// Write the parsed structure to a `photoshopapi.util.File` (or path).
    fn write(&self, document: &Bound<'_, PyAny>) -> PyResult<()> {
        let bytes = self
            .bytes
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("read a file before writing it"))?;
        let file = PhotoshopFile::read(&mut BeReader::new(bytes)).map_err(psd_error)?;
        let mut writer = BeWriter::new();
        file.write(&mut writer).map_err(psd_error)?;
        std::fs::write(file_path(document)?, writer.into_inner())
            .map_err(|error| psd_error(psd::core::PsdError::from(error)))
    }

    /// The bit depth of the file at `path`, from its 26-byte header.
    #[staticmethod]
    fn find_bitdepth(py: Python<'_>, path: PathBuf) -> PyResult<Py<PyAny>> {
        let depth = header_of(&path)?.depth.as_raw();
        bit_depth_to_py(py, depth)
    }
}

/// The file header at `path` (reads only its first bytes).
pub fn header_of(path: &PathBuf) -> PyResult<FileHeader> {
    use std::io::Read;
    let mut file =
        std::fs::File::open(path).map_err(|error| psd_error(psd::core::PsdError::from(error)))?;
    let mut bytes = [0u8; FileHeader::SIZE];
    file.read_exact(&mut bytes)
        .map_err(|error| psd_error(psd::core::PsdError::from(error)))?;
    FileHeader::read(&mut BeReader::new(&bytes)).map_err(psd_error)
}
