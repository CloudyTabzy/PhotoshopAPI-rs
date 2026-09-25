//! Linked Smart Object creation, source decoding, replacement, and transforms.
//!
//! Upstream keeps a shared `LinkedLayers` pointer on each C++ SmartObjectLayer.
//! In the Rust arena the link records live on `LayeredFile`, so creation,
//! source lookup, replacement, and every edit that re-renders the preview
//! (warp and placement changes) are document operations. Untouched link
//! blocks remain raw; creation and replacement append new immutable link
//! records and update only the target layer's placed-data identity.
//!
//! Upstream renders lazily on access or write; here each edit re-renders
//! immediately, so a layer's channels and bounds are always current.

use std::io::Read;
use std::path::{Path, PathBuf};

#[cfg(feature = "image")]
use std::io::Cursor;

use psd_codecs::endian::decode_be_bytes;
use psd_core::{
    AdditionalLayerInfo, BeReader, BeWriter, ColorMode, Descriptor, DescriptorKey, DescriptorValue,
    LinkedDataKind, LinkedLayer, LinkedLayerTaggedBlock, LinkedLayerView, PascalString,
    PhotoshopFile, PlacedLayer, PlacedLayerData, PsdError, Result, TaggedBlock, TaggedBlockKey,
    UnicodeString, Version,
};
use uuid::Uuid;

use crate::channels::ChannelKey;
use crate::geometry::{Homography, Point2};
use crate::layer::{Layer, LayerId};
use crate::render::MAX_RASTER_BYTES;
use crate::{BitDepth, LayeredFile, Raster, Warp, WarpRenderOptions};

const MAX_ENCODED_SOURCE_BYTES: u64 = 512 * 1024 * 1024;

/// How a replacement source is stored in the PSD/PSB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkedStorage {
    /// Embed the encoded image bytes in the document (recommended).
    #[default]
    Embedded,
    /// Store a file reference and resolve it from its descriptor/path context.
    External,
}

struct DecodedSource<T: BitDepth> {
    raster: Raster<T>,
    file_type: [u8; 4],
}

impl<T: BitDepth> LayeredFile<T> {
    // ------------------------------------------------------------------
    // Source lookup
    // ------------------------------------------------------------------

    /// Decode the full-resolution source associated with a Smart Object layer.
    ///
    /// Embedded/external JPEG and PNG sources are supported when the crate's
    /// `image` feature is enabled. Same-depth RGB PSD/PSB sources are decoded
    /// from Raw/RLE merged previews. Alias records and unsupported modes or
    /// codecs remain preserved in the document but return an explicit error.
    pub fn smart_object_source(&self, layer_id: LayerId) -> Result<Raster<T>> {
        self.smart_object_source_of(self.smart_object_layer(layer_id)?)
    }

    /// [`smart_object_source`](Self::smart_object_source) for a layer that
    /// is not (or not yet) in this document but links to its records, such as
    /// one returned by [`create_smart_object`](Self::create_smart_object) or
    /// [`remove_layer`](Self::remove_layer).
    pub fn smart_object_source_of(&self, layer: &Layer<T>) -> Result<Raster<T>> {
        ensure_smart_object(layer)?;
        let unique_id = linked_identity(layer)?;
        let record = self.linked_record(&unique_id)?;
        match record.kind {
            LinkedDataKind::Data => {
                if record.raw_file_bytes.is_empty() {
                    return Err(PsdError::ImageDecode(
                        "embedded linked record has no source bytes".to_owned(),
                    ));
                }
                decode_source_raster(record.raw_file_bytes).map(|source| source.raster)
            }
            LinkedDataKind::External => {
                if !record.raw_file_bytes.is_empty() {
                    return decode_source_raster(record.raw_file_bytes).map(|source| source.raster);
                }
                let path = self.external_source_path(&record)?;
                let bytes = read_source_file(&path)?;
                decode_source_raster(&bytes).map(|source| source.raster)
            }
            LinkedDataKind::Alias => Err(PsdError::ImageDecode(
                "alias smart-object sources are preserved but cannot be decoded".to_owned(),
            )),
        }
    }

    // ------------------------------------------------------------------
    // Creation
    // ------------------------------------------------------------------

    /// Build a new smart-object layer linked to the image at `source_path`
    /// (upstream's `SmartObjectLayer(file, params, path, [warp], linkage)`).
    ///
    /// The link record is added to this document right away (an identical
    /// existing record is reused, as upstream deduplicates by file); the
    /// returned layer is not in the tree yet — insert it with
    /// [`add_layer`](Self::add_layer) or [`insert_layer`](Self::insert_layer).
    /// Without `warp` a flat warp over the full source is used, so the layer
    /// shows the image at its native size at the canvas origin.
    pub fn create_smart_object(
        &mut self,
        name: impl Into<String>,
        source_path: impl AsRef<Path>,
        storage: LinkedStorage,
        warp: Option<Warp>,
    ) -> Result<Layer<T>> {
        let path = absolute_path(source_path.as_ref())?;
        let file_name = file_name_of(&path)?;
        let bytes = read_source_file(&path)?;
        self.create_smart_object_encoded(
            name.into(),
            &file_name,
            &bytes,
            storage,
            Some(&path),
            warp,
        )
    }

    /// [`create_smart_object`](Self::create_smart_object) from already
    /// encoded image bytes; always embedded, since an external link needs a
    /// path.
    pub fn create_smart_object_from_bytes(
        &mut self,
        name: impl Into<String>,
        file_name: impl AsRef<Path>,
        encoded_source: &[u8],
        warp: Option<Warp>,
    ) -> Result<Layer<T>> {
        let file_name = file_name_of(file_name.as_ref())?;
        self.create_smart_object_encoded(
            name.into(),
            &file_name,
            encoded_source,
            LinkedStorage::Embedded,
            None,
            warp,
        )
    }

    fn create_smart_object_encoded(
        &mut self,
        name: String,
        file_name: &str,
        encoded_source: &[u8],
        storage: LinkedStorage,
        external_path: Option<&Path>,
        warp: Option<Warp>,
    ) -> Result<Layer<T>> {
        self.ensure_rgb("creation")?;
        let decoded = decode_source_raster::<T>(encoded_source)?;
        let (source_width, source_height) = (decoded.raster.width(), decoded.raster.height());
        let warp = match warp {
            Some(warp) => warp,
            None => Warp::generate_default(source_width, source_height)?,
        };
        let rendered = warp.apply(&decoded.raster, WarpRenderOptions::default())?;
        drop(decoded.raster);

        let (link_id, record) = self.prepare_link(
            file_name,
            decoded.file_type,
            encoded_source,
            storage,
            external_path,
        )?;
        let placed = new_placed_layer_data(
            &link_id,
            &Uuid::new_v4().to_string(),
            source_width,
            source_height,
        )?;
        let mut payload = BeWriter::new();
        placed.write(&mut payload)?;
        let mut layer = Layer::new_image(name, crate::layer::Rect::default());
        layer
            .blocks
            .push(TaggedBlock::new(TaggedBlockKey::SOLD, payload.into_inner()));
        // Writes the warp into the new SoLd block and installs the preview.
        let (blocks, channels, bounds) =
            layer.prepare_rendered_warp(&warp, rendered, Vec::new())?;
        layer.blocks = blocks;
        layer.bounds = bounds;
        layer
            .image_mut()
            .expect("created as an image-kind layer")
            .channels = channels;

        self.commit_link(&link_id, storage, record, external_path)?;
        Ok(layer)
    }

    // ------------------------------------------------------------------
    // Replacement
    // ------------------------------------------------------------------

    /// Replace a Smart Object source from a file while preserving its warp and
    /// layer transform. By default the encoded bytes are embedded.
    pub fn replace_smart_object(
        &mut self,
        layer_id: LayerId,
        source_path: impl AsRef<Path>,
    ) -> Result<()> {
        self.replace_smart_object_with_storage(layer_id, source_path, LinkedStorage::Embedded)
    }

    /// Replace a Smart Object source while selecting embedded or external
    /// storage. External records store an absolute path descriptor and omit
    /// the source payload.
    pub fn replace_smart_object_with_storage(
        &mut self,
        layer_id: LayerId,
        source_path: impl AsRef<Path>,
        storage: LinkedStorage,
    ) -> Result<()> {
        self.with_slot(layer_id, |document, layer| {
            document.replace_smart_object_layer(layer, source_path, storage)
        })
    }

    /// [`replace_smart_object_with_storage`](Self::replace_smart_object_with_storage)
    /// for a layer outside the tree whose links live in this document.
    pub fn replace_smart_object_layer(
        &mut self,
        layer: &mut Layer<T>,
        source_path: impl AsRef<Path>,
        storage: LinkedStorage,
    ) -> Result<()> {
        let path = absolute_path(source_path.as_ref())?;
        let file_name = file_name_of(&path)?;
        let bytes = read_source_file(&path)?;
        self.replace_smart_object_encoded(layer, &file_name, &bytes, storage, Some(&path))
    }

    /// Replace a Smart Object from already encoded bytes. Byte replacement is
    /// an additive Rust convenience; the upstream public contract is path based.
    /// The data is always embedded because an external record requires a path.
    pub fn replace_smart_object_from_bytes(
        &mut self,
        layer_id: LayerId,
        file_name: impl AsRef<Path>,
        encoded_source: &[u8],
    ) -> Result<()> {
        let file_name = file_name_of(file_name.as_ref())?;
        self.with_slot(layer_id, |document, layer| {
            document.replace_smart_object_encoded(
                layer,
                &file_name,
                encoded_source,
                LinkedStorage::Embedded,
                None,
            )
        })
    }

    fn replace_smart_object_encoded(
        &mut self,
        layer: &mut Layer<T>,
        file_name: &str,
        encoded_source: &[u8],
        storage: LinkedStorage,
        external_path: Option<&Path>,
    ) -> Result<()> {
        self.ensure_rgb("replacement")?;
        ensure_smart_object(layer)?;
        ensure_supported_target_channels(layer)?;
        let mut warp = layer.warp()?.ok_or(missing_warp())?;
        let preserved_masks = mask_planes(layer);
        let decoded = decode_source_raster::<T>(encoded_source)?;
        let source_width = decoded.raster.width();
        let source_height = decoded.raster.height();
        warp.rescale_source(source_width, source_height)?;
        let rendered = warp.apply(&decoded.raster, WarpRenderOptions::default())?;
        drop(decoded.raster);

        let (mut updated_blocks, updated_channels, updated_bounds) =
            layer.prepare_rendered_warp(&warp, rendered, preserved_masks)?;
        let (link_id, record) = self.prepare_link(
            file_name,
            decoded.file_type,
            encoded_source,
            storage,
            external_path,
        )?;
        update_layer_link_identity(
            &mut updated_blocks,
            &link_id,
            &Uuid::new_v4().to_string(),
            source_width,
            source_height,
        )?;
        // All fallible raster and layer edits are prepared before the
        // document-level link is appended. The append helper reserves before
        // moving raw bytes, so no error remains after it commits.
        self.commit_link(&link_id, storage, record, external_path)?;
        layer.blocks = updated_blocks;
        layer.bounds = updated_bounds;
        layer
            .image_mut()
            .expect("replacement layer kind was validated before commit")
            .channels = updated_channels;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Warp and placement edits
    // ------------------------------------------------------------------

    /// Persist `warp` on a smart object and re-render its preview from the
    /// linked source (upstream's `warp` setter). Mask channels are kept.
    pub fn set_smart_object_warp(&mut self, layer_id: LayerId, warp: &Warp) -> Result<()> {
        self.with_slot(layer_id, |document, layer| {
            document.set_smart_object_layer_warp(layer, warp)
        })
    }

    /// [`set_smart_object_warp`](Self::set_smart_object_warp) for a layer
    /// outside the tree whose links live in this document.
    pub fn set_smart_object_layer_warp(&self, layer: &mut Layer<T>, warp: &Warp) -> Result<()> {
        self.ensure_rgb("rendering")?;
        ensure_smart_object(layer)?;
        ensure_supported_target_channels(layer)?;
        let source = self.smart_object_source_of(layer)?;
        let rendered = warp.apply(&source, WarpRenderOptions::default())?;
        drop(source);
        let (blocks, channels, bounds) =
            layer.prepare_rendered_warp(warp, rendered, mask_planes(layer))?;
        layer.blocks = blocks;
        layer.bounds = bounds;
        layer.image_mut().expect("validated as image kind").channels = channels;
        Ok(())
    }

    /// Edit a smart object's warp in place and re-render it; nothing changes
    /// when `edit` or rendering fails.
    pub fn edit_smart_object_warp(
        &mut self,
        layer_id: LayerId,
        edit: impl FnOnce(&mut Warp) -> Result<()>,
    ) -> Result<()> {
        self.with_slot(layer_id, |document, layer| {
            document.edit_smart_object_layer_warp(layer, edit)
        })
    }

    /// [`edit_smart_object_warp`](Self::edit_smart_object_warp) for a layer
    /// outside the tree whose links live in this document.
    pub fn edit_smart_object_layer_warp(
        &self,
        layer: &mut Layer<T>,
        edit: impl FnOnce(&mut Warp) -> Result<()>,
    ) -> Result<()> {
        ensure_smart_object(layer)?;
        let mut warp = layer.warp()?.ok_or(missing_warp())?;
        edit(&mut warp)?;
        self.set_smart_object_layer_warp(layer, &warp)
    }

    /// Move a smart object (with its warp) by `offset` pixels.
    pub fn move_smart_object(&mut self, layer_id: LayerId, offset: Point2) -> Result<()> {
        self.edit_smart_object_warp(layer_id, |warp| warp.translate(offset))
    }

    /// Rotate a smart object by `degrees` (clockwise) around `center`.
    pub fn rotate_smart_object(
        &mut self,
        layer_id: LayerId,
        degrees: f64,
        center: Point2,
    ) -> Result<()> {
        self.edit_smart_object_warp(layer_id, |warp| warp.rotate(degrees, center))
    }

    /// Scale a smart object by `factors` around `center`.
    pub fn scale_smart_object(
        &mut self,
        layer_id: LayerId,
        factors: Point2,
        center: Point2,
    ) -> Result<()> {
        self.edit_smart_object_warp(layer_id, |warp| warp.scale(factors, center))
    }

    /// Apply a 3×3 (possibly perspective) transform; see [`Warp::transform`].
    pub fn transform_smart_object(
        &mut self,
        layer_id: LayerId,
        homography: &Homography,
    ) -> Result<()> {
        self.edit_smart_object_warp(layer_id, |warp| warp.transform(homography))
    }

    /// Map a smart object back onto its source rectangle; see
    /// [`Warp::reset_transform`].
    pub fn reset_smart_object_transform(&mut self, layer_id: LayerId) -> Result<()> {
        self.edit_smart_object_warp(layer_id, Warp::reset_transform)
    }

    /// Flatten a smart object's warp, keeping its placement; see
    /// [`Warp::reset_warp`].
    pub fn reset_smart_object_warp(&mut self, layer_id: LayerId) -> Result<()> {
        self.edit_smart_object_warp(layer_id, Warp::reset_warp)
    }

    /// Scale a smart object around its center so its warped bounds get the
    /// given width and/or height (upstream's `width`/`height` setters).
    pub fn resize_smart_object(
        &mut self,
        layer_id: LayerId,
        width: Option<f64>,
        height: Option<f64>,
    ) -> Result<()> {
        self.edit_smart_object_warp(layer_id, |warp| warp.resize(width, height))
    }

    /// Move a smart object so the center of its warped bounds lands on the
    /// given coordinates (upstream's `center_x`/`center_y` setters).
    pub fn center_smart_object(
        &mut self,
        layer_id: LayerId,
        x: Option<f64>,
        y: Option<f64>,
    ) -> Result<()> {
        self.edit_smart_object_warp(layer_id, |warp| warp.recenter(x, y))
    }

    /// The typed warp of a smart-object layer.
    pub fn smart_object_warp(&self, layer_id: LayerId) -> Result<Warp> {
        self.smart_object_layer(layer_id)?
            .warp()?
            .ok_or(missing_warp())
    }

    // ------------------------------------------------------------------
    // Link storage
    // ------------------------------------------------------------------

    /// How a smart object's source is stored. Alias records have no storage
    /// equivalent and are an error.
    pub fn smart_object_storage(&self, layer_id: LayerId) -> Result<LinkedStorage> {
        self.smart_object_layer_storage(self.smart_object_layer(layer_id)?)
    }

    /// [`smart_object_storage`](Self::smart_object_storage) for a layer
    /// outside the tree whose links live in this document.
    pub fn smart_object_layer_storage(&self, layer: &Layer<T>) -> Result<LinkedStorage> {
        ensure_smart_object(layer)?;
        match self.linked_record(&linked_identity(layer)?)?.kind {
            LinkedDataKind::Data => Ok(LinkedStorage::Embedded),
            LinkedDataKind::External => Ok(LinkedStorage::External),
            LinkedDataKind::Alias => Err(PsdError::ImageDecode(
                "alias smart-object links have no embedded/external storage".to_owned(),
            )),
        }
    }

    /// The file a smart object's source came from: the path it was created
    /// or replaced from in this session, else an external link's path.
    /// Embedded sources read from a file carry no path.
    pub fn smart_object_source_path(&self, layer_id: LayerId) -> Result<Option<PathBuf>> {
        self.smart_object_layer_source_path(self.smart_object_layer(layer_id)?)
    }

    /// [`smart_object_source_path`](Self::smart_object_source_path) for a
    /// layer outside the tree whose links live in this document.
    pub fn smart_object_layer_source_path(&self, layer: &Layer<T>) -> Result<Option<PathBuf>> {
        ensure_smart_object(layer)?;
        let identity = linked_identity(layer)?;
        if let Some(path) = self.linked_sources.get(&identity) {
            return Ok(Some(path.clone()));
        }
        let record = self.linked_record(&identity)?;
        match record.kind {
            LinkedDataKind::External => self.external_source_path(&record).map(Some),
            _ => Ok(None),
        }
    }

    /// Switch a smart object's source between embedded and external storage
    /// (upstream's `linkage` setter). Like upstream, every layer sharing the
    /// link switches with it. Needs the source file: an external link's path
    /// or one this session created or replaced the layer from.
    pub fn set_smart_object_storage(
        &mut self,
        layer_id: LayerId,
        storage: LinkedStorage,
    ) -> Result<()> {
        let layer = self.smart_object_layer(layer_id)?;
        if self.smart_object_layer_storage(layer)? == storage {
            return Ok(());
        }
        let identity = linked_identity(layer)?;
        let path = self.required_source_path(layer)?;
        let sharing: Vec<LayerId> = self
            .layers_with_ids()
            .filter(|(_, layer)| {
                layer.is_smart_object()
                    && linked_identity(layer).is_ok_and(|other| other == identity)
            })
            .map(|(id, _)| id)
            .collect();
        for id in sharing {
            self.replace_smart_object_with_storage(id, &path, storage)?;
        }
        Ok(())
    }

    /// [`set_smart_object_storage`](Self::set_smart_object_storage) for one
    /// layer outside the tree (layers in the tree that share its link are
    /// not changed).
    pub fn set_smart_object_layer_storage(
        &mut self,
        layer: &mut Layer<T>,
        storage: LinkedStorage,
    ) -> Result<()> {
        if self.smart_object_layer_storage(layer)? == storage {
            return Ok(());
        }
        let path = self.required_source_path(layer)?;
        self.replace_smart_object_layer(layer, path, storage)
    }

    /// Copy the link record `layer` needs from `source` into this document
    /// when it is missing here, so a smart object taken from another document
    /// keeps its image. Layers that are not smart objects are ignored.
    pub fn import_smart_object_links(
        &mut self,
        source: &LayeredFile<T>,
        layer: &Layer<T>,
    ) -> Result<()> {
        if !layer.is_smart_object() {
            return Ok(());
        }
        let identity = linked_identity(layer)?;
        if self.linked_record(&identity).is_ok() {
            return Ok(());
        }
        let record = LinkedLayer::from(source.linked_record(&identity)?);
        let storage = match record.kind {
            LinkedDataKind::External => LinkedStorage::External,
            _ => LinkedStorage::Embedded,
        };
        if let Some(path) = source.linked_sources.get(&identity) {
            self.linked_sources.insert(identity, path.clone());
        }
        append_linked_record(self, storage, &record)
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    /// Run `f` with layer `layer_id` taken out of its slot, so document-level
    /// link records can be edited alongside the layer; the layer is put back
    /// whatever `f` returns.
    fn with_slot<R>(
        &mut self,
        layer_id: LayerId,
        f: impl FnOnce(&mut Self, &mut Layer<T>) -> Result<R>,
    ) -> Result<R> {
        self.smart_object_layer(layer_id)?;
        let mut layer = self.slots_mut()[layer_id]
            .take()
            .expect("validated as a live layer");
        let result = f(self, &mut layer);
        self.slots_mut()[layer_id] = Some(layer);
        result
    }

    fn smart_object_layer(&self, layer_id: LayerId) -> Result<&Layer<T>> {
        let layer = self.layer(layer_id).ok_or(PsdError::InvalidData {
            offset: 0,
            message: "smart-object layer id is out of range",
        })?;
        ensure_smart_object(layer)?;
        Ok(layer)
    }

    fn ensure_rgb(&self, operation: &str) -> Result<()> {
        if self.color_mode == ColorMode::Rgb {
            Ok(())
        } else {
            Err(PsdError::ImageDecode(format!(
                "Smart Object {operation} currently requires an RGB document"
            )))
        }
    }

    fn required_source_path(&self, layer: &Layer<T>) -> Result<PathBuf> {
        self.smart_object_layer_source_path(layer)?.ok_or_else(|| {
            PsdError::ImageDecode(
                "the smart object's source file is unknown; replace it from a path instead"
                    .to_owned(),
            )
        })
    }

    /// Choose the link identity for an encoded source (reusing an identical
    /// record) and build the record to append, without changing the document.
    fn prepare_link(
        &self,
        file_name: &str,
        file_type: [u8; 4],
        encoded_source: &[u8],
        storage: LinkedStorage,
        external_path: Option<&Path>,
    ) -> Result<(String, Option<LinkedLayer>)> {
        let existing_links = self.linked_record_views()?;
        let reusable = reusable_link(
            &existing_links,
            storage,
            file_name,
            external_path,
            encoded_source,
            file_type,
        );
        match reusable {
            Some(link_id) => Ok((link_id, None)),
            None => {
                let link_id = fresh_uuid(&existing_links);
                let record = new_linked_record(
                    &link_id,
                    file_name,
                    file_type,
                    encoded_source,
                    storage,
                    external_path,
                )?;
                Ok((link_id, Some(record)))
            }
        }
    }

    fn commit_link(
        &mut self,
        link_id: &str,
        storage: LinkedStorage,
        record: Option<LinkedLayer>,
        external_path: Option<&Path>,
    ) -> Result<()> {
        if let Some(record) = record {
            append_linked_record(self, storage, &record)?;
        }
        if let Some(path) = external_path {
            self.linked_sources
                .insert(link_id.to_owned(), path.to_path_buf());
        }
        Ok(())
    }

    fn linked_record_views(&self) -> Result<Vec<LinkedLayerView<'_>>> {
        self.linked_layer_views()
    }

    fn linked_record(&self, unique_id: &str) -> Result<LinkedLayerView<'_>> {
        let mut matching = self
            .linked_record_views()?
            .into_iter()
            .filter(|record| record.unique_id.value() == unique_id);
        let record = matching.next().ok_or(PsdError::ImageDecode(
            "no document-level linked record matches the layer identity".to_owned(),
        ))?;
        if matching.next().is_some() {
            return Err(PsdError::ImageDecode(
                "multiple document-level linked records match the layer identity".to_owned(),
            ));
        }
        Ok(record)
    }

    fn external_source_path(&self, record: &LinkedLayerView<'_>) -> Result<PathBuf> {
        let base = self.source_path().and_then(Path::parent);
        let mut candidates = Vec::new();
        if let Some(descriptor) = &record.linked_file_descriptor {
            for key in ["originalPath", "fullPath", "relPath"] {
                if let Some(raw) = descriptor.get(key).and_then(DescriptorValue::as_str) {
                    if raw.is_empty() {
                        continue;
                    }
                    let stored = path_from_link_descriptor(raw);
                    if stored.is_absolute() {
                        candidates.push(stored);
                    } else if let Some(base) = base {
                        candidates.push(base.join(stored));
                    }
                }
            }
        }
        let file_name = PathBuf::from(record.file_name.value());
        if file_name.is_absolute() {
            candidates.push(file_name);
        } else if let Some(base) = base {
            candidates.push(base.join(file_name));
        }
        if candidates.is_empty() {
            return Err(PsdError::ImageDecode(
                "relative external source requires a document base path".to_owned(),
            ));
        }
        Ok(candidates
            .iter()
            .find(|candidate| candidate.is_file())
            .cloned()
            .unwrap_or_else(|| candidates.remove(0)))
    }
}

fn ensure_smart_object<T: BitDepth>(layer: &Layer<T>) -> Result<()> {
    if !layer.is_smart_object() {
        return Err(PsdError::InvalidData {
            offset: 0,
            message: "the layer is not a smart object",
        });
    }
    if let Some(data) = layer.smart_object_data()? {
        if data.descriptor.contains("filterFX") {
            return Err(PsdError::ImageDecode(
                "Puppet/Perspective smart-filter warps are unsupported".to_owned(),
            ));
        }
    }
    Ok(())
}

fn missing_warp() -> PsdError {
    PsdError::InvalidData {
        offset: 0,
        message: "smart-object layer has no placed warp descriptor",
    }
}

/// The mask planes of a layer, kept across re-renders.
fn mask_planes<T: BitDepth>(layer: &Layer<T>) -> Vec<(ChannelKey, Vec<T>)> {
    layer
        .channels()
        .into_iter()
        .flat_map(|channels| channels.iter())
        .filter(|(key, _)| key.is_mask())
        .map(|(key, samples)| (key, samples.to_vec()))
        .collect()
}

fn file_name_of(path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned();
    if name.is_empty() {
        return Err(PsdError::InvalidData {
            offset: 0,
            message: "smart-object source path has no file name",
        });
    }
    Ok(name)
}

fn item(key: DescriptorKey, value: DescriptorValue) -> psd_core::DescriptorItem {
    psd_core::DescriptorItem { key, value }
}

fn null_descriptor(items: Vec<psd_core::DescriptorItem>) -> Result<Descriptor> {
    Ok(Descriptor {
        name: UnicodeString::new("\0", 1)?,
        class_id: DescriptorKey::char_id(*b"null"),
        items,
    })
}

/// A fresh `SoLd` payload in the shape Photoshop writes (key order and
/// char-ID encodings as in Photoshop-authored files), with a flat template
/// warp that [`Warp::update_placed_layer_data`] then fills in. The frame,
/// page, and anti-alias values are upstream's defaults.
fn new_placed_layer_data(
    link_id: &str,
    placed_id: &str,
    width: usize,
    height: usize,
) -> Result<PlacedLayerData> {
    let (width, height) = (width as f64, height as f64);
    let explicit = DescriptorKey::new;
    let char_id = DescriptorKey::char_id;
    let integer = DescriptorValue::Integer;
    let quad = DescriptorValue::List(
        [0.0, 0.0, width, 0.0, width, height, 0.0, height]
            .into_iter()
            .map(DescriptorValue::Double)
            .collect(),
    );
    let fraction = |numerator: i32, denominator: i32| -> Result<DescriptorValue> {
        Ok(DescriptorValue::Descriptor(null_descriptor(vec![
            item(explicit("numerator"), integer(numerator)),
            item(explicit("denominator"), integer(denominator)),
        ])?))
    };
    let bounds = Descriptor {
        name: UnicodeString::new("\0", 1)?,
        class_id: explicit("classFloatRect"),
        items: vec![
            item(char_id(*b"Top "), DescriptorValue::Double(0.0)),
            item(char_id(*b"Left"), DescriptorValue::Double(0.0)),
            item(char_id(*b"Btom"), DescriptorValue::Double(height)),
            item(char_id(*b"Rght"), DescriptorValue::Double(width)),
        ],
    };
    let warp = Descriptor {
        name: UnicodeString::new("\0", 1)?,
        class_id: explicit("warp"),
        items: vec![
            item(
                explicit("warpStyle"),
                DescriptorValue::Enumerated {
                    type_id: explicit("warpStyle"),
                    value: explicit("warpNone"),
                },
            ),
            item(explicit("warpValue"), DescriptorValue::Double(0.0)),
            item(explicit("warpPerspective"), DescriptorValue::Double(0.0)),
            item(
                explicit("warpPerspectiveOther"),
                DescriptorValue::Double(0.0),
            ),
            item(
                explicit("warpRotate"),
                DescriptorValue::Enumerated {
                    type_id: char_id(*b"Ornt"),
                    value: char_id(*b"Hrzn"),
                },
            ),
            item(explicit("bounds"), DescriptorValue::Descriptor(bounds)),
            item(explicit("uOrder"), integer(4)),
            item(explicit("vOrder"), integer(4)),
        ],
    };
    let size = Descriptor {
        name: UnicodeString::new("\0", 1)?,
        class_id: char_id(*b"Pnt "),
        items: vec![
            item(char_id(*b"Wdth"), DescriptorValue::Double(width)),
            item(char_id(*b"Hght"), DescriptorValue::Double(height)),
        ],
    };
    let descriptor = null_descriptor(vec![
        item(
            char_id(*b"Idnt"),
            DescriptorValue::String(UnicodeString::new(link_id, 2)?),
        ),
        item(
            explicit("placed"),
            DescriptorValue::String(UnicodeString::new(placed_id, 2)?),
        ),
        item(char_id(*b"PgNm"), integer(1)),
        item(explicit("totalPages"), integer(1)),
        item(char_id(*b"Crop"), integer(1)),
        item(explicit("frameStep"), fraction(0, 600)?),
        item(explicit("duration"), fraction(0, 600)?),
        item(explicit("frameCount"), integer(1)),
        item(char_id(*b"Annt"), integer(16)),
        item(char_id(*b"Type"), integer(2)),
        item(char_id(*b"Trnf"), quad.clone()),
        item(explicit("nonAffineTransform"), quad),
        item(explicit("warp"), DescriptorValue::Descriptor(warp)),
        item(char_id(*b"Sz  "), DescriptorValue::Descriptor(size)),
        item(
            char_id(*b"Rslt"),
            DescriptorValue::UnitFloat {
                unit: *b"#Rsl",
                value: 72.0,
            },
        ),
        item(char_id(*b"comp"), integer(-1)),
        item(
            explicit("compInfo"),
            DescriptorValue::Descriptor(null_descriptor(vec![
                item(explicit("compID"), integer(-1)),
                item(explicit("originalCompID"), integer(-1)),
            ])?),
        ),
    ])?;
    Ok(PlacedLayerData {
        version: 4,
        descriptor_version: 16,
        descriptor,
    })
}

fn linked_identity<T: BitDepth>(layer: &Layer<T>) -> Result<String> {
    if let Some(data) = layer.smart_object_data()? {
        if let Some(value) = data
            .descriptor
            .get("Idnt")
            .and_then(DescriptorValue::as_str)
        {
            if !value.is_empty() {
                return Ok(value.to_owned());
            }
        }
    }
    if let Some(placed) = layer.placed_layer()? {
        let value = placed.unique_id.value();
        if !value.is_empty() {
            return Ok(value.to_owned());
        }
    }
    Err(PsdError::ImageDecode(
        "smart-object layer has no linked-source identity".to_owned(),
    ))
}

fn path_from_link_descriptor(value: &str) -> PathBuf {
    let Some(rest) = value.strip_prefix("file://") else {
        return PathBuf::from(value);
    };
    let rest = percent_decode_path(rest);
    #[cfg(windows)]
    {
        if rest.starts_with('/') && rest.as_bytes().get(2) == Some(&b':') {
            return PathBuf::from(&rest[1..]);
        }
        if let Some((authority, path)) = rest.split_once('/') {
            if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
                return PathBuf::from(format!(r"\\{}\{}", authority, path.replace('/', "\\")));
            }
            if authority.eq_ignore_ascii_case("localhost") {
                if path.as_bytes().get(1) == Some(&b':') {
                    return PathBuf::from(path);
                }
                return PathBuf::from(format!("\\{}", path.replace('/', "\\")));
            }
        }
        PathBuf::from(rest)
    }
    #[cfg(not(windows))]
    {
        if let Some((authority, path)) = rest.split_once('/') {
            if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
                return PathBuf::from(format!("//{authority}/{path}"));
            }
            if authority.eq_ignore_ascii_case("localhost") {
                return PathBuf::from(format!("/{path}"));
            }
        }
        PathBuf::from(rest)
    }
}

fn percent_decode_path(value: &str) -> String {
    let input = value.as_bytes();
    let mut decoded = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%' && index + 2 < input.len() {
            if let (Some(high), Some(low)) =
                (hex_value(input[index + 1]), hex_value(input[index + 2]))
            {
                decoded.push((high << 4) | low);
                index += 3;
                continue;
            }
        }
        decoded.push(input[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn read_source_file(path: &Path) -> Result<Vec<u8>> {
    let path_metadata = std::fs::metadata(path)?;
    if !path_metadata.is_file() {
        return Err(PsdError::ImageDecode(
            "linked source path must be a regular file".to_owned(),
        ));
    }
    check_encoded_source_size(path_metadata.len())?;
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(PsdError::ImageDecode(
            "linked source path must be a regular file".to_owned(),
        ));
    }
    check_encoded_source_size(metadata.len())?;
    let mut bytes = Vec::new();
    file.take(MAX_ENCODED_SOURCE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    check_encoded_source_size(bytes.len() as u64)?;
    Ok(bytes)
}

fn check_encoded_source_size(size: u64) -> Result<()> {
    if size > MAX_ENCODED_SOURCE_BYTES {
        return Err(PsdError::ImageDecode(
            "encoded Smart Object source exceeds the 512 MiB limit".to_owned(),
        ));
    }
    Ok(())
}

fn decode_source_raster<T: BitDepth>(bytes: &[u8]) -> Result<DecodedSource<T>> {
    check_encoded_source_size(bytes.len() as u64)?;
    if bytes.starts_with(b"8BPS") {
        return decode_photoshop_preview(bytes);
    }
    #[cfg(feature = "image")]
    {
        decode_raster_image(bytes)
    }
    #[cfg(not(feature = "image"))]
    {
        let _ = bytes;
        Err(PsdError::ImageDecode(
            "JPEG/PNG Smart Object decoding requires the `image` feature".to_owned(),
        ))
    }
}

fn checked_raster_sample_count<T: BitDepth>(width: u32, height: u32) -> Result<usize> {
    let samples = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or(PsdError::ImageDecode(
            "Smart Object source dimensions overflow addressable memory".to_owned(),
        ))?;
    let bytes = samples
        .checked_mul(4)
        .and_then(|count| count.checked_mul(std::mem::size_of::<T>()))
        .ok_or(PsdError::ImageDecode(
            "Smart Object source channel allocation overflows addressable memory".to_owned(),
        ))?;
    if bytes > MAX_RASTER_BYTES {
        return Err(PsdError::ImageDecode(
            "decoded Smart Object Raster exceeds the 512 MiB limit".to_owned(),
        ));
    }
    Ok(samples)
}

fn raster_from_planar<T: BitDepth>(
    width: usize,
    height: usize,
    channels: Vec<(ChannelKey, Vec<T>)>,
) -> Result<Raster<T>> {
    let mut raster = Raster::new(width, height)?;
    for (key, samples) in channels {
        raster.set_channel(key, samples)?;
    }
    Ok(raster)
}

fn decode_photoshop_preview<T: BitDepth>(bytes: &[u8]) -> Result<DecodedSource<T>> {
    let mut reader = BeReader::new(bytes);
    let file = PhotoshopFile::read(&mut reader)?;
    let header = file.header;
    if header.depth.as_raw() != T::DEPTH {
        return Err(PsdError::UnsupportedBitDepth(header.depth.as_raw()));
    }
    if header.color_mode != ColorMode::Rgb {
        return Err(PsdError::ImageDecode(
            "linked PSD/PSB preview decoding currently supports RGB only".to_owned(),
        ));
    }
    let channel_count = usize::from(header.num_channels);
    if channel_count < 3 {
        return Err(PsdError::ImageDecode(
            "linked RGB PSD/PSB preview must contain at least three channels".to_owned(),
        ));
    }
    // Channels past RGB are saved alpha/spot channels unless the layer count
    // is negative, which marks the first one as the composite's transparency
    // (Photoshop's CMYK/Grayscale fixtures carry a saved "Alpha 1" with a
    // positive count). Treating any fourth channel as transparency cut a saved
    // selection out of the placed image; extra channels are not decoded.
    let decoded_channels =
        if channel_count > 3 && file.layer_and_mask_info.layer_info.has_merged_alpha {
            4
        } else {
            3
        };
    let sample_count = checked_raster_sample_count::<T>(header.width, header.height)?;
    let row_bytes = usize::try_from(header.width)
        .ok()
        .and_then(|width| width.checked_mul(std::mem::size_of::<T>()))
        .ok_or(PsdError::ImageDecode(
            "linked PSD/PSB scanline size overflows addressable memory".to_owned(),
        ))?;
    let compression = reader.u16()?;
    let mut channels = Vec::with_capacity(decoded_channels);

    match compression {
        0 => {
            let total_samples =
                sample_count
                    .checked_mul(decoded_channels)
                    .ok_or(PsdError::ImageDecode(
                        "linked PSD/PSB composite size overflows addressable memory".to_owned(),
                    ))?;
            let total_bytes = total_samples.checked_mul(std::mem::size_of::<T>()).ok_or(
                PsdError::ImageDecode(
                    "linked PSD/PSB composite byte size overflows addressable memory".to_owned(),
                ),
            )?;
            let raw = reader.take(total_bytes)?;
            for channel_index in 0..decoded_channels {
                let start = channel_index * sample_count * std::mem::size_of::<T>();
                let end = start + sample_count * std::mem::size_of::<T>();
                let samples = decode_be_bytes::<T>(&raw[start..end])
                    .map_err(|error| PsdError::Compression(error.to_string()))?;
                channels.push((composite_channel_key(channel_index), samples));
            }
        }
        1 => {
            let height = usize::try_from(header.height).map_err(|_| {
                PsdError::ImageDecode("linked PSD/PSB height does not fit memory".to_owned())
            })?;
            let size_width = if header.version == Version::Psd { 2 } else { 4 };
            let row_count = channel_count
                .checked_mul(height)
                .ok_or(PsdError::ImageDecode(
                    "linked PSD/PSB RLE table size overflows".to_owned(),
                ))?;
            let mut row_sizes = Vec::with_capacity(row_count);
            for _ in 0..row_count {
                let size = if size_width == 2 {
                    usize::from(reader.u16()?)
                } else {
                    usize::try_from(reader.u32()?).map_err(|_| {
                        PsdError::ImageDecode(
                            "linked PSD/PSB RLE row size does not fit memory".to_owned(),
                        )
                    })?
                };
                row_sizes.push(size);
            }
            for channel_index in 0..decoded_channels {
                let mut channel_bytes = Vec::with_capacity(row_bytes * height);
                for row in 0..height {
                    let compressed = reader.take(row_sizes[channel_index * height + row])?;
                    let decoded = psd_codecs::rle::pack_bits_decompress(compressed, row_bytes)
                        .map_err(|error| PsdError::Compression(error.to_string()))?;
                    channel_bytes.extend_from_slice(&decoded);
                }
                let samples = decode_be_bytes::<T>(&channel_bytes)
                    .map_err(|error| PsdError::Compression(error.to_string()))?;
                channels.push((composite_channel_key(channel_index), samples));
            }
        }
        other => {
            return Err(PsdError::ImageDecode(format!(
                "linked PSD/PSB preview compression {other} is unsupported (Raw/RLE only)"
            )));
        }
    }

    Ok(DecodedSource {
        raster: raster_from_planar(header.width as usize, header.height as usize, channels)?,
        file_type: if header.version == Version::Psd {
            *b"8BPS"
        } else {
            *b"8BPB"
        },
    })
}

fn composite_channel_key(index: usize) -> ChannelKey {
    if index == 3 {
        ChannelKey::ALPHA
    } else {
        ChannelKey::color(index as u8)
    }
}

#[cfg(feature = "image")]
fn checked_image_decode_samples<T: BitDepth>(
    width: u32,
    height: u32,
    maximum_bytes_per_pixel: usize,
) -> Result<usize> {
    let sample_count = checked_raster_sample_count::<T>(width, height)?;
    let maximum_decoder_bytes =
        sample_count
            .checked_mul(maximum_bytes_per_pixel)
            .ok_or(PsdError::ImageDecode(
                "decoded image buffer size overflows addressable memory".to_owned(),
            ))?;
    // The image crate's `max_alloc` is best-effort; this format-based header
    // preflight independently caps the largest possible JPEG/PNG output buffer.
    if maximum_decoder_bytes > MAX_RASTER_BYTES {
        return Err(PsdError::ImageDecode(
            "decoded source image buffer exceeds the 512 MiB limit".to_owned(),
        ));
    }
    Ok(sample_count)
}

#[cfg(feature = "image")]
fn decode_raster_image<T: BitDepth>(bytes: &[u8]) -> Result<DecodedSource<T>> {
    let mut dimensions_reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| PsdError::ImageDecode(error.to_string()))?;
    let format = dimensions_reader.format().ok_or_else(|| {
        PsdError::ImageDecode("linked raster format could not be identified".to_owned())
    })?;
    let (file_type, maximum_source_bytes_per_pixel) = match format {
        image::ImageFormat::Jpeg => (*b"JPEG", 4usize),
        image::ImageFormat::Png => (*b"png ", 8usize),
        other => {
            return Err(PsdError::ImageDecode(format!(
                "linked raster format {other:?} is unsupported (JPEG/PNG only)"
            )))
        }
    };
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(300_000);
    limits.max_image_height = Some(300_000);
    limits.max_alloc = Some(MAX_ENCODED_SOURCE_BYTES);
    dimensions_reader.limits(limits.clone());
    let (width, height) = dimensions_reader
        .into_dimensions()
        .map_err(|error| PsdError::ImageDecode(error.to_string()))?;
    let sample_count =
        checked_image_decode_samples::<T>(width, height, maximum_source_bytes_per_pixel)?;

    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| PsdError::ImageDecode(error.to_string()))?;
    if reader.format() != Some(format) {
        return Err(PsdError::ImageDecode(
            "linked raster format changed during decoding".to_owned(),
        ));
    }
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|error| PsdError::ImageDecode(error.to_string()))?;
    if (image.width(), image.height()) != (width, height) {
        return Err(PsdError::ImageDecode(
            "decoded image dimensions changed after header inspection".to_owned(),
        ));
    }
    let raster = match image {
        image::DynamicImage::ImageRgb8(image) => {
            interleaved_to_planar::<T, u8>(image.into_raw(), 3, width, height, sample_count)?
        }
        image::DynamicImage::ImageRgba8(image) => {
            interleaved_to_planar::<T, u8>(image.into_raw(), 4, width, height, sample_count)?
        }
        image::DynamicImage::ImageRgb16(image) => {
            interleaved_to_planar::<T, u16>(image.into_raw(), 3, width, height, sample_count)?
        }
        image::DynamicImage::ImageRgba16(image) => {
            interleaved_to_planar::<T, u16>(image.into_raw(), 4, width, height, sample_count)?
        }
        image::DynamicImage::ImageRgb32F(image) => {
            interleaved_to_planar::<T, f32>(image.into_raw(), 3, width, height, sample_count)?
        }
        image::DynamicImage::ImageRgba32F(image) => {
            interleaved_to_planar::<T, f32>(image.into_raw(), 4, width, height, sample_count)?
        }
        // Grayscale sources are placed as neutral RGB, as Photoshop does when
        // a grayscale file is placed into an RGB document. Upstream keeps only
        // the gray plane as red (and files a gray+alpha alpha plane under
        // green), leaving the layer without green/blue. The
        // widened buffers stay within the per-format preflight above.
        image @ (image::DynamicImage::ImageLuma8(_) | image::DynamicImage::ImageLumaA8(_)) => {
            let rgba = image.into_rgba8();
            interleaved_to_planar::<T, u8>(rgba.into_raw(), 4, width, height, sample_count)?
        }
        image @ (image::DynamicImage::ImageLuma16(_) | image::DynamicImage::ImageLumaA16(_)) => {
            let rgba = image.into_rgba16();
            interleaved_to_planar::<T, u16>(rgba.into_raw(), 4, width, height, sample_count)?
        }
        _ => {
            return Err(PsdError::ImageDecode(
                "linked raster decoding supports RGB, RGBA and grayscale data only".to_owned(),
            ))
        }
    };
    Ok(DecodedSource { raster, file_type })
}

#[cfg(feature = "image")]
fn interleaved_to_planar<T: BitDepth, S: BitDepth>(
    pixels: Vec<S>,
    source_channels: usize,
    width: u32,
    height: u32,
    sample_count: usize,
) -> Result<Raster<T>> {
    if !(source_channels == 3 || source_channels == 4)
        || pixels.len()
            != sample_count
                .checked_mul(source_channels)
                .ok_or(PsdError::ImageDecode(
                    "decoded image sample count overflows".to_owned(),
                ))?
    {
        return Err(PsdError::ImageDecode(
            "decoded RGB/RGBA sample count does not match its dimensions".to_owned(),
        ));
    }

    let mut red = Vec::new();
    let mut green = Vec::new();
    let mut blue = Vec::new();
    let mut alpha = Vec::new();
    for channel in [&mut red, &mut green, &mut blue, &mut alpha] {
        channel.try_reserve_exact(sample_count).map_err(|error| {
            PsdError::ImageDecode(format!("could not allocate decoded channel: {error}"))
        })?;
    }
    let opaque = T::from_f32(1.0);
    for pixel in pixels.chunks_exact(source_channels) {
        red.push(T::from_f32(pixel[0].to_f32()));
        green.push(T::from_f32(pixel[1].to_f32()));
        blue.push(T::from_f32(pixel[2].to_f32()));
        alpha.push(if source_channels == 4 {
            T::from_f32(pixel[3].to_f32())
        } else {
            opaque
        });
    }

    raster_from_planar(
        width as usize,
        height as usize,
        vec![
            (ChannelKey::color(0), red),
            (ChannelKey::color(1), green),
            (ChannelKey::color(2), blue),
            (ChannelKey::ALPHA, alpha),
        ],
    )
}

fn ensure_supported_target_channels<T: BitDepth>(layer: &Layer<T>) -> Result<()> {
    let image = layer.image().ok_or(PsdError::InvalidData {
        offset: 0,
        message: "smart-object replacement requires an image-kind layer",
    })?;
    if image
        .channels
        .keys()
        .any(|key| !key.is_mask() && key != ChannelKey::ALPHA && !(0..=2).contains(&key.index()))
    {
        return Err(PsdError::ImageDecode(
            "Smart Object replacement does not support auxiliary channel layouts".to_owned(),
        ));
    }
    if image.channels.raw_channels().any(|(key, _)| key.is_mask()) {
        return Err(PsdError::InvalidData {
            offset: 0,
            message: "decode raw mask channels before replacing a Smart Object",
        });
    }
    Ok(())
}

fn reusable_link(
    links: &[LinkedLayerView<'_>],
    storage: LinkedStorage,
    file_name: &str,
    external_path: Option<&Path>,
    bytes: &[u8],
    file_type: [u8; 4],
) -> Option<String> {
    links
        .iter()
        .find(|record| {
            if record.file_type != file_type || record.file_name.value() != file_name {
                return false;
            }
            match storage {
                LinkedStorage::Embedded => {
                    record.kind == LinkedDataKind::Data && record.raw_file_bytes == bytes
                }
                LinkedStorage::External => {
                    if record.kind != LinkedDataKind::External {
                        return false;
                    }
                    let Some(path) = external_path else {
                        return false;
                    };
                    record.file_size == bytes.len() as u64 && source_link_path_match(record, path)
                }
            }
        })
        .map(|record| record.unique_id.value().to_owned())
}

fn fresh_uuid(links: &[LinkedLayerView<'_>]) -> String {
    loop {
        let candidate = Uuid::new_v4().to_string();
        if !links
            .iter()
            .any(|record| record.unique_id.value() == candidate)
        {
            return candidate;
        }
    }
}

fn new_descriptor(name: &str, class: &str) -> Result<Descriptor> {
    Ok(Descriptor {
        name: UnicodeString::new(name, 1)?,
        class_id: DescriptorKey::new(class),
        items: Vec::new(),
    })
}

fn new_linked_record(
    unique_id: &str,
    file_name: &str,
    file_type: [u8; 4],
    encoded_source: &[u8],
    storage: LinkedStorage,
    external_path: Option<&Path>,
) -> Result<LinkedLayer> {
    let mut file_open = new_descriptor("null", "null")?;
    let mut comp_info = new_descriptor("null", "null")?;
    comp_info.insert("compID", DescriptorValue::Integer(-1));
    comp_info.insert("originalCompID", DescriptorValue::Integer(-1));
    file_open.insert("compInfo", DescriptorValue::Descriptor(comp_info));

    let (kind, linked_file_descriptor, raw_file_bytes, file_size) = match storage {
        LinkedStorage::Embedded => (LinkedDataKind::Data, None, encoded_source.to_vec(), 0),
        LinkedStorage::External => {
            let path = external_path.ok_or(PsdError::InvalidData {
                offset: 0,
                message: "external linked record requires a filesystem path",
            })?;
            let mut descriptor = new_descriptor("ExternalFileLink", "ExternalFileLink")?;
            descriptor.insert("descVersion", DescriptorValue::Integer(2));
            descriptor.insert(
                "Nm  ",
                DescriptorValue::String(UnicodeString::new(file_name, 2)?),
            );
            let native_path = path.to_string_lossy().into_owned();
            let full_path = path_to_file_uri(path);
            // The final PSD output path is not known to this mutation API, so
            // retain the basename as a relative hint and keep originalPath
            // absolute. A caller moving the PSD can still rewrite this hint.
            let relative_path = file_name.to_owned();
            descriptor.insert(
                "fullPath",
                DescriptorValue::String(UnicodeString::new(&full_path, 2)?),
            );
            descriptor.insert(
                "originalPath",
                DescriptorValue::String(UnicodeString::new(&native_path, 2)?),
            );
            descriptor.insert(
                "relPath",
                DescriptorValue::String(UnicodeString::new(&relative_path, 2)?),
            );
            (
                LinkedDataKind::External,
                Some(descriptor),
                Vec::new(),
                encoded_source.len() as u64,
            )
        }
    };

    Ok(LinkedLayer {
        kind,
        version: 7,
        unique_id: PascalString::new(unique_id, 1),
        file_name: UnicodeString::new(file_name, 2)?,
        file_type,
        file_creator: 0,
        file_open_descriptor: Some(file_open),
        linked_file_descriptor,
        content_id: None,
        date: None,
        raw_file_bytes,
        child_document_id: Some(UnicodeString::new(Uuid::new_v4().to_string(), 2)?),
        asset_mod_time: Some(0.0),
        asset_is_locked: Some(false),
        file_size,
    })
}

fn path_to_file_uri(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    if path.starts_with('/') {
        format!("file://{path}")
    } else {
        format!("file:///{path}")
    }
}

fn append_linked_record<T: BitDepth>(
    document: &mut LayeredFile<T>,
    storage: LinkedStorage,
    record: &LinkedLayer,
) -> Result<()> {
    let key = match storage {
        LinkedStorage::Embedded => TaggedBlockKey::new(*b"lnk2"),
        LinkedStorage::External => TaggedBlockKey::new(*b"lnkE"),
    };
    if let Some(blocks) = document.document_blocks.as_mut() {
        if let Some(block) = blocks.get_mut(key) {
            return LinkedLayerTaggedBlock::append_record_preserving_bytes_in_place(
                &mut block.data,
                record,
            );
        }
    }

    let mut writer = BeWriter::new();
    record.write(&mut writer)?;
    let tagged_block = TaggedBlock::new(key, writer.into_inner());
    if document.document_blocks.is_none() {
        let mut blocks = AdditionalLayerInfo::new();
        blocks
            .blocks
            .try_reserve(1)
            .map_err(|_| PsdError::InvalidData {
                offset: 0,
                message: "unable to reserve memory for linked-layer block",
            })?;
        blocks.push(tagged_block);
        document.document_blocks = Some(blocks);
        return Ok(());
    }
    let blocks = document.document_blocks.as_mut().expect("checked above");
    blocks
        .blocks
        .try_reserve(1)
        .map_err(|_| PsdError::InvalidData {
            offset: 0,
            message: "unable to reserve memory for linked-layer block",
        })?;
    blocks.push(tagged_block);
    Ok(())
}

fn rewrite_placed_data(
    payload: &[u8],
    link_id: &str,
    placed_id: &str,
    width: usize,
    height: usize,
) -> Result<Vec<u8>> {
    let (mut placed, trailing) = PlacedLayerData::read_with_trailing(&mut BeReader::new(payload))?;
    let descriptor = &mut placed.descriptor;
    descriptor.insert(
        "Idnt",
        DescriptorValue::String(UnicodeString::new(link_id, 2)?),
    );
    descriptor.insert(
        "placed",
        DescriptorValue::String(UnicodeString::new(placed_id, 2)?),
    );
    let mut size = descriptor
        .get("Sz  ")
        .and_then(DescriptorValue::as_descriptor)
        .cloned()
        .unwrap_or(new_descriptor("Pnt ", "Pnt ")?);
    // Deviation from upstream: upstream leaves the original `Sz  ` size
    // descriptor untouched; the port rewrites it so the record reflects the
    // replacement's dimensions (Photoshop recomputes it on load anyway).
    size.insert("Wdth", DescriptorValue::Double(width as f64));
    size.insert("Hght", DescriptorValue::Double(height as f64));
    descriptor.insert("Sz  ", DescriptorValue::Descriptor(size));

    let mut writer = BeWriter::new();
    placed.write(&mut writer)?;
    writer.bytes(&trailing);
    Ok(writer.into_inner())
}

fn rewrite_placed_id(payload: &[u8], link_id: &str) -> Result<Vec<u8>> {
    let (mut placed, trailing) = PlacedLayer::read_with_trailing(&mut BeReader::new(payload))?;
    placed.unique_id = PascalString::new(link_id, 1);
    let mut writer = BeWriter::new();
    placed.write(&mut writer)?;
    writer.bytes(&trailing);
    Ok(writer.into_inner())
}

fn update_layer_link_identity(
    blocks: &mut AdditionalLayerInfo,
    link_id: &str,
    placed_id: &str,
    width: usize,
    height: usize,
) -> Result<()> {
    let mut modern_count = 0;
    for key in [*b"SoLd", *b"SoLE"] {
        if let Some(block) = blocks.get_mut(TaggedBlockKey::new(key)) {
            block.data = rewrite_placed_data(&block.data, link_id, placed_id, width, height)?;
            modern_count += 1;
        }
    }
    let mut legacy_count = 0;
    for key in [*b"PlLd", *b"plLd"] {
        if let Some(block) = blocks.get_mut(TaggedBlockKey::new(key)) {
            block.data = rewrite_placed_id(&block.data, link_id)?;
            legacy_count += 1;
        }
    }
    if modern_count == 0 && legacy_count == 0 {
        return Err(PsdError::InvalidData {
            offset: 0,
            message: "smart-object layer has no placed-layer record to update",
        });
    }
    Ok(())
}

fn source_link_path_match(record: &LinkedLayerView<'_>, source_path: &Path) -> bool {
    record
        .linked_file_descriptor
        .as_ref()
        .and_then(|descriptor| descriptor.get("originalPath"))
        .and_then(DescriptorValue::as_str)
        .is_some_and(|stored| path_from_link_descriptor(stored) == source_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use psd_core::{
        BitDepth as CoreBitDepth, ColorModeData, FileHeader, ImageData, ImageResources,
        LayerAndMaskInformation,
    };

    fn photoshop_source(
        version: Version,
        depth: CoreBitDepth,
        width: u32,
        height: u32,
        channels: u16,
        raw_pixels: Option<&[u8]>,
    ) -> Vec<u8> {
        let file = PhotoshopFile {
            header: FileHeader::new(version, channels, width, height, depth, ColorMode::Rgb)
                .unwrap(),
            color_mode_data: ColorModeData::default(),
            image_resources: ImageResources::new(),
            layer_and_mask_info: LayerAndMaskInformation::default(),
            image_data: ImageData::new(channels),
        };
        let mut writer = BeWriter::new();
        file.write(&mut writer).unwrap();
        let mut bytes = writer.into_inner();

        if let Some(raw_pixels) = raw_pixels {
            let mut reader = BeReader::new(&bytes);
            PhotoshopFile::read(&mut reader).unwrap();
            bytes.truncate(reader.position());
            bytes.extend_from_slice(&0u16.to_be_bytes());
            bytes.extend_from_slice(raw_pixels);
        }
        bytes
    }

    #[test]
    fn smart_object_replacement_rejects_undecoded_mask_channels() {
        let mut layer =
            Layer::<u8>::new_image("Masked smart object", crate::layer::Rect::default());
        layer.image_mut().unwrap().channels.insert_raw(
            ChannelKey::USER_MASK,
            crate::channels::RawChannelData::new(
                psd_core::Compression::Raw,
                vec![255],
                1,
                1,
                Version::Psd,
            ),
        );

        assert!(matches!(
            ensure_supported_target_channels(&layer),
            Err(PsdError::InvalidData {
                message: "decode raw mask channels before replacing a Smart Object",
                ..
            })
        ));
    }

    /// A one-pixel RGB document with a transparent layer whose composite is
    /// replaced by `raw_pixels` (four planes). `saved_alpha_channel` writes the
    /// fourth channel as a saved alpha channel (positive layer count) instead
    /// of merged transparency (negative count).
    fn layered_source(saved_alpha_channel: bool, raw_pixels: &[u8]) -> Vec<u8> {
        let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 1, 1).unwrap();
        if saved_alpha_channel {
            document.num_channels = 4;
        }
        let mut layer = Layer::new_image("Layer", crate::layer::Rect::new(0, 0, 1, 1));
        let image = layer.image_mut().unwrap();
        for index in 0..3 {
            image.set_channel(ChannelKey::color(index), vec![7]);
        }
        image.set_channel(ChannelKey::ALPHA, vec![128]);
        document.add_layer(layer);
        let mut bytes = document.to_bytes().unwrap();
        let mut reader = BeReader::new(&bytes);
        let file = PhotoshopFile::read(&mut reader).unwrap();
        assert_eq!(file.header.num_channels, 4);
        assert_eq!(
            file.layer_and_mask_info.layer_info.has_merged_alpha,
            !saved_alpha_channel
        );
        let composite = reader.position();
        bytes.truncate(composite);
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(raw_pixels);
        bytes
    }

    #[test]
    fn linked_psd_transparency_requires_the_merged_alpha_flag() {
        let pixels = [128, 64, 32, 90];
        let transparent = decode_photoshop_preview::<u8>(&layered_source(false, &pixels)).unwrap();
        assert_eq!(
            transparent.raster.channel(ChannelKey::ALPHA),
            Some(&[90][..])
        );

        // A positive layer count makes the fourth plane a saved selection,
        // not transparency: the placed image stays opaque.
        let saved = decode_photoshop_preview::<u8>(&layered_source(true, &pixels)).unwrap();
        assert_eq!(saved.raster.channel(ChannelKey::color(2)), Some(&[32][..]));
        assert!(saved.raster.channel(ChannelKey::ALPHA).is_none());
    }

    #[test]
    fn file_uri_paths_decode_percent_escapes() {
        let path = path_from_link_descriptor("file:///tmp/photo%20final.png");
        assert!(path.to_string_lossy().contains("photo final.png"));
    }

    #[test]
    fn encoded_source_size_limit_rejects_one_byte_over_cap() {
        assert!(check_encoded_source_size(MAX_ENCODED_SOURCE_BYTES).is_ok());
        assert!(matches!(
            check_encoded_source_size(MAX_ENCODED_SOURCE_BYTES + 1),
            Err(PsdError::ImageDecode(message)) if message.contains("512 MiB limit")
        ));
    }

    #[cfg(windows)]
    #[test]
    fn file_uri_unc_authorities_become_unc_paths() {
        let path = path_from_link_descriptor("file://server/share/photo%20final.png");
        assert!(path.is_absolute());
        assert!(path.to_string_lossy().contains("photo final.png"));
    }

    #[test]
    fn linked_psd_raw_composite_decodes_planar_rgb_and_ignores_saved_channels() {
        // Without layers there is no layer count to flag transparency, so the
        // fourth and fifth planes are saved channels (Photoshop's reading).
        let bytes = photoshop_source(
            Version::Psd,
            CoreBitDepth::Eight,
            1,
            1,
            5,
            Some(&[128, 64, 32, 255, 9]),
        );
        let decoded = decode_photoshop_preview::<u8>(&bytes).unwrap();
        assert_eq!(decoded.file_type, *b"8BPS");
        assert_eq!((decoded.raster.width(), decoded.raster.height()), (1, 1));
        assert_eq!(
            decoded.raster.channel(ChannelKey::color(0)),
            Some(&[128][..])
        );
        assert_eq!(
            decoded.raster.channel(ChannelKey::color(1)),
            Some(&[64][..])
        );
        assert_eq!(
            decoded.raster.channel(ChannelKey::color(2)),
            Some(&[32][..])
        );
        assert!(decoded.raster.channel(ChannelKey::ALPHA).is_none());
    }

    #[test]
    fn linked_psb_rle_composite_decodes_at_matching_bit_depth() {
        let bytes = photoshop_source(Version::Psb, CoreBitDepth::Sixteen, 2, 1, 3, None);
        let decoded = decode_photoshop_preview::<u16>(&bytes).unwrap();
        assert_eq!(decoded.file_type, *b"8BPB");
        assert_eq!((decoded.raster.width(), decoded.raster.height()), (2, 1));
        for channel in 0..3 {
            assert_eq!(
                decoded.raster.channel(ChannelKey::color(channel)),
                Some(&[0, 0][..])
            );
        }
        assert!(decoded.raster.channel(ChannelKey::ALPHA).is_none());
    }

    #[test]
    fn linked_photoshop_preview_rejects_depth_mismatch_and_zip() {
        let sixteen_bit = photoshop_source(Version::Psd, CoreBitDepth::Sixteen, 1, 1, 3, None);
        assert!(matches!(
            decode_photoshop_preview::<u8>(&sixteen_bit),
            Err(PsdError::UnsupportedBitDepth(16))
        ));

        let raw = photoshop_source(Version::Psd, CoreBitDepth::Eight, 1, 1, 3, None);
        let mut reader = BeReader::new(&raw);
        PhotoshopFile::read(&mut reader).unwrap();
        let mut zip = raw[..reader.position()].to_vec();
        zip.extend_from_slice(&2u16.to_be_bytes());
        zip.extend_from_slice(&[0u8; 4]);
        assert!(matches!(
            decode_photoshop_preview::<u8>(&zip),
            Err(PsdError::ImageDecode(_))
        ));
    }

    #[cfg(feature = "image")]
    #[test]
    fn jpeg_png_raster_conversion_supports_each_bit_depth() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/documents/SmartObjects/reference/control.png");
        let bytes = read_source_file(&path).unwrap();
        let u8_source = decode_source_raster::<u8>(&bytes).unwrap();
        let u16_source = decode_source_raster::<u16>(&bytes).unwrap();
        let f32_source = decode_source_raster::<f32>(&bytes).unwrap();
        assert_eq!(u8_source.file_type, *b"png ");
        assert_eq!(
            (u8_source.raster.width(), u8_source.raster.height()),
            (u16_source.raster.width(), u16_source.raster.height())
        );

        for key in [
            ChannelKey::color(0),
            ChannelKey::color(1),
            ChannelKey::color(2),
            ChannelKey::ALPHA,
        ] {
            let u8_channel = u8_source.raster.channel(key).unwrap();
            let u16_channel = u16_source.raster.channel(key).unwrap();
            let f32_channel = f32_source.raster.channel(key).unwrap();
            for ((sample8, sample16), sample32) in
                u8_channel.iter().zip(u16_channel).zip(f32_channel)
            {
                assert_eq!(u16::from(*sample8) * 257, *sample16);
                assert!((f32::from(*sample8) / 255.0 - *sample32).abs() < 1.0e-6);
            }
        }
    }

    #[cfg(feature = "image")]
    #[test]
    fn grayscale_sources_decode_as_neutral_rgb() {
        let encode = |image: image::DynamicImage| {
            let mut bytes = Vec::new();
            image
                .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
                .unwrap();
            bytes
        };
        let gray = [0u8, 77, 200, 255];
        let luma = image::GrayImage::from_raw(2, 2, gray.to_vec()).unwrap();
        let luma_alpha =
            image::GrayAlphaImage::from_raw(2, 2, vec![0, 10, 77, 128, 200, 255, 255, 0]).unwrap();
        let wide =
            image::ImageBuffer::<image::Luma<u16>, _>::from_raw(2, 1, vec![0u16, 40_000]).unwrap();

        let decoded = decode_source_raster::<u8>(&encode(luma.into())).unwrap();
        for index in 0..3 {
            assert_eq!(
                decoded.raster.channel(ChannelKey::color(index)).unwrap(),
                gray
            );
        }
        assert_eq!(decoded.raster.channel(ChannelKey::ALPHA).unwrap(), [255; 4]);

        let decoded = decode_source_raster::<u8>(&encode(luma_alpha.into())).unwrap();
        assert_eq!(
            decoded.raster.channel(ChannelKey::color(1)).unwrap(),
            [0, 77, 200, 255]
        );
        assert_eq!(
            decoded.raster.channel(ChannelKey::ALPHA).unwrap(),
            [10, 128, 255, 0]
        );

        let decoded = decode_source_raster::<u16>(&encode(wide.into())).unwrap();
        assert_eq!(
            decoded.raster.channel(ChannelKey::color(2)).unwrap(),
            [0, 40_000]
        );
    }

    #[cfg(feature = "image")]
    #[test]
    fn oversized_png_decoder_buffer_is_rejected_before_decode() {
        assert!(matches!(
            checked_image_decode_samples::<u8>(10_000, 10_000, 8),
            Err(PsdError::ImageDecode(message))
                if message.contains("image buffer exceeds")
        ));
    }

    /// Decodes a PNG with `psd-png` into the same `DecodedSource` shape the `image`-crate
    /// path produces, so the two can be compared byte for byte.
    ///
    /// `psd-png` is always asked for 16-bit RGBA and the result goes through
    /// `interleaved_to_planar`, which is the port's own conversion. That keeps the comparison
    /// to the only thing that differs — which decoder produced the pixels — and leaves every
    /// bit-depth decision in the port's `BitDepth` impl where it already lives.
    ///
    /// Asking for 8-bit instead would not be equivalent. `psd-png` narrows a 16-bit source by
    /// keeping the high byte; the port narrows by `round(v / 257)`. The two disagree by one
    /// least-significant bit on much of the range, and the corpus is entirely 8-bit, so
    /// nothing else in the suite would notice.
    ///
    /// A 16-bit converted row is a byte buffer of big-endian samples rather than a `u16`
    /// buffer, so it is deserialised here. The production path would do this with the
    /// workspace's bulk byte-order helper instead of a per-sample conversion.
    #[cfg(feature = "image")]
    fn decode_raster_png_psd_png<T: BitDepth>(bytes: &[u8]) -> Result<DecodedSource<T>> {
        let info =
            psd_png::read_info(bytes).map_err(|error| PsdError::ImageDecode(error.to_string()))?;
        let sample_count = checked_raster_sample_count::<T>(info.width, info.height)?;
        let mut wide_bytes = Vec::new();
        wide_bytes
            .try_reserve_exact(sample_count.saturating_mul(4).saturating_mul(2))
            .map_err(|error| {
                PsdError::ImageDecode(format!("could not allocate decoded pixels: {error}"))
            })?;
        psd_png::Decoder::new()
            .max_decompressed_size(Some(MAX_ENCODED_SOURCE_BYTES as usize))
            .decode_to_rgba16(bytes, |row| {
                wide_bytes.extend_from_slice(row.bytes);
                Ok::<(), psd_png::Error>(())
            })
            .map_err(|error| PsdError::ImageDecode(error.to_string()))?;
        let pixels: Vec<u16> = wide_bytes
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect();
        let raster =
            interleaved_to_planar::<T, u16>(pixels, 4, info.width, info.height, sample_count)?;
        Ok(DecodedSource {
            raster,
            file_type: *b"png ",
        })
    }

    #[cfg(feature = "image")]
    fn assert_same_raster<T: BitDepth + std::fmt::Debug>(
        image_crate: &Raster<T>,
        psd_png: &Raster<T>,
        what: &str,
    ) {
        assert_eq!(
            (image_crate.width(), image_crate.height()),
            (psd_png.width(), psd_png.height()),
            "{what}: dimensions differ"
        );
        for (channel_index, key) in [
            ChannelKey::color(0),
            ChannelKey::color(1),
            ChannelKey::color(2),
            ChannelKey::ALPHA,
        ]
        .into_iter()
        .enumerate()
        {
            let left = image_crate.channel(key).unwrap();
            let right = psd_png.channel(key).unwrap();
            assert_eq!(
                left.len(),
                right.len(),
                "{what}: channel {channel_index} length differs"
            );
            for (sample_index, (a, b)) in left.iter().zip(right.iter()).enumerate() {
                assert_eq!(
                    a, b,
                    "{what}: channel {channel_index} sample {sample_index} differs"
                );
            }
        }
    }

    #[cfg(feature = "image")]
    fn assert_png_parity<T: BitDepth + std::fmt::Debug>(bytes: &[u8], what: &str) {
        let image_crate = decode_source_raster::<T>(bytes)
            .unwrap_or_else(|error| panic!("{what}: image-crate path failed: {error}"));
        let psd_png = decode_raster_png_psd_png::<T>(bytes)
            .unwrap_or_else(|error| panic!("{what}: psd-png path failed: {error}"));
        assert_eq!(
            image_crate.file_type, *b"png ",
            "{what}: unexpected file type"
        );
        assert_eq!(psd_png.file_type, *b"png ", "{what}: unexpected file type");
        assert_same_raster(&image_crate.raster, &psd_png.raster, what);
    }

    #[cfg(feature = "image")]
    fn corpus_pngs() -> Vec<std::path::PathBuf> {
        fn walk(dir: &Path, found: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, found);
                } else if path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
                {
                    found.push(path);
                }
            }
        }
        let mut found = Vec::new();
        walk(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures"),
            &mut found,
        );
        found.sort();
        found
    }

    /// The gate for swapping the smart-object PNG decoder over: the two decoders must agree
    /// byte for byte, at every bit depth the port supports, on every PNG in the corpus.
    #[cfg(feature = "image")]
    #[test]
    fn psd_png_agrees_with_the_image_crate_on_every_corpus_png() {
        let pngs = corpus_pngs();
        assert!(
            !pngs.is_empty(),
            "no PNG fixtures found: the parity gate would pass without comparing anything"
        );
        for path in &pngs {
            let bytes = read_source_file(path).unwrap();
            let what = path.display().to_string();
            assert_png_parity::<u8>(&bytes, &format!("{what} as u8"));
            assert_png_parity::<u16>(&bytes, &format!("{what} as u16"));
            assert_png_parity::<f32>(&bytes, &format!("{what} as f32"));
        }
    }

    /// Greyscale is the case the port deliberately treats differently from upstream: it is
    /// placed as neutral RGB, the way Photoshop places it. `psd-png` has to agree with that
    /// rather than with the file's own single plane.
    #[cfg(feature = "image")]
    #[test]
    fn psd_png_places_grayscale_as_neutral_rgb() {
        let encode = |image: image::DynamicImage| {
            let mut bytes = Vec::new();
            image
                .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
                .unwrap();
            bytes
        };
        let gray = [0u8, 77, 200, 255];
        let luma = image::GrayImage::from_raw(2, 2, gray.to_vec()).unwrap();
        let bytes = encode(luma.into());

        assert_png_parity::<u8>(&bytes, "8-bit greyscale as u8");
        assert_png_parity::<u16>(&bytes, "8-bit greyscale as u16");

        let decoded = decode_raster_png_psd_png::<u8>(&bytes).unwrap();
        for index in 0..3 {
            assert_eq!(
                decoded.raster.channel(ChannelKey::color(index)).unwrap(),
                gray,
                "colour channel {index} is not neutral grey"
            );
        }
        assert_eq!(decoded.raster.channel(ChannelKey::ALPHA).unwrap(), [255; 4]);

        let luma_alpha =
            image::GrayAlphaImage::from_raw(2, 2, vec![0, 10, 77, 128, 200, 255, 255, 0]).unwrap();
        let bytes = encode(luma_alpha.into());
        assert_png_parity::<u8>(&bytes, "8-bit greyscale+alpha as u8");
        let decoded = decode_raster_png_psd_png::<u8>(&bytes).unwrap();
        assert_eq!(
            decoded.raster.channel(ChannelKey::ALPHA).unwrap(),
            [10, 128, 255, 0]
        );
    }

    /// A 16-bit source narrowed into an 8-bit document is the one place the two decoders can
    /// silently disagree, and the corpus cannot catch it: every PNG in `fixtures/` is 8-bit.
    /// `51_200` is one of the values where the two plausible rules part company —
    /// `round(51200 / 257)` is 199, while keeping the high byte is 200.
    #[cfg(feature = "image")]
    #[test]
    fn sixteen_bit_sources_keep_the_ports_narrowing_rule() {
        let encode = |image: image::DynamicImage| {
            let mut bytes = Vec::new();
            image
                .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
                .unwrap();
            bytes
        };
        let samples = [0u16, 40_000, 51_200, 65_535];
        let wide =
            image::ImageBuffer::<image::Luma<u16>, _>::from_raw(4, 1, samples.to_vec()).unwrap();
        let bytes = encode(wide.into());

        assert_png_parity::<u8>(&bytes, "16-bit greyscale as u8");
        assert_png_parity::<u16>(&bytes, "16-bit greyscale as u16");
        assert_png_parity::<f32>(&bytes, "16-bit greyscale as f32");

        // Pinned rather than merely compared: this is the rule a future decoder swap would
        // change, and parity alone would not say which side moved.
        let decoded = decode_raster_png_psd_png::<u8>(&bytes).unwrap();
        assert_eq!(
            decoded.raster.channel(ChannelKey::color(0)).unwrap(),
            [0, 156, 199, 255]
        );
        let decoded = decode_raster_png_psd_png::<u16>(&bytes).unwrap();
        assert_eq!(
            decoded.raster.channel(ChannelKey::color(0)).unwrap(),
            samples
        );
    }

    /// Counts the largest single allocation and the total requested while a counter window is
    /// open. The largest is the number that matters here: the `image` crate allocates the whole
    /// decoded image in one buffer, where a streaming decode allocates a bounded stage.
    #[cfg(feature = "image")]
    mod allocation {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::sync::atomic::{AtomicUsize, Ordering};

        pub static LARGEST: AtomicUsize = AtomicUsize::new(0);
        pub static TOTAL: AtomicUsize = AtomicUsize::new(0);

        pub struct Counting;

        // SAFETY: every method forwards to `System` unchanged; the counters are atomics that
        // only observe, so the allocator's contract is untouched.
        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                record(layout.size());
                unsafe { System.alloc(layout) }
            }

            unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
                unsafe { System.dealloc(pointer, layout) }
            }

            unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
                record(new_size);
                unsafe { System.realloc(pointer, layout, new_size) }
            }

            unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
                record(layout.size());
                unsafe { System.alloc_zeroed(layout) }
            }
        }

        fn record(size: usize) {
            TOTAL.fetch_add(size, Ordering::Relaxed);
            LARGEST.fetch_max(size, Ordering::Relaxed);
        }

        /// Runs `body` with the counters open, returning its result alongside what it allocated.
        pub fn measure<R>(body: impl FnOnce() -> R) -> (R, usize, usize) {
            LARGEST.store(0, Ordering::Relaxed);
            TOTAL.store(0, Ordering::Relaxed);
            let result = body();
            (
                result,
                LARGEST.load(Ordering::Relaxed),
                TOTAL.load(Ordering::Relaxed),
            )
        }
    }

    #[cfg(feature = "image")]
    #[global_allocator]
    static ALLOCATION_COUNTER: allocation::Counting = allocation::Counting;

    /// The shape the production switch would use: the destination channels are allocated once
    /// and each reconstructed row is de-interleaved straight into them, so no interleaved
    /// image ever exists. Narrowing goes through `from_f32` exactly as `interleaved_to_planar`
    /// does, so all three benchmark arms pay the same per-sample conversion cost and the
    /// comparison isolates the decoder and the pipeline shape.
    #[cfg(feature = "image")]
    fn decode_raster_png_streaming<T: BitDepth>(bytes: &[u8]) -> Result<DecodedSource<T>> {
        let info =
            psd_png::read_info(bytes).map_err(|error| PsdError::ImageDecode(error.to_string()))?;
        let sample_count = checked_raster_sample_count::<T>(info.width, info.height)?;
        let mut channels = Vec::new();
        for _ in 0..4 {
            let mut channel = Vec::new();
            channel.try_reserve_exact(sample_count).map_err(|error| {
                PsdError::ImageDecode(format!("could not allocate decoded channel: {error}"))
            })?;
            channels.push(channel);
        }
        let [mut red, mut green, mut blue, mut alpha] = <[Vec<T>; 4]>::try_from(channels)
            .map_err(|_| PsdError::ImageDecode("expected four decoded channels".to_owned()))?;
        psd_png::Decoder::new()
            .max_decompressed_size(Some(MAX_ENCODED_SOURCE_BYTES as usize))
            .decode_to_rgba16(bytes, |row| {
                // A 16-bit converted row is big-endian bytes, four samples per pixel.
                for pixel in row.bytes.chunks_exact(8) {
                    let wide = |offset: usize| {
                        u16::from_be_bytes([pixel[offset], pixel[offset + 1]]).to_f32()
                    };
                    red.push(T::from_f32(wide(0)));
                    green.push(T::from_f32(wide(2)));
                    blue.push(T::from_f32(wide(4)));
                    alpha.push(T::from_f32(wide(6)));
                }
                Ok::<(), psd_png::Error>(())
            })
            .map_err(|error| PsdError::ImageDecode(error.to_string()))?;
        let raster = raster_from_planar(
            info.width as usize,
            info.height as usize,
            vec![
                (ChannelKey::color(0), red),
                (ChannelKey::color(1), green),
                (ChannelKey::color(2), blue),
                (ChannelKey::ALPHA, alpha),
            ],
        )?;
        Ok(DecodedSource {
            raster,
            file_type: *b"png ",
        })
    }

    /// V2: what the two decoders cost in the port's own pipeline, on the port's own corpus.
    ///
    /// Four arms, because two questions are being asked at once. `image` against `psd-png`
    /// through the parity helper answers whether the decoder itself is faster, with the
    /// pipeline shape held fixed on both sides. `psd-png` streaming answers the other half:
    /// whether routing rows straight into the destination channels, which is what the
    /// production switch would do, is worth anything on top. The fourth arm re-runs `image` as
    /// a control, so the day's noise floor is measured rather than assumed — this machine
    /// throttles, and several of these deltas are small enough that an unmeasured floor would
    /// be meaningless.
    ///
    /// Memory is reported as the largest single allocation during a decode, which is the
    /// number that separates the two designs: the `image` crate allocates the whole decoded
    /// image in one buffer, a streaming decode allocates a bounded stage. It is not peak
    /// resident memory and is not labelled as such.
    ///
    /// Ignored by default: this is a measurement, not an assertion, and it takes minutes.
    #[cfg(feature = "image")]
    #[test]
    #[ignore = "benchmark; run with --release -- --ignored --nocapture"]
    fn benchmark_image_crate_against_psd_png() {
        const ROUNDS: usize = 5;
        const ARMS: usize = 4;

        let files: Vec<(String, Vec<u8>)> = corpus_pngs()
            .into_iter()
            .map(|path| {
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                (name, read_source_file(&path).unwrap())
            })
            .collect();
        assert!(!files.is_empty(), "no PNG fixtures found");

        type Arm = fn(&[u8]) -> Result<DecodedSource<u8>>;
        let arms: [Arm; ARMS] = [
            decode_source_raster::<u8>,
            decode_raster_png_psd_png::<u8>,
            decode_raster_png_streaming::<u8>,
            decode_source_raster::<u8>, // control: the same binary as arm 0
        ];

        for (_, bytes) in &files {
            for arm in arms {
                arm(bytes).unwrap();
            }
        }

        let mut best = vec![[f64::MAX; ARMS]; files.len()];
        for _ in 0..ROUNDS {
            for (index, (_, bytes)) in files.iter().enumerate() {
                for (slot, arm) in arms.iter().enumerate() {
                    let start = std::time::Instant::now();
                    let decoded = arm(bytes).unwrap();
                    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                    std::hint::black_box(&decoded);
                    best[index][slot] = best[index][slot].min(elapsed);
                }
            }
        }

        // Allocation is counted in its own pass so the counters never sit inside a timed one.
        let mut largest = vec![[0usize; 3]; files.len()];
        for (index, (_, bytes)) in files.iter().enumerate() {
            for (slot, arm) in arms[..3].iter().enumerate() {
                let (_, peak, _) = allocation::measure(|| arm(bytes).unwrap());
                largest[index][slot] = peak;
            }
        }

        println!(
            "\n{:<32} {:>9} {:>9} {:>10} {:>9} {:>9} {:>8} {:>17}",
            "fixture",
            "image",
            "parity",
            "streaming",
            "parity d",
            "stream d",
            "ctrl d",
            "largest MiB img/psd"
        );
        let mut totals = [0.0f64; ARMS];
        for (index, (name, _)) in files.iter().enumerate() {
            let row = best[index];
            let (image, parity, streaming, control) = (row[0], row[1], row[2], row[3]);
            let percent = |value: f64| (value - image) / image * 100.0;
            println!(
                "{:<32} {:>9.3} {:>9.3} {:>10.3} {:>8.1}% {:>8.1}% {:>7.1}% {:>7.2}/{:.2}",
                name,
                image,
                parity,
                streaming,
                percent(parity),
                percent(streaming),
                percent(control),
                largest[index][0] as f64 / (1024.0 * 1024.0),
                largest[index][2] as f64 / (1024.0 * 1024.0),
            );
            for (slot, value) in row.iter().enumerate() {
                totals[slot] += value;
            }
        }
        let (image, parity, streaming, control) = (totals[0], totals[1], totals[2], totals[3]);
        let percent = |value: f64| (value - image) / image * 100.0;
        println!(
            "{:<32} {:>9.3} {:>9.3} {:>10.3} {:>8.1}% {:>8.1}% {:>7.1}%",
            "TOTAL",
            image,
            parity,
            streaming,
            percent(parity),
            percent(streaming),
            percent(control),
        );
    }
}
