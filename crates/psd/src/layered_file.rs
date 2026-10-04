//! `LayeredFile<T>`: the user-facing document.
//!
//! Reshapes upstream `LayeredFile.h` + `Impl/LayeredFileImpl.h` +
//! `Util/GenerateLayerMaskInfo.cpp`: an arena of layers,
//! index-addressed, with read/write driven by `psd-core` sections and
//! `psd-codecs` for channel payloads.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use psd_core::{
    AdditionalLayerInfo, AdjustmentBlock, AdjustmentKind, Artboard, ArtboardSettings, BeReader,
    BeWriter, BitDepth as CoreBitDepth, BlendMode, ChannelData, ChannelId as CoreChannelId,
    ChannelImageData, ChannelInfo, ColorMode, ColorModeData, Compression, FileHeader,
    GlobalLayerMaskInfo, IccProfileBlock, ImageData, ImageResources, LayerAndMaskInformation,
    LayerInfo, LayerRecord, PascalString, PhotoshopFile, PsdError, ResolutionInfoBlock, Result,
    SectionDivider, TaggedBlock, TaggedBlockKey, UnicodeString, VectorBlock, Version,
};

use crate::bitdepth::BitDepth;
use crate::channels::{
    compress_channel, decompress_channel, ChannelKey, ChannelStore, RawChannelData,
};
use crate::composite::merged::MergedImageData;
use crate::geometry::Point2;
use crate::layer::upsert_block;
use crate::layer::{
    AdjustmentLayer, GroupLayer, ImageLayer, Layer, LayerId, LayerKind, Rect, ShapeLayer, TextLayer,
};
use crate::progress::{ignore_progress, ProgressEvent};
use crate::text::TextCacheBaseline;

/// The name Photoshop gives the bounding section divider of a group.
const DIVIDER_NAME: &str = "</Layer group>";

/// The resolution a document has when nothing says otherwise: the format's own
/// default, and what a new document is created with.
const DEFAULT_DPI: f32 = 72.0;

/// A temporary path beside `path`, so replacing it is a rename within one
/// filesystem. A process-local sequence keeps concurrent saves from sharing
/// a temporary file, even when both writers target the same document path.
fn temporary_sibling(path: &Path) -> Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .ok_or(PsdError::InvalidData {
            offset: 0,
            message: "a document path needs a file name",
        })?
        .to_string_lossy();
    Ok(path.with_file_name(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )))
}

fn absolute_or_current_dir(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// Number of color channels Photoshop stores for a color mode (merged image).
pub fn color_channel_count(color_mode: ColorMode) -> u16 {
    match color_mode {
        ColorMode::Bitmap | ColorMode::Grayscale | ColorMode::Indexed | ColorMode::Duotone => 1,
        ColorMode::Rgb | ColorMode::Lab => 3,
        ColorMode::Cmyk => 4,
        ColorMode::Multichannel => 0,
    }
}

/// Default cumulative budget for decoded layer-channel bitmaps: 2 GiB.
pub const DEFAULT_TOTAL_MEMORY_LIMIT: usize = 2 * 1024 * 1024 * 1024;

/// Options for reading a PSD or PSB document.
///
/// The cumulative bitmap budget follows a caller-controlled design used by
/// independent PSD parsers, adapted to this crate's planar typed channel
/// storage. The default is 2 GiB. Set
/// [`total_memory_limit`](Self::total_memory_limit) to `None` to disable the
/// limit explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadOptions {
    /// Maximum cumulative bytes of decoded layer-channel samples retained by
    /// the parsed document. This does not limit input bytes, parsed metadata,
    /// or temporary codec workspace.
    pub total_memory_limit: Option<usize>,
    /// Retain compressed layer and mask channels until explicitly decoded.
    /// Untouched raw channels are written from their stored payloads without
    /// decoding. The default is `false` to
    /// preserve eager pixel access.
    pub use_raw_data: bool,
}

impl ReadOptions {
    /// Read without a cumulative decoded-channel memory limit.
    pub const fn unlimited() -> Self {
        Self {
            total_memory_limit: None,
            use_raw_data: false,
        }
    }

    /// Select whether layer and mask channels remain compressed after reading.
    pub const fn with_raw_data(mut self, use_raw_data: bool) -> Self {
        self.use_raw_data = use_raw_data;
        self
    }
}

impl Default for ReadOptions {
    fn default() -> Self {
        Self {
            total_memory_limit: Some(DEFAULT_TOTAL_MEMORY_LIMIT),
            use_raw_data: false,
        }
    }
}

fn depth_enum<T: BitDepth>() -> CoreBitDepth {
    match T::DEPTH {
        8 => CoreBitDepth::Eight,
        16 => CoreBitDepth::Sixteen,
        _ => CoreBitDepth::ThirtyTwo,
    }
}

/// Photoshop writes alpha first, then color channels, then mask channels.
fn channel_sort_key(key: ChannelKey) -> (u8, i16) {
    match key.index() {
        -1 => (0, -1),
        index if index >= 0 => (1, index),
        index => (2, index),
    }
}

/// The canvas bounds of mask channel `key`: `-2` follows the first mask
/// record and `-3` the pixel ("real user") mask, per
/// [`LayerMaskData::record_for_channel`](psd_core::LayerMaskData::record_for_channel).
/// Upstream sizes every mask channel from the pixel mask, which misreads
/// `-2` when a vector mask with different bounds exists.
fn mask_channel_rect(mask: Option<&psd_core::LayerMaskData>, key: ChannelKey) -> Option<Rect> {
    mask.and_then(|data| data.record_for_channel(key.index()))
        .map(|mask| Rect::new(mask.top, mask.left, mask.bottom, mask.right))
}

/// Validate layer and mask bounds before deriving bitmap sizes,
/// using the PSD/PSB dimension limits and doing subtraction in i64.
fn read_rect_extents(rect: Rect, kind: &'static str, version: Version) -> Result<(usize, usize)> {
    let width_i64 = i64::from(rect.right) - i64::from(rect.left);
    let height_i64 = i64::from(rect.bottom) - i64::from(rect.top);
    let maximum = match version {
        Version::Psd => 30_000,
        Version::Psb => 300_000,
    };
    if width_i64 < 0 || height_i64 < 0 || width_i64 > maximum || height_i64 > maximum {
        return Err(PsdError::InvalidImageBounds {
            kind,
            width: width_i64,
            height: height_i64,
        });
    }
    let width = usize::try_from(width_i64).map_err(|_| PsdError::InvalidImageBounds {
        kind,
        width: width_i64,
        height: height_i64,
    })?;
    let height = usize::try_from(height_i64).map_err(|_| PsdError::InvalidImageBounds {
        kind,
        width: width_i64,
        height: height_i64,
    })?;
    Ok((width, height))
}

fn mask_channel_extents(
    rect: Rect,
    kind: &'static str,
    version: Version,
    empty_payload: bool,
) -> Result<(usize, usize)> {
    match read_rect_extents(rect, kind, version) {
        Ok(extents) => Ok(extents),
        // PhotoshopAPI/src/PhotoshopFile/LayerAndMaskInformation.cpp passes
        // mask extents to its decoder without checking them, while other
        // parsers reject inverted extents. The corpus has an empty vector-mask
        // channel with a 0x-1 placeholder; retain it as zero-area because it
        // has no bytes and cannot allocate pixels.
        Err(PsdError::InvalidImageBounds { width, height, .. })
            if empty_payload && matches!((width, height), (0, -1) | (-1, 0)) =>
        {
            Ok((0, 0))
        }
        Err(error) => Err(error),
    }
}

/// Charge one retained channel plane before calling its decoder. This adapts
/// a remaining-budget model used by independent readers to planar typed
/// channels.
fn layer_record_extents(record: &LayerRecord<'_>, version: Version) -> Result<(usize, usize)> {
    let rect = Rect::new(record.top, record.left, record.bottom, record.right);
    match read_rect_extents(rect, "layer", version) {
        Ok(extents) => Ok(extents),
        Err(PsdError::InvalidImageBounds { .. })
            if record
                .channels
                .iter()
                .filter(|info| !ChannelKey(info.index).is_mask())
                .all(|info| info.size <= 2) =>
        {
            Ok((0, 0))
        }
        Err(error) => Err(error),
    }
}

fn channel_record_extents(
    record: &LayerRecord<'_>,
    key: ChannelKey,
    empty: bool,
    layer_extents: (usize, usize),
    version: Version,
) -> Result<(usize, usize)> {
    if key.is_mask() {
        let bounds = Rect::new(record.top, record.left, record.bottom, record.right);
        let rect = mask_channel_rect(record.mask_data.as_ref(), key).unwrap_or(bounds);
        let kind = if key == ChannelKey::REAL_USER_MASK {
            "real mask"
        } else {
            "mask"
        };
        mask_channel_extents(rect, kind, version, empty)
    } else {
        Ok(layer_extents)
    }
}

pub(crate) fn charge_decoded_bitmap(
    remaining: &mut Option<usize>,
    width: usize,
    height: usize,
    bytes_per_sample: usize,
) -> Result<()> {
    let requested = width
        .checked_mul(height)
        .and_then(|samples| samples.checked_mul(bytes_per_sample))
        .ok_or(PsdError::InvalidImageBounds {
            kind: "layer channel",
            width: i64::try_from(width).unwrap_or(i64::MAX),
            height: i64::try_from(height).unwrap_or(i64::MAX),
        })?;
    if let Some(available) = *remaining {
        if requested > available {
            return Err(PsdError::ExceededMemoryLimit {
                requested,
                available,
            });
        }
        *remaining = Some(available - requested);
    }
    Ok(())
}

/// Byte range of the blend-mode key inside an `lsct` payload
/// (`type(4)`, `8BIM`, `key(4)`, optional sub-type).
const LSCT_BLEND_KEY: std::ops::Range<usize> = 8..12;

/// A parsed/created Photoshop document.
#[derive(Debug, Clone)]
pub struct LayeredFile<T: BitDepth> {
    /// Container version (PSD or PSB); drives all variable-width lengths.
    pub version: Version,
    pub width: u32,
    pub height: u32,
    pub color_mode: ColorMode,
    /// Merged-image channel count written to the header and `ImageData`.
    /// Read documents keep the file's value; created documents start at the
    /// color-mode channel count.
    pub num_channels: u16,
    /// The file's on-disk bit depth. It differs from the document type only
    /// for 1-bit (bitmap mode) documents, whose packed pixels are expanded to
    /// 8-bit samples on read; saving such a document writes 8-bit.
    pub source_depth: u16,
    /// Document resolution in dots per inch, read from the resolution resource's
    /// horizontal axis (72 when the file has none).
    ///
    /// A save rewrites the resolution resource only when this value differs from
    /// what the resource says, so an unedited document keeps its own axes, units,
    /// and payload. Setting it sets both axes to that many pixels per inch; the
    /// full resource stays available through
    /// [`image_resources`](Self::image_resources) for anything finer.
    pub dpi: f32,
    /// Raw ICC profile bytes (empty = no profile). This is the document's only
    /// copy: a read moves the profile here out of `image_resources`, and a save
    /// writes it back into the resource block.
    pub icc_profile: Vec<u8>,
    /// Palette/toning data, preserved verbatim.
    pub color_mode_data: ColorModeData,
    /// Document image resources, preserved (DPI is refreshed on write). Its ICC
    /// block, when the file had one, is an empty placeholder that keeps the
    /// block's position; the profile itself is [`icc_profile`](Self::icc_profile).
    pub image_resources: ImageResources,
    /// Undocumented legacy section, preserved verbatim.
    pub global_layer_mask_info: GlobalLayerMaskInfo,
    /// Document-level tagged blocks (`lnk2`, `Patt`, …); `Lr16`/`Lr32` are
    /// regenerated from the layer tree on write.
    pub document_blocks: Option<AdditionalLayerInfo>,
    /// On-disk layer count was negative (merged image alpha present).
    pub has_merged_alpha: bool,
    /// Optional per-channel compression override (`None` = automatic).
    pub compression: Option<Compression>,
    /// Runtime-only base path used to resolve relative external smart-object
    /// links. It is deliberately excluded from PSD serialization/equality.
    source_path: Option<PathBuf>,
    /// Runtime-only snapshot of the document-level `Txt2` text cache and the
    /// text layers it was written for, used to drop the cache on write once
    /// text changes (see [`LayeredFile::text_cache_is_stale`]). Excluded from
    /// equality like `source_path`.
    pub(crate) text_cache: Option<TextCacheBaseline>,
    /// Runtime-only source files of linked records created or replaced from
    /// a path in this session, keyed by link identity. Lets an embedded
    /// smart object be re-linked externally later (upstream keeps the path
    /// on its `LinkedLayerData`). Excluded from equality.
    pub(crate) linked_sources: std::collections::HashMap<String, PathBuf>,
    /// Remaining cumulative budget for channels materialized by a lazy read.
    /// Runtime state, excluded from document equality.
    remaining_bitmap_memory: Option<usize>,
    /// The merged composite of a source document that has no layer records.
    /// It is the only pixel source for such a document and is retained for
    /// rendering plus lossless unedited writes.
    pub(crate) stored_merged_image: Option<MergedImageData>,
    /// Slot arena: removed layers leave `None` so ids stay stable and are
    /// never reused (see [`LayerId`]).
    layers: Vec<Option<Layer<T>>>,
    root_children: Vec<LayerId>,
}

impl<T: BitDepth> PartialEq for LayeredFile<T> {
    fn eq(&self, other: &Self) -> bool {
        self.version == other.version
            && self.source_depth == other.source_depth
            && self.width == other.width
            && self.height == other.height
            && self.color_mode == other.color_mode
            && self.num_channels == other.num_channels
            && self.dpi == other.dpi
            && self.icc_profile == other.icc_profile
            && self.color_mode_data == other.color_mode_data
            && self.image_resources == other.image_resources
            && self.global_layer_mask_info == other.global_layer_mask_info
            && self.document_blocks == other.document_blocks
            && self.has_merged_alpha == other.has_merged_alpha
            && self.compression == other.compression
            && self.layers == other.layers
            && self.root_children == other.root_children
            && self.stored_merged_image == other.stored_merged_image
    }
}

impl<T: BitDepth> LayeredFile<T> {
    /// Return a copy of this document with channel samples converted to
    /// another PSD bit depth. Layer structure, masks, tagged blocks, profiles,
    /// and unknown passthrough data are preserved. Lazy channels are decoded
    /// and converted in the returned document.
    ///
    /// Samples are normalized, then rounded to the destination integer range.
    /// Converting 32-bit float samples to integer depth clips values outside
    /// `0..=1`; converting from integer depth cannot restore precision that was
    /// discarded in the source. This is a sample conversion, not Photoshop's
    /// color-managed bit-depth conversion.
    ///
    /// The source document remains unchanged. The target's decoded layer
    /// channels and the temporary work for lazy channels must fit the remaining
    /// bitmap memory budget.
    pub fn convert_bit_depth<U: BitDepth>(&self) -> Result<LayeredFile<U>> {
        let mode_supports_depth = match self.color_mode {
            ColorMode::Bitmap => U::DEPTH == 8,
            ColorMode::Indexed | ColorMode::Duotone => U::DEPTH == 8,
            ColorMode::Cmyk | ColorMode::Lab | ColorMode::Multichannel => {
                matches!(U::DEPTH, 8 | 16)
            }
            ColorMode::Grayscale | ColorMode::Rgb => matches!(U::DEPTH, 8 | 16 | 32),
        };
        if !mode_supports_depth {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "target bit depth is not supported by the document color mode",
            });
        }
        let _ = FileHeader::new(
            self.version,
            self.num_channels,
            self.width,
            self.height,
            depth_enum::<U>(),
            self.color_mode,
        )?;

        let mut target_bytes = 0usize;
        let mut raw_scratch_peak = 0usize;
        for layer in self.layers.iter().flatten() {
            layer.conversion_footprint::<U>(&mut target_bytes, &mut raw_scratch_peak)?;
        }
        if let Some(available) = self.remaining_bitmap_memory {
            let requested =
                target_bytes
                    .checked_add(raw_scratch_peak)
                    .ok_or(PsdError::InvalidData {
                        offset: 0,
                        message: "bit-depth conversion memory size overflows",
                    })?;
            if requested > available {
                return Err(PsdError::ExceededMemoryLimit {
                    requested,
                    available,
                });
            }
        }

        let layers = self
            .layers
            .iter()
            .map(|layer| {
                layer
                    .as_ref()
                    .map(|layer| layer.convert_bit_depth::<U>(self.source_depth))
                    .transpose()
            })
            .collect::<Result<Vec<_>>>()?;
        let stored_merged_image = self
            .stored_merged_image
            .as_ref()
            .map(MergedImageData::convert_depth::<T, U>)
            .transpose()?;

        Ok(LayeredFile {
            version: self.version,
            width: self.width,
            height: self.height,
            color_mode: self.color_mode,
            num_channels: self.num_channels,
            source_depth: if self.source_depth == 1 && U::DEPTH == 8 {
                1
            } else {
                U::DEPTH
            },
            dpi: self.dpi,
            icc_profile: self.icc_profile.clone(),
            color_mode_data: self.color_mode_data.clone(),
            image_resources: self.image_resources.clone(),
            global_layer_mask_info: self.global_layer_mask_info.clone(),
            document_blocks: self.document_blocks.clone(),
            has_merged_alpha: self.has_merged_alpha,
            compression: self.compression,
            source_path: self.source_path.clone(),
            text_cache: self.text_cache.clone(),
            linked_sources: self.linked_sources.clone(),
            remaining_bitmap_memory: self
                .remaining_bitmap_memory
                .map(|remaining| remaining - target_bytes),
            stored_merged_image,
            layers,
            root_children: self.root_children.clone(),
        })
    }

    /// Create an empty document (no layers). Defaults to a PSD container.
    pub fn new(color_mode: ColorMode, width: u32, height: u32) -> Result<Self> {
        let num_channels = color_channel_count(color_mode);
        // Validate the geometry through the header constructor.
        let _ = FileHeader::new(
            Version::Psd,
            num_channels.max(1),
            width,
            height,
            depth_enum::<T>(),
            color_mode,
        )?;
        // A new document carries the 72 ppi resolution resource Photoshop's own
        // documents have. A file read without one keeps lacking it: the write
        // path preserves that absence rather than normalizing it.
        let mut image_resources = ImageResources::new();
        image_resources.set_resolution_info(ResolutionInfoBlock::new(DEFAULT_DPI));
        Ok(Self {
            version: Version::Psd,
            source_depth: T::DEPTH,
            width,
            height,
            color_mode,
            num_channels: num_channels.max(1),
            dpi: DEFAULT_DPI,
            icc_profile: Vec::new(),
            color_mode_data: ColorModeData::default(),
            image_resources,
            global_layer_mask_info: GlobalLayerMaskInfo::default(),
            document_blocks: None,
            has_merged_alpha: false,
            compression: None,
            source_path: None,
            text_cache: None,
            linked_sources: std::collections::HashMap::new(),
            remaining_bitmap_memory: Some(DEFAULT_TOTAL_MEMORY_LIMIT),
            stored_merged_image: None,
            layers: Vec::new(),
            root_children: Vec::new(),
        })
    }

    /// Read a document from disk (mmap-backed).
    pub fn read(path: impl AsRef<Path>) -> Result<Self> {
        Self::read_with_options(path, ReadOptions::default())
    }

    /// Read a document from disk, reporting one [`ProgressEvent`] per layer.
    pub fn read_with_progress(
        path: impl AsRef<Path>,
        progress: &mut dyn FnMut(ProgressEvent<'_>),
    ) -> Result<Self> {
        Self::read_with_options_and_progress(path, ReadOptions::default(), progress)
    }

    /// Read a document from disk with explicit bitmap memory options.
    /// Set [`ReadOptions::use_raw_data`] to retain compressed layer and mask
    /// channels until explicitly decoded.
    pub fn read_with_options(path: impl AsRef<Path>, options: ReadOptions) -> Result<Self> {
        Self::read_with_options_and_progress(path, options, &mut ignore_progress)
    }

    /// Read a document from disk with explicit bitmap memory options and report
    /// one [`ProgressEvent`] per layer.
    pub fn read_with_options_and_progress(
        path: impl AsRef<Path>,
        options: ReadOptions,
        progress: &mut dyn FnMut(ProgressEvent<'_>),
    ) -> Result<Self> {
        let path = path.as_ref();
        let file = std::fs::File::open(path)?;
        // SAFETY: the mapping outlives the parse call and every field of the
        // returned document owns its data — nothing borrows the mapping.
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        let mut document = Self::from_bytes_with_options_and_progress(&mmap, options, progress)?;
        document.source_path = Some(absolute_or_current_dir(path)?);
        Ok(document)
    }

    /// Read a document from memory.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        Self::from_bytes_with_options(bytes, ReadOptions::default())
    }

    /// Read a document from memory while retaining the path it came from as
    /// context for relative external smart-object links. The path is runtime
    /// metadata only and is not serialized into the PSD/PSB.
    pub fn from_bytes_with_source_path(
        bytes: &[u8],
        source_path: impl AsRef<Path>,
    ) -> Result<Self> {
        Self::from_bytes_with_source_path_and_options(bytes, source_path, ReadOptions::default())
    }

    /// Read from memory with explicit options while retaining the path as
    /// context for relative external smart-object links.
    pub fn from_bytes_with_source_path_and_options(
        bytes: &[u8],
        source_path: impl AsRef<Path>,
        options: ReadOptions,
    ) -> Result<Self> {
        let mut document = Self::from_bytes_with_options(bytes, options)?;
        document.source_path = Some(absolute_or_current_dir(source_path.as_ref())?);
        Ok(document)
    }

    /// The optional runtime path context used for relative external links.
    pub fn source_path(&self) -> Option<&Path> {
        self.source_path.as_deref()
    }

    /// Read a document from memory, reporting one [`ProgressEvent`] per layer.
    pub fn from_bytes_with_progress(
        bytes: &[u8],
        progress: &mut dyn FnMut(ProgressEvent<'_>),
    ) -> Result<Self> {
        Self::from_bytes_with_options_and_progress(bytes, ReadOptions::default(), progress)
    }

    /// Read a document from memory with explicit bitmap memory options.
    ///
    /// [`ReadOptions::default()`] limits cumulative decoded layer-channel
    /// samples to 2 GiB; use [`ReadOptions::unlimited()`] to disable the limit.
    /// With `use_raw_data`, pixel getters return `None` until their channels
    /// are explicitly decoded. Untouched raw channels remain writable.
    pub fn from_bytes_with_options(bytes: &[u8], options: ReadOptions) -> Result<Self> {
        Self::from_bytes_with_options_and_progress(bytes, options, &mut ignore_progress)
    }

    /// Read a document from memory with explicit bitmap memory options and
    /// report one [`ProgressEvent`] per layer.
    pub fn from_bytes_with_options_and_progress(
        bytes: &[u8],
        options: ReadOptions,
        progress: &mut dyn FnMut(ProgressEvent<'_>),
    ) -> Result<Self> {
        let mut reader = BeReader::new(bytes);
        let file = PhotoshopFile::read_borrowed(&mut reader)?;
        let stored_merged_image = if file.layer_and_mask_info.layer_info.layer_records.is_empty() {
            let start = reader.position();
            // LayerInfo cannot express a negative zero layer count. For a
            // layerless image, the one extra merged channel is its available
            // transparency plane.
            let merged_transparency = file.layer_and_mask_info.layer_info.has_merged_alpha
                || file.header.color_mode != ColorMode::Multichannel
                    && file.header.num_channels
                        == color_channel_count(file.header.color_mode).saturating_add(1);
            MergedImageData::read(
                bytes.get(start..).unwrap_or_default(),
                &file.header,
                merged_transparency,
            )?
        } else {
            None
        };
        Self::from_photoshop_file(file, options, progress, stored_merged_image)
    }

    fn from_photoshop_file(
        file: PhotoshopFile,
        options: ReadOptions,
        progress: &mut dyn FnMut(ProgressEvent<'_>),
        stored_merged_image: Option<MergedImageData>,
    ) -> Result<Self> {
        let header = file.header;
        // A 1-bit (bitmap mode) document is read as an 8-bit one: its packed
        // pixels are expanded during channel decode. Saving writes 8-bit —
        // the port has no sample type for packed bits.
        let one_bit = header.depth == CoreBitDepth::One;
        if one_bit && T::DEPTH != 8 {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "a 1-bit (bitmap mode) document reads as an 8-bit document",
            });
        }
        if !one_bit && header.depth.as_raw() != T::DEPTH {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "file bit depth does not match the document type",
            });
        }

        let mut image_resources = file.image_resources;
        let dpi = image_resources
            .resolution_info()
            .map(|info| info.horizontal_resolution.to_f32())
            .unwrap_or(DEFAULT_DPI);
        // The profile moves out of its resource block, which stays as an empty
        // placeholder that keeps its place among the resources. `icc_profile` is
        // what a save writes, so a copy left in the block would only sit in memory
        // beside it, and could disagree with it.
        let icc_profile = image_resources.take_icc_profile().unwrap_or_default();
        let mut layer_and_mask_info = file.layer_and_mask_info;
        let has_merged_alpha = layer_and_mask_info.layer_info.has_merged_alpha
            || stored_merged_image
                .as_ref()
                .is_some_and(|merged| merged.transparency);

        let mut document = Self {
            version: header.version,
            source_depth: header.depth.as_raw(),
            width: header.width,
            height: header.height,
            color_mode: header.color_mode,
            num_channels: header.num_channels,
            dpi,
            icc_profile,
            color_mode_data: file.color_mode_data,
            image_resources,
            global_layer_mask_info: layer_and_mask_info.global_layer_mask_info,
            document_blocks: layer_and_mask_info
                .additional_layer_info
                .map(|blocks| blocks.into_owned()),
            has_merged_alpha,
            compression: None,
            source_path: None,
            text_cache: None,
            linked_sources: std::collections::HashMap::new(),
            remaining_bitmap_memory: options.total_memory_limit,
            stored_merged_image,
            layers: Vec::new(),
            root_children: Vec::new(),
        };
        let mut remaining_bitmap_memory = options.total_memory_limit;
        document.build_layers(
            std::mem::take(&mut layer_and_mask_info.layer_info),
            header.version,
            options,
            &mut remaining_bitmap_memory,
            progress,
        )?;
        document.remaining_bitmap_memory = remaining_bitmap_memory;
        document.text_cache = TextCacheBaseline::capture(&document);
        Ok(document)
    }

    fn build_layers(
        &mut self,
        info: LayerInfo<'_>,
        version: Version,
        options: ReadOptions,
        remaining_bitmap_memory: &mut Option<usize>,
        progress: &mut dyn FnMut(ProgressEvent<'_>),
    ) -> Result<()> {
        if info.layer_records.len() != info.channel_image_data.len() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "layer records and channel image data count mismatch",
            });
        }

        let total = info.layer_records.len();
        let mut jobs = Vec::with_capacity(total);
        // Reserve the complete decoded footprint before workers allocate.
        // Geometry is shared with construction so mask and stub rules agree.
        for (record, data) in info.layer_records.into_iter().zip(info.channel_image_data) {
            let extents = layer_record_extents(&record, version)?;
            let divider = record
                .additional_layer_info
                .as_ref()
                .and_then(|blocks| blocks.get(TaggedBlockKey::LSCT))
                .and_then(|block| block.data.get(..4))
                .map(|bytes| {
                    SectionDivider::from_raw(u32::from_be_bytes(bytes.try_into().unwrap()))
                });
            let mut scratch = 0usize;
            for (channel_info, channel) in record.channels.iter().zip(&data.channels) {
                let key = ChannelKey(channel_info.index);
                let keep = match divider {
                    Some(SectionDivider::BoundingSection | SectionDivider::Unknown(_)) => false,
                    Some(SectionDivider::OpenFolder | SectionDivider::ClosedFolder) => {
                        key.is_mask()
                    }
                    Some(SectionDivider::Any) | None => true,
                };
                if !keep {
                    continue;
                }
                let (width, height) = channel_record_extents(
                    &record,
                    key,
                    channel.data.is_empty(),
                    extents,
                    version,
                )?;
                if options.use_raw_data {
                    scratch = scratch.saturating_add(channel.data.len());
                } else {
                    charge_decoded_bitmap(remaining_bitmap_memory, width, height, T::SIZE)?;
                    scratch = scratch.max(width.saturating_mul(height).saturating_mul(T::SIZE));
                }
            }
            jobs.push(((record, data), scratch));
        }
        let source_depth = self.source_depth;
        let mut ids = Vec::with_capacity(total);
        crate::parallel::for_each_ordered(
            jobs,
            |(record, data)| {
                Self::build_layer(record, data, version, options, &mut None, source_depth)
            },
            |layer| {
                progress(ProgressEvent::Layer {
                    name: &layer.name,
                    index: ids.len(),
                    total,
                });
                ids.push(self.layers.len());
                self.layers.push(Some(layer));
                Ok(())
            },
        )?;

        // Assign parents by walking the (bottom-to-top) record order in
        // reverse with a group stack: a group's start record follows its
        // contents, so reverse order opens groups before their children and
        // section dividers close them.
        let mut parent: Vec<Option<LayerId>> = vec![None; ids.len()];
        let mut stack: Vec<LayerId> = Vec::new();
        for &id in ids.iter().rev() {
            match &self.slot(id).kind {
                LayerKind::SectionDivider(_) => {
                    stack.pop();
                    parent[id] = stack.last().copied();
                }
                LayerKind::Group(_) => {
                    parent[id] = stack.last().copied();
                    stack.push(id);
                }
                LayerKind::Image(_)
                | LayerKind::Text(_)
                | LayerKind::Adjustment(_)
                | LayerKind::Shape(_) => parent[id] = stack.last().copied(),
            }
        }
        // Fill children lists in on-disk (file) order.
        for &id in &ids {
            match parent[id] {
                Some(group_id) => match &mut self.slot_mut(group_id).kind {
                    LayerKind::Group(group) => group.children.push(id),
                    _ => unreachable!("only groups are recorded as parents"),
                },
                None => self.root_children.push(id),
            }
        }
        Ok(())
    }

    fn build_layer(
        record: LayerRecord<'_>,
        channel_data: ChannelImageData<'_>,
        version: Version,
        options: ReadOptions,
        remaining_bitmap_memory: &mut Option<usize>,
        source_depth: u16,
    ) -> Result<Layer<T>> {
        let layer_extents = layer_record_extents(&record, version)?;
        let blocks = record
            .additional_layer_info
            .map(Cow::into_owned)
            .unwrap_or_default();

        // The unicode name ('luni') takes precedence over the pascal name.
        // Photoshop pads the block to four bytes and some other writers do
        // not, so the read must not assume either: the code-unit count is the
        // whole payload and padding is only ever trailing bytes.
        let mut name = record.name.value().to_string();
        if let Some(block) = blocks.get(TaggedBlockKey::LUNI) {
            let mut reader = BeReader::new(&block.data);
            if let Ok(unicode) = UnicodeString::read(&mut reader, 1) {
                name = unicode.value().to_string();
            }
        }

        let divider = blocks
            .get(TaggedBlockKey::LSCT)
            .and_then(|block| block.data.get(..4))
            .map(|bytes| SectionDivider::from_raw(u32::from_be_bytes(bytes.try_into().unwrap())));

        let has_text_metadata = blocks.get(TaggedBlockKey::new(*b"TySh")).is_some()
            || blocks.get(TaggedBlockKey::new(*b"Txt2")).is_some();

        if record.channels.len() != channel_data.channels.len() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "layer channel count mismatch",
            });
        }
        // PhotoshopAPI/src/LayeredFile/LayerTypes/ImageLayer.h assigns parsed
        // channels into a keyed map and overwrites repeated indices. Reject a
        // duplicate before either storage mode could discard its payload.
        let mut seen_channels = BTreeSet::new();
        for channel in &record.channels {
            if !seen_channels.insert(channel.index) {
                return Err(PsdError::InvalidData {
                    offset: 0,
                    message: "duplicate layer channel index",
                });
            }
        }
        let mut channel_iter = channel_data.channels.into_iter();
        let layer_rect = Rect::new(record.top, record.left, record.bottom, record.right);
        // A layer whose non-mask channels all carry no payload has no pixels
        // to allocate, so a degenerate rectangle (an empty gradient-fill
        // layer with a 0 x -1 rectangle, say) is read as zero-area rather
        // than rejected. Photoshop opens these files.
        let is_group = matches!(
            divider,
            Some(SectionDivider::OpenFolder | SectionDivider::ClosedFolder)
        );
        let mut decode_channels = |keep: fn(ChannelKey) -> bool| -> Result<ChannelStore<T>> {
            let mut channels = ChannelStore::new();
            for (info, channel) in record.channels.iter().zip(channel_iter.by_ref()) {
                let key = ChannelKey(info.index);
                if !keep(key) {
                    continue;
                }
                let (width, height) = if key.is_mask() {
                    let rect =
                        mask_channel_rect(record.mask_data.as_ref(), key).unwrap_or(layer_rect);
                    let kind = if key == ChannelKey::REAL_USER_MASK {
                        "real mask"
                    } else {
                        "mask"
                    };
                    mask_channel_extents(rect, kind, version, channel.data.is_empty())?
                } else {
                    layer_extents
                };
                if options.use_raw_data {
                    channels.insert_raw(
                        key,
                        RawChannelData::new(
                            channel.compression,
                            channel.data.into_owned(),
                            width,
                            height,
                            version,
                        ),
                    );
                } else {
                    charge_decoded_bitmap(remaining_bitmap_memory, width, height, T::SIZE)?;
                    let decoded = decompress_channel::<T>(
                        channel.compression,
                        &channel.data,
                        width,
                        height,
                        version,
                        source_depth,
                    );
                    match decoded {
                        Ok(samples) => {
                            channels.insert(key, samples);
                        }
                        // Older Photoshop files carry compression-marker-only
                        // or undersized `-3` records. Photoshop ignores that
                        // plane's payload, so a record that does not decode is
                        // kept raw rather than failing the whole read.
                        Err(_) if key == ChannelKey::REAL_USER_MASK => {
                            channels.insert_raw(
                                key,
                                RawChannelData::new(
                                    channel.compression,
                                    channel.data.into_owned(),
                                    width,
                                    height,
                                    version,
                                ),
                            );
                        }
                        // Any other channel whose stream does not decode is
                        // replaced with a zero-filled channel of its declared
                        // size, the way mature readers recover: one corrupt
                        // channel costs that channel, not the document. An
                        // unknown compression marker already failed above, so
                        // this only covers damaged data of a known codec.
                        Err(error) => {
                            tracing::warn!(
                                "channel {key:?} of layer {:?} did not decode ({error});                                  substituting a zero-filled channel",
                                record.name.value()
                            );
                            channels.insert(key, vec![T::ZERO; width * height]);
                        }
                    }
                }
            }
            Ok(channels)
        };

        let carries_adjustment_settings = blocks.blocks.iter().any(|block| {
            AdjustmentKind::from_key(block.key).is_some_and(AdjustmentKind::marks_layer)
        });
        let has_vector_mask = blocks
            .blocks
            .iter()
            .any(|block| psd_core::vector::is_vector_mask_key(block.key));
        let has_shape_fill = blocks.blocks.iter().any(|block| {
            block.key.as_bytes() == *b"vscg"
                || AdjustmentKind::from_key(block.key).is_some_and(AdjustmentKind::is_fill)
        });
        let carries_shape_settings = has_vector_mask && has_shape_fill;
        let kind = match divider {
            // Any `lsct` value this build does not know is still a divider
            // record: it keeps its pairing (and its bytes) instead of being
            // read as a pixel layer and gaining a synthesized divider on save.
            Some(kind @ (SectionDivider::BoundingSection | SectionDivider::Unknown(_))) => {
                LayerKind::SectionDivider(kind)
            }
            // Groups keep only their mask channels; their color/alpha
            // channels are the empty stubs Photoshop writes for every group.
            Some(SectionDivider::OpenFolder | SectionDivider::ClosedFolder) => {
                LayerKind::Group(GroupLayer {
                    children: Vec::new(),
                    open: divider == Some(SectionDivider::OpenFolder),
                    channels: decode_channels(ChannelKey::is_mask)?,
                })
            }
            // 'Any' and absent dividers are pixel layers.
            Some(SectionDivider::Any) | None => {
                let channels = decode_channels(|_| true)?;
                if has_text_metadata {
                    LayerKind::Text(TextLayer { channels })
                } else if carries_shape_settings {
                    LayerKind::Shape(ShapeLayer { channels })
                } else if carries_adjustment_settings {
                    LayerKind::Adjustment(AdjustmentLayer { channels })
                } else {
                    LayerKind::Image(ImageLayer { channels })
                }
            }
        };

        // A group's effective blend mode lives on its `lsct` block (Photoshop
        // writes `pass` there and `norm` on the record), as upstream reads it.
        let blend_mode = blocks
            .get(TaggedBlockKey::LSCT)
            .filter(|_| is_group)
            .and_then(|block| block.data.get(LSCT_BLEND_KEY))
            .map_or(record.blend_mode, |key| {
                BlendMode::from_bytes(key.try_into().expect("four bytes"))
            });

        Ok(Layer {
            name,
            bounds: layer_rect,
            opacity: record.opacity,
            blend_mode,
            flags: record.flags,
            clipping: record.clipping,
            mask: record.mask_data,
            blocks,
            blending_ranges: record.blending_ranges,
            kind,
            compression: None,
            mask_compression: None,
        })
    }

    // ------------------------------------------------------------------
    // Accessors
    // ------------------------------------------------------------------

    /// Every layer in the document, in arena order (creation order, which for
    /// read documents is Photoshop's bottom-to-top record order). Removed
    /// layers are skipped.
    pub fn layers(&self) -> impl Iterator<Item = &Layer<T>> + '_ {
        self.layers.iter().flatten()
    }

    /// [`layers`](Self::layers) paired with their ids.
    pub fn layers_with_ids(&self) -> impl Iterator<Item = (LayerId, &Layer<T>)> + '_ {
        self.layers
            .iter()
            .enumerate()
            .filter_map(|(id, slot)| slot.as_ref().map(|layer| (id, layer)))
    }

    /// The layer with this id, `None` when it was removed or never existed.
    pub fn layer(&self, id: LayerId) -> Option<&Layer<T>> {
        self.layers.get(id)?.as_ref()
    }

    pub fn layer_mut(&mut self, id: LayerId) -> Option<&mut Layer<T>> {
        self.layers.get_mut(id)?.as_mut()
    }

    /// Move a layer and everything it carries, the way Photoshop's Move tool
    /// does.
    ///
    /// Every layer in the subtree moves — a group takes its children with it —
    /// and each layer moves what it owns: pixel bounds, a text layer's
    /// transform, and vector geometry (a shape's path or a `vmsk`/`vsms` vector
    /// mask). Paths are stored as fractions of the document, so this is the
    /// entry point that can convert a pixel move for them; a layer on its own
    /// cannot, and [`Layer::translate`] refuses rather than moving halfway.
    ///
    /// Mask **rects** are never rewritten: a mask's stored rect is relative to
    /// the layer when its link flag is set and absolute otherwise, so a linked
    /// mask follows implicitly and an unlinked one stays where it is. A smart
    /// object routes through [`move_smart_object`](Self::move_smart_object) so
    /// its warp re-renders.
    pub fn translate_layer(&mut self, id: LayerId, dx: i32, dy: i32) -> Result<()> {
        if self.layer(id).is_none() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "unknown layer id",
            });
        }
        let path_dx = fraction_delta(dx, self.width)?;
        let path_dy = fraction_delta(dy, self.height)?;
        let mut stack = vec![id];
        while let Some(current) = stack.pop() {
            if let Some(children) = self.children(Some(current)) {
                stack.extend(children.iter().copied());
            }
            if self.layer(current).is_some_and(Layer::is_smart_object) {
                self.move_smart_object(current, Point2::new(f64::from(dx), f64::from(dy)))?;
                continue;
            }
            if let Some(layer) = self.layer_mut(current) {
                layer.translate_with_path_delta(dx, dy, Some((path_dx, path_dy)))?;
            }
        }
        Ok(())
    }

    /// Decode one raw-backed layer or mask channel into the layer's
    /// [`ChannelStore`], releasing that channel's compressed payload.
    ///
    /// Returns `false` when an existing layer or key has no raw payload. The document's
    /// cumulative read budget is charged before decoding and is not refunded.
    /// Decoded samples become canonical channel data; no second cache is kept.
    pub fn decode_layer_channel(&mut self, id: LayerId, key: ChannelKey) -> Result<bool> {
        let mut remaining = self.remaining_bitmap_memory;
        let source_depth = self.source_depth;
        let decoded = {
            let layer = self.layer_mut(id).ok_or(PsdError::InvalidData {
                offset: 0,
                message: "cannot decode channels for an absent layer",
            })?;
            let Some(channels) = layer.channels_mut() else {
                return Ok(false);
            };
            let Some(raw) = channels.raw(key) else {
                return Ok(false);
            };
            charge_decoded_bitmap(&mut remaining, raw.width, raw.height, T::SIZE)?;
            let samples = decompress_channel::<T>(
                raw.compression,
                &raw.payload,
                raw.width,
                raw.height,
                raw.version,
                source_depth,
            )?;
            channels.insert(key, samples);
            true
        };
        if decoded {
            self.remaining_bitmap_memory = remaining;
        }
        Ok(decoded)
    }

    /// Decode all remaining raw-backed channels of one layer, then release
    /// their compressed payloads. Other layers remain lazy. If any channel
    /// fails, the layer remains unchanged and its budget is not consumed.
    /// Decoded samples become canonical channel data; no second cache is kept.
    pub fn decode_layer_pixels(&mut self, id: LayerId) -> Result<()> {
        let mut remaining = self.remaining_bitmap_memory;
        let source_depth = self.source_depth;
        let decoded_count = {
            let layer = self.layer_mut(id).ok_or(PsdError::InvalidData {
                offset: 0,
                message: "cannot decode channels for an absent layer",
            })?;
            let Some(channels) = layer.channels_mut() else {
                return Ok(());
            };
            let decoded = channels.decode_raw(source_depth, &mut remaining)?;
            let decoded_count = decoded.len();
            for (key, samples) in decoded {
                channels.insert(key, samples);
            }
            decoded_count
        };
        if decoded_count > 0 {
            self.remaining_bitmap_memory = remaining;
        }
        Ok(())
    }

    /// Decode all raw-backed layer and mask channels with bounded parallel
    /// workspace. The complete pixel budget is checked before decoding starts.
    /// Channels decode independently: failed channels stay raw, successful
    /// channels release their payloads immediately. All channels are attempted;
    /// the first error in arena and channel-key order is returned. Use
    /// [`decode_layer_pixels`](Self::decode_layer_pixels) for an atomic layer decode.
    pub fn decode_all_layer_pixels(&mut self) -> Result<()> {
        let mut reserved = self.remaining_bitmap_memory;
        let mut jobs = Vec::new();
        for layer in self.layers.iter_mut().flatten() {
            let Some(channels) = layer.channels() else {
                continue;
            };
            let mut scratch = 0;
            for (_, raw) in channels.raw_channels() {
                charge_decoded_bitmap(&mut reserved, raw.width, raw.height, T::SIZE)?;
                let size = raw.width.saturating_mul(raw.height).saturating_mul(T::SIZE);
                scratch = scratch.max(size);
            }
            // Even zero-area raw channels must become decoded entries.
            if channels.raw_channels().next().is_some() {
                jobs.push((layer, scratch));
            }
        }
        let source_depth = self.source_depth;
        let remaining = &mut self.remaining_bitmap_memory;
        let mut first_error = None;
        crate::parallel::for_each_ordered(
            jobs,
            |layer| {
                Ok(layer
                    .channels_mut()
                    .expect("prepared channel store")
                    .decode_raw_in_place(source_depth))
            },
            |(bytes, error)| {
                if let Some(remaining) = remaining {
                    *remaining -= bytes;
                }
                if first_error.is_none() {
                    first_error = error;
                }
                Ok(())
            },
        )?;
        first_error.map_or(Ok(()), Err)
    }

    /// Turn a source document's layerless merged image into an editable
    /// bottom image layer. This is needed before saving layer edits made over
    /// a layerless document; otherwise no layer record exists to carry the
    /// original pixels.
    pub fn materialize_merged_image(&mut self) -> Result<Option<LayerId>> {
        let Some(merged) = self.stored_merged_image.as_ref() else {
            return Ok(None);
        };
        if !self.root_children.is_empty() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "materialize the layerless image before adding layers",
            });
        }

        let width = usize::try_from(merged.width).map_err(|_| PsdError::InvalidData {
            offset: 0,
            message: "merged image width is unaddressable",
        })?;
        let height = usize::try_from(merged.height).map_err(|_| PsdError::InvalidData {
            offset: 0,
            message: "merged image height is unaddressable",
        })?;
        let channel_count = usize::from(merged.channels);
        let mut remaining = self.remaining_bitmap_memory;
        for _ in 0..channel_count {
            charge_decoded_bitmap(&mut remaining, width, height, T::SIZE)?;
        }
        let mut planes = merged.decode::<T>()?;
        let rect = Rect::new(0, 0, self.height as i32, self.width as i32);
        let mut layer = Layer::new_image("Background", rect);
        let color_channels = if self.color_mode == ColorMode::Multichannel {
            channel_count - usize::from(merged.transparency)
        } else {
            usize::from(color_channel_count(self.color_mode))
        };
        if merged.transparency && planes.len() > color_channels {
            if self.color_mode == ColorMode::Indexed {
                return Err(PsdError::UnsupportedColorMode(ColorMode::Indexed.as_raw()));
            }
            let alpha: Vec<f32> = planes[color_channels]
                .iter()
                .map(|sample| sample.to_f32().clamp(0.0, 1.0))
                .collect();
            for (channel, samples) in planes.iter_mut().take(color_channels).enumerate() {
                let matte =
                    crate::composite::merged_matte_value(self.color_mode, channel, merged.depth);
                for (sample, &alpha) in samples.iter_mut().zip(&alpha) {
                    *sample = T::from_f32(crate::composite::unmatte_sample(
                        sample.to_f32(),
                        alpha,
                        matte,
                    ));
                }
            }
        }
        for (index, samples) in planes.into_iter().enumerate() {
            let key = if merged.transparency && index == color_channels {
                ChannelKey::ALPHA
            } else {
                ChannelKey::color(u8::try_from(index).map_err(|_| PsdError::InvalidData {
                    offset: 0,
                    message: "merged image has too many channels for a layer",
                })?)
            };
            layer
                .image_mut()
                .expect("new_image creates an image layer")
                .set_channel(key, samples);
        }
        let id = self.add_layer(layer);
        self.remaining_bitmap_memory = remaining;
        self.stored_merged_image = None;
        Ok(Some(id))
    }

    /// Number of layers in the document (removed layers excluded).
    pub fn layer_count(&self) -> usize {
        self.layers.iter().flatten().count()
    }

    /// The raw slot arena, indexed by [`LayerId`].
    pub(crate) fn slots(&self) -> &[Option<Layer<T>>] {
        &self.layers
    }

    pub(crate) fn slots_mut(&mut self) -> &mut Vec<Option<Layer<T>>> {
        &mut self.layers
    }

    pub(crate) fn root_children_mut(&mut self) -> &mut Vec<LayerId> {
        &mut self.root_children
    }

    /// A layer reachable from the tree. Tree links only ever name live slots.
    fn slot(&self, id: LayerId) -> &Layer<T> {
        self.layers[id]
            .as_ref()
            .expect("tree links name live layers")
    }

    fn slot_mut(&mut self, id: LayerId) -> &mut Layer<T> {
        self.layers[id]
            .as_mut()
            .expect("tree links name live layers")
    }

    pub fn root_children(&self) -> &[LayerId] {
        &self.root_children
    }

    /// Append an ordinary layer at the top level.
    ///
    /// A layer that carries a `lyid` block keeps its id unless another layer in
    /// the document already has it, in which case it gets a fresh one: a clone,
    /// or a copy from another document, would otherwise repeat an id that
    /// Photoshop expects to be unique. A document read from disk never passes
    /// through here, so its own ids round-trip untouched.
    ///
    /// To add an artboard, use [`add_artboard`](Self::add_artboard), which
    /// also creates or updates the document-level artboard count.
    pub fn add_layer(&mut self, mut layer: Layer<T>) -> LayerId {
        self.ensure_unique_layer_id(&mut layer);
        self.layers.push(Some(layer));
        let id = self.layers.len() - 1;
        self.root_children.push(id);
        id
    }

    /// Give `layer` a fresh `lyid` when its own is already taken.
    ///
    /// Called from every insertion path (`add_layer`, `add_layer_to_group`, and
    /// `allocate_tree` for detached trees), so a clone or a copy from another
    /// document cannot repeat an id. A document read from disk never passes
    /// through here, so its own ids round-trip untouched.
    pub(crate) fn ensure_unique_layer_id(&mut self, layer: &mut Layer<T>) {
        let Some(existing) = layer.layer_id() else {
            return;
        };
        if !self
            .layers
            .iter()
            .flatten()
            .any(|other| other.layer_id() == Some(existing))
        {
            return;
        }
        let next = self
            .layers
            .iter()
            .flatten()
            .filter_map(Layer::layer_id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        layer.set_layer_id(next);
    }

    /// Create a root artboard group and keep the document's artboard count in
    /// its `artd` settings block.
    pub fn add_artboard(
        &mut self,
        name: impl Into<String>,
        artboard: &Artboard,
    ) -> Result<LayerId> {
        let count = self
            .artboards()
            .len()
            .checked_add(1)
            .ok_or(PsdError::InvalidData {
                offset: 0,
                message: "artboard count exceeds the descriptor integer range",
            })?;
        artboard.to_tagged_block()?;
        let settings = self.artboard_settings_block_with_count(count)?;
        let mut layer = Layer::new_group(name);
        layer.set_artboard(artboard)?;
        self.upsert_document_block(settings);
        Ok(self.add_layer(layer))
    }

    /// Set the document-level `artd` artboard-tool settings.
    pub fn set_artboard_settings(&mut self, settings: &ArtboardSettings) -> Result<()> {
        let block = settings.to_tagged_block(self.version)?;
        self.upsert_document_block(block);
        Ok(())
    }

    pub(crate) fn upsert_document_block(&mut self, block: TaggedBlock) {
        let document_blocks = self
            .document_blocks
            .get_or_insert_with(AdditionalLayerInfo::new);
        match document_blocks
            .blocks
            .iter_mut()
            .find(|existing| existing.key == block.key)
        {
            Some(existing) if existing.data == block.data => {}
            Some(existing) => *existing = block,
            None => document_blocks.push(block),
        }
    }

    pub(crate) fn artboard_settings_block_with_count(&self, count: usize) -> Result<TaggedBlock> {
        let count = i32::try_from(count).map_err(|_| PsdError::InvalidData {
            offset: 0,
            message: "artboard count exceeds the descriptor integer range",
        })?;
        let mut settings = self.artboard_settings()?.unwrap_or_default();
        settings.set_count(count);
        settings.to_tagged_block(self.version)
    }

    /// Set or replace an artboard block on an existing group. The document's
    /// artboard count is adjusted when the group changes artboard status.
    pub fn set_artboard(&mut self, id: LayerId, artboard: &Artboard) -> Result<()> {
        let layer = self.layer(id).ok_or(PsdError::InvalidData {
            offset: 0,
            message: "artboard target layer does not exist",
        })?;
        if layer.group().is_none() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "only a group layer can be an artboard",
            });
        }
        if !layer.is_artboard()
            && (self.parent_is_within_artboard(self.parent(id))
                || self
                    .artboards()
                    .into_iter()
                    .any(|artboard| self.is_descendant(artboard, id)))
        {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "artboards cannot be nested inside other artboards",
            });
        }
        artboard.to_tagged_block()?;
        let was_artboard = layer.is_artboard();
        let mut settings = self.artboard_settings()?;
        if !was_artboard || settings.is_some() {
            let count = self
                .artboards()
                .len()
                .checked_add(usize::from(!was_artboard))
                .and_then(|count| i32::try_from(count).ok())
                .ok_or(PsdError::InvalidData {
                    offset: 0,
                    message: "artboard count exceeds the descriptor integer range",
                })?;
            let mut value = settings.take().unwrap_or_default();
            value.set_count(count);
            settings = Some(value);
        }
        let block = settings
            .as_ref()
            .map(|settings| settings.to_tagged_block(self.version))
            .transpose()?;
        self.layer_mut(id)
            .expect("validated above")
            .set_artboard(artboard)?;
        if let Some(block) = block {
            self.upsert_document_block(block);
        }
        Ok(())
    }

    /// Remove artboard metadata from an existing layer and update the document count.
    pub fn clear_artboard(&mut self, id: LayerId) -> Result<bool> {
        let was_artboard = self
            .layer(id)
            .ok_or(PsdError::InvalidData {
                offset: 0,
                message: "artboard target layer does not exist",
            })?
            .is_artboard();
        if !was_artboard {
            return Ok(false);
        }
        let settings = self.artboard_settings()?;
        let block = if let Some(mut settings) = settings {
            let remaining = self.artboards().len().saturating_sub(1);
            let count = i32::try_from(remaining).map_err(|_| PsdError::InvalidData {
                offset: 0,
                message: "artboard count exceeds the descriptor integer range",
            })?;
            settings.set_count(count);
            Some(settings.to_tagged_block(self.version)?)
        } else {
            None
        };
        let removed = self
            .layer_mut(id)
            .expect("validated above")
            .clear_artboard();
        if let Some(block) = block {
            self.upsert_document_block(block);
        }
        Ok(removed)
    }

    /// Remove the document-level `artd` block, if present.
    pub fn clear_artboard_settings(&mut self) -> bool {
        let Some(blocks) = &mut self.document_blocks else {
            return false;
        };
        let before = blocks.blocks.len();
        blocks
            .blocks
            .retain(|block| block.key != TaggedBlockKey::new(*b"artd"));
        let removed = before != blocks.blocks.len();
        if blocks.blocks.is_empty() {
            self.document_blocks = None;
        }
        removed
    }

    /// Create an adjustment or fill layer from one typed settings block.
    ///
    /// Adjustment layers use empty bounds; fill layers cover the document
    /// canvas. Add companion data such as CgEd with
    /// [`Layer::set_adjustment`](crate::Layer::set_adjustment) after creation.
    pub fn add_adjustment_layer(
        &mut self,
        name: impl Into<String>,
        settings: &psd_core::AdjustmentBlock,
    ) -> Result<LayerId> {
        let layer = self.build_adjustment_layer(name, settings)?;
        Ok(self.add_layer(layer))
    }

    /// Create an adjustment or fill layer inside a group.
    pub fn add_adjustment_layer_to_group(
        &mut self,
        group: LayerId,
        name: impl Into<String>,
        settings: &psd_core::AdjustmentBlock,
    ) -> Result<LayerId> {
        let layer = self.build_adjustment_layer(name, settings)?;
        self.add_layer_to_group(group, layer)
    }

    /// Create a shape layer from typed fill, mask, stroke, and origination blocks.
    pub fn add_shape_layer(
        &mut self,
        name: impl Into<String>,
        bounds: Rect,
        vector_blocks: &[psd_core::VectorBlock],
    ) -> Result<LayerId> {
        let layer = self.build_shape_layer(name, bounds, vector_blocks)?;
        Ok(self.add_layer(layer))
    }

    /// Create a shape layer inside a group.
    pub fn add_shape_layer_to_group(
        &mut self,
        group: LayerId,
        name: impl Into<String>,
        bounds: Rect,
        vector_blocks: &[psd_core::VectorBlock],
    ) -> Result<LayerId> {
        let layer = self.build_shape_layer(name, bounds, vector_blocks)?;
        self.add_layer_to_group(group, layer)
    }

    /// Create a shape layer from tagged blocks, accepting both legacy fill
    /// blocks (`SoCo`/`GdFl`/`PtFl`) and modern `vscg` content.
    pub fn add_shape_layer_from_blocks(
        &mut self,
        name: impl Into<String>,
        bounds: Rect,
        blocks: &[TaggedBlock],
    ) -> Result<LayerId> {
        let layer = self.build_shape_layer_from_blocks(name, bounds, blocks)?;
        Ok(self.add_layer(layer))
    }

    /// Create a tagged-block shape layer inside a group.
    pub fn add_shape_layer_from_blocks_to_group(
        &mut self,
        group: LayerId,
        name: impl Into<String>,
        bounds: Rect,
        blocks: &[TaggedBlock],
    ) -> Result<LayerId> {
        let layer = self.build_shape_layer_from_blocks(name, bounds, blocks)?;
        self.add_layer_to_group(group, layer)
    }

    fn build_shape_layer(
        &self,
        name: impl Into<String>,
        bounds: Rect,
        vector_blocks: &[psd_core::VectorBlock],
    ) -> Result<Layer<T>> {
        read_rect_extents(bounds, "shape layer", self.version)?;
        let mut layer = Layer::new_shape(name, bounds);
        for block in vector_blocks {
            layer.set_vector_block(block)?;
        }
        if !layer.has_shape_markers() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "a shape layer needs both a vector mask and a fill block",
            });
        }
        Ok(layer)
    }

    fn build_shape_layer_from_blocks(
        &self,
        name: impl Into<String>,
        bounds: Rect,
        blocks: &[TaggedBlock],
    ) -> Result<Layer<T>> {
        read_rect_extents(bounds, "shape layer", self.version)?;
        let mut layer = Layer::new_shape(name, bounds);
        for block in blocks {
            if let Some(adjustment) = AdjustmentBlock::read(block)? {
                layer.set_adjustment(&adjustment)?;
            } else if let Some(vector) = VectorBlock::read(block)? {
                layer.set_vector_block(&vector)?;
            } else {
                return Err(PsdError::InvalidData {
                    offset: 0,
                    message: "shape-layer builder received an unsupported tagged block",
                });
            }
        }
        if !layer.has_shape_markers() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "a shape layer needs both a vector mask and a fill block",
            });
        }
        Ok(layer)
    }

    fn build_adjustment_layer(
        &self,
        name: impl Into<String>,
        settings: &psd_core::AdjustmentBlock,
    ) -> Result<Layer<T>> {
        if !settings.kind.marks_layer() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "CgEd accompanies an adjustment and cannot create a layer",
            });
        }
        let bounds = if settings.kind.is_fill() {
            let width = i32::try_from(self.width).map_err(|_| PsdError::InvalidData {
                offset: 0,
                message: "document width exceeds layer-coordinate range",
            })?;
            let height = i32::try_from(self.height).map_err(|_| PsdError::InvalidData {
                offset: 0,
                message: "document height exceeds layer-coordinate range",
            })?;
            Rect::new(0, 0, height, width)
        } else {
            Rect::default()
        };
        let mut layer = Layer::new_adjustment(name, bounds);
        layer.set_adjustment(settings)?;
        Ok(layer)
    }

    /// Append a layer inside a group.
    pub fn add_layer_to_group(&mut self, group: LayerId, layer: Layer<T>) -> Result<LayerId> {
        // Validate before pushing so a failed call does not leave an orphan
        // in the arena (an unreferenced layer would silently vanish from
        // flatten()/writes while still counting toward layer_count()).
        if !matches!(
            self.layer(group).map(|layer| &layer.kind),
            Some(LayerKind::Group(_))
        ) {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "target layer is not a group",
            });
        }
        if self.parent_is_within_artboard(Some(group)) && layer.is_artboard() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "artboards cannot be nested inside other artboards",
            });
        }
        let mut layer = layer;
        self.ensure_unique_layer_id(&mut layer);
        let settings_update = if layer.is_artboard() {
            let count = self
                .artboards()
                .len()
                .checked_add(1)
                .ok_or(PsdError::InvalidData {
                    offset: 0,
                    message: "artboard count exceeds the descriptor integer range",
                })?;
            Some(self.artboard_settings_block_with_count(count)?)
        } else {
            None
        };
        self.layers.push(Some(layer));
        let id = self.layers.len() - 1;
        match &mut self.slot_mut(group).kind {
            LayerKind::Group(group) => group.children.push(id),
            _ => unreachable!("validated as a group above"),
        }
        if let Some(block) = settings_update {
            self.upsert_document_block(block);
        }
        Ok(id)
    }

    /// Find a layer by Photoshop-style path (`"Group/Image"`).
    pub fn find_layer(&self, path: &str) -> Option<LayerId> {
        let mut children: &[LayerId] = &self.root_children;
        let mut current = None;
        for part in path.split('/').filter(|part| !part.is_empty()) {
            let id = children
                .iter()
                .copied()
                .find(|&id| self.slot(id).name == part)?;
            current = Some(id);
            children = match &self.slot(id).kind {
                LayerKind::Group(group) => &group.children,
                _ => &[],
            };
        }
        current
    }

    /// Find a layer by path; see [`find_layer`](Self::find_layer).
    pub fn layer_by_path(&self, path: &str) -> Option<&Layer<T>> {
        self.find_layer(path).and_then(|id| self.layer(id))
    }

    /// Use `compression` for every channel of every layer (upstream
    /// `set_compression`): sets the document default and clears the
    /// per-layer overrides so it applies everywhere. `None` restores the
    /// automatic choice. Raw-backed channels with a different codec must be
    /// decoded before writing with the new setting.
    pub fn set_compression(&mut self, compression: Option<Compression>) {
        self.compression = compression;
        for layer in self.layers.iter_mut().flatten() {
            layer.compression = None;
            layer.mask_compression = None;
        }
    }

    /// Whether a layer's bounds cover the whole canvas (Photoshop's
    /// Background form for the bottom record).
    fn covers_canvas(&self, layer: &Layer<T>) -> bool {
        let bounds = layer.bounds;
        bounds.top == 0
            && bounds.left == 0
            && bounds.right == self.width as i32
            && bounds.bottom == self.height as i32
    }

    /// Whether any image layer carries an alpha channel.
    pub fn has_alpha(&self) -> bool {
        self.layers().any(|layer| match &layer.kind {
            LayerKind::Image(image) => image.channels.contains(ChannelKey::ALPHA),
            LayerKind::Text(text) => text.channels.contains(ChannelKey::ALPHA),
            LayerKind::Adjustment(adjustment) => adjustment.channels.contains(ChannelKey::ALPHA),
            LayerKind::Shape(shape) => shape.channels.contains(ChannelKey::ALPHA),
            _ => false,
        })
    }

    /// Borrowed views over the document's `lnk2`/`lnkD`/`lnkE`/`lnk3` records.
    /// Prefer this over [`linked_layers`](Self::linked_layers) when the
    /// payloads are only read: it borrows the block bytes instead of cloning
    /// every record's embedded data.
    pub fn linked_layer_views(&self) -> Result<Vec<psd_core::LinkedLayerView<'_>>> {
        let mut out = Vec::new();
        if let Some(ali) = &self.document_blocks {
            let link_keys = [*b"lnk2", *b"lnkD", *b"lnkE", *b"lnk3"];
            for block in &ali.blocks {
                if link_keys.contains(&block.key.as_bytes()) {
                    out.extend(psd_core::LinkedLayerTaggedBlock::read_views(&block.data)?);
                }
            }
        }
        Ok(out)
    }

    /// All linked-file records of the document's `lnk2`/`lnkD`/`lnkE`/`lnk3`
    /// blocks. A smart-object layer's [`Layer::placed_layer`] uuid references
    /// one of these records. This materializes owned records; see
    /// [`linked_layer_views`](Self::linked_layer_views) for the borrowing form.
    pub fn linked_layers(&self) -> Result<Vec<psd_core::LinkedLayer>> {
        Ok(self
            .linked_layer_views()?
            .into_iter()
            .map(psd_core::LinkedLayer::from)
            .collect())
    }

    /// The work path and saved paths stored in the image resources, in
    /// resource order, parsed on demand. When the document's `pths` block
    /// lists exactly one name per saved path, each saved path also carries
    /// its Unicode name. The resources stay the write source.
    pub fn document_paths(&self) -> Result<Vec<psd_core::vector::DocumentPath>> {
        use psd_core::vector::{
            DocumentPath, PathNames, SAVED_PATH_RESOURCE_IDS, WORK_PATH_RESOURCE_ID,
        };
        let mut paths = Vec::new();
        for block in self.image_resources.blocks() {
            let psd_core::ResourceBlock::Raw(raw) = block else {
                continue;
            };
            if raw.id == WORK_PATH_RESOURCE_ID || SAVED_PATH_RESOURCE_IDS.contains(&raw.id) {
                paths.push(DocumentPath {
                    resource_id: raw.id,
                    name: raw.name.value().to_owned(),
                    unicode_name: None,
                    path: psd_core::VectorPath::read(&raw.data)?,
                });
            }
        }
        let names = self
            .document_blocks
            .as_ref()
            .and_then(|blocks| blocks.get(TaggedBlockKey::new(*b"pths")))
            .map(|block| PathNames::read(&block.data))
            .transpose()?;
        if let Some(names) = names {
            let names = names.names();
            let mut saved: Vec<_> = paths
                .iter_mut()
                .filter(|path| !path.is_work_path())
                .collect();
            if names.len() == saved.len() {
                for (path, name) in saved.iter_mut().zip(names) {
                    path.unicode_name = name.map(str::to_owned);
                }
            }
        }
        Ok(paths)
    }

    /// The document's artboard tool settings (the document-level `artd`
    /// block), parsed on demand.
    pub fn artboard_settings(&self) -> Result<Option<psd_core::ArtboardSettings>> {
        self.document_blocks
            .as_ref()
            .and_then(|blocks| blocks.get(TaggedBlockKey::new(*b"artd")))
            .map(|block| psd_core::ArtboardSettings::read(&block.data))
            .transpose()
    }

    /// Ids of the artboard groups, in arena (id) order.
    pub fn artboards(&self) -> Vec<LayerId> {
        self.layers_with_ids()
            .filter(|(_, layer)| layer.is_artboard())
            .map(|(id, _)| id)
            .collect()
    }

    /// Layers in on-disk record order (groups expanded in place, group
    /// records after their contents, section dividers where they appear).
    pub fn flatten(&self) -> Vec<LayerId> {
        let mut out = Vec::with_capacity(self.layers.len());
        self.flatten_children(&self.root_children, &mut out);
        out
    }

    fn flatten_children(&self, children: &[LayerId], out: &mut Vec<LayerId>) {
        for &id in children {
            match &self.slot(id).kind {
                LayerKind::Group(group) => {
                    self.flatten_children(&group.children, out);
                    out.push(id);
                }
                _ => out.push(id),
            }
        }
    }

    // ------------------------------------------------------------------
    // Writing
    // ------------------------------------------------------------------

    /// Assemble the raw-format representation.
    pub fn to_photoshop_file(&self) -> Result<PhotoshopFile<'_>> {
        self.to_photoshop_file_with_progress(&mut ignore_progress)
    }

    /// Assemble the raw-format representation, reporting one
    /// [`ProgressEvent`] per emitted record.
    pub fn to_photoshop_file_with_progress(
        &self,
        progress: &mut dyn FnMut(ProgressEvent<'_>),
    ) -> Result<PhotoshopFile<'_>> {
        // The merged channel count is computed at write time: created
        // documents start at the color-mode count and would otherwise never
        // pick up alpha layers (upstream's `hasAlpha &=` latent bug; upstream
        // computes this dynamically). The max keeps read documents with extra
        // channels at their file's count.
        let color_channels = color_channel_count(self.color_mode);
        // A pixel record with no transparency channel gains a synthesized one
        // on write unless it is the canvas-covering bottom record (Photoshop's
        // Background form), so the document's channel count has to include the
        // plane those records will carry.
        let bottom_id = self.root_children.first().copied();
        // An authored empty layerless document is transparent. With no layer
        // record there is no negative-zero layer count to mark merged alpha,
        // so carry an explicit zero alpha plane in the merged header instead.
        let empty_layerless_placeholder =
            self.root_children.is_empty() && self.stored_merged_image.is_none();
        let synthesizes_alpha = self.layers_with_ids().any(|(id, layer)| {
            matches!(layer.kind, LayerKind::Image(_) | LayerKind::Text(_))
                && !layer
                    .channels()
                    .is_some_and(|channels| channels.contains(ChannelKey::ALPHA))
                && !(bottom_id == Some(id) && self.covers_canvas(layer))
        });
        let num_channels = self.num_channels.max(
            color_channels
                + u16::from(self.has_alpha() || self.has_merged_alpha || synthesizes_alpha)
                + u16::from(empty_layerless_placeholder),
        );
        // Photoshop reads an extra composite channel as a saved alpha channel
        // ("Alpha 1") unless the layer count is negative, which marks it as the
        // merged transparency. When the channel exists only because layers are
        // transparent, flag it so created documents do not gain a phantom
        // alpha channel; read files keep their own flag and saved channels.
        let has_merged_alpha = self.has_merged_alpha
            || empty_layerless_placeholder
            || ((self.has_alpha() || synthesizes_alpha) && self.num_channels <= color_channels);
        // A layerless 1-bit source can retain and write its original packed
        // merged section. Once materialized as an 8-bit background layer, the
        // normal document-depth path below writes the expanded samples.
        let output_depth = self
            .stored_merged_image
            .as_ref()
            .filter(|merged| self.root_children.is_empty() && merged.depth == CoreBitDepth::One)
            .map_or_else(depth_enum::<T>, |_| CoreBitDepth::One);
        let header = FileHeader::new(
            self.version,
            num_channels,
            self.width,
            self.height,
            output_depth,
            self.color_mode,
        )?;

        if self.stored_merged_image.is_some() && !self.root_children.is_empty() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "materialize a layerless image before saving its added layers",
            });
        }

        let mut image_data = ImageData::new(num_channels);
        if self.root_children.is_empty() {
            if let Some(merged) = &self.stored_merged_image {
                let matches_source = merged.version == header.version
                    && merged.width == header.width
                    && merged.height == header.height
                    && merged.depth == header.depth
                    && merged.color_mode == header.color_mode
                    && merged.channels == header.num_channels;
                if !matches_source {
                    return Err(PsdError::InvalidData {
                        offset: 0,
                        message: "the retained layerless image no longer matches the document header; materialize it before saving",
                    });
                }
                image_data.set_raw_section(Some(merged.section().to_vec()));
            }
        }

        let mut plan = Vec::new();
        self.plan_records(&self.root_children, &mut plan);
        let total = plan.len();
        let mut records = Vec::with_capacity(total);
        let mut channel_data = Vec::with_capacity(total);
        let jobs = plan
            .into_iter()
            .enumerate()
            .map(|(index, layer)| {
                let mut scratch = layer.and_then(Layer::channels).map_or(0, |channels| {
                    channels
                        .iter()
                        .map(|(_, samples)| samples.len().saturating_mul(T::SIZE).saturating_mul(2))
                        .max()
                        .unwrap_or(0)
                });
                if let Some(layer) = layer.filter(|layer| {
                    matches!(layer.kind, LayerKind::Image(_) | LayerKind::Text(_))
                        && !layer
                            .channels()
                            .is_some_and(|channels| channels.contains(ChannelKey::ALPHA))
                        && !(index == 0 && self.covers_canvas(layer))
                }) {
                    let (width, height) = read_rect_extents(layer.bounds, "layer", self.version)?;
                    // Synthesized transparency is temporary storage, separate
                    // from the channels already owned by the document.
                    scratch = scratch.saturating_add(
                        width
                            .saturating_mul(height)
                            .saturating_mul(std::mem::size_of::<T>()),
                    );
                }
                Ok(((index, layer), scratch))
            })
            .collect::<Result<Vec<_>>>()?;
        crate::parallel::for_each_ordered(
            jobs,
            |(index, layer)| {
                let (record, data) = match layer {
                    Some(layer) => {
                        self.build_record(layer, self.blocks_for_record(layer)?, index == 0)?
                    }
                    None => self.build_divider_record()?,
                };
                Ok((layer, record, data))
            },
            |(layer, record, data)| {
                progress(ProgressEvent::Layer {
                    name: layer.map_or(DIVIDER_NAME, |layer| layer.name.as_str()),
                    index: records.len(),
                    total,
                });
                records.push(record);
                channel_data.push(data);
                Ok(())
            },
        )?;

        // Refresh DPI/ICC while preserving every other resource block. The
        // resolution resource is the authority for a document read from disk: it
        // is rewritten only when the scalar `dpi` was changed, so unequal X/Y
        // resolutions, non-inch units, and the exact payload all survive an
        // unedited save. Setting `dpi` sets both axes to that many pixels per
        // inch, which is what the scalar means.
        let mut image_resources = self.image_resources.clone();
        let existing_dpi = image_resources
            .resolution_info()
            .map(|info| info.horizontal_resolution.to_f32());
        match existing_dpi {
            // The resource already says what `dpi` says: leave it, bytes and all.
            Some(resolution) if resolution == self.dpi => {}
            // The file had no resolution resource and nobody changed `dpi`: keep it
            // that way, since adding the block would change a file that round-tripped
            // without it. The format's default is 72 ppi either way.
            None if self.dpi == DEFAULT_DPI => {}
            _ => image_resources.set_resolution_info(ResolutionInfoBlock::new(self.dpi)),
        }
        if self.icc_profile.is_empty() {
            image_resources.remove_icc_profile();
        } else {
            image_resources.set_icc_profile(IccProfileBlock::new(self.icc_profile.clone()));
        }

        Ok(PhotoshopFile {
            header,
            color_mode_data: self.color_mode_data.clone(),
            image_resources,
            layer_and_mask_info: LayerAndMaskInformation {
                layer_info: LayerInfo {
                    layer_records: records,
                    channel_image_data: channel_data,
                    has_merged_alpha,
                },
                global_layer_mask_info: self.global_layer_mask_info.clone(),
                additional_layer_info: self.document_blocks_for_output(),
            },
            image_data,
        })
    }

    /// Serialize the whole document.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.to_bytes_with_progress(&mut ignore_progress)
    }

    /// Serialize the whole document, reporting one [`ProgressEvent`] per layer.
    pub fn to_bytes_with_progress(
        &self,
        progress: &mut dyn FnMut(ProgressEvent<'_>),
    ) -> Result<Vec<u8>> {
        let mut file = self.to_photoshop_file_with_progress(progress)?;
        // Streaming into a buffer of the right size: the file is assembled in
        // place, not grown by doubling (which briefly holds two copies).
        let mut bytes = Vec::with_capacity(file.size_hint());
        file.write_to(&mut bytes)?;
        Ok(bytes)
    }

    /// Write the document to disk.
    ///
    /// The file is streamed out channel by channel, so writing needs memory for
    /// the compressed channels but not for a second, whole-file copy of them.
    /// The bytes go to a sibling temporary file that replaces the target only
    /// once the whole document is written: a failure part way leaves whatever
    /// was at the path untouched, which matters when a document is saved over
    /// its own source.
    pub fn write(&self, path: impl AsRef<Path>) -> Result<()> {
        self.write_with_progress(path, &mut ignore_progress)
    }

    /// Write the document to disk, reporting one [`ProgressEvent`] per layer.
    pub fn write_with_progress(
        &self,
        path: impl AsRef<Path>,
        progress: &mut dyn FnMut(ProgressEvent<'_>),
    ) -> Result<()> {
        use std::io::Write as _;
        let path = path.as_ref();
        // Compression can fail; it happens before any file is created.
        let mut file = self.to_photoshop_file_with_progress(progress)?;
        let temporary = temporary_sibling(path)?;
        // Exclusive creation leaves an existing temporary file untouched. If
        // it fails, nothing created by this save needs cleanup.
        let sink = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let written = (|| -> Result<()> {
            let mut sink = std::io::BufWriter::with_capacity(1 << 20, sink);
            file.write_to(&mut sink)?;
            sink.flush()?;
            Ok(())
        })();
        let written =
            written.and_then(|()| std::fs::rename(&temporary, path).map_err(PsdError::from));
        if written.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        written
    }

    /// Plan records in file order. `None` synthesizes a missing bounding
    /// divider; codec jobs can then run independently without changing the tree.
    fn plan_records<'a>(&'a self, children: &[LayerId], plan: &mut Vec<Option<&'a Layer<T>>>) {
        let mut previous_was_divider = false;
        for &id in children {
            let layer = self.slot(id);
            if let LayerKind::Group(group) = &layer.kind {
                if !previous_was_divider {
                    plan.push(None);
                }
                self.plan_records(&group.children, plan);
            }
            plan.push(Some(layer));
            previous_was_divider = matches!(layer.kind, LayerKind::SectionDivider(_));
        }
    }

    /// A record's tagged blocks: borrowed from the layer as-is when they
    /// already describe it, otherwise an owned clone with the derived blocks
    /// refreshed. Borrowing avoids deep-copying every `TySh`/`SoLd`/`PlLd`
    /// payload per layer per save.
    ///
    /// Derived blocks:
    /// - `luni` always carries the current name. The pascal record name is
    ///   lossy, and a stale `luni` would silently undo a rename on the next
    ///   read. It is written like Photoshop does (no trailing null; upstream
    ///   appends one).
    /// - Groups' `lsct` carries the open/closed type and the effective blend
    ///   mode; section dividers get one when missing.
    fn blocks_for_record<'a>(
        &'a self,
        layer: &'a Layer<T>,
    ) -> Result<Option<Cow<'a, AdditionalLayerInfo>>> {
        if matches!(layer.kind, LayerKind::Shape(_)) && !layer.has_shape_markers() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "a shape layer needs both a vector mask and a fill block",
            });
        }
        let mut owned: Option<AdditionalLayerInfo> = None;

        // Read tolerantly: an unpadded block (some writers leave them so;
        // Photoshop pads) fails a padding-4 read, which used to look like a
        // rename and rewrote the block on every save.
        let current_name = layer
            .blocks
            .get(TaggedBlockKey::LUNI)
            .and_then(|block| UnicodeString::read(&mut BeReader::new(&block.data), 1).ok());
        // An existing `luni` is refreshed when the name has changed. A layer whose
        // name lives only in the legacy pascal record keeps it there: the record
        // carries the name, so adding the block changes a file that round-tripped
        // without it. The block is added only when the record cannot carry the
        // name itself — a character outside Windows-1252, or a payload past the
        // one-byte length marker — where the pascal string alone would corrupt it.
        let needs_name_block = match current_name.as_ref() {
            Some(current) => current.value() != layer.name,
            None => !layer.name.is_empty() && !PascalString::fits(&layer.name, 4),
        };
        if needs_name_block {
            let mut writer = BeWriter::new();
            UnicodeString::new(layer.name.as_str(), 4)?.write_verbatim(&mut writer)?;
            upsert_block(
                owned.get_or_insert_with(|| layer.blocks.clone()),
                TaggedBlockKey::LUNI,
                writer.into_inner(),
            );
        }

        match &layer.kind {
            LayerKind::Group(group) => {
                let kind = if group.open {
                    SectionDivider::OpenFolder
                } else {
                    SectionDivider::ClosedFolder
                };
                let blend = layer.blend_mode.as_bytes();
                let current = layer.blocks.get(TaggedBlockKey::LSCT).map(|b| &b.data);
                let wanted = match current {
                    Some(data) if data.len() >= LSCT_BLEND_KEY.end => {
                        let mut data = data.clone();
                        data[..4].copy_from_slice(&kind.as_raw().to_be_bytes());
                        data[LSCT_BLEND_KEY].copy_from_slice(&blend);
                        data
                    }
                    // A short `lsct` has no blend key, so the record's mode
                    // is the one that applies. Keep the short form: a
                    // pass-through group then keeps `pass` on the record
                    // (Photoshop writes that spelling when the `lsct` is
                    // type-only), and any other mode is already on the record.
                    Some(data)
                        if layer.blend_mode != BlendMode::PASSTHROUGH
                            || data.len() < LSCT_BLEND_KEY.end =>
                    {
                        let mut data = data.clone();
                        let len = data.len().min(4);
                        data[..len].copy_from_slice(&kind.as_raw().to_be_bytes()[..len]);
                        data
                    }
                    _ => {
                        let mut data = Vec::with_capacity(12);
                        data.extend_from_slice(&kind.as_raw().to_be_bytes());
                        data.extend_from_slice(b"8BIM");
                        data.extend_from_slice(&blend);
                        data
                    }
                };
                if current != Some(&wanted) {
                    upsert_block(
                        owned.get_or_insert_with(|| layer.blocks.clone()),
                        TaggedBlockKey::LSCT,
                        wanted,
                    );
                }
            }
            LayerKind::SectionDivider(divider)
                if layer.blocks.get(TaggedBlockKey::LSCT).is_none() =>
            {
                owned
                    .get_or_insert_with(|| layer.blocks.clone())
                    .push(TaggedBlock::new(
                        TaggedBlockKey::LSCT,
                        divider.as_raw().to_be_bytes().to_vec(),
                    ));
            }
            _ => {}
        }

        Ok(match owned {
            Some(blocks) => Some(Cow::Owned(blocks)),
            None if layer.blocks.blocks.is_empty() => None,
            None => Some(Cow::Borrowed(&layer.blocks)),
        })
    }

    fn build_divider_record(&self) -> Result<(LayerRecord<'static>, ChannelImageData<'static>)> {
        // The bounding-divider record is fixed-shape; build it directly with
        // owned blocks so no borrow of a temporary layer escapes.
        let bounds = Rect::default();
        let mut keys = vec![ChannelKey::ALPHA];
        keys.extend((0..color_channel_count(self.color_mode)).map(|i| ChannelKey::color(i as u8)));
        let mut channels = Vec::with_capacity(keys.len());
        let mut data = Vec::with_capacity(keys.len());
        for key in keys {
            let (compression, payload) =
                compress_channel::<T>(&[], 0, 0, self.version, self.compression)?;
            channels.push(ChannelInfo {
                id: CoreChannelId::from_index(key.index(), self.color_mode),
                index: key.index(),
                size: payload.len() as u64 + 2,
            });
            data.push(ChannelData {
                compression,
                data: payload.into(),
            });
        }
        // Photoshop names the divider in `luni` as well (upstream writes an
        // empty `luni`). Without it, re-saving a read document added the block
        // the first save had left out, so the two saves differed.
        let mut name = BeWriter::new();
        UnicodeString::new(DIVIDER_NAME, 4)?.write_verbatim(&mut name)?;
        let mut blocks = AdditionalLayerInfo::new();
        blocks.push(TaggedBlock::new(TaggedBlockKey::LUNI, name.into_inner()));
        blocks.push(TaggedBlock::new(
            TaggedBlockKey::LSCT,
            SectionDivider::BoundingSection
                .as_raw()
                .to_be_bytes()
                .to_vec(),
        ));
        let record = LayerRecord {
            name: PascalString::new(DIVIDER_NAME, 4),
            top: bounds.top,
            left: bounds.left,
            bottom: bounds.bottom,
            right: bounds.right,
            channels,
            blend_mode: BlendMode::NORMAL,
            opacity: 255,
            clipping: 0,
            flags: psd_core::LayerFlags::from_bits(
                psd_core::LayerFlags::BIT4_USEFUL | psd_core::LayerFlags::PIXEL_DATA_IRRELEVANT,
            ),
            mask_data: None,
            blending_ranges: Default::default(),
            additional_layer_info: Some(Cow::Owned(blocks)),
        };
        Ok((record, ChannelImageData { channels: data }))
    }

    fn build_record<'a>(
        &'a self,
        layer: &'a Layer<T>,
        blocks: Option<Cow<'a, AdditionalLayerInfo>>,
        is_bottom_record: bool,
    ) -> Result<(LayerRecord<'a>, ChannelImageData<'a>)> {
        let bounds = layer.bounds;
        let stub_channels = matches!(
            layer.kind,
            LayerKind::Group(_) | LayerKind::SectionDivider(_)
        );
        let mut keys: Vec<ChannelKey> = match &layer.kind {
            LayerKind::Image(image) => image.channels.keys().collect(),
            LayerKind::Text(text) => text.channels.keys().collect(),
            LayerKind::Adjustment(adjustment) => adjustment.channels.keys().collect(),
            LayerKind::Shape(shape) => shape.channels.keys().collect(),
            // Groups and section dividers carry empty stub channels on disk;
            // groups add their mask channels.
            LayerKind::Group(_) | LayerKind::SectionDivider(_) => {
                let mut keys = vec![ChannelKey::ALPHA];
                keys.extend(
                    (0..color_channel_count(self.color_mode)).map(|i| ChannelKey::color(i as u8)),
                );
                if let LayerKind::Group(group) = &layer.kind {
                    keys.extend(group.channels.keys().filter(|key| key.is_mask()));
                }
                keys
            }
        };
        // Photoshop reads a pixel record without a transparency channel as its
        // Background layer: opaque over the WHOLE canvas whatever the record
        // bounds say, hiding everything beneath it. It omits the channel only
        // for the bottom record covering exactly the canvas, so an opaque
        // pixel record anywhere else gets an all-opaque transparency channel.
        let synthesize_alpha = matches!(layer.kind, LayerKind::Image(_) | LayerKind::Text(_))
            && !keys.contains(&ChannelKey::ALPHA)
            && !(is_bottom_record && self.covers_canvas(layer));
        if synthesize_alpha {
            keys.push(ChannelKey::ALPHA);
        }
        keys.sort_by_key(|key| channel_sort_key(*key));
        let opaque_alpha = if synthesize_alpha {
            let (width, height) = read_rect_extents(bounds, "layer", self.version)?;
            let samples = width
                .checked_mul(height)
                .filter(|&samples| {
                    samples
                        .checked_mul(std::mem::size_of::<T>())
                        .is_some_and(|bytes| bytes <= isize::MAX as usize)
                })
                .ok_or(PsdError::InvalidImageBounds {
                    kind: "layer",
                    width: i64::from(bounds.right) - i64::from(bounds.left),
                    height: i64::from(bounds.bottom) - i64::from(bounds.top),
                })?;
            vec![T::from_f32(1.0); samples]
        } else {
            Vec::new()
        };

        let mut channels = Vec::new();
        let mut data = Vec::new();
        for key in keys {
            let rect = if key.is_mask() {
                mask_channel_rect(layer.mask.as_ref(), key).unwrap_or(bounds)
            } else if stub_channels {
                // Stubs stay empty whatever the record bounds say; Photoshop
                // and upstream write groups with mask-derived bounds this way.
                Rect::default()
            } else {
                bounds
            };
            let (compression, payload) =
                if let Some(raw) = layer.channels().and_then(|channels| channels.raw(key)) {
                    // Preserve the encoded payload until a caller decodes or
                    // replaces it.
                    let (width, height) = if key.is_mask() {
                        let kind = if key == ChannelKey::REAL_USER_MASK {
                            "real mask"
                        } else {
                            "mask"
                        };
                        mask_channel_extents(rect, kind, self.version, raw.payload.is_empty())?
                    } else if stub_channels {
                        (0, 0)
                    } else {
                        read_rect_extents(rect, "layer", self.version)?
                    };
                    if (raw.width, raw.height) != (width, height) {
                        return Err(PsdError::InvalidData {
                            offset: 0,
                            message: "decode raw channel data before changing its geometry",
                        });
                    }
                    // PhotoshopAPI/src/PhotoshopFile/LayerAndMaskInformation.cpp
                    // converts plain ZIP to ZIP prediction for 32-bit channels.
                    // Raw passthrough must keep that write-time requirement.
                    if T::DEPTH == 32 && raw.compression == Compression::Zip {
                        return Err(PsdError::InvalidData {
                            offset: 0,
                            message: "decode raw 32-bit ZIP channels before writing",
                        });
                    }
                    if raw.compression == Compression::Rle && raw.version != self.version {
                        return Err(PsdError::InvalidData {
                            offset: 0,
                            message: "decode raw RLE channels before changing PSD/PSB version",
                        });
                    }
                    let requested_compression =
                        layer.write_compression(key, self.compression).map(|codec| {
                            if T::DEPTH == 32 && codec == Compression::Zip {
                                Compression::ZipPrediction
                            } else {
                                codec
                            }
                        });
                    if requested_compression.is_some_and(|codec| codec != raw.compression) {
                        return Err(PsdError::InvalidData {
                            offset: 0,
                            message: "decode raw channels before changing their compression",
                        });
                    }
                    (raw.compression, Cow::Borrowed(raw.payload.as_slice()))
                } else {
                    let samples: &[T] = if key == ChannelKey::ALPHA && synthesize_alpha {
                        &opaque_alpha
                    } else {
                        layer
                            .channels()
                            .and_then(|channels| channels.get(key))
                            .unwrap_or(&[])
                    };
                    let (compression, payload) = compress_channel(
                        samples,
                        rect.width().max(0) as usize,
                        rect.height().max(0) as usize,
                        self.version,
                        layer.write_compression(key, self.compression),
                    )?;
                    (compression, Cow::Owned(payload))
                };
            channels.push(ChannelInfo {
                id: CoreChannelId::from_index(key.index(), self.color_mode),
                index: key.index(),
                size: payload.len() as u64 + 2,
            });
            data.push(ChannelData {
                compression,
                data: payload,
            });
        }

        // Pass-through is spelled one of two ways in the wild: `pass` on the
        // record when the group's `lsct` is the short, type-only form, or
        // `norm` on the record with `pass` on the `lsct`'s blend key. The
        // layer's own `lsct` says which one this document uses, and the
        // spelling is preserved rather than normalized.
        let blend_mode = match layer.kind {
            LayerKind::Group(_) if layer.blend_mode == BlendMode::PASSTHROUGH => {
                let short_lsct = blocks
                    .as_ref()
                    .and_then(|blocks| blocks.get(TaggedBlockKey::LSCT))
                    .is_some_and(|block| block.data.len() < LSCT_BLEND_KEY.end);
                if short_lsct {
                    BlendMode::PASSTHROUGH
                } else {
                    BlendMode::NORMAL
                }
            }
            _ => layer.blend_mode,
        };
        let record = LayerRecord {
            name: PascalString::new(layer.name.as_str(), 4),
            top: bounds.top,
            left: bounds.left,
            bottom: bounds.bottom,
            right: bounds.right,
            channels,
            blend_mode,
            opacity: layer.opacity,
            clipping: layer.clipping,
            flags: layer.flags,
            mask_data: layer.mask.clone(),
            blending_ranges: layer.blending_ranges.clone(),
            additional_layer_info: blocks,
        };
        Ok((record, ChannelImageData { channels: data }))
    }

    /// The patterns the document carries, from its `Patt`, `Pat2` and `Pat3`
    /// blocks, as RGBA8 tiles keyed by their id (the `Idnt` a pattern overlay,
    /// pattern fill or bevel texture refers to).
    ///
    /// A record that cannot be decoded ends that block's list with a warning:
    /// the patterns before it are still returned.
    pub fn patterns(&self) -> Vec<psd_core::pattern::Pattern> {
        let mut patterns = Vec::new();
        let Some(blocks) = &self.document_blocks else {
            return patterns;
        };
        for block in &blocks.blocks {
            let bytes = block.key.as_bytes();
            if !matches!(&bytes, b"Patt" | b"Pat2" | b"Pat3") {
                continue;
            }
            let mut reader = psd_core::io::BeReader::new(&block.data);
            // A record is at least its length prefix and version.
            while reader.remaining() >= 8 {
                match psd_core::pattern::read_pattern(&mut reader) {
                    Ok(pattern) => patterns.push(pattern),
                    Err(error) => {
                        tracing::warn!("unreadable pattern record skipped: {error}");
                        break;
                    }
                }
            }
        }
        patterns
    }
}

/// A pixel delta in the 8.24 fixed-point unit path coordinates use — a fraction
/// of the document extent.
fn fraction_delta(pixels: i32, extent: u32) -> Result<i32> {
    if extent == 0 {
        return Ok(0);
    }
    let fraction = f64::from(pixels) / f64::from(extent) * f64::from(1 << 24);
    if !fraction.is_finite() || fraction.abs() > f64::from(i32::MAX) {
        return Err(PsdError::InvalidData {
            offset: 0,
            message: "layer move is outside the path coordinate range",
        });
    }
    Ok(fraction.round() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesized_alpha_rejects_extreme_coordinates_before_allocation() {
        for bounds in [
            Rect::new(i32::MIN, i32::MIN, i32::MAX, i32::MAX),
            Rect::new(0, 0, -1, -1),
        ] {
            let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 1, 1).unwrap();
            document.add_layer(Layer::new_image("Invalid", bounds));
            assert!(matches!(
                document.to_bytes(),
                Err(PsdError::InvalidImageBounds { .. })
            ));
        }
    }

    fn layer_with_pixel(value: u8) -> Layer<u8> {
        let mut layer = Layer::new_image("Layer", Rect::new(0, 0, 4, 4));
        let image = layer.image_mut().unwrap();
        image.set_channel(ChannelKey::color(0), vec![value; 16]);
        image.set_channel(ChannelKey::color(1), vec![value; 16]);
        image.set_channel(ChannelKey::color(2), vec![value; 16]);
        layer
    }

    /// A name the pascal record can carry needs no `luni`: adding one would change
    /// the bytes of every legacy file that round-tripped without it, and a second
    /// save would differ from the first. A name the record cannot carry still gets
    /// the block, so the name survives.
    #[test]
    fn a_pascal_only_name_gains_a_unicode_block_only_when_it_must() {
        let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 4, 4).unwrap();
        let mut layer = layer_with_pixel(10);
        layer.name = "Layer".to_owned();
        document.add_layer(layer);
        let mut unicode = layer_with_pixel(20);
        unicode.name = "名前 – ü".to_owned();
        document.add_layer(unicode);

        let back = LayeredFile::<u8>::from_bytes(&document.to_bytes().unwrap()).unwrap();
        let plain = back.layer_by_path("Layer").unwrap();
        assert_eq!(plain.name, "Layer");
        assert!(
            plain.blocks.get(TaggedBlockKey::LUNI).is_none(),
            "a representable name must not gain a luni block"
        );
        assert_eq!(back.layer_by_path("名前 – ü").unwrap().name, "名前 – ü");
        assert!(
            back.layer_by_path("名前 – ü")
                .unwrap()
                .blocks
                .get(TaggedBlockKey::LUNI)
                .is_some(),
            "a name outside the record's codepage needs the unicode block"
        );

        // The second save is identical to the first: nothing is added later either.
        let again = LayeredFile::<u8>::from_bytes(&back.to_bytes().unwrap()).unwrap();
        assert!(again
            .layer_by_path("Layer")
            .unwrap()
            .blocks
            .get(TaggedBlockKey::LUNI)
            .is_none());
        assert_eq!(again.to_bytes().unwrap(), back.to_bytes().unwrap());
    }

    #[test]
    fn synthetic_document_round_trips() {
        let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 4, 4).unwrap();
        document.add_layer(layer_with_pixel(10));
        let group = document.add_layer(Layer::new_group("Group"));
        document
            .add_layer_to_group(group, layer_with_pixel(20))
            .unwrap();
        document.dpi = 300.0;

        let bytes = document.to_bytes().unwrap();
        let back = LayeredFile::<u8>::from_bytes(&bytes).unwrap();

        assert_eq!(back.width, 4);
        assert_eq!(back.dpi, 300.0);
        // Layer 10, a synthesized divider, layer 20, the group.
        assert_eq!(back.layer_count(), 4);
        assert_eq!(
            back.layer_by_path("Layer")
                .unwrap()
                .image()
                .unwrap()
                .channel(ChannelKey::color(0)),
            Some([10u8; 16].as_slice())
        );
        assert_eq!(
            back.layer_by_path("Group/Layer")
                .unwrap()
                .image()
                .unwrap()
                .channel(ChannelKey::color(0)),
            Some([20u8; 16].as_slice())
        );
        // Group records are written after their contents, preceded by the
        // synthesized section divider.
        let flat = back.flatten();
        let names: Vec<&str> = flat
            .iter()
            .map(|&id| back.layer(id).unwrap().name.as_str())
            .collect();
        assert_eq!(names, vec!["Layer", "</Layer group>", "Layer", "Group"]);
    }

    #[test]
    fn external_source_path_is_runtime_context_not_document_content() {
        let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 8, 8).unwrap();
        document.add_layer(Layer::new_image("Image", Rect::new(0, 0, 8, 8)));
        let bytes = document.to_bytes().unwrap();
        let without_path = LayeredFile::<u8>::from_bytes(&bytes).unwrap();
        let with_path =
            LayeredFile::<u8>::from_bytes_with_source_path(&bytes, "folder/document.psd").unwrap();
        assert_eq!(without_path, with_path);
        assert!(with_path.source_path().unwrap().is_absolute());
    }

    #[test]
    fn progress_reports_every_record() {
        let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 4, 4).unwrap();
        document.add_layer(layer_with_pixel(1));
        let group = document.add_layer(Layer::new_group("Group"));
        document
            .add_layer_to_group(group, layer_with_pixel(2))
            .unwrap();

        let mut events = Vec::new();
        let bytes = document
            .to_bytes_with_progress(&mut |event| {
                let ProgressEvent::Layer { name, index, total } = event;
                events.push((name.to_string(), index, total));
            })
            .unwrap();
        assert_eq!(
            events,
            vec![
                ("Layer".to_string(), 0, 4),
                ("</Layer group>".to_string(), 1, 4),
                ("Layer".to_string(), 2, 4),
                ("Group".to_string(), 3, 4),
            ]
        );

        let mut read_events = Vec::new();
        LayeredFile::<u8>::from_bytes_with_progress(&bytes, &mut |event| {
            let ProgressEvent::Layer { name, index, total } = event;
            read_events.push((name.to_string(), index, total));
        })
        .unwrap();
        assert_eq!(read_events, events);
    }

    #[test]
    fn created_document_defaults() {
        let document = LayeredFile::<u16>::new(ColorMode::Cmyk, 64, 32).unwrap();
        assert_eq!(document.num_channels, 4);
        assert_eq!(document.dpi, 72.0);
        assert!(document.icc_profile.is_empty());
        assert_eq!(document.layer_count(), 0);
    }

    #[test]
    fn path_lookup_handles_nesting() {
        let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 4, 4).unwrap();
        let outer = document.add_layer(Layer::new_group("Outer"));
        let inner = document
            .add_layer_to_group(outer, Layer::new_group("Inner"))
            .unwrap();
        document
            .add_layer_to_group(inner, layer_with_pixel(1))
            .unwrap();

        assert!(document.find_layer("Outer/Inner/Layer").is_some());
        assert!(document.find_layer("Outer/Layer").is_none());
        assert!(document.find_layer("Missing").is_none());
    }

    #[test]
    fn add_layer_to_group_error_leaves_no_orphan() {
        let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 4, 4).unwrap();
        let image = document.add_layer(layer_with_pixel(1));
        // Targeting a non-group must fail *without* growing the arena.
        assert!(document
            .add_layer_to_group(image, layer_with_pixel(2))
            .is_err());
        assert_eq!(document.layer_count(), 1);
        assert!(document
            .add_layer_to_group(usize::MAX, layer_with_pixel(3))
            .is_err());
        assert_eq!(document.layer_count(), 1);
    }

    #[test]
    fn adding_alpha_to_a_created_document_widens_num_channels_on_write() {
        // Created documents start at the color-mode count; an alpha channel
        // added later must reach the written header (upstream's `hasAlpha &=`
        // latent bug).
        let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 4, 4).unwrap();
        assert_eq!(document.num_channels, 3);
        let mut layer = layer_with_pixel(10);
        layer
            .image_mut()
            .unwrap()
            .set_channel(ChannelKey::ALPHA, vec![255u8; 16]);
        document.add_layer(layer);

        let bytes = document.to_bytes().unwrap();
        let back = LayeredFile::<u8>::from_bytes(&bytes).unwrap();
        assert_eq!(back.num_channels, 4);
        assert!(back.has_alpha());
        // The widened channel is flagged as merged transparency (negative
        // layer count); unflagged, Photoshop would list it as "Alpha 1".
        assert!(back.has_merged_alpha);
    }

    #[test]
    fn saved_alpha_channels_are_not_reflagged_as_transparency() {
        // Photoshop's CMYK/Grayscale fixtures carry a real saved "Alpha 1"
        // channel with a positive layer count; a round trip must keep it a
        // saved channel rather than turning it into merged transparency.
        for name in ["CMYK/CMYK_8.psd", "Grayscale/Grayscale_8.psb"] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/documents")
                .join(name);
            let document = LayeredFile::<u8>::read(&path).unwrap();
            assert!(!document.has_merged_alpha, "{name}");
            let back = LayeredFile::<u8>::from_bytes(&document.to_bytes().unwrap()).unwrap();
            assert_eq!(back.num_channels, document.num_channels, "{name}");
            assert!(!back.has_merged_alpha, "{name}");
        }
    }

    #[test]
    fn duplicate_layer_channel_indices_are_rejected() {
        let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 2, 2).unwrap();
        let mut layer = Layer::new_image("Duplicate", Rect::new(0, 0, 2, 2));
        layer
            .image_mut()
            .unwrap()
            .set_channel(ChannelKey::color(0), vec![1; 4]);
        layer
            .image_mut()
            .unwrap()
            .set_channel(ChannelKey::color(1), vec![2; 4]);
        document.add_layer(layer);
        let mut file = document.to_photoshop_file().unwrap();
        let channels = &mut file.layer_and_mask_info.layer_info.layer_records[0].channels;
        let first = channels[0];
        channels[1].index = first.index;
        channels[1].id = first.id;
        let mut writer = BeWriter::new();
        file.write(&mut writer).unwrap();

        assert!(matches!(
            LayeredFile::<u8>::from_bytes(&writer.into_inner()),
            Err(PsdError::InvalidData {
                message: "duplicate layer channel index",
                ..
            })
        ));
    }

    #[test]
    fn raw_float_zip_requires_prediction_before_write() {
        let pixels = vec![0.0f32, 0.25, 0.5, 1.0];
        let payload =
            psd_codecs::zip::compress(&psd_codecs::endian::encode_be_bytes(&pixels)).unwrap();
        let mut document = LayeredFile::<f32>::new(ColorMode::Rgb, 2, 2).unwrap();
        let mut layer = Layer::new_image("Float", Rect::new(0, 0, 2, 2));
        layer.image_mut().unwrap().channels.insert_raw(
            ChannelKey::color(0),
            RawChannelData::new(Compression::Zip, payload, 2, 2, Version::Psd),
        );
        let id = document.add_layer(layer);

        assert!(matches!(
            document.to_bytes(),
            Err(PsdError::InvalidData {
                message: "decode raw 32-bit ZIP channels before writing",
                ..
            })
        ));
        assert!(document
            .layer(id)
            .unwrap()
            .image()
            .unwrap()
            .channels
            .is_raw(ChannelKey::color(0)));

        assert!(document
            .decode_layer_channel(id, ChannelKey::color(0))
            .unwrap());
        let bytes = document.to_bytes().unwrap();
        let file = PhotoshopFile::read(&mut BeReader::new(&bytes)).unwrap();
        assert_eq!(
            file.layer_and_mask_info.layer_info.channel_image_data[0].channels[0].compression,
            Compression::ZipPrediction
        );
        let reopened = LayeredFile::<f32>::from_bytes(&bytes).unwrap();
        assert_eq!(
            reopened
                .layer_by_path("Float")
                .unwrap()
                .image()
                .unwrap()
                .channel(ChannelKey::color(0)),
            Some(pixels.as_slice())
        );
    }
}
