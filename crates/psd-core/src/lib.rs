//! Raw PSD/PSB on-disk format layer.
//!
//! This crate mirrors the section structs of the upstream C++ `PhotoshopFile/`
//! layer plus the shared structures from `Core/Struct/` and `Util/Enum.h`.
//! Everything on disk is big-endian and length-prefixed; section and
//! tagged-block layouts follow the Adobe PSD/PSB specification.
//!
//! Module map (upstream reference in parentheses):
//! - `enums` (types re-exported at the crate root): version / bit depth / color mode / compression + string-keyed maps (`Util/Enum.h`)
//! - `error`: the workspace-wide [`PsdError`] (reshapes `Util/Logger.h`'s throw-on-error)
//! - [`io`]: big-endian primitive readers/writers + PSD/PSB length-marker helpers (`Core/FileIO/`)
//! - [`strings`]: Pascal/Unicode strings with Windows-1252/Mac-Roman tables (`Core/Struct/PascalString.h`, `UnicodeString.h`)
//! - [`types`]: small shared numeric structs (`Core/Struct/PhotoshopTypes.h`)
//! - [`header`]: the 26-byte file header (`PhotoshopFile/FileHeader.h`)
//! - `color_mode_data`, [`image_resources`], [`layer_and_mask_info`], [`image_data`]: top-level sections
//! - [`photoshop_file`]: the five sections read/written in order (`PhotoshopFile/PhotoshopFile.h`)
//! - [`tagged_blocks`]: 4-char-keyed additional-layer-info registry with raw passthrough (`Core/TaggedBlocks/`)
//! - [`descriptor`], [`engine_data`]: Photoshop's action-descriptor and text-engine formats (`Core/Struct/`)
//! - [`color`], [`gradient`], [`style_values`]: typed values (colours, gradients, contours, points, pattern references) that read from, patch and build descriptors, shared by effects and fills
//! - [`text_tool`], [`placed_layer`], [`linked_layer`], [`layer_effects`], [`adjustments`], [`vector`], [`artboard`]: typed views over text, placed/linked data, effect blocks, adjustment/fill settings, vector paths/shapes, and artboards
//! - `serialize` (feature `serde`): `Serialize` views of descriptors and EngineData (upstream `to_json`)
//!
//! Codec math lives in `psd-codecs`; the document API lives in `psd`. This crate
//! does no IO of its own — readers consume `&[u8]`, writers produce `Vec<u8>`.

pub mod adjustments;
pub mod artboard;
pub mod color;
mod color_mode_data;
pub mod descriptor;
mod descriptor_build;
pub mod engine_data;
mod enums;
mod error;
pub mod gradient;
pub mod header;
pub mod image_data;
pub mod image_resources;
pub mod io;
pub mod layer_and_mask_info;
pub mod layer_effects;
pub mod linked_layer;
pub mod photoshop_file;
pub mod placed_layer;
#[cfg(feature = "serde")]
mod serialize;
pub mod strings;
pub mod style_values;
pub mod tagged_blocks;
pub mod text_tool;
pub mod types;
pub mod vector;
mod views;

pub use adjustments::{AdjustmentBlock, AdjustmentData, AdjustmentKind, AdjustmentPreset};
pub use artboard::{Artboard, ArtboardBackground, ArtboardRect, ArtboardSettings};
pub use color::Color;
pub use color_mode_data::ColorModeData;
pub use descriptor::{
    read_item, write_item, Descriptor, DescriptorIntegerSpan, DescriptorItem, DescriptorKey,
    DescriptorPayloadSpans, DescriptorValue, ObjectArray,
};
pub use descriptor_build::{UNIT_ANGLE, UNIT_PERCENT, UNIT_PIXELS};
pub use engine_data::{EngineNumber, EngineValue, EngineValueKind, PayloadPatch};
pub use enums::{
    BitDepth, BlendMode, ChannelId, ColorMode, Compression, DisplayUnit, LayerColor,
    ResolutionUnit, SectionDivider, Version,
};
pub use error::{PsdError, Result};
pub use gradient::{
    ColorStop, Gradient, GradientKind, NoiseColorModel, NoiseGradient, SolidGradient, StopSource,
    TransparencyStop,
};
pub use header::FileHeader;
pub use image_data::ImageData;
pub use image_resources::{
    IccProfileBlock, ImageResources, RawResourceBlock, ResolutionInfoBlock, ResourceBlock,
    ID_ICC_PROFILE, ID_RESOLUTION_INFO,
};
pub use io::{BeReader, BeWriter};
pub use layer_and_mask_info::{
    BlendingRange, ChannelData, ChannelImageData, ChannelInfo, GlobalLayerMaskInfo,
    LayerAndMaskInformation, LayerBlendingRanges, LayerFlags, LayerInfo, LayerMask, LayerMaskData,
    LayerMaskFlags, LayerRecord, MaskParams, MAX_LAYER_CHANNELS,
};
pub use layer_effects::{
    EffectDescriptor, EffectKind, LayerEffectsBlock, LayerEffectsData, LegacyEffectColor,
    LegacyEffectRecord, LegacyLayerEffects, ModernLayerEffects,
};
pub use linked_layer::{
    Date, LinkedDataKind, LinkedLayer, LinkedLayerTaggedBlock, LinkedLayerView,
};
pub use photoshop_file::PhotoshopFile;
pub use placed_layer::{
    PlacedLayer, PlacedLayerData, PlacedLayerType, Point, Transform, PLACED_LAYER_DATA_SIGNATURE,
    PLACED_LAYER_SIGNATURE,
};
pub use strings::{PascalEncoding, PascalString, UnicodeString};
pub use style_values::{Contour, ContourPoint, Offset, OffsetUnits, PatternRef};
pub use tagged_blocks::{AdditionalLayerInfo, TaggedBlock, TaggedBlockKey};
pub use text_tool::{
    parse_text_engine_data, TypeToolDescriptorSpans, TypeToolTaggedBlock, TypeToolTextPayloadSpans,
};
pub use types::{FixedFloat4, RawColor};
pub use vector::{DocumentPath, VectorBlock, VectorData, VectorMask, VectorPath};
