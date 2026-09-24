//! The per-depth class families (`*_8bit`, `*_16bit`, `*_32bit`).
//!
//! Each module instantiates the class macros from `classes/` for one sample
//! type; everything else is shared generic code.

// The class macros expand in the depth modules below and use these names.
#[allow(unused_imports)]
use std::sync::Arc;

use numpy::{IntoPyArray, PyArray1, PyArray2, PyReadonlyArray2};
use psd::core::{BitDepth as CoreBitDepth, ColorMode, FileHeader, Version};
use psd::{
    color_channel_count, AntiAliasMethod, BitDepth, ChannelKey, FontScript, FontType, Layer,
    LayerId, LayeredFile, LinkedStorage, Rect, TextBoxBounds, TextLayerBuilder, TextWarpRotation,
    TextWritingDirection,
};
use pyo3::exceptions::{
    PyFileExistsError, PyIndexError, PyKeyError, PyRuntimeError, PyTypeError, PyValueError,
};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};

use crate::convert::{
    bit_depth_to_py, color_mode_from_py, compression_from_py, enum_value, icc_bytes, index_value,
    linkage_from_py, linkage_number, py_enum, read_options_from_py, Io,
};
use crate::layer_ops::{self, check_name};
use crate::smart_warp::PySmartWarp;
use crate::state::{self, psd_error, read_document, write_document, Document, LayerHandle};
use crate::{text_ops, tree_ops};

macro_rules! depth_module {
    (
        $module:ident, $sample:ty, $document:literal, $layer:literal, $image:literal,
        $group:literal, $text:literal, $smart:literal
    ) => {
        pub mod $module {
            use super::*;

            pub(crate) type Sample = $sample;

            layer_class!($layer);
            image_class!($image);
            group_class!($group);
            text_class!($text);
            smart_class!($smart);
            document_class!($document);
        }
    };
}

depth_module!(
    depth8,
    u8,
    "LayeredFile_8bit",
    "Layer_8bit",
    "ImageLayer_8bit",
    "GroupLayer_8bit",
    "TextLayer_8bit",
    "SmartObjectLayer_8bit"
);
depth_module!(
    depth16,
    u16,
    "LayeredFile_16bit",
    "Layer_16bit",
    "ImageLayer_16bit",
    "GroupLayer_16bit",
    "TextLayer_16bit",
    "SmartObjectLayer_16bit"
);
depth_module!(
    depth32,
    f32,
    "LayeredFile_32bit",
    "Layer_32bit",
    "ImageLayer_32bit",
    "GroupLayer_32bit",
    "TextLayer_32bit",
    "SmartObjectLayer_32bit"
);
