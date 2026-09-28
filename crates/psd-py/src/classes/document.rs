//! `LayeredFile_*bit`: the document.

macro_rules! document_class {
    ($document_name:literal) => {
        #[pyclass(name = $document_name, module = "photoshopapi")]
        pub struct PyDocument {
            pub(crate) inner: Document<Sample>,
        }

        impl PyDocument {
            pub(crate) fn from_rust(file: LayeredFile<Sample>) -> Self {
                Self {
                    inner: std::sync::Arc::new(std::sync::RwLock::new(file)),
                }
            }

            pub(crate) fn open(
                source: &Bound<'_, PyAny>,
                options: psd::ReadOptions,
            ) -> PyResult<Self> {
                match Io::from_py(source)? {
                    Io::Path(path) => LayeredFile::read_with_options(&path, options)
                        .map(Self::from_rust)
                        .map_err(psd_error),
                    stream => LayeredFile::from_bytes_with_options(&stream.read_bytes()?, options)
                        .map(Self::from_rust)
                        .map_err(psd_error),
                }
            }

            fn color_mode(&self) -> PyResult<ColorMode> {
                read_document(&self.inner, |file| Ok(file.color_mode))
            }

            fn wrap_ids(&self, py: Python<'_>, ids: Vec<LayerId>) -> PyResult<Vec<Py<PyAny>>> {
                wrap_all(py, tree_ops::handles(&self.inner, ids), self.color_mode()?)
            }

            fn resolve(&self, value: &Bound<'_, PyAny>) -> PyResult<LayerId> {
                tree_ops::resolve(&self.inner, value, handle_of)
            }

            fn set_header_size(&self, width: Option<u32>, height: Option<u32>) -> PyResult<()> {
                write_document(&self.inner, |file| {
                    let (width, height) =
                        (width.unwrap_or(file.width), height.unwrap_or(file.height));
                    FileHeader::new(
                        file.version,
                        file.num_channels,
                        width,
                        height,
                        CoreBitDepth::from_raw(<Sample as BitDepth>::DEPTH).map_err(psd_error)?,
                        file.color_mode,
                    )
                    .map_err(psd_error)?;
                    file.width = width;
                    file.height = height;
                    Ok(())
                })
            }
        }

        #[pymethods]
        impl PyDocument {
            #[new]
            #[pyo3(signature = (color_mode=None, width=1, height=1))]
            fn new(
                color_mode: Option<&Bound<'_, PyAny>>,
                width: u32,
                height: u32,
            ) -> PyResult<Self> {
                let mode = match color_mode {
                    Some(mode) => color_mode_from_py(mode)?,
                    None => ColorMode::Rgb,
                };
                LayeredFile::new(mode, width, height)
                    .map(Self::from_rust)
                    .map_err(psd_error)
            }

            /// Read a document from a path or a binary file-like object.
            ///
            /// `memory_limit` caps the cumulative decoded channel memory:
            /// `None` (the default) uses the library default, `0` means
            /// unlimited, and a positive value is a byte budget.
            #[staticmethod]
            #[pyo3(signature = (path, memory_limit=None))]
            fn read(path: &Bound<'_, PyAny>, memory_limit: Option<i64>) -> PyResult<Self> {
                Self::open(path, read_options_from_py(memory_limit)?)
            }

            /// Read a document from bytes.
            ///
            /// `memory_limit` behaves as in `read`.
            #[staticmethod]
            #[pyo3(signature = (data, memory_limit=None))]
            fn from_bytes(data: &[u8], memory_limit: Option<i64>) -> PyResult<Self> {
                LayeredFile::from_bytes_with_options(data, read_options_from_py(memory_limit)?)
                    .map(Self::from_rust)
                    .map_err(psd_error)
            }

            /// Write to a `.psd`/`.psb` path (the extension picks the
            /// container) or a binary file-like object. Unlike upstream the
            /// document stays usable afterwards.
            #[pyo3(signature = (path, force_overwrite=true))]
            fn write(&self, path: &Bound<'_, PyAny>, force_overwrite: bool) -> PyResult<()> {
                let target = Io::from_py(path)?;
                if let Io::Path(path) = &target {
                    if !force_overwrite && path.exists() {
                        return Err(PyFileExistsError::new_err(path.display().to_string()));
                    }
                    let version = match path
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .map(str::to_ascii_lowercase)
                        .as_deref()
                    {
                        Some("psd") => Version::Psd,
                        Some("psb") => Version::Psb,
                        _ => {
                            return Err(PyValueError::new_err(
                                "the output path must end in .psd or .psb",
                            ))
                        }
                    };
                    write_document(&self.inner, |file| {
                        file.version = version;
                        Ok(())
                    })?;
                }
                let bytes = read_document(&self.inner, |file| file.to_bytes().map_err(psd_error))?;
                target.write_bytes(&bytes)
            }

            fn to_bytes<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
                let bytes = read_document(&self.inner, |file| file.to_bytes().map_err(psd_error))?;
                Ok(PyBytes::new(py, &bytes))
            }

            // --- document settings --------------------------------------

            #[getter]
            fn width(&self) -> PyResult<u32> {
                read_document(&self.inner, |file| Ok(file.width))
            }

            #[setter]
            fn set_width(&self, width: u32) -> PyResult<()> {
                self.set_header_size(Some(width), None)
            }

            #[getter]
            fn height(&self) -> PyResult<u32> {
                read_document(&self.inner, |file| Ok(file.height))
            }

            #[setter]
            fn set_height(&self, height: u32) -> PyResult<()> {
                self.set_header_size(None, Some(height))
            }

            #[getter]
            fn dpi(&self) -> PyResult<f32> {
                read_document(&self.inner, |file| Ok(file.dpi))
            }

            #[setter]
            fn set_dpi(&self, dpi: f32) -> PyResult<()> {
                if !dpi.is_finite() || dpi <= 0.0 {
                    return Err(PyValueError::new_err("dpi must be finite and positive"));
                }
                write_document(&self.inner, |file| {
                    file.dpi = dpi;
                    Ok(())
                })
            }

            #[getter]
            fn bit_depth(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
                bit_depth_to_py(py, <Sample as BitDepth>::DEPTH)
            }

            /// The file's on-disk depth. It differs from
            /// [`bit_depth`](Self::bit_depth) only for 1-bit (bitmap mode)
            /// documents, which read as 8-bit ones; saving such a document
            /// writes 8-bit.
            #[getter]
            fn source_depth(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
                let depth = read_document(&self.inner, |file| Ok(file.source_depth))?;
                bit_depth_to_py(py, depth)
            }

            #[getter(color_mode)]
            fn py_color_mode(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
                py_enum(py, "ColorMode", i64::from(self.color_mode()?.as_raw()))
            }

            /// Channels of the merged image (excluding masks).
            #[getter]
            fn num_channels(&self) -> PyResult<u16> {
                read_document(&self.inner, |file| {
                    let alpha = u16::from(file.has_alpha() || file.has_merged_alpha);
                    Ok(file
                        .num_channels
                        .max(color_channel_count(file.color_mode) + alpha))
                })
            }

            /// The ICC profile bytes (empty when there is none).
            #[getter]
            fn icc<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyArray1<u8>>> {
                let bytes = read_document(&self.inner, |file| Ok(file.icc_profile.clone()))?;
                Ok(bytes.into_pyarray(py))
            }

            /// Assign a path to an `.icc` file or the profile bytes.
            #[setter]
            fn set_icc(&self, value: &Bound<'_, PyAny>) -> PyResult<()> {
                let bytes = icc_bytes(value)?;
                write_document(&self.inner, |file| {
                    file.icc_profile = bytes;
                    Ok(())
                })
            }

            /// Write-only: the compression of every channel (clears
            /// per-layer settings, like upstream's `set_compression`).
            #[getter]
            fn compression(&self) -> PyResult<()> {
                Err(PyTypeError::new_err("compression property has no getter"))
            }

            #[setter]
            fn set_compression(&self, value: &Bound<'_, PyAny>) -> PyResult<()> {
                let compression = compression_from_py(value)?;
                write_document(&self.inner, |file| {
                    file.set_compression(Some(compression));
                    Ok(())
                })
            }

            /// Drop the document's `Txt2` text cache so Photoshop re-renders
            /// every text layer on open. (Writing already drops it when text
            /// layers changed; see `text_cache_is_stale`.)
            fn invalidate_text_cache(&self) -> PyResult<()> {
                write_document(&self.inner, |file| {
                    file.invalidate_text_cache();
                    Ok(())
                })
            }

            fn text_cache_is_stale(&self) -> PyResult<bool> {
                read_document(&self.inner, |file| Ok(file.text_cache_is_stale()))
            }

            /// Keep writing the `Txt2` cache even after text edits.
            fn retain_text_cache(&self) -> PyResult<()> {
                write_document(&self.inner, |file| {
                    file.retain_text_cache();
                    Ok(())
                })
            }

            // --- layers -------------------------------------------------

            /// Top-level layers, top to bottom.
            #[getter]
            fn layers(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
                let ids = read_document(&self.inner, |file| {
                    Ok(tree_ops::visible_children(file, None))
                })?;
                self.wrap_ids(py, ids)
            }

            /// Every layer, top to bottom, each group before its children.
            #[getter]
            fn flat_layers(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
                let ids = read_document(&self.inner, |file| Ok(tree_ops::flat_ids(file)))?;
                self.wrap_ids(py, ids)
            }

            /// Alias of `flat_layers` (upstream's stub name).
            #[getter]
            fn layers_flat(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
                self.flat_layers(py)
            }

            /// The layer at a `"Group/Layer"` path.
            fn find_layer(&self, py: Python<'_>, path: &str) -> PyResult<Py<PyAny>> {
                let id = read_document(&self.inner, |file| {
                    file.find_layer(path).ok_or_else(|| {
                        PyValueError::new_err(format!(
                            "Path '{path}' is not valid in the layered_file"
                        ))
                    })
                })?;
                wrap(
                    py,
                    LayerHandle::attached(std::sync::Arc::clone(&self.inner), id),
                    self.color_mode()?,
                )
            }

            fn __getitem__(&self, py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
                let id = tree_ops::child_named(&self.inner, None, name)?;
                wrap(
                    py,
                    LayerHandle::attached(std::sync::Arc::clone(&self.inner), id),
                    self.color_mode()?,
                )
            }

            fn __len__(&self) -> PyResult<usize> {
                read_document(&self.inner, |file| {
                    Ok(tree_ops::visible_children(file, None).len())
                })
            }

            fn __iter__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
                let layers = self.layers(py)?;
                Ok(pyo3::types::PyList::new(py, layers)?
                    .try_iter()?
                    .into_any()
                    .unbind())
            }

            /// Add a layer (not yet in a document) at the bottom of the root.
            fn add_layer(&self, layer: &Bound<'_, PyAny>) -> PyResult<()> {
                let handle = handle_of(layer)
                    .ok_or_else(|| PyTypeError::new_err("expected a layer of this bit depth"))?;
                tree_ops::add_child(None, Some(&self.inner), &handle)
            }

            /// Move `child` (layer or path) under `parent` (group layer or
            /// path; the root when omitted), at the bottom like upstream.
            #[pyo3(signature = (child, parent=None))]
            fn move_layer(
                &self,
                child: &Bound<'_, PyAny>,
                parent: Option<&Bound<'_, PyAny>>,
            ) -> PyResult<()> {
                let child = self.resolve(child)?;
                let parent = match parent {
                    None => None,
                    Some(parent) if parent.is_none() => None,
                    Some(parent)
                        if parent.extract::<String>().is_ok_and(|path| path.is_empty()) =>
                    {
                        None
                    }
                    Some(parent) => Some(self.resolve(parent)?),
                };
                tree_ops::move_id(&self.inner, child, parent)
            }

            /// Remove a layer (object or path) with everything below it. The
            /// layer object passed stays usable and can be added again.
            fn remove_layer(&self, layer: &Bound<'_, PyAny>) -> PyResult<()> {
                let id = self.resolve(layer)?;
                tree_ops::remove_id(&self.inner, id, handle_of(layer).as_ref())
            }

            /// Whether the layer object is part of this document.
            fn is_layer_in_document(&self, layer: &Bound<'_, PyAny>) -> PyResult<bool> {
                let Some(handle) = handle_of(layer) else {
                    return Ok(false);
                };
                Ok(match handle.place()? {
                    state::Location::Attached(owner, id) => {
                        std::sync::Arc::ptr_eq(&owner, &self.inner)
                            && read_document(&self.inner, |file| Ok(file.contains_layer(id)))?
                    }
                    _ => false,
                })
            }

            fn __repr__(&self) -> PyResult<String> {
                read_document(&self.inner, |file| {
                    Ok(format!(
                        "{}({}x{}, {} bit, {} layers)",
                        $document_name,
                        file.width,
                        file.height,
                        <Sample as BitDepth>::DEPTH,
                        tree_ops::flat_ids(file).len()
                    ))
                })
            }
        }
    };
}
