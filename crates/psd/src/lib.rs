//! User-facing Photoshop document API.
//!
//! The idiomatic-Rust reshape of upstream `LayeredFile/`: a
//! `LayeredFile<T: BitDepth>` owning an arena of layers (`Vec<Layer<T>>` plus
//! child-index lists), addressed by `LayerId` or Photoshop-style paths
//! (`"Group/Image"`). Enum dispatch replaces `dynamic_pointer_cast` soup;
//! composition replaces CRTP mixins.
//!
//! Module map (upstream reference in parentheses):
//! - [`layered_file`]: `LayeredFile<T>` read/write orchestration (`LayeredFile.h`, `Impl/LayeredFileImpl.h`)
//! - [`smart_object`]: linked-source lookup, decoding, and transactional replacement (`SmartObjectLayer.h`, `LinkedLayerData.h`)
//! - [`layer`]: `Layer { common, kind }` + `LayerKind` enum — Image/Text/Adjustment/Shape/Group/SectionDivider (`LayerTypes/`)
//! - [`tree`]: removing, moving, and inserting layers; detached [`LayerTree`]s (`LayeredFile::move_layer`/`remove_layer`)
//! - [`channels`]: planar channel store keyed by `ChannelKey` (alpha = -1, user mask = -2) (`ImageDataMixins.h`, `Core/Struct/ImageChannel.h`)
//! - `bitdepth`: the [`BitDepth`] sample-type trait (`bpp8_t`/`bpp16_t`/`bpp32_t`)
//! - [`geometry`], [`warp`], [`render`]: typed smart-object geometry, descriptor views and raster resampling (`Core/Warp/`, `Core/Render/`, `Core/Geometry/`)
//! - [`text`]: TySh-backed text access/editing and EngineData metadata (`LayerTypes/TextLayer/`)
//!
//! Reading is mmap-backed via `memmap2`; writing serializes sections through
//! `psd-core` writers and channel payloads through `psd-codecs`. The merged
//! composite `ImageData` is never rendered: saves write a zeroed RLE section
//! (upstream parity, 20–50% smaller files).

mod bitdepth;
pub mod channels;
pub mod geometry;
pub mod layer;
pub mod layered_file;
pub mod progress;
pub mod render;
pub mod smart_object;
pub mod text;
pub mod tree;
pub mod warp;

// Re-export the raw-format layer so callers can reach placed-layer and
// linked-data types (`psd::core::PlacedLayer`, …) through the document API.
pub use psd_core as core;

pub use bitdepth::BitDepth;
pub use channels::{ChannelKey, ChannelStore};
pub use geometry::{BezierSurface, BoundingBox, Homography, MeshVertex, Point2, QuadMesh};
pub use layer::{
    AdjustmentLayer, GroupLayer, ImageLayer, Layer, LayerId, LayerKind, Rect, ShapeLayer, TextLayer,
};
pub use layered_file::{color_channel_count, LayeredFile, ReadOptions, DEFAULT_TOTAL_MEMORY_LIMIT};
pub use progress::ProgressEvent;
pub use render::{composite_rgb, render_warped, Interpolation, Raster, WarpRenderOptions};
pub use smart_object::LinkedStorage;
pub use text::{
    AntiAliasMethod, BaselineDirection, CharacterDirection, CharacterStyle, CharacterStyleMut,
    CharacterStyleRange, DiacriticPosition, FontBaseline, FontCaps, FontScript, FontType,
    Justification, KinsokuOrder, LeadingType, Occurrence, ParagraphStyle, ParagraphStyleMut,
    ParagraphStyleRange, TextBoxBounds, TextFont, TextLayerBuilder, TextShape, TextWarpRotation,
    TextWarpStyle, TextWritingDirection,
};
pub use tree::LayerTree;
pub use warp::{Warp, WarpKind};
