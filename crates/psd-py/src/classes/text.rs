//! `TextLayer_*bit`: editable Photoshop text.
//!
//! The flat `style_run_*`/`style_normal_*`/`paragraph_run_*`/
//! `paragraph_normal_*` methods, the proxy classes, and the range helpers
//! are generated in `photoshopapi/_text.py` on top of the `_style`,
//! `_paragraph`, `_style_range`, `_paragraph_range`, and `_range_count`
//! accessors below.

macro_rules! text_class {
    ($text_name:literal) => {
        #[pyclass(name = $text_name, module = "photoshopapi", extends = PyLayer)]
        pub struct PyTextLayer;

        fn text_edit<R>(
            layer: &PyRef<'_, PyTextLayer>,
            edit: impl FnOnce(&mut Layer<Sample>) -> psd::core::Result<R>,
        ) -> PyResult<R> {
            layer
                .as_super()
                .handle
                .with_layer_mut(|layer| edit(layer).map_err(psd_error))
        }

        fn text_read<R>(
            layer: &PyRef<'_, PyTextLayer>,
            read: impl FnOnce(&Layer<Sample>) -> R,
        ) -> PyResult<R> {
            layer.as_super().handle.with_layer(|layer| Ok(read(layer)))
        }

        #[pymethods]
        impl PyTextLayer {
            /// Create a text layer from scratch (upstream's constructor).
            /// `fill_color` is `[A, R, G, B]` in `0..=1`; a zero box size
            /// makes point text.
            #[new]
            // Upstream's keyword constructor, argument for argument.
            #[allow(clippy::too_many_arguments)]
            #[pyo3(signature = (
                layer_name, text, font="ArialMT", font_size=24.0, fill_color=None,
                position_x=20.0, position_y=50.0, box_width=0.0, box_height=0.0, layer_mask=None,
                width=0, height=0, blend_mode=None, pos_x=0.0, pos_y=0.0, opacity=1.0,
                compression=None, color_mode=None, is_visible=true, is_locked=false
            ))]
            fn new<'py>(
                py: Python<'py>,
                layer_name: String,
                text: String,
                font: &str,
                font_size: f64,
                fill_color: Option<Vec<f64>>,
                position_x: f64,
                position_y: f64,
                box_width: f64,
                box_height: f64,
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
                let mut builder = TextLayerBuilder::new(layer_name, text)
                    .font(font)
                    .font_size(font_size)
                    .position(position_x, position_y);
                if let Some(color) = fill_color {
                    let color: [f64; 4] = color.try_into().map_err(|_| {
                        PyValueError::new_err("fill_color must have four values [A, R, G, B]")
                    })?;
                    builder = builder.fill_color(color);
                }
                if box_width != 0.0 || box_height != 0.0 {
                    builder = builder.box_size(box_width, box_height);
                }
                let mut layer = builder.build::<Sample>().map_err(psd_error)?;
                if let Some(mask) = layer_mask {
                    let (width, height) = (width as usize, height as usize);
                    let pixels = crate::numpy_io::channel_plane::<Sample>(mask, height, width)
                        .map_err(|_| {
                            PyValueError::new_err(
                                "layer_mask must have the same size as width * height",
                            )
                        })?;
                    let rect = Rect::centered(pos_x, pos_y, width as u32, height as u32)
                        .map_err(psd_error)?;
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

            // --- text ---------------------------------------------------

            /// The text, or `None` without a text payload.
            #[getter]
            fn text(self_: PyRef<'_, Self>) -> PyResult<Option<String>> {
                text_read(&self_, |layer| layer.text())
            }

            fn set_text(self_: PyRef<'_, Self>, value: &str) -> PyResult<()> {
                text_edit(&self_, |layer| layer.set_text(value))
            }

            #[pyo3(signature = (old_text, new_text, replace_all=true))]
            fn replace_text(
                self_: PyRef<'_, Self>,
                old_text: &str,
                new_text: &str,
                replace_all: bool,
            ) -> PyResult<()> {
                text_edit(&self_, |layer| {
                    layer.replace_text_with_options(old_text, new_text, replace_all)
                })
            }

            fn set_text_equal_length(self_: PyRef<'_, Self>, value: &str) -> PyResult<()> {
                text_edit(&self_, |layer| layer.set_text_equal_length(value))
            }

            #[pyo3(signature = (old_text, new_text, replace_all=true))]
            fn replace_text_equal_length(
                self_: PyRef<'_, Self>,
                old_text: &str,
                new_text: &str,
                replace_all: bool,
            ) -> PyResult<()> {
                text_edit(&self_, |layer| {
                    layer.replace_text_equal_length_with_options(old_text, new_text, replace_all)
                })
            }

            // --- runs and sheets ----------------------------------------

            #[getter]
            fn style_run_count(self_: PyRef<'_, Self>) -> PyResult<usize> {
                text_read(&self_, |layer| layer.style_run_count())
            }

            #[getter]
            fn paragraph_run_count(self_: PyRef<'_, Self>) -> PyResult<usize> {
                text_read(&self_, |layer| layer.paragraph_run_count())
            }

            #[getter]
            fn style_sheet_count(self_: PyRef<'_, Self>) -> PyResult<usize> {
                text_read(&self_, |layer| layer.style_sheet_count())
            }

            #[getter]
            fn paragraph_sheet_count(self_: PyRef<'_, Self>) -> PyResult<usize> {
                text_read(&self_, |layer| layer.paragraph_sheet_count())
            }

            /// Style run lengths in UTF-16 code units (including the final CR).
            fn style_run_lengths(self_: PyRef<'_, Self>) -> PyResult<Option<Vec<i32>>> {
                text_read(&self_, |layer| layer.style_run_lengths())
            }

            fn paragraph_run_lengths(self_: PyRef<'_, Self>) -> PyResult<Option<Vec<i32>>> {
                text_read(&self_, |layer| layer.paragraph_run_lengths())
            }

            /// Split style run `run_index` at a UTF-16 offset within it.
            fn split_style_run(
                self_: PyRef<'_, Self>,
                run_index: i64,
                char_offset: i64,
            ) -> PyResult<()> {
                let (run, offset) = (index_value(run_index)?, index_value(char_offset)?);
                text_edit(&self_, |layer| layer.split_style_run(run, offset))
            }

            fn split_paragraph_run(
                self_: PyRef<'_, Self>,
                run_index: i64,
                char_offset: i64,
            ) -> PyResult<()> {
                let (run, offset) = (index_value(run_index)?, index_value(char_offset)?);
                text_edit(&self_, |layer| layer.split_paragraph_run(run, offset))
            }

            fn style_normal_sheet_index(self_: PyRef<'_, Self>) -> PyResult<Option<usize>> {
                text_read(&self_, |layer| layer.style_normal_sheet_index())
            }

            fn set_style_normal_sheet_index(self_: PyRef<'_, Self>, index: i64) -> PyResult<()> {
                let index = index_value(index)?;
                text_edit(&self_, |layer| layer.set_style_normal_sheet_index(index))
            }

            fn paragraph_normal_sheet_index(self_: PyRef<'_, Self>) -> PyResult<Option<usize>> {
                text_read(&self_, |layer| layer.paragraph_normal_sheet_index())
            }

            fn set_paragraph_normal_sheet_index(self_: PyRef<'_, Self>, index: i64) -> PyResult<()> {
                let index = index_value(index)?;
                text_edit(&self_, |layer| layer.set_paragraph_normal_sheet_index(index))
            }

            /// Character property `name` of a style run (`kind="run"`) or the
            /// normal sheet (`kind="normal"`).
            #[pyo3(signature = (kind, index, name))]
            fn _style(
                self_: PyRef<'_, Self>,
                py: Python<'_>,
                kind: &str,
                index: i64,
                name: &str,
            ) -> PyResult<Py<PyAny>> {
                let sheet = text_ops::Sheet::from_py(kind, index.max(0))?;
                if index < 0 {
                    return Ok(py.None());
                }
                self_
                    .as_super()
                    .handle
                    .with_layer(|layer| text_ops::get_character(py, layer, sheet, name))
            }

            #[pyo3(signature = (kind, index, name, value))]
            fn _set_style(
                self_: PyRef<'_, Self>,
                kind: &str,
                index: i64,
                name: &str,
                value: &Bound<'_, PyAny>,
            ) -> PyResult<()> {
                let sheet = text_ops::Sheet::from_py(kind, index)?;
                self_
                    .as_super()
                    .handle
                    .with_layer_mut(|layer| text_ops::set_character(layer, sheet, name, value))
            }

            #[pyo3(signature = (kind, index, name))]
            fn _paragraph(
                self_: PyRef<'_, Self>,
                py: Python<'_>,
                kind: &str,
                index: i64,
                name: &str,
            ) -> PyResult<Py<PyAny>> {
                let sheet = text_ops::Sheet::from_py(kind, index.max(0))?;
                if index < 0 {
                    return Ok(py.None());
                }
                self_
                    .as_super()
                    .handle
                    .with_layer(|layer| text_ops::get_paragraph(py, layer, sheet, name))
            }

            #[pyo3(signature = (kind, index, name, value))]
            fn _set_paragraph(
                self_: PyRef<'_, Self>,
                kind: &str,
                index: i64,
                name: &str,
                value: &Bound<'_, PyAny>,
            ) -> PyResult<()> {
                let sheet = text_ops::Sheet::from_py(kind, index)?;
                self_
                    .as_super()
                    .handle
                    .with_layer_mut(|layer| text_ops::set_paragraph(layer, sheet, name, value))
            }

            /// Set a character property on a range (`spec` from `_text.py`).
            fn _style_range(
                self_: PyRef<'_, Self>,
                spec: &Bound<'_, PyAny>,
                name: &str,
                value: &Bound<'_, PyAny>,
            ) -> PyResult<()> {
                let spec = text_ops::RangeSpec::from_py(spec)?;
                self_.as_super().handle.with_layer_mut(|layer| {
                    text_ops::set_character_range(layer, &spec, name, value)
                })
            }

            fn _paragraph_range(
                self_: PyRef<'_, Self>,
                spec: &Bound<'_, PyAny>,
                name: &str,
                value: &Bound<'_, PyAny>,
            ) -> PyResult<()> {
                let spec = text_ops::RangeSpec::from_py(spec)?;
                self_.as_super().handle.with_layer_mut(|layer| {
                    text_ops::set_paragraph_range(layer, &spec, name, value)
                })
            }

            #[pyo3(signature = (spec, paragraph=false))]
            fn _range_count(
                self_: PyRef<'_, Self>,
                spec: &Bound<'_, PyAny>,
                paragraph: bool,
            ) -> PyResult<usize> {
                let spec = text_ops::RangeSpec::from_py(spec)?;
                self_
                    .as_super()
                    .handle
                    .with_layer_mut(|layer| Ok(text_ops::range_count(layer, &spec, paragraph)))
            }

            // --- fonts --------------------------------------------------

            #[getter]
            fn font_count(self_: PyRef<'_, Self>) -> PyResult<usize> {
                text_read(&self_, |layer| layer.font_count())
            }

            fn font_postscript_name(self_: PyRef<'_, Self>, font_index: i64) -> PyResult<Option<String>> {
                font_field(&self_, font_index, |font| font.postscript_name)
            }

            fn font_name(self_: PyRef<'_, Self>, font_index: i64) -> PyResult<Option<String>> {
                font_field(&self_, font_index, |font| font.postscript_name)
            }

            fn font_script(
                self_: PyRef<'_, Self>,
                py: Python<'_>,
                font_index: i64,
            ) -> PyResult<Option<Py<PyAny>>> {
                font_field(&self_, font_index, |font| font.script.raw())?
                    .map(|raw| py_enum(py, "FontScript", i64::from(raw)))
                    .transpose()
            }

            fn font_type(
                self_: PyRef<'_, Self>,
                py: Python<'_>,
                font_index: i64,
            ) -> PyResult<Option<Py<PyAny>>> {
                font_field(&self_, font_index, |font| font.font_type.raw())?
                    .map(|raw| py_enum(py, "FontType", i64::from(raw)))
                    .transpose()
            }

            fn font_synthetic(self_: PyRef<'_, Self>, font_index: i64) -> PyResult<Option<i32>> {
                font_field(&self_, font_index, |font| font.synthetic)
            }

            fn is_sentinel_font(self_: PyRef<'_, Self>, font_index: i64) -> PyResult<bool> {
                Ok(font_field(&self_, font_index, |font| font.is_sentinel())?.unwrap_or(false))
            }

            fn used_font_indices(self_: PyRef<'_, Self>) -> PyResult<Vec<usize>> {
                text_read(&self_, |layer| layer.used_font_indices())
            }

            fn used_font_names(self_: PyRef<'_, Self>) -> PyResult<Vec<String>> {
                text_read(&self_, |layer| layer.used_font_names())
            }

            /// Append a font to the FontSet; returns its index.
            #[pyo3(signature = (postscript_name, font_type=None, script=None, synthetic=0))]
            fn add_font(
                self_: PyRef<'_, Self>,
                postscript_name: &str,
                font_type: Option<&Bound<'_, PyAny>>,
                script: Option<&Bound<'_, PyAny>>,
                synthetic: i32,
            ) -> PyResult<usize> {
                let font_type = font_type.map(enum_value).transpose()?.unwrap_or(0);
                let script = script.map(enum_value).transpose()?.unwrap_or(0);
                text_edit(&self_, |layer| {
                    layer.add_font(
                        postscript_name,
                        FontType::from_raw(font_type as i32),
                        FontScript::from_raw(script as i32),
                        synthetic,
                    )
                })
            }

            fn set_font_postscript_name(
                self_: PyRef<'_, Self>,
                font_index: i64,
                new_name: &str,
            ) -> PyResult<()> {
                let index = index_value(font_index)?;
                text_edit(&self_, |layer| layer.rename_font(index, new_name))
            }

            /// Index of a FontSet entry by PostScript name, `-1` if absent.
            fn find_font_index(self_: PyRef<'_, Self>, postscript_name: &str) -> PyResult<i64> {
                text_read(&self_, |layer| {
                    layer
                        .font_index(postscript_name)
                        .map_or(-1, |index| index as i64)
                })
            }

            fn set_style_run_font_by_name(
                self_: PyRef<'_, Self>,
                run_index: i64,
                postscript_name: &str,
            ) -> PyResult<()> {
                let run = index_value(run_index)?;
                text_edit(&self_, |layer| {
                    layer.style_run_mut(run).set_font(postscript_name).map(|_| ())
                })
            }

            fn set_style_normal_font_by_name(
                self_: PyRef<'_, Self>,
                postscript_name: &str,
            ) -> PyResult<()> {
                text_edit(&self_, |layer| {
                    layer.style_normal_mut().set_font(postscript_name).map(|_| ())
                })
            }

            /// PostScript name of the first style run's font.
            #[getter]
            fn primary_font_name(self_: PyRef<'_, Self>) -> PyResult<Option<String>> {
                text_read(&self_, |layer| layer.primary_font_name())
            }

            /// Use this font for the whole text (adding it when needed).
            fn set_font(self_: PyRef<'_, Self>, postscript_name: &str) -> PyResult<()> {
                text_edit(&self_, |layer| layer.set_font(postscript_name))
            }

            // --- orientation and shape ----------------------------------

            fn orientation(self_: PyRef<'_, Self>, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
                text_read(&self_, |layer| layer.orientation().map(TextWritingDirection::raw))?
                    .map(|raw| py_enum(py, "WritingDirection", i64::from(raw)))
                    .transpose()
            }

            #[getter]
            fn is_vertical(self_: PyRef<'_, Self>) -> PyResult<bool> {
                text_read(&self_, |layer| layer.is_vertical())
            }

            fn set_orientation(self_: PyRef<'_, Self>, writing_direction: &Bound<'_, PyAny>) -> PyResult<()> {
                let raw = enum_value(writing_direction)? as i32;
                text_edit(&self_, |layer| {
                    layer.set_orientation(TextWritingDirection::from_raw(raw))
                })
            }

            fn shape_type(self_: PyRef<'_, Self>, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
                text_read(&self_, |layer| layer.text_shape().map(|shape| shape.raw()))?
                    .map(|raw| py_enum(py, "ShapeType", i64::from(raw)))
                    .transpose()
            }

            #[getter]
            fn is_box_text(self_: PyRef<'_, Self>) -> PyResult<bool> {
                text_read(&self_, |layer| layer.is_box_text())
            }

            #[getter]
            fn is_point_text(self_: PyRef<'_, Self>) -> PyResult<bool> {
                text_read(&self_, |layer| layer.is_point_text())
            }

            /// `[top, left, bottom, right]` of box text.
            fn box_bounds(self_: PyRef<'_, Self>) -> PyResult<Option<[f64; 4]>> {
                text_read(&self_, |layer| {
                    layer
                        .box_bounds()
                        .map(|bounds| [bounds.top, bounds.left, bounds.bottom, bounds.right])
                })
            }

            fn box_width(self_: PyRef<'_, Self>) -> PyResult<Option<f64>> {
                text_read(&self_, |layer| layer.box_width())
            }

            fn box_height(self_: PyRef<'_, Self>) -> PyResult<Option<f64>> {
                text_read(&self_, |layer| layer.box_height())
            }

            fn set_box_bounds(
                self_: PyRef<'_, Self>,
                top: f64,
                left: f64,
                bottom: f64,
                right: f64,
            ) -> PyResult<()> {
                text_edit(&self_, |layer| {
                    layer.set_box_bounds(TextBoxBounds {
                        top,
                        left,
                        bottom,
                        right,
                    })
                })
            }

            fn set_box_size(self_: PyRef<'_, Self>, width: f64, height: f64) -> PyResult<()> {
                text_edit(&self_, |layer| layer.set_box_size(width, height))
            }

            fn set_box_width(self_: PyRef<'_, Self>, width: f64) -> PyResult<()> {
                text_edit(&self_, |layer| layer.set_box_width(width))
            }

            fn set_box_height(self_: PyRef<'_, Self>, height: f64) -> PyResult<()> {
                text_edit(&self_, |layer| layer.set_box_height(height))
            }

            fn convert_to_box_text(self_: PyRef<'_, Self>, width: f64, height: f64) -> PyResult<()> {
                text_edit(&self_, |layer| layer.convert_to_box_text(width, height))
            }

            fn convert_to_point_text(self_: PyRef<'_, Self>) -> PyResult<()> {
                text_edit(&self_, |layer| layer.convert_to_point_text())
            }

            // --- transform ----------------------------------------------

            /// The TySh transform `[xx, xy, yx, yy, tx, ty]` (empty without one).
            fn transform(self_: PyRef<'_, Self>) -> PyResult<Vec<f64>> {
                text_read(&self_, |layer| {
                    layer
                        .text_transform()
                        .map_or_else(Vec::new, |matrix| matrix.to_vec())
                })
            }

            fn transform_component(self_: PyRef<'_, Self>, index: i64) -> PyResult<Option<f64>> {
                let index = index_value(index)?;
                text_read(&self_, |layer| layer.text_transform_component(index))
            }

            #[getter]
            fn transform_xx(self_: PyRef<'_, Self>) -> PyResult<Option<f64>> {
                text_read(&self_, |layer| layer.text_transform_component(0))
            }

            #[getter]
            fn transform_xy(self_: PyRef<'_, Self>) -> PyResult<Option<f64>> {
                text_read(&self_, |layer| layer.text_transform_component(1))
            }

            #[getter]
            fn transform_yx(self_: PyRef<'_, Self>) -> PyResult<Option<f64>> {
                text_read(&self_, |layer| layer.text_transform_component(2))
            }

            #[getter]
            fn transform_yy(self_: PyRef<'_, Self>) -> PyResult<Option<f64>> {
                text_read(&self_, |layer| layer.text_transform_component(3))
            }

            #[getter]
            fn transform_tx(self_: PyRef<'_, Self>) -> PyResult<Option<f64>> {
                text_read(&self_, |layer| layer.text_transform_component(4))
            }

            #[getter]
            fn transform_ty(self_: PyRef<'_, Self>) -> PyResult<Option<f64>> {
                text_read(&self_, |layer| layer.text_transform_component(5))
            }

            fn set_transform(self_: PyRef<'_, Self>, values: Vec<f64>) -> PyResult<()> {
                let transform: [f64; 6] = values.try_into().map_err(|_| {
                    PyValueError::new_err("the transform needs exactly six values")
                })?;
                text_edit(&self_, |layer| layer.set_text_transform(transform))
            }

            fn set_transform_component(self_: PyRef<'_, Self>, index: i64, value: f64) -> PyResult<()> {
                let index = index_value(index)?;
                text_edit(&self_, |layer| layer.set_text_transform_component(index, value))
            }

            fn set_transform_xx(self_: PyRef<'_, Self>, value: f64) -> PyResult<()> {
                text_edit(&self_, |layer| layer.set_text_transform_component(0, value))
            }

            fn set_transform_xy(self_: PyRef<'_, Self>, value: f64) -> PyResult<()> {
                text_edit(&self_, |layer| layer.set_text_transform_component(1, value))
            }

            fn set_transform_yx(self_: PyRef<'_, Self>, value: f64) -> PyResult<()> {
                text_edit(&self_, |layer| layer.set_text_transform_component(2, value))
            }

            fn set_transform_yy(self_: PyRef<'_, Self>, value: f64) -> PyResult<()> {
                text_edit(&self_, |layer| layer.set_text_transform_component(3, value))
            }

            fn set_transform_tx(self_: PyRef<'_, Self>, value: f64) -> PyResult<()> {
                text_edit(&self_, |layer| layer.set_text_transform_component(4, value))
            }

            fn set_transform_ty(self_: PyRef<'_, Self>, value: f64) -> PyResult<()> {
                text_edit(&self_, |layer| layer.set_text_transform_component(5, value))
            }

            #[getter]
            fn rotation_angle(self_: PyRef<'_, Self>) -> PyResult<Option<f64>> {
                text_read(&self_, |layer| layer.text_rotation_angle())
            }

            #[getter]
            fn scale_x(self_: PyRef<'_, Self>) -> PyResult<Option<f64>> {
                text_read(&self_, |layer| layer.text_scale_x())
            }

            #[getter]
            fn scale_y(self_: PyRef<'_, Self>) -> PyResult<Option<f64>> {
                text_read(&self_, |layer| layer.text_scale_y())
            }

            fn set_rotation_angle(self_: PyRef<'_, Self>, angle_degrees: f64) -> PyResult<()> {
                text_edit(&self_, |layer| layer.set_text_rotation_angle(angle_degrees))
            }

            fn set_scale_x(self_: PyRef<'_, Self>, sx: f64) -> PyResult<()> {
                text_edit(&self_, |layer| layer.set_text_scale_x(sx))
            }

            fn set_scale_y(self_: PyRef<'_, Self>, sy: f64) -> PyResult<()> {
                text_edit(&self_, |layer| layer.set_text_scale_y(sy))
            }

            #[pyo3(signature = (sx, sy=None))]
            fn set_scale(self_: PyRef<'_, Self>, sx: f64, sy: Option<f64>) -> PyResult<()> {
                text_edit(&self_, |layer| layer.set_text_scale(sx, sy.unwrap_or(sx)))
            }

            /// The text anchor `(tx, ty)`.
            fn position(self_: PyRef<'_, Self>) -> PyResult<Option<(f64, f64)>> {
                text_read(&self_, |layer| layer.text_position())
            }

            fn set_position(self_: PyRef<'_, Self>, x: f64, y: f64) -> PyResult<()> {
                text_edit(&self_, |layer| layer.set_text_position(x, y))
            }

            fn reset_transform(self_: PyRef<'_, Self>) -> PyResult<()> {
                text_edit(&self_, |layer| layer.reset_text_transform())
            }

            // --- warp and anti-aliasing ---------------------------------

            #[getter]
            fn has_warp(self_: PyRef<'_, Self>) -> PyResult<bool> {
                text_read(&self_, |layer| layer.has_text_warp())
            }

            #[getter]
            fn warp_style(self_: PyRef<'_, Self>, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
                text_read(&self_, |layer| {
                    layer.text_warp_style().as_ref().and_then(text_ops::warp_style_number)
                })?
                .map(|raw| py_enum(py, "WarpStyle", i64::from(raw)))
                .transpose()
            }

            fn set_warp_style(self_: PyRef<'_, Self>, style: &Bound<'_, PyAny>) -> PyResult<()> {
                let style = text_ops::warp_style_from_number(enum_value(style)?)?;
                text_edit(&self_, |layer| layer.set_text_warp_style(&style))
            }

            #[getter]
            fn warp_value(self_: PyRef<'_, Self>) -> PyResult<Option<f64>> {
                text_read(&self_, |layer| layer.text_warp_value())
            }

            fn set_warp_value(self_: PyRef<'_, Self>, value: f64) -> PyResult<()> {
                text_edit(&self_, |layer| layer.set_text_warp_value(value))
            }

            #[getter]
            fn warp_horizontal_distortion(self_: PyRef<'_, Self>) -> PyResult<Option<f64>> {
                text_read(&self_, |layer| layer.text_warp_horizontal_distortion())
            }

            fn set_warp_horizontal_distortion(self_: PyRef<'_, Self>, value: f64) -> PyResult<()> {
                text_edit(&self_, |layer| layer.set_text_warp_horizontal_distortion(value))
            }

            #[getter]
            fn warp_vertical_distortion(self_: PyRef<'_, Self>) -> PyResult<Option<f64>> {
                text_read(&self_, |layer| layer.text_warp_vertical_distortion())
            }

            fn set_warp_vertical_distortion(self_: PyRef<'_, Self>, value: f64) -> PyResult<()> {
                text_edit(&self_, |layer| layer.set_text_warp_vertical_distortion(value))
            }

            #[getter]
            fn warp_rotation(self_: PyRef<'_, Self>, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
                text_read(&self_, |layer| match layer.text_warp_rotation() {
                    Some(TextWarpRotation::Horizontal) => Some(0),
                    Some(TextWarpRotation::Vertical) => Some(1),
                    _ => None,
                })?
                .map(|raw| py_enum(py, "WarpRotation", raw))
                .transpose()
            }

            fn set_warp_rotation(self_: PyRef<'_, Self>, rotation: &Bound<'_, PyAny>) -> PyResult<()> {
                let rotation = match enum_value(rotation)? {
                    0 => TextWarpRotation::Horizontal,
                    1 => TextWarpRotation::Vertical,
                    _ => return Err(PyValueError::new_err("unknown text warp rotation")),
                };
                text_edit(&self_, |layer| layer.set_text_warp_rotation(&rotation))
            }

            fn anti_alias(self_: PyRef<'_, Self>, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
                text_read(&self_, |layer| match layer.anti_alias() {
                    Some(AntiAliasMethod::NoAntiAlias) => Some(0),
                    Some(AntiAliasMethod::Crisp) => Some(1),
                    Some(AntiAliasMethod::Strong) => Some(2),
                    Some(AntiAliasMethod::Smooth) => Some(3),
                    Some(AntiAliasMethod::Sharp) => Some(4),
                    _ => None,
                })?
                .map(|raw| py_enum(py, "AntiAliasMethod", raw))
                .transpose()
            }

            fn set_anti_alias(self_: PyRef<'_, Self>, method: &Bound<'_, PyAny>) -> PyResult<()> {
                let method = match enum_value(method)? {
                    0 => AntiAliasMethod::NoAntiAlias,
                    1 => AntiAliasMethod::Crisp,
                    2 => AntiAliasMethod::Strong,
                    3 => AntiAliasMethod::Smooth,
                    4 => AntiAliasMethod::Sharp,
                    _ => return Err(PyValueError::new_err("unknown text anti-alias method")),
                };
                text_edit(&self_, |layer| layer.set_anti_alias(&method))
            }
        }

        fn font_field<R>(
            layer: &PyRef<'_, PyTextLayer>,
            font_index: i64,
            field: impl FnOnce(psd::TextFont) -> R,
        ) -> PyResult<Option<R>> {
            if font_index < 0 {
                return Ok(None);
            }
            text_read(layer, |layer| layer.font(font_index as usize).map(field))
        }
    };
}
