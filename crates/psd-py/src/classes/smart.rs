//! `SmartObjectLayer_*bit`: linked smart objects, their warp and placement.
//!
//! Image data is read-only (upstream): edit it with `replace()`. Every edit
//! re-renders the preview right away. A layer built with the constructor
//! works before it is added to its document, since the link records live on
//! the `LayeredFile` passed in.

macro_rules! smart_class {
    ($smart_name:literal) => {
        fn smart_edit(
            layer: &PyRef<'_, PySmartObjectLayer>,
            edit: impl Fn(&mut psd::Warp) -> psd::core::Result<()>,
        ) -> PyResult<()> {
            layer.as_super().handle.with_smart_object(
                |file, id| file.edit_smart_object_warp(id, &edit).map_err(psd_error),
                |file, layer| {
                    file.edit_smart_object_layer_warp(layer, &edit)
                        .map_err(psd_error)
                },
            )
        }

        fn smart_read<R>(
            layer: &PyRef<'_, PySmartObjectLayer>,
            attached: impl FnOnce(&LayeredFile<Sample>, LayerId) -> psd::core::Result<R>,
            detached: impl FnOnce(&LayeredFile<Sample>, &Layer<Sample>) -> psd::core::Result<R>,
        ) -> PyResult<R> {
            layer.as_super().handle.with_smart_object(
                |file, id| attached(file, id).map_err(psd_error),
                |file, layer| detached(file, layer).map_err(psd_error),
            )
        }

        fn linked_identity_of(layer: &Layer<Sample>) -> PyResult<String> {
            if let Some(data) = layer.smart_object_data().map_err(psd_error)? {
                if let Some(identity) = data
                    .descriptor
                    .get("Idnt")
                    .and_then(psd::core::DescriptorValue::as_str)
                    .filter(|identity| !identity.is_empty())
                {
                    return Ok(identity.to_owned());
                }
            }
            if let Some(placed) = layer.placed_layer().map_err(psd_error)? {
                if !placed.unique_id.value().is_empty() {
                    return Ok(placed.unique_id.value().to_owned());
                }
            }
            Err(PyRuntimeError::new_err(
                "the smart object has no linked-source identity",
            ))
        }

        fn linked_file_name(file: &LayeredFile<Sample>, identity: &str) -> PyResult<String> {
            file.linked_layer_views()
                .map_err(psd_error)?
                .into_iter()
                .find(|record| record.unique_id.value() == identity)
                .map(|record| record.file_name.value().to_owned())
                .ok_or_else(|| PyRuntimeError::new_err("the smart object's link record is missing"))
        }

        data_class!($smart_name, PySmartObjectLayer, {
            /// Link the image at `path` into `layered_file` and build a
            /// smart-object layer showing it (add it with `add_layer`).
            /// Without `warp` the image keeps its native size.
            #[new]
            // Upstream's keyword constructor, argument for argument.
            #[allow(clippy::too_many_arguments)]
            #[pyo3(signature = (
                        layered_file, path, layer_name, link_type=None, warp=None, layer_mask=None,
                        blend_mode=None, opacity=1.0, compression=None, color_mode=None,
                        is_visible=true, is_locked=false
                    ))]
            fn new<'py>(
                py: Python<'py>,
                layered_file: PyRef<'py, PyDocument>,
                path: std::path::PathBuf,
                layer_name: String,
                link_type: Option<&Bound<'py, PyAny>>,
                warp: Option<PyRef<'py, PySmartWarp>>,
                layer_mask: Option<&Bound<'py, PyAny>>,
                blend_mode: Option<Bound<'py, PyAny>>,
                opacity: f32,
                compression: Option<Bound<'py, PyAny>>,
                color_mode: Option<&Bound<'py, PyAny>>,
                is_visible: bool,
                is_locked: bool,
            ) -> PyResult<PyClassInitializer<Self>> {
                check_name(&layer_name)?;
                let storage = linkage_from_py(link_type)?;
                let document = std::sync::Arc::clone(&layered_file.inner);
                let mode = match color_mode {
                    Some(mode) => color_mode_from_py(mode)?,
                    None => read_document(&document, |file| Ok(file.color_mode))?,
                };
                let warp = warp.map(|warp| warp.warp.clone());
                let mut layer = write_document(&document, |file| {
                    file.create_smart_object(layer_name, &path, storage, warp)
                        .map_err(psd_error)
                })?;
                if let Some(mask) = layer_mask {
                    let (pixels, height, width) = crate::numpy_io::mask_array::<Sample>(mask)?;
                    let bounds = layer.bounds;
                    if (height, width)
                        != (
                            bounds.height().max(0) as usize,
                            bounds.width().max(0) as usize,
                        )
                    {
                        return Err(PyValueError::new_err(format!(
                            "layer_mask must match the smart object's size ({}, {})",
                            bounds.height(),
                            bounds.width()
                        )));
                    }
                    layer.set_mask(pixels, bounds).map_err(psd_error)?;
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
                    LayerHandle::detached(layer, Some(document)),
                    mode,
                ))
                .add_subclass(Self))
            }

            /// The warp (a copy; assign it back to apply edits).
            #[getter]
            fn warp(self_: PyRef<'_, Self>, py: Python<'_>) -> PyResult<Py<PySmartWarp>> {
                let warp = self_
                    .as_super()
                    .handle
                    .with_layer(|layer| layer.warp().map_err(psd_error))?
                    .ok_or_else(|| PyRuntimeError::new_err("the smart object has no warp"))?;
                Py::new(py, PySmartWarp::from_warp(warp))
            }

            #[setter]
            fn set_warp(self_: PyRef<'_, Self>, warp: PyRef<'_, PySmartWarp>) -> PyResult<()> {
                let warp = warp.warp.clone();
                self_.as_super().handle.with_smart_object(
                    |file, id| file.set_smart_object_warp(id, &warp).map_err(psd_error),
                    |file, layer| {
                        file.set_smart_object_layer_warp(layer, &warp)
                            .map_err(psd_error)
                    },
                )
            }

            #[getter]
            fn linkage(self_: PyRef<'_, Self>, py: Python<'_>) -> PyResult<Py<PyAny>> {
                let storage = smart_read(
                    &self_,
                    |file, id| file.smart_object_storage(id),
                    |file, layer| file.smart_object_layer_storage(layer),
                )?;
                py_enum(py, "LinkedLayerType", linkage_number(storage))
            }

            /// Switch between embedded and external storage (all layers
            /// sharing the link switch when the layer is in a document).
            #[setter]
            fn set_linkage(self_: PyRef<'_, Self>, value: &Bound<'_, PyAny>) -> PyResult<()> {
                let storage = linkage_from_py(Some(value))?;
                self_.as_super().handle.with_smart_object(
                    |file, id| {
                        file.set_smart_object_storage(id, storage)
                            .map_err(psd_error)
                    },
                    |file, layer| {
                        file.set_smart_object_layer_storage(layer, storage)
                            .map_err(psd_error)
                    },
                )
            }

            /// Replace the linked image, keeping the warp and placement.
            #[pyo3(signature = (path, link_externally=false))]
            fn replace(
                self_: PyRef<'_, Self>,
                path: std::path::PathBuf,
                link_externally: bool,
            ) -> PyResult<()> {
                let storage = if link_externally {
                    LinkedStorage::External
                } else {
                    LinkedStorage::Embedded
                };
                self_.as_super().handle.with_smart_object(
                    |file, id| {
                        file.replace_smart_object_with_storage(id, &path, storage)
                            .map_err(psd_error)
                    },
                    |file, layer| {
                        file.replace_smart_object_layer(layer, &path, storage)
                            .map_err(psd_error)
                    },
                )
            }

            /// The link identity shared with the document's linked records.
            fn hash(self_: PyRef<'_, Self>) -> PyResult<String> {
                self_.as_super().handle.with_layer(linked_identity_of)
            }

            fn filename(self_: PyRef<'_, Self>) -> PyResult<String> {
                let identity = self_.as_super().handle.with_layer(linked_identity_of)?;
                self_.as_super().handle.with_smart_object(
                    |file, _| linked_file_name(file, &identity),
                    |file, _| linked_file_name(file, &identity),
                )
            }

            /// The source file: the path this session linked, or an external
            /// link's path; `None` for embedded sources read from a file.
            fn filepath(self_: PyRef<'_, Self>) -> PyResult<Option<std::path::PathBuf>> {
                smart_read(
                    &self_,
                    |file, id| file.smart_object_source_path(id),
                    |file, layer| file.smart_object_layer_source_path(layer),
                )
            }

            /// The full-resolution linked image, `{index: array}`.
            fn get_original_image_data<'py>(
                self_: PyRef<'_, Self>,
                py: Python<'py>,
            ) -> PyResult<Bound<'py, PyDict>> {
                let source = smart_read(
                    &self_,
                    |file, id| file.smart_object_source(id),
                    |file, layer| file.smart_object_source_of(layer),
                )?;
                let result = PyDict::new(py);
                for (key, data) in source.channels().iter() {
                    result.set_item(
                        key.index(),
                        crate::numpy_io::channel_array(py, data, source.height(), source.width())?,
                    )?;
                }
                Ok(result)
            }

            fn original_width(self_: PyRef<'_, Self>) -> PyResult<usize> {
                Ok(smart_read(
                    &self_,
                    |file, id| file.smart_object_source(id),
                    |file, layer| file.smart_object_source_of(layer),
                )?
                .width())
            }

            fn original_height(self_: PyRef<'_, Self>) -> PyResult<usize> {
                Ok(smart_read(
                    &self_,
                    |file, id| file.smart_object_source(id),
                    |file, layer| file.smart_object_source_of(layer),
                )?
                .height())
            }

            /// Move by `(x_offset, y_offset)` pixels.
            #[pyo3(name = "move")]
            fn move_by(self_: PyRef<'_, Self>, x_offset: f64, y_offset: f64) -> PyResult<()> {
                let offset = psd::Point2::new(x_offset, y_offset);
                smart_edit(&self_, |warp| warp.translate(offset))
            }

            /// Rotate by `angle` degrees (clockwise) around `(x, y)`, e.g.
            /// `layer.rotate(45, layer.center_x, layer.center_y)`.
            #[pyo3(signature = (angle, x, y))]
            fn rotate(self_: PyRef<'_, Self>, angle: f64, x: f64, y: f64) -> PyResult<()> {
                let center = psd::Point2::new(x, y);
                smart_edit(&self_, |warp| warp.rotate(angle, center))
            }

            /// Scale by `(x_scalar, y_scalar)` around `(x, y)`.
            fn scale(
                self_: PyRef<'_, Self>,
                x_scalar: f64,
                y_scalar: f64,
                x: f64,
                y: f64,
            ) -> PyResult<()> {
                let (factors, center) =
                    (psd::Point2::new(x_scalar, y_scalar), psd::Point2::new(x, y));
                smart_edit(&self_, |warp| warp.scale(factors, center))
            }

            /// Apply a 3×3 affine or perspective matrix.
            fn transform(
                self_: PyRef<'_, Self>,
                matrix: PyReadonlyArray2<'_, f64>,
            ) -> PyResult<()> {
                let view = matrix.as_array();
                if view.shape() != [3, 3] {
                    return Err(PyValueError::new_err("the transform must be a 3x3 matrix"));
                }
                let rows =
                    std::array::from_fn(|row| std::array::from_fn(|column| view[[row, column]]));
                let homography = psd::Homography::from_rows(rows).map_err(psd_error)?;
                smart_edit(&self_, |warp| warp.transform(&homography))
            }

            /// Undo the placement transforms, keeping the warp.
            fn reset_transform(self_: PyRef<'_, Self>) -> PyResult<()> {
                smart_edit(&self_, psd::Warp::reset_transform)
            }

            /// Flatten the warp, keeping the placement.
            fn reset_warp(self_: PyRef<'_, Self>) -> PyResult<()> {
                smart_edit(&self_, psd::Warp::reset_warp)
            }
        });
    };
}
