//! `Layer_*bit`: the settings, geometry, and mask API every layer shares.

macro_rules! layer_class {
    ($layer_name:literal) => {
        #[pyclass(name = $layer_name, module = "photoshopapi", subclass)]
        pub struct PyLayer {
            pub(crate) handle: LayerHandle<Sample>,
            pub(crate) color_mode: ColorMode,
        }

        impl PyLayer {
            pub(crate) fn new_base(handle: LayerHandle<Sample>, color_mode: ColorMode) -> Self {
                Self { handle, color_mode }
            }
        }

        #[pymethods]
        impl PyLayer {
            #[getter]
            fn name(&self) -> PyResult<String> {
                self.handle.with_layer(|layer| Ok(layer.name.clone()))
            }

            #[setter]
            fn set_name(&self, name: String) -> PyResult<()> {
                check_name(&name)?;
                self.handle.with_layer_mut(|layer| {
                    layer.name = name;
                    Ok(())
                })
            }

            #[getter]
            fn blend_mode(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
                layer_ops::blend_mode(py, &self.handle)
            }

            #[setter]
            fn set_blend_mode(&self, py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<()> {
                layer_ops::set_blend_mode(py, &self.handle, value)
            }

            #[getter]
            fn opacity(&self) -> PyResult<f32> {
                layer_ops::opacity(&self.handle)
            }

            #[setter]
            fn set_opacity(&self, py: Python<'_>, value: f32) -> PyResult<()> {
                layer_ops::set_opacity(py, &self.handle, value)
            }

            #[getter]
            fn fill(&self) -> PyResult<f32> {
                layer_ops::fill(&self.handle)
            }

            #[setter]
            fn set_fill(&self, py: Python<'_>, value: f32) -> PyResult<()> {
                layer_ops::set_fill(py, &self.handle, value)
            }

            #[getter]
            fn width(&self) -> PyResult<i64> {
                layer_ops::width(&self.handle)
            }

            #[setter]
            fn set_width(&self, value: i64) -> PyResult<()> {
                layer_ops::set_size(&self.handle, Some(value), None)
            }

            #[getter]
            fn height(&self) -> PyResult<i64> {
                layer_ops::height(&self.handle)
            }

            #[setter]
            fn set_height(&self, value: i64) -> PyResult<()> {
                layer_ops::set_size(&self.handle, None, Some(value))
            }

            #[getter]
            fn center_x(&self) -> PyResult<f64> {
                Ok(layer_ops::center(&self.handle)?.0)
            }

            #[setter]
            fn set_center_x(&self, value: f64) -> PyResult<()> {
                layer_ops::set_center(&self.handle, Some(value), None)
            }

            #[getter]
            fn center_y(&self) -> PyResult<f64> {
                Ok(layer_ops::center(&self.handle)?.1)
            }

            #[setter]
            fn set_center_y(&self, value: f64) -> PyResult<()> {
                layer_ops::set_center(&self.handle, None, Some(value))
            }

            #[getter]
            fn is_visible(&self) -> PyResult<bool> {
                self.handle.with_layer(|layer| Ok(layer.is_visible()))
            }

            #[setter]
            fn set_is_visible(&self, value: &Bound<'_, PyAny>) -> PyResult<()> {
                layer_ops::set_flag(&self.handle, value, |layer, visible| {
                    layer.set_visible(visible)
                })
            }

            #[getter]
            fn is_locked(&self) -> PyResult<bool> {
                self.handle.with_layer(|layer| Ok(layer.is_locked()))
            }

            #[setter]
            fn set_is_locked(&self, value: &Bound<'_, PyAny>) -> PyResult<()> {
                layer_ops::set_flag(&self.handle, value, |layer, locked| {
                    layer.set_locked(locked)
                })
            }

            #[getter]
            fn clipping_mask(&self) -> PyResult<bool> {
                self.handle.with_layer(|layer| Ok(layer.is_clipping_mask()))
            }

            #[setter]
            fn set_clipping_mask(&self, value: &Bound<'_, PyAny>) -> PyResult<()> {
                layer_ops::set_flag(&self.handle, value, |layer, clipped| {
                    layer.set_clipping_mask(clipped)
                })
            }

            #[getter]
            fn display_color(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
                layer_ops::display_color(py, &self.handle)
            }

            #[setter]
            fn set_display_color(&self, value: &Bound<'_, PyAny>) -> PyResult<()> {
                layer_ops::set_display_color(&self.handle, value)
            }

            /// Whether the layer has a pixel mask.
            fn has_mask(&self) -> PyResult<bool> {
                self.handle.with_layer(|layer| Ok(layer.has_mask()))
            }

            /// The pixel mask as a `(height, width)` array; empty without one.
            #[getter]
            fn mask<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
                layer_ops::mask(py, &self.handle)
            }

            #[setter]
            fn set_mask(&self, value: &Bound<'_, PyAny>) -> PyResult<()> {
                layer_ops::set_mask(&self.handle, value)
            }

            #[getter]
            fn mask_disabled(&self) -> PyResult<bool> {
                Ok(layer_ops::mask_flags(&self.handle)?.0)
            }

            #[setter]
            fn set_mask_disabled(&self, value: &Bound<'_, PyAny>) -> PyResult<()> {
                layer_ops::set_mask_flag(&self.handle, value, true)
            }

            #[getter]
            fn mask_relative_to_layer(&self) -> PyResult<bool> {
                Ok(layer_ops::mask_flags(&self.handle)?.1)
            }

            #[setter]
            fn set_mask_relative_to_layer(&self, value: &Bound<'_, PyAny>) -> PyResult<()> {
                layer_ops::set_mask_flag(&self.handle, value, false)
            }

            #[getter]
            fn mask_default_color(&self) -> PyResult<u8> {
                Ok(layer_ops::mask_flags(&self.handle)?.2)
            }

            #[setter]
            fn set_mask_default_color(&self, value: &Bound<'_, PyAny>) -> PyResult<()> {
                layer_ops::set_mask_default_color(&self.handle, value)
            }

            #[getter]
            fn mask_density(&self) -> PyResult<Option<u8>> {
                layer_ops::mask_density(&self.handle)
            }

            #[setter]
            fn set_mask_density(&self, value: &Bound<'_, PyAny>) -> PyResult<()> {
                layer_ops::set_mask_density(&self.handle, value)
            }

            #[getter]
            fn mask_feather(&self) -> PyResult<Option<f64>> {
                layer_ops::mask_feather(&self.handle)
            }

            #[setter]
            fn set_mask_feather(&self, value: Option<f64>) -> PyResult<()> {
                layer_ops::set_mask_feather(&self.handle, value)
            }

            #[getter]
            fn mask_position(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
                layer_ops::mask_position(py, &self.handle)
            }

            #[setter]
            fn set_mask_position(&self, value: &Bound<'_, PyAny>) -> PyResult<()> {
                layer_ops::set_mask_position(&self.handle, value)
            }

            fn mask_width(&self) -> PyResult<i64> {
                Ok(layer_ops::mask_size(&self.handle)?.0)
            }

            fn mask_height(&self) -> PyResult<i64> {
                Ok(layer_ops::mask_size(&self.handle)?.1)
            }

            /// Write compression for the mask channel.
            fn set_mask_compression(&self, compression: &Bound<'_, PyAny>) -> PyResult<()> {
                layer_ops::set_mask_compression(&self.handle, compression)
            }

            /// Write compression for every channel of this layer.
            fn set_compression(&self, compression: &Bound<'_, PyAny>) -> PyResult<()> {
                let compression = crate::convert::compression_from_py(compression)?;
                self.handle.with_layer_mut(|layer| {
                    layer.compression = Some(compression);
                    layer.mask_compression = None;
                    Ok(())
                })
            }

            /// `"image"`, `"group"`, `"text"`, `"smart_object"`, or
            /// `"section_divider"`.
            #[getter]
            fn kind(&self) -> PyResult<&'static str> {
                self.handle.with_layer(|layer| {
                    Ok(match layer_ops::kind_of(layer) {
                        layer_ops::Kind::Image => "image",
                        layer_ops::Kind::Group => "group",
                        layer_ops::Kind::Text => "text",
                        layer_ops::Kind::SmartObject => "smart_object",
                        layer_ops::Kind::Divider => "section_divider",
                    })
                })
            }

            fn __eq__(&self, other: &Bound<'_, PyAny>) -> PyResult<bool> {
                match other.extract::<PyRef<'_, PyLayer>>() {
                    Ok(other) => Ok(self.handle.identity()? == other.handle.identity()?),
                    Err(_) => Ok(false),
                }
            }

            fn __hash__(&self) -> PyResult<u64> {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                self.handle.identity()?.hash(&mut hasher);
                Ok(hasher.finish())
            }

            fn __repr__(slf: &Bound<'_, Self>) -> PyResult<String> {
                let name = slf
                    .borrow()
                    .handle
                    .with_layer(|layer| Ok(layer.name.clone()))?;
                Ok(format!("{}({:?})", slf.get_type().qualname()?, name))
            }
        }

        /// The Python object for a handle, of the class matching its layer.
        pub(crate) fn wrap(
            py: Python<'_>,
            handle: LayerHandle<Sample>,
            color_mode: ColorMode,
        ) -> PyResult<Py<PyAny>> {
            let kind = handle.with_layer(|layer| Ok(layer_ops::kind_of(layer)))?;
            let base = PyClassInitializer::from(PyLayer::new_base(handle, color_mode));
            Ok(match kind {
                layer_ops::Kind::Image => Py::new(py, base.add_subclass(PyImageLayer))?.into_any(),
                layer_ops::Kind::Group => Py::new(py, base.add_subclass(PyGroupLayer))?.into_any(),
                layer_ops::Kind::Text => Py::new(py, base.add_subclass(PyTextLayer))?.into_any(),
                layer_ops::Kind::SmartObject => {
                    Py::new(py, base.add_subclass(PySmartObjectLayer))?.into_any()
                }
                layer_ops::Kind::Divider => Py::new(py, base)?.into_any(),
            })
        }

        pub(crate) fn wrap_all(
            py: Python<'_>,
            handles: Vec<LayerHandle<Sample>>,
            color_mode: ColorMode,
        ) -> PyResult<Vec<Py<PyAny>>> {
            handles
                .into_iter()
                .map(|handle| wrap(py, handle, color_mode))
                .collect()
        }

        /// The handle behind a Python layer object of this depth.
        pub(crate) fn handle_of(value: &Bound<'_, PyAny>) -> Option<LayerHandle<Sample>> {
            value
                .extract::<PyRef<'_, PyLayer>>()
                .ok()
                .map(|layer| layer.handle.clone())
        }
    };
}
