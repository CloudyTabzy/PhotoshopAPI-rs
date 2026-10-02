//! The layer tree: arena-allocated layers with enum-kinded payloads.
//!
//! Reshapes upstream's `shared_ptr` graph + `dynamic_pointer_cast` dispatch
//! dispatch: [`LayeredFile`](crate::LayeredFile) owns a slot arena of
//! layers; parent/child relationships are index lists, and layer kinds are an
//! enum match.
//!
//! Upstream's `MaskMixin` and the per-layer settings of `Layer.h` (fill,
//! lock, display color, clipping) become plain methods here; their on-disk
//! state stays in the preserved record fields and tagged blocks so unknown
//! bits round-trip.

use psd_core::artboard::is_artboard_key;
use psd_core::layer_effects::{LayerEffectsData, ModernLayerEffects};
use psd_core::vector::is_vector_mask_key;
use psd_core::Artboard;
use psd_core::{
    effects_block_data, legacy_effects_block_data, AdditionalLayerInfo, AdjustmentBlock,
    AdjustmentKind, BeReader, BlendMode, Compression, Descriptor, LayerBlendingRanges, LayerColor,
    LayerEffects, LayerEffectsBlock, LayerFlags, LayerMask, LayerMaskData, LayerMaskFlags,
    PlacedLayer, PlacedLayerData, PsdError, Result, SectionDivider, TaggedBlock, TaggedBlockKey,
    VectorBlock, VectorMask,
};

use crate::bitdepth::BitDepth;
use crate::channels::{ChannelKey, ChannelStore};

/// Index into the document's layer arena. Stable for the lifetime of the
/// document: removing a layer leaves its slot empty and slots are never
/// reused, so an id can never silently start naming a different layer.
pub type LayerId = usize;

/// Pixel bounds of a layer (`top`/`left`/`bottom`/`right`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub top: i32,
    pub left: i32,
    pub bottom: i32,
    pub right: i32,
}

impl Rect {
    pub const fn new(top: i32, left: i32, bottom: i32, right: i32) -> Self {
        Self {
            top,
            left,
            bottom,
            right,
        }
    }

    /// A `width` × `height` rect whose center is `(center_x, center_y)`.
    ///
    /// Uses upstream's `generate_extents` rounding: the top-left corner is
    /// `round(center - size / 2)`, so the size is always exact.
    pub fn centered(center_x: f64, center_y: f64, width: u32, height: u32) -> Result<Self> {
        let left = (center_x - f64::from(width) / 2.0).round();
        let top = (center_y - f64::from(height) / 2.0).round();
        let fits = |value: f64| value.is_finite() && value.abs() <= f64::from(i32::MAX);
        let (width, height) = (i32::try_from(width), i32::try_from(height));
        match (width, height) {
            (Ok(width), Ok(height)) if fits(left) && fits(top) => {
                let (left, top) = (left as i32, top as i32);
                match (left.checked_add(width), top.checked_add(height)) {
                    (Some(right), Some(bottom)) => Ok(Self::new(top, left, bottom, right)),
                    _ => Err(coordinate_overflow()),
                }
            }
            _ => Err(coordinate_overflow()),
        }
    }

    pub const fn width(self) -> i32 {
        self.right - self.left
    }

    pub const fn height(self) -> i32 {
        self.bottom - self.top
    }

    /// Center point in canvas coordinates.
    pub fn center(self) -> (f64, f64) {
        (
            (f64::from(self.left) + f64::from(self.right)) / 2.0,
            (f64::from(self.top) + f64::from(self.bottom)) / 2.0,
        )
    }

    /// The rect moved by `(dx, dy)`, or an error on coordinate overflow.
    pub fn translated(self, dx: i32, dy: i32) -> Result<Self> {
        match (
            self.top.checked_add(dy),
            self.left.checked_add(dx),
            self.bottom.checked_add(dy),
            self.right.checked_add(dx),
        ) {
            (Some(top), Some(left), Some(bottom), Some(right)) => {
                Ok(Self::new(top, left, bottom, right))
            }
            _ => Err(coordinate_overflow()),
        }
    }

    /// Number of samples a channel of this rect holds (0 for degenerate rects).
    pub const fn sample_count(self) -> usize {
        let width = self.width();
        let height = self.height();
        if width <= 0 || height <= 0 {
            0
        } else {
            (width as usize) * (height as usize)
        }
    }
}

fn coordinate_overflow() -> PsdError {
    PsdError::InvalidData {
        offset: 0,
        message: "layer coordinates overflow the i32 range",
    }
}

fn invalid(message: &'static str) -> PsdError {
    PsdError::InvalidData { offset: 0, message }
}

/// `lspf` bit for "lock all" (the high bit of the flag word, upstream's
/// `m_IsLocked`). The partial locks below it are preserved untouched.
const LOCK_ALL: u32 = 0x8000_0000;
/// `lspf` bit for the transparency lock (mirrored in the record flags).
const LOCK_TRANSPARENCY: u32 = 0x0000_0001;

/// The blocks that hold layer effects: the three descriptor forms and the legacy `lrFX`.
fn is_effects_key(key: TaggedBlockKey) -> bool {
    matches!(&key.as_bytes(), b"lfx2" | b"lmfx" | b"lfxs" | b"lrFX")
}

fn vector_block_rank(key: TaggedBlockKey) -> Option<u8> {
    match &key.as_bytes() {
        b"vscg" => Some(0),
        b"vmsk" | b"vsms" => Some(1),
        b"vstk" => Some(2),
        b"vogk" => Some(3),
        _ => None,
    }
}

fn is_layer_metadata_key(key: TaggedBlockKey) -> bool {
    matches!(
        &key.as_bytes(),
        b"luni"
            | b"lnsr"
            | b"lyid"
            | b"cmls"
            | b"clbl"
            | b"infx"
            | b"knko"
            | b"lspf"
            | b"lclr"
            | b"shmd"
            | b"sn2P"
            | b"fxrp"
            | b"lyvr"
    )
}

fn has_vector_mask_block(blocks: &AdditionalLayerInfo) -> bool {
    blocks
        .blocks
        .iter()
        .any(|block| is_vector_mask_key(block.key))
}

fn has_shape_fill_block(blocks: &AdditionalLayerInfo) -> bool {
    blocks.blocks.iter().any(|block| {
        block.key.as_bytes() == *b"vscg"
            || AdjustmentKind::from_key(block.key).is_some_and(AdjustmentKind::is_fill)
    })
}

/// A single layer: metadata plus its kind-specific payload.
#[derive(Debug, Clone, PartialEq)]
pub struct Layer<T: BitDepth> {
    pub name: String,
    pub bounds: Rect,
    /// 0–255.
    pub opacity: u8,
    /// The effective blend mode. For groups this is the `lsct` block's mode
    /// when present (Photoshop stores pass-through there and `norm` on the
    /// record); writing keeps both places in sync.
    pub blend_mode: BlendMode,
    pub flags: LayerFlags,
    /// 0 or 1.
    pub clipping: u8,
    /// Pixel and/or vector mask metadata (mask *pixels* live in the layer's
    /// channel store under [`Layer::pixel_mask_key`]).
    pub mask: Option<LayerMaskData>,
    /// Per-layer tagged blocks, preserved verbatim (the unicode name and
    /// section-divider blocks are also interpreted into `name`/`kind`).
    pub blocks: AdditionalLayerInfo,
    pub blending_ranges: LayerBlendingRanges,
    pub kind: LayerKind<T>,
    /// Write-compression override for this layer's color and alpha channels
    /// (`None` defers to the document's [`LayeredFile::compression`](crate::LayeredFile::compression)).
    pub compression: Option<Compression>,
    /// Write-compression override for mask channels (`None` defers to
    /// [`compression`](Self::compression)).
    pub mask_compression: Option<Compression>,
}

impl<T: BitDepth> Layer<T> {
    pub(crate) fn conversion_footprint<U: BitDepth>(
        &self,
        target_bytes: &mut usize,
        raw_scratch_peak: &mut usize,
    ) -> Result<()> {
        match &self.kind {
            LayerKind::Image(layer) => layer
                .channels
                .conversion_footprint::<U>(target_bytes, raw_scratch_peak),
            LayerKind::Text(layer) => layer
                .channels
                .conversion_footprint::<U>(target_bytes, raw_scratch_peak),
            LayerKind::Adjustment(layer) => layer
                .channels
                .conversion_footprint::<U>(target_bytes, raw_scratch_peak),
            LayerKind::Shape(layer) => layer
                .channels
                .conversion_footprint::<U>(target_bytes, raw_scratch_peak),
            LayerKind::Group(layer) => layer
                .channels
                .conversion_footprint::<U>(target_bytes, raw_scratch_peak),
            LayerKind::SectionDivider(_) => Ok(()),
        }
    }

    pub(crate) fn convert_bit_depth<U: BitDepth>(&self, source_depth: u16) -> Result<Layer<U>> {
        let kind = match &self.kind {
            LayerKind::Image(layer) => LayerKind::Image(ImageLayer {
                channels: layer.channels.convert_bit_depth::<U>(source_depth)?,
            }),
            LayerKind::Text(layer) => LayerKind::Text(TextLayer {
                channels: layer.channels.convert_bit_depth::<U>(source_depth)?,
            }),
            LayerKind::Adjustment(layer) => LayerKind::Adjustment(AdjustmentLayer {
                channels: layer.channels.convert_bit_depth::<U>(source_depth)?,
            }),
            LayerKind::Shape(layer) => LayerKind::Shape(ShapeLayer {
                channels: layer.channels.convert_bit_depth::<U>(source_depth)?,
            }),
            LayerKind::Group(layer) => LayerKind::Group(GroupLayer {
                children: layer.children.clone(),
                open: layer.open,
                channels: layer.channels.convert_bit_depth::<U>(source_depth)?,
            }),
            LayerKind::SectionDivider(divider) => LayerKind::SectionDivider(*divider),
        };
        Ok(Layer {
            name: self.name.clone(),
            bounds: self.bounds,
            opacity: self.opacity,
            blend_mode: self.blend_mode,
            flags: self.flags,
            clipping: self.clipping,
            mask: self.mask.clone(),
            blocks: self.blocks.clone(),
            blending_ranges: self.blending_ranges.clone(),
            kind,
            compression: self.compression,
            mask_compression: self.mask_compression,
        })
    }

    /// Build an empty image layer with the given name and bounds.
    pub fn new_image(name: impl Into<String>, bounds: Rect) -> Self {
        Self {
            name: name.into(),
            bounds,
            opacity: 255,
            blend_mode: BlendMode::NORMAL,
            // Bit 3 marks bit 4 as meaningful; every Photoshop pixel layer
            // carries it (0x08), and a record without it gets legacy
            // semantics from Photoshop.
            flags: LayerFlags::from_bits(LayerFlags::BIT4_USEFUL),
            clipping: 0,
            mask: None,
            blocks: AdditionalLayerInfo::new(),
            blending_ranges: LayerBlendingRanges::default(),
            kind: LayerKind::Image(ImageLayer::new()),
            compression: None,
            mask_compression: None,
        }
    }

    /// Build an open, pass-through group layer with the given name
    /// (Photoshop's default for new groups).
    pub fn new_group(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            bounds: Rect::default(),
            opacity: 255,
            blend_mode: BlendMode::PASSTHROUGH,
            flags: LayerFlags::from_bits(
                LayerFlags::BIT4_USEFUL | LayerFlags::PIXEL_DATA_IRRELEVANT,
            ),
            clipping: 0,
            mask: None,
            blocks: AdditionalLayerInfo::new(),
            blending_ranges: LayerBlendingRanges::default(),
            kind: LayerKind::Group(GroupLayer::new()),
            compression: None,
            mask_compression: None,
        }
    }

    /// Build an adjustment or fill layer with no settings yet.
    ///
    /// Use empty bounds for adjustment layers and canvas-sized bounds for fill
    /// layers. [`LayeredFile::add_adjustment_layer`](crate::LayeredFile::add_adjustment_layer)
    /// chooses those bounds automatically when it receives a typed settings
    /// block.
    pub fn new_adjustment(name: impl Into<String>, bounds: Rect) -> Self {
        let mut layer = Self::new_image(name, bounds);
        layer.flags =
            LayerFlags::from_bits(LayerFlags::BIT4_USEFUL | LayerFlags::PIXEL_DATA_IRRELEVANT);
        layer.kind = LayerKind::Adjustment(AdjustmentLayer::new());
        layer
    }

    /// Build an empty shape layer with the supplied canvas bounds.
    ///
    /// Add its fill and vector data with [`set_adjustment`](Self::set_adjustment)
    /// and [`set_vector_block`](Self::set_vector_block).
    pub fn new_shape(name: impl Into<String>, bounds: Rect) -> Self {
        let mut layer = Self::new_image(name, bounds);
        layer.flags =
            LayerFlags::from_bits(LayerFlags::BIT4_USEFUL | LayerFlags::PIXEL_DATA_IRRELEVANT);
        layer.kind = LayerKind::Shape(ShapeLayer::new());
        layer
    }

    pub fn is_visible(&self) -> bool {
        !self.flags.hidden()
    }

    pub fn set_visible(&mut self, visible: bool) {
        self.flags.set_hidden(!visible);
    }

    pub fn image(&self) -> Option<&ImageLayer<T>> {
        match &self.kind {
            LayerKind::Image(image) => Some(image),
            _ => None,
        }
    }

    pub fn image_mut(&mut self) -> Option<&mut ImageLayer<T>> {
        match &mut self.kind {
            LayerKind::Image(image) => Some(image),
            _ => None,
        }
    }

    pub fn group(&self) -> Option<&GroupLayer<T>> {
        match &self.kind {
            LayerKind::Group(group) => Some(group),
            _ => None,
        }
    }

    pub fn group_mut(&mut self) -> Option<&mut GroupLayer<T>> {
        match &mut self.kind {
            LayerKind::Group(group) => Some(group),
            _ => None,
        }
    }

    pub fn text_layer(&self) -> Option<&TextLayer<T>> {
        match &self.kind {
            LayerKind::Text(text) => Some(text),
            _ => None,
        }
    }

    pub fn text_layer_mut(&mut self) -> Option<&mut TextLayer<T>> {
        match &mut self.kind {
            LayerKind::Text(text) => Some(text),
            _ => None,
        }
    }

    /// The layer's channel store: pixels for image, text, adjustment, and
    /// shape layers, mask channels for groups, and `None` for section dividers.
    pub fn channels(&self) -> Option<&ChannelStore<T>> {
        match &self.kind {
            LayerKind::Image(image) => Some(&image.channels),
            LayerKind::Text(text) => Some(&text.channels),
            LayerKind::Adjustment(adjustment) => Some(&adjustment.channels),
            LayerKind::Shape(shape) => Some(&shape.channels),
            LayerKind::Group(group) => Some(&group.channels),
            LayerKind::SectionDivider(_) => None,
        }
    }

    /// Mutable form of [`channels`](Self::channels).
    pub fn channels_mut(&mut self) -> Option<&mut ChannelStore<T>> {
        match &mut self.kind {
            LayerKind::Image(image) => Some(&mut image.channels),
            LayerKind::Text(text) => Some(&mut text.channels),
            LayerKind::Adjustment(adjustment) => Some(&mut adjustment.channels),
            LayerKind::Shape(shape) => Some(&mut shape.channels),
            LayerKind::Group(group) => Some(&mut group.channels),
            LayerKind::SectionDivider(_) => None,
        }
    }

    /// Whether this layer carries Photoshop text metadata (`TySh` or `Txt2`).
    pub fn is_text_layer(&self) -> bool {
        self.blocks.get(TaggedBlockKey::new(*b"TySh")).is_some()
            || self.blocks.get(TaggedBlockKey::new(*b"Txt2")).is_some()
    }

    /// Whether the layer carries smart-object data (`SoLd`/`SoLE` or `PlLd`).
    pub fn is_smart_object(&self) -> bool {
        self.blocks.get(TaggedBlockKey::new(*b"SoLd")).is_some()
            || self.blocks.get(TaggedBlockKey::new(*b"SoLE")).is_some()
            || self.blocks.get(TaggedBlockKey::new(*b"PlLd")).is_some()
            || self.blocks.get(TaggedBlockKey::new(*b"plLd")).is_some()
    }

    /// This layer's `SoLd`/`SoLE` placed-layer descriptor, parsed on demand
    /// (the raw block is always preserved in [`blocks`](Self::blocks)).
    pub fn smart_object_data(&self) -> Result<Option<PlacedLayerData>> {
        let block = self
            .blocks
            .get(TaggedBlockKey::new(*b"SoLd"))
            .or_else(|| self.blocks.get(TaggedBlockKey::new(*b"SoLE")));
        match block {
            Some(block) => PlacedLayerData::read(&mut BeReader::new(&block.data)).map(Some),
            None => Ok(None),
        }
    }

    /// The legacy `PlLd` placed-layer record (uuid, transform, warp
    /// descriptor), parsed on demand.
    pub fn placed_layer(&self) -> Result<Option<PlacedLayer>> {
        let block = self
            .blocks
            .get(TaggedBlockKey::new(*b"PlLd"))
            .or_else(|| self.blocks.get(TaggedBlockKey::new(*b"plLd")));
        match block {
            Some(block) => PlacedLayer::read(&mut BeReader::new(&block.data)).map(Some),
            None => Ok(None),
        }
    }

    /// Every layer-effects block in source order, parsed on demand. The raw
    /// blocks in [`blocks`](Self::blocks) remain the write source, including
    /// fields that these typed views do not interpret.
    ///
    /// A block that does not parse is skipped with a warning rather than
    /// failing the call: a file with one unreadable effects block still has
    /// its other layers and effects, and the raw block is preserved for
    /// writing either way.
    pub fn effects(&self) -> Result<Vec<LayerEffectsBlock>> {
        Ok(self
            .blocks
            .blocks
            .iter()
            .filter_map(|block| match LayerEffectsBlock::read(block) {
                Ok(Some(effects)) => Some(effects),
                Ok(None) => None,
                Err(error) => {
                    tracing::warn!(
                        "skipping unreadable layer-effects block {:?}: {error}",
                        block.key
                    );
                    None
                }
            })
            .collect())
    }

    /// The layer's effects as a typed, editable set.
    ///
    /// Read from the block Photoshop treats as authoritative: `lmfx` if the layer has one, else
    /// `lfx2`, else `lfxs`. `None` when the layer has none of them; a layer that has only a
    /// legacy `lrFX` block (which Photoshop ignores beside a descriptor block) has no set. A
    /// block that cannot be read is an error here, unlike [`effects`](Self::effects), which
    /// skips it.
    pub fn layer_effects(&self) -> Result<Option<LayerEffects>> {
        let Some(index) = self.effects_block_index() else {
            return Ok(None);
        };
        let descriptor = Self::effects_descriptor(&self.blocks.blocks[index])?;
        Ok(Some(LayerEffects::from_descriptor(&descriptor)))
    }

    /// Make the layer's effects exactly `effects`.
    ///
    /// The layer's existing effects block is edited in place, so what the set does not model
    /// (unknown descriptor items, the form a file used for its effect lists) survives, and a
    /// layer without one gets a new block placed where Photoshop puts it, after the layer's
    /// content block and before its vector-mask and name blocks. That new block is `lmfx` when
    /// the set has more than one instance of a repeatable effect, as Photoshop writes it, and
    /// `lfx2` otherwise. An existing block keeps its key (`lfx2`, `lfxs` or `lmfx`), since
    /// Photoshop-authored files hold several instances in either.
    ///
    /// Setting a layer's own effects back to what it has changes nothing: the block, and the
    /// legacy `lrFX` mirror beside it, are left as they were. Any real change rewrites the
    /// descriptor block and regenerates the `lrFX` mirror right after it, so a reader that
    /// only knows the legacy block sees the edit too. The mirror can hold one shadow, glow,
    /// bevel and colour overlay (the first of each); see
    /// [`legacy_effects_block_data`]. Photoshop ignores
    /// `lrFX` whenever a descriptor block is present.
    ///
    /// An existing block that cannot be read is replaced by a fresh one.
    pub fn set_layer_effects(&mut self, effects: &LayerEffects) -> Result<()> {
        let existing = self.effects_block_index();
        let old_modern = existing.and_then(|i| Self::modern_effects(&self.blocks.blocks[i]).ok());
        let old_root = old_modern.as_ref().map(|modern| &modern.descriptor);
        let root = match old_root {
            Some(old) => {
                let mut root = old.clone();
                effects.apply_to(&mut root);
                root
            }
            None => effects.to_descriptor(),
        };

        // An existing block keeps its key: Photoshop-authored files hold several instances in
        // `lfx2` as well as in `lmfx`, and an edit has no reason to move a layer from one to
        // the other. A new block uses `lmfx` for several instances, as Photoshop does, and
        // `lfx2` otherwise.
        let existing_key = existing.map(|i| self.blocks.blocks[i].key.as_bytes());
        let key = existing_key.unwrap_or(if effects.has_multiple_instances() {
            *b"lmfx"
        } else {
            *b"lfx2"
        });

        // Nothing changed when the same descriptor would be stored under the same key. That is
        // decided on the descriptors and not on the block's bytes: the padding a file put after
        // its descriptor is not ours to normalise.
        if old_root == Some(&root) && existing_key == Some(key) {
            return Ok(());
        }
        // The legacy mirror is derived from what the descriptor now says, not from `effects`:
        // a field the set leaves `None` keeps the value the descriptor has.
        let mirror = legacy_effects_block_data(&LayerEffects::from_descriptor(&root));
        let data = match old_modern {
            Some(mut modern) => {
                modern.descriptor = root;
                modern.to_payload()?
            }
            None => effects_block_data(&root)?,
        };
        let mut replacement = TaggedBlock::new(TaggedBlockKey::new(key), data);
        if let Some(index) = existing {
            replacement.signature = self.blocks.blocks[index].signature;
        }

        let position = existing.unwrap_or_else(|| self.new_effects_block_position());
        self.replace_effects_block(position, replacement, mirror);
        Ok(())
    }

    /// Replace the full modern effects payload (`lfx2`, `lmfx`, or `lfxs`).
    ///
    /// Unknown descriptor fields and trailing bytes are retained, and the
    /// legacy `lrFX` mirror is regenerated from the supplied descriptor. A
    /// byte-identical replacement of the authoritative block leaves all
    /// existing blocks unchanged. Replacing a block under the same key keeps
    /// its original signature. Malformed or legacy payloads are rejected
    /// before any layer data changes.
    pub fn set_layer_effects_block(&mut self, block: &TaggedBlock) -> Result<()> {
        if !matches!(&block.signature, b"8BIM" | b"8B64") {
            return Err(PsdError::InvalidSignature {
                expected: "8BIM or 8B64",
                found: block.signature,
                offset: 0,
            });
        }
        let modern = Self::modern_effects(block)?;
        let existing = self.effects_block_index();
        if existing.is_some_and(|index| {
            let existing = &self.blocks.blocks[index];
            existing.key == block.key && existing.data == block.data
        }) {
            return Ok(());
        }
        let mirror = legacy_effects_block_data(&LayerEffects::from_descriptor(&modern.descriptor));
        let mut replacement = block.clone();
        if let Some(index) = existing {
            if self.blocks.blocks[index].key == block.key {
                replacement.signature = self.blocks.blocks[index].signature;
            }
        }
        let position = existing.unwrap_or_else(|| self.new_effects_block_position());
        self.replace_effects_block(position, replacement, mirror);
        Ok(())
    }

    fn replace_effects_block(
        &mut self,
        position: usize,
        replacement: TaggedBlock,
        mirror: Vec<u8>,
    ) {
        let blocks = &mut self.blocks.blocks;
        let dropped_before = blocks[..position]
            .iter()
            .filter(|b| is_effects_key(b.key))
            .count();
        blocks.retain(|block| !is_effects_key(block.key));
        // `lrFX` follows the descriptor block immediately, as in Photoshop-authored files.
        blocks.insert(
            position - dropped_before,
            TaggedBlock::new(TaggedBlockKey::new(*b"lrFX"), mirror),
        );
        blocks.insert(position - dropped_before, replacement);
    }

    /// Remove every effects block from the layer, descriptor and legacy alike.
    pub fn clear_layer_effects(&mut self) {
        self.blocks
            .blocks
            .retain(|block| !is_effects_key(block.key));
    }

    /// Index of the block that holds the layer's effects: `lmfx`, then `lfx2`, then `lfxs`.
    fn effects_block_index(&self) -> Option<usize> {
        [*b"lmfx", *b"lfx2", *b"lfxs"].into_iter().find_map(|key| {
            self.blocks
                .blocks
                .iter()
                .position(|block| block.key.as_bytes() == key)
        })
    }

    fn effects_descriptor(block: &TaggedBlock) -> Result<Descriptor> {
        Ok(Self::modern_effects(block)?.descriptor)
    }

    fn modern_effects(block: &TaggedBlock) -> Result<ModernLayerEffects> {
        match LayerEffectsBlock::read(block)? {
            Some(LayerEffectsBlock {
                data: LayerEffectsData::Modern(modern),
                ..
            }) => Ok(modern),
            _ => Err(PsdError::InvalidData {
                offset: 0,
                message: "not a descriptor-based layer effects block",
            }),
        }
    }

    /// Where a layer with no effects block gets one: where an existing legacy `lrFX` block is,
    /// else before the first vector-mask, vector-origination or name block, else last.
    fn new_effects_block_position(&self) -> usize {
        let blocks = &self.blocks.blocks;
        blocks
            .iter()
            .position(|block| block.key.as_bytes() == *b"lrFX")
            .or_else(|| {
                blocks.iter().position(|block| {
                    matches!(&block.key.as_bytes(), b"vmsk" | b"vsms" | b"vogk" | b"luni")
                })
            })
            .unwrap_or(blocks.len())
    }

    /// Whether this is an adjustment or fill layer, or it carries a recognized
    /// adjustment settings block (`levl`, `curv`, `SoCo`, ...; see
    /// [`AdjustmentKind`]). Photoshop also stores shape fills in `SoCo`,
    /// `GdFl`, or `PtFl`, so shape layers with those blocks report `true` too.
    pub fn is_adjustment_layer(&self) -> bool {
        matches!(self.kind, LayerKind::Adjustment(_))
            || self.blocks.blocks.iter().any(|block| {
                AdjustmentKind::from_key(block.key).is_some_and(AdjustmentKind::marks_layer)
            })
    }

    /// Every adjustment and fill block in source order, including a `CgEd`
    /// companion block, parsed on demand. The raw blocks in
    /// [`blocks`](Self::blocks) remain the write source, so reading these
    /// views changes no bytes. A malformed or unsupported payload is an error.
    pub fn adjustments(&self) -> Result<Vec<AdjustmentBlock>> {
        self.blocks
            .blocks
            .iter()
            .filter_map(|block| AdjustmentBlock::read(block).transpose())
            .collect()
    }

    /// Read one adjustment or fill settings block by kind.
    pub fn adjustment(&self, kind: AdjustmentKind) -> Result<Option<AdjustmentBlock>> {
        self.blocks
            .blocks
            .iter()
            .find(|block| block.key == kind.key())
            .map(AdjustmentBlock::read)
            .transpose()
            .map(Option::flatten)
    }

    /// Add or replace one typed adjustment, fill, or CgEd companion block.
    ///
    /// Existing blocks keep their position. A new settings block is placed
    /// beside other adjustment settings at the start of the layer's tagged
    /// blocks; CgEd follows the adjustment it accompanies. Unknown blocks are
    /// left in their original order. An image layer is promoted to the
    /// adjustment kind and marked as derived pixel data. A CgEd block requires
    /// an existing adjustment; groups and section dividers cannot carry these
    /// settings.
    pub fn set_adjustment(&mut self, adjustment: &AdjustmentBlock) -> Result<()> {
        let mut block = adjustment.to_tagged_block()?;
        if matches!(
            self.kind,
            LayerKind::Group(_) | LayerKind::SectionDivider(_)
        ) {
            return Err(invalid("adjustment settings need a non-group layer"));
        }
        if adjustment.kind == AdjustmentKind::ContentGenerator && !self.is_adjustment_layer() {
            return Err(invalid("CgEd needs an adjustment or fill layer"));
        }
        if (adjustment.kind.is_fill() || has_shape_fill_block(&self.blocks))
            && has_vector_mask_block(&self.blocks)
            && matches!(self.kind, LayerKind::Text(_))
        {
            return Err(invalid("text layers cannot be promoted to shapes"));
        }
        if (adjustment.kind.marks_layer() || self.is_adjustment_layer())
            && matches!(self.kind, LayerKind::Image(_))
        {
            let old = std::mem::replace(
                &mut self.kind,
                LayerKind::Adjustment(AdjustmentLayer::new()),
            );
            let LayerKind::Image(image) = old else {
                unreachable!("the image kind was checked before replacement")
            };
            self.kind = LayerKind::Adjustment(AdjustmentLayer {
                channels: image.channels,
            });
            self.flags.set_pixel_data_irrelevant(true);
        }
        if let Some(existing) = self
            .blocks
            .blocks
            .iter_mut()
            .find(|existing| existing.key == block.key)
        {
            block.signature = existing.signature;
            *existing = block;
            self.promote_to_shape_if_marked()?;
            return Ok(());
        }
        let position = if adjustment.kind == AdjustmentKind::ContentGenerator {
            self.blocks
                .blocks
                .iter()
                .rposition(|existing| {
                    AdjustmentKind::from_key(existing.key).is_some_and(AdjustmentKind::marks_layer)
                })
                .map_or(0, |index| index + 1)
        } else {
            self.blocks
                .blocks
                .iter()
                .rposition(|existing| {
                    AdjustmentKind::from_key(existing.key).is_some_and(AdjustmentKind::marks_layer)
                })
                .map_or_else(
                    || {
                        self.blocks
                            .blocks
                            .iter()
                            .position(|existing| {
                                AdjustmentKind::from_key(existing.key)
                                    == Some(AdjustmentKind::ContentGenerator)
                            })
                            .unwrap_or(0)
                    },
                    |index| index + 1,
                )
        };
        self.blocks.blocks.insert(position, block);
        self.promote_to_shape_if_marked()?;
        Ok(())
    }

    /// Remove the block for one adjustment or fill kind.
    pub fn clear_adjustment(&mut self, kind: AdjustmentKind) -> bool {
        let before = self.blocks.blocks.len();
        self.blocks.blocks.retain(|block| block.key != kind.key());
        before != self.blocks.blocks.len()
    }

    /// Every vector block (`vmsk`/`vsms` masks, `vstk` strokes, `vscg`
    /// content, `vogk` origination) in source order, parsed on demand. The
    /// raw blocks remain the write source. A malformed or unsupported payload
    /// is an error.
    pub fn vector_blocks(&self) -> Result<Vec<VectorBlock>> {
        self.blocks
            .blocks
            .iter()
            .filter_map(|block| VectorBlock::read(block).transpose())
            .collect()
    }

    /// Add or replace one typed vector-mask, fill, stroke, or live-shape block.
    ///
    /// Existing blocks keep their position. New vector blocks follow Photoshop's
    /// fill, mask, stroke, origination order and are placed before layer metadata.
    /// A pixel or fill layer is promoted to `LayerKind::Shape` once its blocks
    /// describe both a vector mask and a fill.
    pub fn set_vector_block(&mut self, vector: &VectorBlock) -> Result<()> {
        let mut block = vector.to_tagged_block()?;
        if block.key.as_bytes() == *b"pths" {
            return Err(invalid("path names are a document-level block"));
        }
        if matches!(
            self.kind,
            LayerKind::Group(_) | LayerKind::SectionDivider(_)
        ) {
            return Err(invalid("vector shape data needs a non-group layer"));
        }
        let would_be_shape = (is_vector_mask_key(block.key) || has_vector_mask_block(&self.blocks))
            && (block.key.as_bytes() == *b"vscg" || has_shape_fill_block(&self.blocks));
        if would_be_shape && matches!(self.kind, LayerKind::Text(_)) {
            return Err(invalid("text layers cannot be promoted to shapes"));
        }
        if let Some(existing) = self
            .blocks
            .blocks
            .iter_mut()
            .find(|existing| existing.key == block.key)
        {
            block.signature = existing.signature;
            *existing = block;
        } else {
            let rank = vector_block_rank(block.key).unwrap_or(u8::MAX);
            let position = self
                .blocks
                .blocks
                .iter()
                .position(|existing| {
                    vector_block_rank(existing.key)
                        .is_some_and(|existing_rank| existing_rank > rank)
                        || is_layer_metadata_key(existing.key)
                })
                .unwrap_or(self.blocks.blocks.len());
            self.blocks.blocks.insert(position, block);
        }
        self.promote_to_shape_if_marked()
    }

    /// Remove one layer-level vector block. Other keys return `false`.
    pub fn clear_vector_block(&mut self, key: TaggedBlockKey) -> bool {
        if vector_block_rank(key).is_none() {
            return false;
        }
        let before = self.blocks.blocks.len();
        self.blocks.blocks.retain(|block| block.key != key);
        before != self.blocks.blocks.len()
    }

    fn promote_to_shape_if_marked(&mut self) -> Result<()> {
        if !self.is_shape_layer() || matches!(self.kind, LayerKind::Shape(_)) {
            return Ok(());
        }
        let channels = match &mut self.kind {
            LayerKind::Image(image) => std::mem::take(&mut image.channels),
            LayerKind::Adjustment(adjustment) => std::mem::take(&mut adjustment.channels),
            LayerKind::Text(_) => return Err(invalid("text layers cannot be promoted to shapes")),
            LayerKind::Group(_) | LayerKind::SectionDivider(_) => {
                return Err(invalid("vector shape data needs a non-group layer"))
            }
            LayerKind::Shape(_) => unreachable!("checked above"),
        };
        self.kind = LayerKind::Shape(ShapeLayer { channels });
        self.flags.set_pixel_data_irrelevant(true);
        Ok(())
    }

    /// The layer's vector mask (`vmsk`, or `vsms` from Photoshop CS6 on),
    /// parsed on demand. Path points are fractions of the document size.
    pub fn vector_mask(&self) -> Result<Option<VectorMask>> {
        self.blocks
            .blocks
            .iter()
            .find(|block| is_vector_mask_key(block.key))
            .map(|block| VectorMask::read(&block.data))
            .transpose()
    }

    /// Whether the layer is a shape layer: a vector mask together with a fill
    /// (`SoCo`/`GdFl`/`PtFl`, or `vscg` from Photoshop CS6 on). A pixel layer
    /// with a vector mask is not a shape layer. Upstream classifies any layer
    /// with `vogk`, `vmsk`, `vstk`, or `vscg` as a shape, but only after its
    /// adjustment check, which already claims every `SoCo` layer.
    pub fn is_shape_layer(&self) -> bool {
        matches!(self.kind, LayerKind::Shape(_)) || self.has_shape_markers()
    }

    pub(crate) fn has_shape_markers(&self) -> bool {
        has_vector_mask_block(&self.blocks) && has_shape_fill_block(&self.blocks)
    }

    /// Whether the layer is an artboard: a group whose record carries an
    /// artboard block (`artb`, `artd`, or `abdd`). Artboards stay
    /// [`LayerKind::Group`] layers in the tree, so every group API applies.
    pub fn is_artboard(&self) -> bool {
        matches!(self.kind, LayerKind::Group(_))
            && self
                .blocks
                .blocks
                .iter()
                .any(|block| is_artboard_key(block.key))
    }

    /// This group's artboard data, parsed on demand; `None` for other layers.
    /// The raw block remains the write source.
    pub fn artboard(&self) -> Result<Option<Artboard>> {
        if !matches!(self.kind, LayerKind::Group(_)) {
            return Ok(None);
        }
        match self
            .blocks
            .blocks
            .iter()
            .find(|block| is_artboard_key(block.key))
        {
            Some(block) => Artboard::read(block),
            None => Ok(None),
        }
    }

    /// Add or replace this group's typed artboard block.
    ///
    /// Use this on a detached layer. For a layer already in a document, use
    /// [`LayeredFile::set_artboard`](crate::LayeredFile::set_artboard) so the
    /// document-level artboard count stays in sync. To append a new root
    /// artboard, use [`LayeredFile::add_artboard`](crate::LayeredFile::add_artboard).
    pub fn set_artboard(&mut self, artboard: &Artboard) -> Result<()> {
        if self.group().is_none() {
            return Err(invalid("only a group layer can be an artboard"));
        }
        let existing = self
            .blocks
            .blocks
            .iter()
            .position(|block| is_artboard_key(block.key));
        let mut artboard = artboard.clone();
        if let Some(index) = existing {
            artboard.key = self.blocks.blocks[index].key;
        }
        let mut replacement = artboard.to_tagged_block()?;
        if let Some(index) = existing {
            replacement.signature = self.blocks.blocks[index].signature;
        }
        if existing.is_some_and(|index| self.blocks.blocks[index] == replacement) {
            return Ok(());
        }
        let position = existing.unwrap_or_else(|| {
            self.blocks
                .blocks
                .iter()
                .position(|block| {
                    is_layer_metadata_key(block.key)
                        || block.key.as_bytes() == *b"lsct"
                        || block.key.as_bytes() == *b"lsdk"
                })
                .unwrap_or(0)
        });
        let blocks = &mut self.blocks.blocks;
        let removed_before = blocks[..position]
            .iter()
            .filter(|block| is_artboard_key(block.key))
            .count();
        blocks.retain(|block| !is_artboard_key(block.key));
        blocks.insert(position - removed_before, replacement);
        Ok(())
    }

    /// Remove every artboard tagged block carried by this layer.
    ///
    /// Use this on a detached layer. For a layer already in a document, use
    /// [`LayeredFile::clear_artboard`](crate::LayeredFile::clear_artboard) so
    /// the document-level artboard count stays in sync.
    pub fn clear_artboard(&mut self) -> bool {
        let before = self.blocks.blocks.len();
        self.blocks
            .blocks
            .retain(|block| !is_artboard_key(block.key));
        before != self.blocks.blocks.len()
    }

    // ------------------------------------------------------------------
    // Layer settings (upstream `Layer.h`)
    // ------------------------------------------------------------------

    /// Fill opacity (`iOpa`), 0–255; 255 when the block is absent.
    pub fn fill(&self) -> u8 {
        self.blocks
            .get(TaggedBlockKey::IOPA)
            .and_then(|block| block.data.first().copied())
            .unwrap_or(255)
    }

    /// Set the fill opacity, keeping the block's padding bytes.
    pub fn set_fill(&mut self, fill: u8) {
        match self.blocks.get_mut(TaggedBlockKey::IOPA) {
            Some(block) if !block.data.is_empty() => block.data[0] = fill,
            _ => upsert_block(&mut self.blocks, TaggedBlockKey::IOPA, vec![fill, 0, 0, 0]),
        }
    }

    /// The raw `lspf` lock-flag word (0 when absent).
    pub fn protection_flags(&self) -> u32 {
        self.blocks
            .get(TaggedBlockKey::LSPF)
            .and_then(|block| block.data.get(..4))
            .map_or(0, |bytes| {
                u32::from_be_bytes(bytes.try_into().expect("four bytes"))
            })
    }

    /// Whether every property of the layer is locked (`lspf` "lock all").
    ///
    /// This is the layer's own flag, as the file records it. Photoshop also
    /// treats a layer as locked when an ancestor group is locked, a state the
    /// format does not store anywhere: walk the parents for that reading.
    pub fn is_locked(&self) -> bool {
        self.protection_flags() & LOCK_ALL != 0
    }

    /// Photoshop's layer id (`lyid`), when the block is present and well
    /// formed.
    ///
    /// Ids are meant to be unique within a document. A layer cloned from
    /// another, or copied between documents, keeps its id only when nothing in
    /// the destination has it; the add methods assign a fresh one otherwise.
    pub fn layer_id(&self) -> Option<u32> {
        let block = self.blocks.get(TaggedBlockKey::LYID)?;
        Some(u32::from_be_bytes(block.data.get(..4)?.try_into().ok()?))
    }

    /// This layer's state in each layer comp (`cmls`), in file order.
    ///
    /// A comp whose entry carries no enable flag leaves the layer's own
    /// visibility in force, which is what the returned states report. An empty
    /// vector means the layer takes part in no comp, or the block is malformed
    /// — the raw bytes are preserved either way.
    pub fn comp_states(&self) -> Vec<psd_core::layer_comps::LayerCompState> {
        self.blocks
            .get(TaggedBlockKey::CMLS)
            .and_then(|block| {
                psd_core::layer_comps::read_comp_states(&block.data, self.is_visible()).ok()
            })
            .unwrap_or_default()
    }

    /// Set the layer id, replacing an existing `lyid` block or adding one.
    pub fn set_layer_id(&mut self, id: u32) {
        let data = id.to_be_bytes().to_vec();
        match self.blocks.get_mut(TaggedBlockKey::LYID) {
            Some(block) => block.data = data,
            None => self
                .blocks
                .push(TaggedBlock::new(TaggedBlockKey::LYID, data)),
        }
    }

    /// Lock or unlock the layer. Partial locks (position, pixels, …) are kept;
    /// the record's transparency-protected flag follows the lock like
    /// upstream's `BitFlags(m_IsLocked, …)`.
    pub fn set_locked(&mut self, locked: bool) {
        let mut flags = self.protection_flags();
        if locked {
            flags |= LOCK_ALL;
        } else {
            flags &= !LOCK_ALL;
        }
        match self.blocks.get_mut(TaggedBlockKey::LSPF) {
            Some(block) if block.data.len() >= 4 => {
                block.data[..4].copy_from_slice(&flags.to_be_bytes());
            }
            _ => upsert_block(
                &mut self.blocks,
                TaggedBlockKey::LSPF,
                flags.to_be_bytes().to_vec(),
            ),
        }
        self.flags
            .set_transparency_protected(locked || flags & LOCK_TRANSPARENCY != 0);
    }

    /// Display color in the layers panel (`lclr`); `None` when absent.
    ///
    /// This is the layer's own value. Photoshop lets descendants inherit a
    /// group's label when their own is absent, which the format does not
    /// record either: walk the parents for that reading.
    pub fn display_color(&self) -> LayerColor {
        self.blocks
            .get(TaggedBlockKey::LCLR)
            .and_then(|block| block.data.get(..2))
            .map_or(LayerColor::None, |bytes| {
                LayerColor::from_raw(u16::from_be_bytes([bytes[0], bytes[1]]))
            })
    }

    /// Set the display color, keeping the block's trailing bytes.
    pub fn set_display_color(&mut self, color: LayerColor) {
        let raw = color.as_raw().to_be_bytes();
        match self.blocks.get_mut(TaggedBlockKey::LCLR) {
            Some(block) if block.data.len() >= 2 => block.data[..2].copy_from_slice(&raw),
            _ => {
                let mut data = raw.to_vec();
                data.resize(8, 0);
                upsert_block(&mut self.blocks, TaggedBlockKey::LCLR, data);
            }
        }
    }

    /// Whether the layer is clipped to the layer below it.
    pub fn is_clipping_mask(&self) -> bool {
        self.clipping != 0
    }

    pub fn set_clipping_mask(&mut self, clipped: bool) {
        self.clipping = u8::from(clipped);
    }

    /// The layer's center in canvas coordinates. Groups without pixel bounds
    /// report their mask's center, as upstream derives group extents from the
    /// mask when the record is empty.
    pub fn center(&self) -> (f64, f64) {
        match (self.bounds.sample_count(), self.mask_rect()) {
            (0, Some(mask)) if self.group().is_some() => mask.center(),
            _ => self.bounds.center(),
        }
    }

    /// Move the layer by whole pixels.
    ///
    /// What moves: the pixel bounds, and a text layer's `TySh` transform so
    /// Photoshop re-renders the text at the new position. **Mask rects are
    /// never rewritten** — a mask's stored rect is interpreted relative to the
    /// layer when its link flag is set and as absolute otherwise, so a linked
    /// mask follows implicitly and an unlinked one stays where it is, which is
    /// what Photoshop's Move tool does.
    ///
    /// What this method cannot do, and refuses instead of doing halfway:
    ///
    /// - **smart objects** need the document to re-render their warp; move them
    ///   through [`LayeredFile::move_smart_object`](crate::LayeredFile::move_smart_object);
    /// - **vector geometry** (a shape layer's path, or any `vmsk`/`vsms` vector
    ///   mask) is stored as fractions of the document, so moving it needs the
    ///   document size; use [`LayeredFile::translate_layer`](crate::LayeredFile::translate_layer),
    ///   which also carries a group's children. Moving only the bounds would
    ///   leave the path behind and Photoshop would snap the layer back on its
    ///   next re-render.
    pub fn translate(&mut self, dx: i32, dy: i32) -> Result<()> {
        self.translate_with_path_delta(dx, dy, None)
    }

    /// [`translate`](Self::translate) with the document-relative delta vector
    /// geometry needs: `path_delta` is in 8.24 fixed point, the unit path
    /// points are stored in, and is `Some` only when the caller knows the
    /// document size.
    pub fn translate_with_path_delta(
        &mut self,
        dx: i32,
        dy: i32,
        path_delta: Option<(i32, i32)>,
    ) -> Result<()> {
        if self.is_smart_object() {
            return Err(invalid(
                "smart-object layers move through LayeredFile::move_smart_object",
            ));
        }
        let has_vector_geometry = self
            .blocks
            .blocks
            .iter()
            .any(|block| is_vector_mask_key(block.key));
        let (path_dx, path_dy) = match path_delta {
            Some(delta) => delta,
            None => {
                if has_vector_geometry {
                    return Err(invalid(
                        "a layer with vector geometry moves through LayeredFile::translate_layer, \
                         which converts the pixel move into the document-relative units a path \
                         is stored in",
                    ));
                }
                (0, 0)
            }
        };
        let bounds = self.bounds.translated(dx, dy)?;
        if has_vector_geometry {
            for block in &mut self.blocks.blocks {
                if !is_vector_mask_key(block.key) {
                    continue;
                }
                let mut mask = VectorMask::read(&block.data)?;
                mask.translate(path_dx, path_dy)?;
                block.data = mask.to_payload()?;
            }
        }
        if self.is_text_layer() {
            if let Some((x, y)) = self.text_position() {
                self.set_text_position(x + f64::from(dx), y + f64::from(dy))?;
            }
        }
        self.bounds = bounds;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Pixel mask (upstream `MaskMixin`)
    // ------------------------------------------------------------------

    /// Channel holding the layer's pixel mask: `-3` (the "real user mask")
    /// when a vector mask also exists, otherwise `-2`.
    pub fn pixel_mask_key(&self) -> ChannelKey {
        match &self.mask {
            Some(data) if data.vector_mask.is_some() && data.pixel_mask.is_some() => {
                ChannelKey::REAL_USER_MASK
            }
            _ => ChannelKey::USER_MASK,
        }
    }

    /// Whether the layer has pixel-mask samples.
    ///
    /// A vector-only mask is not a pixel mask here even though Photoshop
    /// stores its rendering in channel `-2`; upstream reports it as one.
    pub fn has_mask(&self) -> bool {
        let has_pixel_record = self
            .mask
            .as_ref()
            .is_none_or(|data| data.pixel_mask.is_some());
        has_pixel_record
            && self
                .channels()
                .is_some_and(|channels| channels.contains(self.pixel_mask_key()))
    }

    /// The pixel mask samples, row-major over [`mask_rect`](Self::mask_rect).
    /// Returns `None` until a raw-backed mask channel is decoded.
    pub fn mask_pixels(&self) -> Option<&[T]> {
        if !self.has_mask() {
            return None;
        }
        self.channels()?.get(self.pixel_mask_key())
    }

    /// The pixel mask record (bounds, default color, flags, parameters).
    pub fn mask_record(&self) -> Option<&LayerMask> {
        self.mask.as_ref()?.pixel_mask.as_ref()
    }

    fn mask_record_mut(&mut self) -> Result<&mut LayerMask> {
        self.mask
            .as_mut()
            .and_then(|data| data.pixel_mask.as_mut())
            .ok_or(invalid("the layer has no pixel mask"))
    }

    /// Canvas-space bounds of the pixel mask.
    pub fn mask_rect(&self) -> Option<Rect> {
        self.mask_record()
            .map(|mask| Rect::new(mask.top, mask.left, mask.bottom, mask.right))
    }

    /// Set the pixel mask samples and bounds, creating the mask record (white
    /// default color, no flags) when the layer has none. Existing record flags
    /// and parameters are kept.
    pub fn set_mask(&mut self, pixels: Vec<T>, rect: Rect) -> Result<()> {
        if rect.width() < 0 || rect.height() < 0 || pixels.len() != rect.sample_count() {
            return Err(invalid(
                "mask samples must match the mask bounds' width × height",
            ));
        }
        if self.channels().is_none() {
            return Err(invalid("section dividers cannot carry a mask"));
        }
        let data = self.mask.get_or_insert_with(LayerMaskData::default);
        let record = data.pixel_mask.get_or_insert(LayerMask {
            top: rect.top,
            left: rect.left,
            bottom: rect.bottom,
            right: rect.right,
            default_color: 255,
            flags: LayerMaskFlags::default(),
            params: None,
        });
        (record.top, record.left, record.bottom, record.right) =
            (rect.top, rect.left, rect.bottom, rect.right);
        let key = self.pixel_mask_key();
        self.channels_mut()
            .expect("checked above")
            .insert(key, pixels);
        Ok(())
    }

    /// Remove the pixel mask (samples and record), returning its samples.
    /// A vector mask, if any, is kept. Removing a raw-backed mask discards its
    /// compressed payload and returns `None` because no samples were decoded.
    pub fn remove_mask(&mut self) -> Option<Vec<T>> {
        let key = self.pixel_mask_key();
        let pixels = self.channels_mut()?.remove(key);
        if let Some(data) = self.mask.as_mut() {
            data.pixel_mask = None;
            if data.vector_mask.is_none() {
                self.mask = None;
            }
        }
        pixels
    }

    /// Move the pixel mask so its center lands on `(x, y)` (canvas space),
    /// rounding like upstream's `generate_extents`.
    pub fn set_mask_center(&mut self, x: f64, y: f64) -> Result<()> {
        let current = self
            .mask_rect()
            .ok_or(invalid("the layer has no pixel mask"))?;
        let moved = Rect::centered(x, y, current.width() as u32, current.height() as u32)?;
        let record = self.mask_record_mut()?;
        (record.top, record.left, record.bottom, record.right) =
            (moved.top, moved.left, moved.bottom, moved.right);
        Ok(())
    }

    pub fn mask_disabled(&self) -> Option<bool> {
        self.mask_record().map(|mask| mask.flags.disabled())
    }

    pub fn set_mask_disabled(&mut self, disabled: bool) -> Result<()> {
        self.mask_record_mut()?.flags.set_disabled(disabled);
        Ok(())
    }

    pub fn mask_relative_to_layer(&self) -> Option<bool> {
        self.mask_record()
            .map(|mask| mask.flags.position_relative_to_layer())
    }

    pub fn set_mask_relative_to_layer(&mut self, relative: bool) -> Result<()> {
        self.mask_record_mut()?
            .flags
            .set_position_relative_to_layer(relative);
        Ok(())
    }

    /// Mask value outside the mask bounds (0 or 255 in Photoshop files).
    pub fn mask_default_color(&self) -> Option<u8> {
        self.mask_record().map(|mask| mask.default_color)
    }

    pub fn set_mask_default_color(&mut self, color: u8) -> Result<()> {
        self.mask_record_mut()?.default_color = color;
        Ok(())
    }

    /// User mask density (0–255, the mask's opacity).
    pub fn mask_density(&self) -> Option<u8> {
        self.mask.as_ref()?.user_mask_density()
    }

    pub fn set_mask_density(&mut self, density: Option<u8>) -> Result<()> {
        self.mask_record_mut()?;
        self.mask
            .as_mut()
            .expect("checked above")
            .set_user_mask_density(density)
    }

    /// User mask feather radius in pixels.
    pub fn mask_feather(&self) -> Option<f64> {
        self.mask.as_ref()?.user_mask_feather()
    }

    pub fn set_mask_feather(&mut self, feather: Option<f64>) -> Result<()> {
        self.mask_record_mut()?;
        self.mask
            .as_mut()
            .expect("checked above")
            .set_user_mask_feather(feather)
    }

    /// The compression a channel is written with: the mask override for mask
    /// channels, then the layer override, then `document`.
    pub(crate) fn write_compression(
        &self,
        key: ChannelKey,
        document: Option<Compression>,
    ) -> Option<Compression> {
        let layer = self.compression.or(document);
        if key.is_mask() {
            self.mask_compression.or(layer)
        } else {
            layer
        }
    }
}

/// Replace a block's payload in place (keeping its position and signature) or
/// append a new block.
pub(crate) fn upsert_block(blocks: &mut AdditionalLayerInfo, key: TaggedBlockKey, data: Vec<u8>) {
    match blocks.get_mut(key) {
        Some(block) => block.data = data,
        None => blocks.push(TaggedBlock::new(key, data)),
    }
}

/// Kind-specific layer payload.
#[derive(Debug, Clone, PartialEq)]
pub enum LayerKind<T: BitDepth> {
    /// Pixel layer: planar channels.
    Image(ImageLayer<T>),
    /// Text layer and its ordinary raster preview channels. The text model is
    /// kept in the losslessly preserved `TySh`/`Txt2` blocks on [`Layer`].
    Text(TextLayer<T>),
    /// Adjustment or fill layer. Its pixels are derived from its settings;
    /// the channel store carries any preserved preview or mask channels.
    Adjustment(AdjustmentLayer<T>),
    /// Vector shape with its fill, path, and optional stroke in tagged blocks.
    Shape(ShapeLayer<T>),
    /// Group layer: child layers, the open/closed state, and mask channels.
    Group(GroupLayer<T>),
    /// Section divider marker (the `</Layer group>` records Photoshop writes).
    SectionDivider(SectionDivider),
}

/// Typed layer kind for adjustment and fill layers.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AdjustmentLayer<T: BitDepth> {
    pub channels: ChannelStore<T>,
}

impl<T: BitDepth> AdjustmentLayer<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pixels of one channel, when the file carries a preview channel.
    pub fn channel(&self, key: ChannelKey) -> Option<&[T]> {
        self.channels.get(key)
    }

    /// Replace a preserved preview or mask channel.
    pub fn set_channel(&mut self, key: ChannelKey, data: Vec<T>) {
        self.channels.insert(key, data);
    }
}

/// Typed layer kind for vector shapes.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ShapeLayer<T: BitDepth> {
    pub channels: ChannelStore<T>,
}

impl<T: BitDepth> ShapeLayer<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pixels of one preserved preview or mask channel.
    pub fn channel(&self, key: ChannelKey) -> Option<&[T]> {
        self.channels.get(key)
    }

    /// Replace a preserved preview or mask channel.
    pub fn set_channel(&mut self, key: ChannelKey, data: Vec<T>) {
        self.channels.insert(key, data);
    }
}

/// A pixel layer's channels.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImageLayer<T: BitDepth> {
    pub channels: ChannelStore<T>,
}

impl<T: BitDepth> ImageLayer<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pixels of one channel.
    pub fn channel(&self, key: ChannelKey) -> Option<&[T]> {
        self.channels.get(key)
    }

    /// Replace the pixels of one channel.
    pub fn set_channel(&mut self, key: ChannelKey, data: Vec<T>) {
        self.channels.insert(key, data);
    }
}

/// Raster preview channels for a Photoshop text layer.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TextLayer<T: BitDepth> {
    pub channels: ChannelStore<T>,
}

impl<T: BitDepth> TextLayer<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn channel(&self, key: ChannelKey) -> Option<&[T]> {
        self.channels.get(key)
    }

    pub fn set_channel(&mut self, key: ChannelKey, data: Vec<T>) {
        self.channels.insert(key, data);
    }
}

/// A group's children (indices into the document arena), open state, and
/// mask channels.
///
/// Groups carry no color pixels on disk (Photoshop writes empty stub
/// channels), but they can have a pixel mask; `channels` holds only mask keys.
/// Upstream groups get their mask from the same `MaskMixin` as other layers.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupLayer<T: BitDepth> {
    pub children: Vec<LayerId>,
    /// `true` for `lsct` type 1 (open folder), `false` for type 2.
    pub open: bool,
    pub channels: ChannelStore<T>,
}

impl<T: BitDepth> Default for GroupLayer<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: BitDepth> GroupLayer<T> {
    pub fn new() -> Self {
        Self {
            children: Vec::new(),
            open: true,
            channels: ChannelStore::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_sample_count_handles_degenerate_bounds() {
        assert_eq!(Rect::new(0, 0, 64, 64).sample_count(), 4096);
        assert_eq!(Rect::new(0, 0, 0, 0).sample_count(), 0);
        assert_eq!(Rect::new(5, 5, 2, 2).sample_count(), 0);
    }

    #[test]
    fn centered_rects_use_upstream_rounding() {
        assert_eq!(
            Rect::centered(16.0, 8.0, 32, 16).unwrap(),
            Rect::new(0, 0, 16, 32)
        );
        // An odd size puts the half pixel on the far side, like upstream.
        assert_eq!(
            Rect::centered(10.0, 10.0, 3, 3).unwrap(),
            Rect::new(9, 9, 12, 12)
        );
        assert!(Rect::centered(f64::INFINITY, 0.0, 1, 1).is_err());
        assert!(Rect::new(0, i32::MAX - 1, 1, i32::MAX)
            .translated(5, 0)
            .is_err());
    }

    #[test]
    fn visibility_maps_to_the_hidden_flag() {
        let mut layer = Layer::<u8>::new_image("Layer", Rect::new(0, 0, 8, 8));
        assert!(layer.is_visible());
        layer.set_visible(false);
        assert!(!layer.is_visible());
        assert!(layer.flags.hidden());
    }

    #[test]
    fn settings_blocks_are_created_and_edited_in_place() {
        let mut layer = Layer::<u8>::new_image("Layer", Rect::new(0, 0, 2, 2));
        assert_eq!(layer.fill(), 255);
        assert!(!layer.is_locked());
        assert_eq!(layer.display_color(), LayerColor::None);

        layer.set_fill(130);
        layer.set_locked(true);
        layer.set_display_color(LayerColor::Violet);
        assert_eq!(layer.fill(), 130);
        assert!(layer.is_locked());
        assert!(layer.flags.transparency_protected());
        assert_eq!(layer.display_color(), LayerColor::Violet);
        assert_eq!(
            layer.blocks.get(TaggedBlockKey::IOPA).unwrap().data,
            [130, 0, 0, 0]
        );
        assert_eq!(
            layer.blocks.get(TaggedBlockKey::LCLR).unwrap().data,
            [0, 6, 0, 0, 0, 0, 0, 0]
        );

        // Partial locks and trailing bytes survive edits.
        layer.blocks.get_mut(TaggedBlockKey::LSPF).unwrap().data = vec![0x80, 0, 0, 0x04];
        layer.set_locked(false);
        assert_eq!(layer.protection_flags(), 0x04);
        assert!(!layer.flags.transparency_protected());
        layer.blocks.get_mut(TaggedBlockKey::LCLR).unwrap().data = vec![0, 1, 9, 9, 9, 9, 9, 9];
        layer.set_display_color(LayerColor::Green);
        assert_eq!(
            layer.blocks.get(TaggedBlockKey::LCLR).unwrap().data,
            [0, 4, 9, 9, 9, 9, 9, 9]
        );
    }

    #[test]
    fn pixel_mask_lifecycle_and_parameters() {
        let mut layer = Layer::<u8>::new_image("Layer", Rect::new(0, 0, 4, 4));
        assert!(!layer.has_mask());
        assert!(layer.set_mask_disabled(true).is_err());
        assert!(layer.set_mask(vec![1; 3], Rect::new(0, 0, 2, 2)).is_err());

        layer.set_mask(vec![7; 6], Rect::new(1, 1, 3, 4)).unwrap();
        assert!(layer.has_mask());
        assert_eq!(layer.mask_pixels(), Some(&[7u8; 6][..]));
        assert_eq!(layer.mask_default_color(), Some(255));
        layer.set_mask_disabled(true).unwrap();
        layer.set_mask_relative_to_layer(true).unwrap();
        layer.set_mask_density(Some(64)).unwrap();
        layer.set_mask_feather(Some(2.5)).unwrap();
        assert_eq!(layer.mask_disabled(), Some(true));
        assert_eq!(layer.mask_relative_to_layer(), Some(true));
        assert_eq!(layer.mask_density(), Some(64));
        assert_eq!(layer.mask_feather(), Some(2.5));

        layer.set_mask_center(10.0, 10.0).unwrap();
        assert_eq!(
            layer.mask_rect(),
            Some(Rect::centered(10.0, 10.0, 3, 2).unwrap())
        );

        // Replacing the pixels keeps the record's flags and parameters.
        layer.set_mask(vec![9; 4], Rect::new(0, 0, 2, 2)).unwrap();
        assert_eq!(layer.mask_disabled(), Some(true));
        assert_eq!(layer.remove_mask(), Some(vec![9; 4]));
        assert!(!layer.has_mask());
        assert!(layer.mask.is_none());
    }

    #[test]
    fn groups_store_masks_and_report_their_center() {
        let mut group = Layer::<u16>::new_group("Group");
        assert_eq!(group.blend_mode, BlendMode::PASSTHROUGH);
        group
            .set_mask(vec![0; 8], Rect::new(10, 20, 12, 24))
            .unwrap();
        assert!(group.has_mask());
        assert_eq!(group.center(), (22.0, 11.0));
        group.translate(-20, -10).unwrap();
        // The mask's stored rect is not rewritten by a move: its link flag says
        // whether it follows the layer, and this mask is unlinked (absolute).
        assert_eq!(group.mask_rect(), Some(Rect::new(10, 20, 12, 24)));
        assert_eq!(group.bounds, Rect::new(-10, -20, -10, -20));
    }

    #[test]
    fn compression_overrides_fall_back_in_order() {
        let mut layer = Layer::<u8>::new_image("Layer", Rect::new(0, 0, 1, 1));
        let doc = Some(Compression::Zip);
        assert_eq!(layer.write_compression(ChannelKey::color(0), doc), doc);
        layer.compression = Some(Compression::Raw);
        assert_eq!(
            layer.write_compression(ChannelKey::USER_MASK, doc),
            Some(Compression::Raw)
        );
        layer.mask_compression = Some(Compression::Rle);
        assert_eq!(
            layer.write_compression(ChannelKey::USER_MASK, doc),
            Some(Compression::Rle)
        );
        assert_eq!(
            layer.write_compression(ChannelKey::ALPHA, doc),
            Some(Compression::Raw)
        );
    }
}
