//! `ImageLayer_*bit`: pixel layers built from and read as NumPy arrays.

/// A layer class with upstream's read-only `ImageDataMixin` methods plus
/// `$extra` methods. The shared methods are written out here (not invoked
/// as a nested macro) so `#[pymethods]` sees them.
macro_rules! data_class {
    ($py_name:literal, $ty:ident, { $($extra:tt)* }) => {
        #[pyclass(name = $py_name, module = "photoshopapi", extends = PyLayer)]
        pub struct $ty;

        #[pymethods]
        impl $ty {
            #[pyo3(signature = (include_mask=true))]
            fn channel_indices(self_: PyRef<'_, Self>, include_mask: bool) -> PyResult<Vec<i16>> {
                layer_ops::channel_indices(&self_.as_super().handle, include_mask)
            }

            #[pyo3(signature = (include_mask=true))]
            fn num_channels(self_: PyRef<'_, Self>, include_mask: bool) -> PyResult<usize> {
                Ok(layer_ops::channel_indices(&self_.as_super().handle, include_mask)?.len())
            }

            /// Every channel as `{index: (height, width) array}`; mask
            /// channels have the mask's own size.
            fn get_image_data<'py>(
                self_: PyRef<'_, Self>,
                py: Python<'py>,
            ) -> PyResult<Bound<'py, PyDict>> {
                layer_ops::get_image_data(py, &self_.as_super().handle)
            }

            fn __getitem__<'py>(
                self_: PyRef<'_, Self>,
                py: Python<'py>,
                key: &Bound<'_, PyAny>,
            ) -> PyResult<Bound<'py, PyArray2<Sample>>> {
                let base = self_.as_super();
                let key = layer_ops::channel_key(base.color_mode, key)?;
                layer_ops::get_channel(py, &base.handle, key)
            }

            #[pyo3(signature = (key))]
            fn get_channel_by_index<'py>(
                self_: PyRef<'_, Self>,
                py: Python<'py>,
                key: i16,
            ) -> PyResult<Bound<'py, PyArray2<Sample>>> {
                let base = self_.as_super();
                crate::numpy_io::validate_index(key, base.color_mode)?;
                layer_ops::get_channel(py, &base.handle, ChannelKey(key))
            }

            #[pyo3(signature = (key))]
            fn get_channel_by_id<'py>(
                self_: PyRef<'_, Self>,
                py: Python<'py>,
                key: &Bound<'_, PyAny>,
            ) -> PyResult<Bound<'py, PyArray2<Sample>>> {
                let base = self_.as_super();
                let key = crate::numpy_io::channel_id_key(base.color_mode, enum_value(key)?)?;
                layer_ops::get_channel(py, &base.handle, key)
            }

            $($extra)*
        }
    };
}

macro_rules! image_class {
    ($image_name:literal) => {
        data_class!($image_name, PyImageLayer, {
            /// Build a layer from image data: a `(channels, height, width)`
            /// or `(channels, height * width)` array, or a dict of channel
            /// index or `ChannelID` to `(height, width)` arrays (which may
            /// carry the mask at `-2`). `pos_x`/`pos_y` are the layer's
            /// center on the canvas, as upstream.
            #[new]
            // Upstream's keyword constructor, argument for argument.
            #[allow(clippy::too_many_arguments)]
            #[pyo3(signature = (
                image_data, layer_name, layer_mask=None, width=0, height=0, blend_mode=None,
                pos_x=0.0, pos_y=0.0, opacity=1.0, compression=None, color_mode=None,
                is_visible=true, is_locked=false
            ))]
            fn new<'py>(
                py: Python<'py>,
                image_data: &Bound<'py, PyAny>,
                layer_name: String,
                layer_mask: Option<&Bound<'py, PyAny>>,
                width: i64,
                height: i64,
                blend_mode: Option<Bound<'py, PyAny>>,
                pos_x: f64,
                pos_y: f64,
                opacity: f32,
                compression: Option<Bound<'py, PyAny>>,
                color_mode: Option<&Bound<'py, PyAny>>,
                is_visible: bool,
                is_locked: bool,
            ) -> PyResult<PyClassInitializer<Self>> {
                check_name(&layer_name)?;
                if width < 0 || height < 0 {
                    return Err(PyValueError::new_err("width and height cannot be negative"));
                }
                let mode = match color_mode {
                    Some(mode) => color_mode_from_py(mode)?,
                    None => ColorMode::Rgb,
                };
                let (height, width) = if width == 0 || height == 0 {
                    crate::numpy_io::infer_dimensions::<Sample>(image_data, mode)?
                } else {
                    (height as usize, width as usize)
                };
                let parsed =
                    crate::numpy_io::parse_image::<Sample>(image_data, height, width, mode)?;
                let bounds = Rect::centered(pos_x, pos_y, width as u32, height as u32)
                    .map_err(psd_error)?;
                let mut layer = Layer::<Sample>::new_image(layer_name, bounds);
                layer.image_mut().expect("new image layer").channels = parsed.channels;
                let mask = match (layer_mask, parsed.mask) {
                    (Some(_), Some(_)) => {
                        return Err(PyValueError::new_err("the mask was given twice"));
                    }
                    (Some(mask), None) => {
                        let pixels = crate::numpy_io::channel_plane::<Sample>(mask, height, width)
                            .map_err(|_| {
                                PyValueError::new_err(
                                    "layer_mask must have the same size as the layer (width * height)",
                                )
                            })?;
                        Some((pixels, bounds))
                    }
                    (None, Some((pixels, mask_height, mask_width))) => {
                        let rect =
                            Rect::centered(pos_x, pos_y, mask_width as u32, mask_height as u32)
                                .map_err(psd_error)?;
                        Some((pixels, rect))
                    }
                    (None, None) => None,
                };
                if let Some((pixels, rect)) = mask {
                    layer.set_mask(pixels, rect).map_err(psd_error)?;
                }
                layer_ops::CommonOptions {
                    opacity,
                    blend_mode,
                    compression,
                    is_visible,
                    is_locked,
                }
                .apply(py, &mut layer)?;
                Ok(PyClassInitializer::from(PyLayer::new_base(
                    LayerHandle::detached(layer, None),
                    mode,
                ))
                .add_subclass(Self))
            }

            /// Replace every channel; pass `width` and `height` to resize the
            /// layer (around its center) to new image data.
            #[pyo3(signature = (data, width=None, height=None))]
            fn set_image_data(
                self_: PyRef<'_, Self>,
                data: &Bound<'_, PyAny>,
                width: Option<i64>,
                height: Option<i64>,
            ) -> PyResult<()> {
                let base = self_.as_super();
                layer_ops::set_image_data(&base.handle, base.color_mode, data, width, height)
            }

            fn __setitem__(
                self_: PyRef<'_, Self>,
                key: &Bound<'_, PyAny>,
                data: &Bound<'_, PyAny>,
            ) -> PyResult<()> {
                let base = self_.as_super();
                let key = layer_ops::channel_key(base.color_mode, key)?;
                layer_ops::set_channel(&base.handle, key, data)
            }

            fn set_channel_by_index(
                self_: PyRef<'_, Self>,
                key: i16,
                data: &Bound<'_, PyAny>,
            ) -> PyResult<()> {
                let base = self_.as_super();
                crate::numpy_io::validate_index(key, base.color_mode)?;
                layer_ops::set_channel(&base.handle, ChannelKey(key), data)
            }

            fn set_channel_by_id(
                self_: PyRef<'_, Self>,
                key: &Bound<'_, PyAny>,
                data: &Bound<'_, PyAny>,
            ) -> PyResult<()> {
                let base = self_.as_super();
                let key = crate::numpy_io::channel_id_key(base.color_mode, enum_value(key)?)?;
                layer_ops::set_channel(&base.handle, key, data)
            }
        });
    };
}
