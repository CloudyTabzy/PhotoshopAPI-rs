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

            /// `"image"`, `"group"`, `"artboard"`, `"text"`, `"adjustment"`, `"shape"`,
            /// `"smart_object"`, or `"section_divider"`.
            #[getter]
            fn kind(&self) -> PyResult<&'static str> {
                self.handle.with_layer(|layer| {
                    Ok(match layer_ops::kind_of(layer) {
                        layer_ops::Kind::Image => "image",
                        layer_ops::Kind::Group => "group",
                        layer_ops::Kind::Text => "text",
                        layer_ops::Kind::Adjustment => "adjustment",
                        layer_ops::Kind::Shape => "shape",
                        layer_ops::Kind::Artboard => "artboard",
                        layer_ops::Kind::SmartObject => "smart_object",
                        layer_ops::Kind::Divider => "section_divider",
                    })
                })
            }

            /// Whether this layer carries adjustment or fill settings.
            fn is_adjustment_layer(&self) -> PyResult<bool> {
                self.handle
                    .with_layer(|layer| Ok(layer.is_adjustment_layer()))
            }

            /// Whether this layer carries a fill and a vector mask.
            fn is_shape_layer(&self) -> PyResult<bool> {
                self.handle.with_layer(|layer| Ok(layer.is_shape_layer()))
            }

            /// Whether this group carries artboard data.
            fn is_artboard(&self) -> PyResult<bool> {
                self.handle.with_layer(|layer| Ok(layer.is_artboard()))
            }

            /// The artboard tagged block as `(four_byte_key, payload_bytes)`.
            fn artboard_block(&self) -> PyResult<Option<(String, Vec<u8>)>> {
                layer_ops::artboard_block(&self.handle)
            }

            /// Artboard fields as a small dictionary, or `None` on a regular group.
            fn artboard_info<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyDict>>> {
                let artboard = self
                    .handle
                    .with_layer(|layer| layer.artboard().map_err(psd_error))?;
                let Some(artboard) = artboard else {
                    return Ok(None);
                };
                let info = PyDict::new(py);
                if let Some(rect) = artboard.rect() {
                    info.set_item("bounds", (rect.top, rect.left, rect.bottom, rect.right))?;
                }
                info.set_item("preset_name", artboard.preset_name())?;
                info.set_item(
                    "background_type",
                    artboard.background().map(ArtboardBackground::as_raw),
                )?;
                info.set_item("guide_indices", artboard.guide_indices())?;
                let color = artboard.background_color().and_then(Color::from_descriptor);
                let rgb = match color {
                    Some(Color::Rgb { red, green, blue }) => Some((red, green, blue)),
                    _ => None,
                };
                info.set_item("background_color", rgb)?;
                Ok(Some(info))
            }

            /// Set or replace the artboard block from a validated payload.
            fn set_artboard_block(&self, key: &str, payload: Vec<u8>) -> PyResult<()> {
                layer_ops::set_artboard_block(&self.handle, key, payload)
            }

            /// Remove artboard tagging from the group.
            fn clear_artboard(&self) -> PyResult<bool> {
                layer_ops::clear_artboard(&self.handle)
            }

            /// Adjustment/fill payloads as `(four_byte_key, payload_bytes)` pairs.
            fn adjustment_blocks(&self) -> PyResult<Vec<(String, Vec<u8>)>> {
                layer_ops::adjustment_blocks(&self.handle)
            }

            /// Add or replace a settings payload, such as `b"levl"` or `b"SoCo"`.
            fn set_adjustment_block(&self, key: &str, payload: Vec<u8>) -> PyResult<()> {
                layer_ops::set_adjustment(&self.handle, key, payload)
            }

            /// Remove one settings payload by its four-byte key.
            fn clear_adjustment_block(&self, key: &str) -> PyResult<bool> {
                layer_ops::clear_adjustment(&self.handle, key)
            }

            /// Vector payloads as `(four_byte_key, payload_bytes)` pairs.
            fn vector_blocks(&self) -> PyResult<Vec<(String, Vec<u8>)>> {
                layer_ops::vector_blocks(&self.handle)
            }

            /// Add or replace a vector payload, such as `b"vsms"` or `b"vscg"`.
            fn set_vector_block(&self, key: &str, payload: Vec<u8>) -> PyResult<()> {
                layer_ops::set_vector_block(&self.handle, key, payload)
            }

            /// Remove one vector payload by its four-byte key.
            fn clear_vector_block(&self, key: &str) -> PyResult<bool> {
                layer_ops::clear_vector_block(&self.handle, key)
            }

            /// Effects blocks as `(four_byte_key, payload_bytes)` pairs.
            fn layer_effects_blocks(&self) -> PyResult<Vec<(String, Vec<u8>)>> {
                layer_ops::layer_effects_blocks(&self.handle)
            }

            /// Replace the full modern effects descriptor, preserving unknown
            /// fields and trailing bytes. The legacy `lrFX` mirror is regenerated.
            fn set_layer_effects_block(&self, key: &str, payload: Vec<u8>) -> PyResult<()> {
                layer_ops::set_layer_effects_block(&self.handle, key, payload)
            }

            /// Remove the descriptor and legacy effects blocks.
            fn clear_layer_effects(&self) -> PyResult<()> {
                layer_ops::clear_layer_effects(&self.handle)
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
                layer_ops::Kind::Adjustment | layer_ops::Kind::Shape | layer_ops::Kind::Divider => {
                    Py::new(py, base)?.into_any()
                }
                layer_ops::Kind::Artboard => {
                    Py::new(py, base.add_subclass(PyGroupLayer))?.into_any()
                }
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
