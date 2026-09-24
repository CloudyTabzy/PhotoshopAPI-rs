//! `LayeredFile<T>`: the user-facing document.
//!
//! Reshapes upstream `LayeredFile.h` + `Impl/LayeredFileImpl.h` +
//! `Util/GenerateLayerMaskInfo.cpp`: an arena of layers,
//! index-addressed, with read/write driven by `psd-core` sections and
//! `psd-codecs` for channel payloads.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use psd_core::{
    AdditionalLayerInfo, BeReader, BeWriter, BitDepth as CoreBitDepth, BlendMode, ChannelData,
    ChannelId as CoreChannelId, ChannelImageData, ChannelInfo, ColorMode, ColorModeData,
    Compression, FileHeader, GlobalLayerMaskInfo, IccProfileBlock, ImageData, ImageResources,
    LayerAndMaskInformation, LayerInfo, LayerRecord, PascalString, PhotoshopFile, PsdError,
    ResolutionInfoBlock, Result, SectionDivider, TaggedBlock, TaggedBlockKey, UnicodeString,
    Version,
};

use crate::bitdepth::BitDepth;
use crate::channels::{compress_channel, decompress_channel, ChannelKey, ChannelStore};
use crate::layer::upsert_block;
use crate::layer::{GroupLayer, ImageLayer, Layer, LayerId, LayerKind, Rect, TextLayer};
use crate::progress::{ignore_progress, ProgressEvent};
use crate::text::TextCacheBaseline;

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
    /// Document resolution in dots per inch.
    pub dpi: f32,
    /// Raw ICC profile bytes (empty = no profile).
    pub icc_profile: Vec<u8>,
    /// Palette/toning data, preserved verbatim.
    pub color_mode_data: ColorModeData,
    /// Document image resources, preserved (DPI/ICC are refreshed on write).
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
    /// Slot arena: removed layers leave `None` so ids stay stable and are
    /// never reused (see [`LayerId`]).
    layers: Vec<Option<Layer<T>>>,
    root_children: Vec<LayerId>,
}

impl<T: BitDepth> PartialEq for LayeredFile<T> {
    fn eq(&self, other: &Self) -> bool {
        self.version == other.version
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
    }
}

impl<T: BitDepth> LayeredFile<T> {
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
        Ok(Self {
            version: Version::Psd,
            width,
            height,
            color_mode,
            num_channels: num_channels.max(1),
            dpi: 72.0,
            icc_profile: Vec::new(),
            color_mode_data: ColorModeData::default(),
            image_resources: ImageResources::new(),
            global_layer_mask_info: GlobalLayerMaskInfo::default(),
            document_blocks: None,
            has_merged_alpha: false,
            compression: None,
            source_path: None,
            text_cache: None,
            linked_sources: std::collections::HashMap::new(),
            layers: Vec::new(),
            root_children: Vec::new(),
        })
    }

    /// Read a document from disk (mmap-backed).
    pub fn read(path: impl AsRef<Path>) -> Result<Self> {
        Self::read_with_progress(path, &mut ignore_progress)
    }

    /// Read a document from disk, reporting one [`ProgressEvent`] per layer.
    pub fn read_with_progress(
        path: impl AsRef<Path>,
        progress: &mut dyn FnMut(ProgressEvent<'_>),
    ) -> Result<Self> {
        let path = path.as_ref();
        let file = std::fs::File::open(path)?;
        // SAFETY: the mapping outlives the parse call and every field of the
        // returned document owns its data — nothing borrows the mapping.
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        let mut document = Self::from_bytes_with_progress(&mmap, progress)?;
        document.source_path = Some(absolute_or_current_dir(path)?);
        Ok(document)
    }

    /// Read a document from memory.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        Self::from_bytes_with_progress(bytes, &mut ignore_progress)
    }

    /// Read a document from memory while retaining the path it came from as
    /// context for relative external smart-object links. The path is runtime
    /// metadata only and is not serialized into the PSD/PSB.
    pub fn from_bytes_with_source_path(
        bytes: &[u8],
        source_path: impl AsRef<Path>,
    ) -> Result<Self> {
        let mut document = Self::from_bytes(bytes)?;
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
        let mut reader = BeReader::new(bytes);
        let file = PhotoshopFile::read(&mut reader)?;
        Self::from_photoshop_file(file, progress)
    }

    fn from_photoshop_file(
        file: PhotoshopFile,
        progress: &mut dyn FnMut(ProgressEvent<'_>),
    ) -> Result<Self> {
        let header = file.header;
        if header.depth.as_raw() != T::DEPTH {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "file bit depth does not match the document type",
            });
        }

        let dpi = file
            .image_resources
            .resolution_info()
            .map(|info| info.horizontal_resolution.to_f32())
            .unwrap_or(72.0);
        let icc_profile = file
            .image_resources
            .icc_profile()
            .map(|icc| icc.data().to_vec())
            .unwrap_or_default();

        let mut document = Self {
            version: header.version,
            width: header.width,
            height: header.height,
            color_mode: header.color_mode,
            num_channels: header.num_channels,
            dpi,
            icc_profile,
            color_mode_data: file.color_mode_data,
            image_resources: file.image_resources,
            global_layer_mask_info: file.layer_and_mask_info.global_layer_mask_info,
            document_blocks: file
                .layer_and_mask_info
                .additional_layer_info
                .map(|blocks| blocks.into_owned()),
            has_merged_alpha: file.layer_and_mask_info.layer_info.has_merged_alpha,
            compression: None,
            source_path: None,
            text_cache: None,
            linked_sources: std::collections::HashMap::new(),
            layers: Vec::new(),
            root_children: Vec::new(),
        };
        document.build_layers(
            &file.layer_and_mask_info.layer_info,
            header.version,
            progress,
        )?;
        document.text_cache = TextCacheBaseline::capture(&document);
        Ok(document)
    }

    fn build_layers(
        &mut self,
        info: &LayerInfo,
        version: Version,
        progress: &mut dyn FnMut(ProgressEvent<'_>),
    ) -> Result<()> {
        if info.layer_records.len() != info.channel_image_data.len() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "layer records and channel image data count mismatch",
            });
        }

        let total = info.layer_records.len();
        let mut ids = Vec::with_capacity(total);
        for (index, (record, channel_data)) in info
            .layer_records
            .iter()
            .zip(&info.channel_image_data)
            .enumerate()
        {
            progress(ProgressEvent::Layer {
                name: record.name.value(),
                index,
                total,
            });
            ids.push(self.build_layer(record, channel_data, version)?);
        }

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
                LayerKind::Image(_) | LayerKind::Text(_) => parent[id] = stack.last().copied(),
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
        &mut self,
        record: &LayerRecord,
        channel_data: &ChannelImageData,
        version: Version,
    ) -> Result<LayerId> {
        let blocks = record
            .additional_layer_info
            .as_ref()
            .map(|ali| ali.as_ref().clone())
            .unwrap_or_default();

        // The unicode name ('luni') takes precedence over the pascal name.
        let mut name = record.name.value().to_string();
        if let Some(block) = blocks.get(TaggedBlockKey::LUNI) {
            let mut reader = BeReader::new(&block.data);
            if let Ok(unicode) = UnicodeString::read(&mut reader, 4) {
                name = unicode.value().to_string();
            }
        }

        let divider = blocks
            .get(TaggedBlockKey::LSCT)
            .and_then(|block| block.data.get(..4))
            .and_then(|bytes| {
                SectionDivider::from_raw(u32::from_be_bytes(bytes.try_into().unwrap()))
            });

        let has_text_metadata = blocks.get(TaggedBlockKey::new(*b"TySh")).is_some()
            || blocks.get(TaggedBlockKey::new(*b"Txt2")).is_some();

        if record.channels.len() != channel_data.channels.len() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "layer channel count mismatch",
            });
        }
        let layer_rect = Rect::new(record.top, record.left, record.bottom, record.right);
        let is_group = matches!(
            divider,
            Some(SectionDivider::OpenFolder | SectionDivider::ClosedFolder)
        );
        let decode_channels = |keep: fn(ChannelKey) -> bool| -> Result<ChannelStore<T>> {
            let mut channels = ChannelStore::new();
            for (info, channel) in record.channels.iter().zip(&channel_data.channels) {
                let key = ChannelKey(info.index);
                if !keep(key) {
                    continue;
                }
                let rect = if key.is_mask() {
                    mask_channel_rect(record.mask_data.as_ref(), key).unwrap_or(layer_rect)
                } else {
                    layer_rect
                };
                let samples = decompress_channel::<T>(
                    channel.compression,
                    &channel.data,
                    rect.width().max(0) as usize,
                    rect.height().max(0) as usize,
                    version,
                )?;
                channels.insert(key, samples);
            }
            Ok(channels)
        };

        let kind = match divider {
            Some(SectionDivider::BoundingSection) => {
                LayerKind::SectionDivider(SectionDivider::BoundingSection)
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

        self.layers.push(Some(Layer {
            name,
            bounds: layer_rect,
            opacity: record.opacity,
            blend_mode,
            flags: record.flags,
            clipping: record.clipping,
            mask: record.mask_data.clone(),
            blocks,
            blending_ranges: record.blending_ranges.clone(),
            kind,
            compression: None,
            mask_compression: None,
        }));
        Ok(self.layers.len() - 1)
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

    /// Append a layer at the top level.
    pub fn add_layer(&mut self, layer: Layer<T>) -> LayerId {
        self.layers.push(Some(layer));
        let id = self.layers.len() - 1;
        self.root_children.push(id);
        id
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
        self.layers.push(Some(layer));
        let id = self.layers.len() - 1;
        match &mut self.slot_mut(group).kind {
            LayerKind::Group(group) => group.children.push(id),
            _ => unreachable!("validated as a group above"),
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
    /// automatic choice.
    pub fn set_compression(&mut self, compression: Option<Compression>) {
        self.compression = compression;
        for layer in self.layers.iter_mut().flatten() {
            layer.compression = None;
            layer.mask_compression = None;
        }
    }

    /// Whether any image layer carries an alpha channel.
    pub fn has_alpha(&self) -> bool {
        self.layers().any(|layer| match &layer.kind {
            LayerKind::Image(image) => image.channels.contains(ChannelKey::ALPHA),
            LayerKind::Text(text) => text.channels.contains(ChannelKey::ALPHA),
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
        let num_channels = self
            .num_channels
            .max(color_channels + u16::from(self.has_alpha() || self.has_merged_alpha));
        // Photoshop reads an extra composite channel as a saved alpha channel
        // ("Alpha 1") unless the layer count is negative, which marks it as the
        // merged transparency. When the channel exists only because layers are
        // transparent, flag it so created documents do not gain a phantom
        // alpha channel; read files keep their own flag and saved channels.
        let has_merged_alpha =
            self.has_merged_alpha || (self.has_alpha() && self.num_channels <= color_channels);
        let header = FileHeader::new(
            self.version,
            num_channels,
            self.width,
            self.height,
            depth_enum::<T>(),
            self.color_mode,
        )?;

        let mut records = Vec::new();
        let mut channel_data = Vec::new();
        let total = self.count_records(&self.root_children);
        let mut index = 0;
        self.emit_records(
            &self.root_children,
            &mut records,
            &mut channel_data,
            &mut index,
            total,
            progress,
        )?;

        // Refresh DPI/ICC while preserving every other resource block.
        let mut image_resources = self.image_resources.clone();
        image_resources.set_resolution_info(ResolutionInfoBlock::new(self.dpi));
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
            image_data: ImageData::new(num_channels),
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
        let file = self.to_photoshop_file_with_progress(progress)?;
        let mut writer = BeWriter::new();
        file.write(&mut writer)?;
        Ok(writer.into_inner())
    }

    /// Write the document to disk.
    pub fn write(&self, path: impl AsRef<Path>) -> Result<()> {
        std::fs::write(path, self.to_bytes()?)?;
        Ok(())
    }

    /// Write the document to disk, reporting one [`ProgressEvent`] per layer.
    pub fn write_with_progress(
        &self,
        path: impl AsRef<Path>,
        progress: &mut dyn FnMut(ProgressEvent<'_>),
    ) -> Result<()> {
        std::fs::write(path, self.to_bytes_with_progress(progress)?)?;
        Ok(())
    }

    /// Number of records a subtree serializes to (groups gain a synthesized
    /// divider when they lack one).
    fn count_records(&self, children: &[LayerId]) -> usize {
        let mut count = 0;
        let mut previous_was_divider = false;
        for &id in children {
            match &self.slot(id).kind {
                LayerKind::Group(group) => {
                    if !previous_was_divider {
                        count += 1;
                    }
                    count += self.count_records(&group.children) + 1;
                    previous_was_divider = false;
                }
                LayerKind::SectionDivider(_) => {
                    count += 1;
                    previous_was_divider = true;
                }
                LayerKind::Image(_) | LayerKind::Text(_) => {
                    count += 1;
                    previous_was_divider = false;
                }
            }
        }
        count
    }

    /// Emit records in on-disk order, synthesizing a bounding section divider
    /// for every group that lacks one (Photoshop terminates a group's extent
    /// with a `</Layer group>` record placed before its contents).
    fn emit_records<'a>(
        &'a self,
        children: &[LayerId],
        records: &mut Vec<LayerRecord<'a>>,
        channel_data: &mut Vec<ChannelImageData>,
        index: &mut usize,
        total: usize,
        progress: &mut dyn FnMut(ProgressEvent<'_>),
    ) -> Result<()> {
        let mut previous_was_divider = false;
        for &id in children {
            let layer = self.slot(id);
            match &layer.kind {
                LayerKind::Group(group) => {
                    if !previous_was_divider {
                        let (record, data) = self.build_divider_record()?;
                        progress(ProgressEvent::Layer {
                            name: record.name.value(),
                            index: *index,
                            total,
                        });
                        *index += 1;
                        records.push(record);
                        channel_data.push(data);
                    }
                    self.emit_records(
                        &group.children,
                        records,
                        channel_data,
                        index,
                        total,
                        progress,
                    )?;
                    let (record, data) =
                        self.build_record(layer, self.blocks_for_record(layer)?)?;
                    progress(ProgressEvent::Layer {
                        name: layer.name.as_str(),
                        index: *index,
                        total,
                    });
                    *index += 1;
                    records.push(record);
                    channel_data.push(data);
                    previous_was_divider = false;
                }
                LayerKind::SectionDivider(_) | LayerKind::Image(_) | LayerKind::Text(_) => {
                    let (record, data) =
                        self.build_record(layer, self.blocks_for_record(layer)?)?;
                    progress(ProgressEvent::Layer {
                        name: layer.name.as_str(),
                        index: *index,
                        total,
                    });
                    *index += 1;
                    records.push(record);
                    channel_data.push(data);
                    previous_was_divider = matches!(layer.kind, LayerKind::SectionDivider(_));
                }
            }
        }
        Ok(())
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
        let mut owned: Option<AdditionalLayerInfo> = None;

        let current_name = layer
            .blocks
            .get(TaggedBlockKey::LUNI)
            .and_then(|block| UnicodeString::read(&mut BeReader::new(&block.data), 4).ok());
        if current_name.as_ref().map(UnicodeString::value) != Some(layer.name.as_str()) {
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
                    // A short `lsct` has no blend key: the record's mode
                    // applies, which cannot say pass-through.
                    Some(data) if layer.blend_mode != BlendMode::PASSTHROUGH => {
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

    fn build_divider_record(&self) -> Result<(LayerRecord<'static>, ChannelImageData)> {
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
                data: payload,
            });
        }
        let mut blocks = AdditionalLayerInfo::new();
        blocks.push(TaggedBlock::new(
            TaggedBlockKey::LSCT,
            SectionDivider::BoundingSection
                .as_raw()
                .to_be_bytes()
                .to_vec(),
        ));
        let record = LayerRecord {
            name: PascalString::new("</Layer group>", 4),
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
    ) -> Result<(LayerRecord<'a>, ChannelImageData)> {
        let bounds = layer.bounds;
        let stub_channels = matches!(
            layer.kind,
            LayerKind::Group(_) | LayerKind::SectionDivider(_)
        );
        let mut keys: Vec<ChannelKey> = match &layer.kind {
            LayerKind::Image(image) => image.channels.keys().collect(),
            LayerKind::Text(text) => text.channels.keys().collect(),
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
        keys.sort_by_key(|key| channel_sort_key(*key));

        let mut channels = Vec::new();
        let mut data = Vec::new();
        for key in keys {
            let samples: &[T] = layer
                .channels()
                .and_then(|channels| channels.get(key))
                .unwrap_or(&[]);
            let rect = if key.is_mask() {
                mask_channel_rect(layer.mask.as_ref(), key).unwrap_or(bounds)
            } else if stub_channels {
                // Stubs stay empty whatever the record bounds say; Photoshop
                // and upstream write groups with mask-derived bounds this way.
                Rect::default()
            } else {
                bounds
            };
            let (compression, payload) = compress_channel(
                samples,
                rect.width().max(0) as usize,
                rect.height().max(0) as usize,
                self.version,
                layer.write_compression(key, self.compression),
            )?;
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

        // Pass-through is spelled on the group's `lsct` block with `norm` on
        // the record, as Photoshop and upstream write it.
        let blend_mode = match layer.kind {
            LayerKind::Group(_) if layer.blend_mode == BlendMode::PASSTHROUGH => BlendMode::NORMAL,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer_with_pixel(value: u8) -> Layer<u8> {
        let mut layer = Layer::new_image("Layer", Rect::new(0, 0, 4, 4));
        let image = layer.image_mut().unwrap();
        image.set_channel(ChannelKey::color(0), vec![value; 16]);
        image.set_channel(ChannelKey::color(1), vec![value; 16]);
        image.set_channel(ChannelKey::color(2), vec![value; 16]);
        layer
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
}
