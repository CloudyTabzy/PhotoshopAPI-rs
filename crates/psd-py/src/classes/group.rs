//! `GroupLayer_*bit`: groups, their children, and group masks.

macro_rules! group_class {
    ($group_name:literal) => {
        #[pyclass(name = $group_name, module = "photoshopapi", extends = PyLayer)]
        pub struct PyGroupLayer;

        #[pymethods]
        impl PyGroupLayer {
            /// Build a group. `layer_mask` (a `width * height` 1-D or 2-D
            /// array) is centered on `pos_x`/`pos_y`; like upstream, a size
            /// without a mask is an error.
            #[new]
            // Upstream's keyword constructor, argument for argument.
            #[allow(clippy::too_many_arguments)]
            #[pyo3(signature = (
                layer_name, layer_mask=None, width=0, height=0, blend_mode=None, pos_x=0.0,
                pos_y=0.0, opacity=1.0, compression=None, color_mode=None, is_collapsed=false,
                is_visible=true, is_locked=false
            ))]
            fn new<'py>(
                py: Python<'py>,
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
                is_collapsed: bool,
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
                let mut layer = Layer::<Sample>::new_group(layer_name);
                match layer_mask {
                    Some(mask) => {
                        let (width, height) = (width as usize, height as usize);
                        let pixels = crate::numpy_io::channel_plane::<Sample>(mask, height, width)
                            .map_err(|_| {
                                PyValueError::new_err(
                                    "layer_mask must have the same size as the layer (width * height)",
                                )
                            })?;
                        if width == 0 || height == 0 {
                            return Err(PyValueError::new_err(
                                "pass the mask's width and height with layer_mask",
                            ));
                        }
                        let rect = Rect::centered(pos_x, pos_y, width as u32, height as u32)
                            .map_err(psd_error)?;
                        layer.set_mask(pixels, rect).map_err(psd_error)?;
                    }
                    None if width > 0 || height > 0 => {
                        return Err(PyRuntimeError::new_err(format!(
                            "a group size ({width}x{height}) only applies to a mask; pass layer_mask"
                        )));
                    }
                    None => {}
                }
                layer.group_mut().expect("new group").open = !is_collapsed;
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

            /// Child layers, top to bottom.
            #[getter]
            fn layers(self_: PyRef<'_, Self>, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
                let base = self_.as_super();
                wrap_all(py, tree_ops::group_children(&base.handle)?, base.color_mode)
            }

            /// Replace the children: current children not listed are
            /// removed, listed layers are moved or added (top to bottom).
            #[setter]
            fn set_layers(self_: PyRef<'_, Self>, layers: Vec<Bound<'_, PyAny>>) -> PyResult<()> {
                let base = self_.as_super();
                let (document, group) = base.handle.location().map_err(|_| {
                    PyValueError::new_err("add the group to a document before assigning its layers")
                })?;
                let wanted: Vec<LayerHandle<Sample>> = layers
                    .iter()
                    .map(|layer| {
                        handle_of(layer)
                            .ok_or_else(|| PyTypeError::new_err("expected layers of this bit depth"))
                    })
                    .collect::<PyResult<_>>()?;
                let wanted_ids: Vec<Option<LayerId>> = wanted
                    .iter()
                    .map(|handle| match handle.place() {
                        Ok(state::Location::Attached(owner, id)) if Arc::ptr_eq(&owner, &document) => {
                            Some(id)
                        }
                        _ => None,
                    })
                    .collect();
                let current = read_document(&document, |file| {
                    Ok(tree_ops::visible_children(file, Some(group)))
                })?;
                for id in current {
                    if !wanted_ids.contains(&Some(id)) {
                        tree_ops::remove_id(&document, id, None)?;
                    }
                }
                // Bottom first, each placed on top of the previous one.
                for (handle, id) in wanted.iter().zip(&wanted_ids).rev() {
                    match id {
                        Some(id) => write_document(&document, |file| {
                            file.move_layer(*id, Some(group), None).map_err(psd_error)
                        })?,
                        None => {
                            handle.attach_to(&document, Some(group), None)?;
                        }
                    }
                }
                Ok(())
            }

            #[getter]
            fn is_collapsed(self_: PyRef<'_, Self>) -> PyResult<bool> {
                self_
                    .as_super()
                    .handle
                    .with_layer(|layer| Ok(layer.group().is_some_and(|group| !group.open)))
            }

            #[setter]
            fn set_is_collapsed(self_: PyRef<'_, Self>, value: &Bound<'_, PyAny>) -> PyResult<()> {
                layer_ops::set_flag(&self_.as_super().handle, value, |layer, collapsed| {
                    if let Some(group) = layer.group_mut() {
                        group.open = !collapsed;
                    }
                })
            }

            /// Add a layer at the bottom of the group. Upstream's signature
            /// passes the document first (`add_layer(layered_file, layer)`);
            /// both forms are accepted.
            #[pyo3(signature = (*args))]
            fn add_layer(self_: PyRef<'_, Self>, args: &Bound<'_, pyo3::types::PyTuple>) -> PyResult<()> {
                let layer = match args.len() {
                    1 => args.get_item(0)?,
                    2 => args.get_item(1)?,
                    _ => {
                        return Err(PyTypeError::new_err(
                            "add_layer(layer) or add_layer(layered_file, layer)",
                        ))
                    }
                };
                let child = handle_of(&layer)
                    .ok_or_else(|| PyTypeError::new_err("expected a layer of this bit depth"))?;
                tree_ops::add_child(Some(&self_.as_super().handle), None, &child)
            }

            /// Remove a child by index (top to bottom), layer, or name.
            fn remove_layer(self_: PyRef<'_, Self>, value: &Bound<'_, PyAny>) -> PyResult<()> {
                let base = self_.as_super();
                let children = tree_ops::group_children(&base.handle)?;
                let position = if let Ok(index) = value.extract::<i64>() {
                    usize::try_from(index)
                        .ok()
                        .filter(|&index| index < children.len())
                        .ok_or_else(|| PyIndexError::new_err("child index is out of range"))?
                } else if let Ok(name) = value.extract::<String>() {
                    let mut found = None;
                    for (index, child) in children.iter().enumerate() {
                        if child.with_layer(|layer| Ok(layer.name == name))? {
                            found = Some(index);
                            break;
                        }
                    }
                    found.ok_or_else(|| PyKeyError::new_err(format!("no child named '{name}'")))?
                } else {
                    let target = handle_of(value)
                        .ok_or_else(|| PyTypeError::new_err("expected an index, layer, or name"))?;
                    let identity = target.identity()?;
                    let mut found = None;
                    for (index, child) in children.iter().enumerate() {
                        if child.identity()? == identity {
                            found = Some(index);
                            break;
                        }
                    }
                    found.ok_or_else(|| {
                        PyValueError::new_err("the layer is not a child of this group")
                    })?
                };
                let passed = handle_of(value);
                match base.handle.place()? {
                    state::Location::Attached(document, _) => {
                        let (_, id) = children[position].location()?;
                        tree_ops::remove_id(&document, id, passed.as_ref())
                    }
                    _ => {
                        // Detached trees store children bottom to top.
                        let stored = children.len() - 1 - position;
                        base.handle.take_nested_child(stored).map(|_| ())
                    }
                }
            }

            fn __getitem__(self_: PyRef<'_, Self>, py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
                let base = self_.as_super();
                for child in tree_ops::group_children(&base.handle)? {
                    if child.with_layer(|layer| Ok(layer.name == name))? {
                        return wrap(py, child, base.color_mode);
                    }
                }
                Err(PyKeyError::new_err(format!("no layer named '{name}' in the group")))
            }

            fn __len__(self_: PyRef<'_, Self>) -> PyResult<usize> {
                Ok(tree_ops::group_children(&self_.as_super().handle)?.len())
            }

            fn __iter__(self_: PyRef<'_, Self>, py: Python<'_>) -> PyResult<Py<PyAny>> {
                let base = self_.as_super();
                let layers = wrap_all(py, tree_ops::group_children(&base.handle)?, base.color_mode)?;
                Ok(pyo3::types::PyList::new(py, layers)?.try_iter()?.into_any().unbind())
            }
        }
    };
}
