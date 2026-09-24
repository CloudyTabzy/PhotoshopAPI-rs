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

use psd_core::vector::is_vector_mask_key;
use psd_core::{
    AdditionalLayerInfo, AdjustmentBlock, AdjustmentKind, BeReader, BlendMode, Compression,
    LayerBlendingRanges, LayerColor, LayerEffectsBlock, LayerFlags, LayerMask, LayerMaskData,
    LayerMaskFlags, PlacedLayer, PlacedLayerData, PsdError, Result, SectionDivider, TaggedBlock,
    TaggedBlockKey, VectorBlock, VectorMask,
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
    /// Build an empty image layer with the given name and bounds.
    pub fn new_image(name: impl Into<String>, bounds: Rect) -> Self {
        Self {
            name: name.into(),
            bounds,
            opacity: 255,
            blend_mode: BlendMode::NORMAL,
            flags: LayerFlags::default(),
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

    /// The layer's channel store: pixels for image and text layers, mask
    /// channels only for groups, `None` for section dividers.
    pub fn channels(&self) -> Option<&ChannelStore<T>> {
        match &self.kind {
            LayerKind::Image(image) => Some(&image.channels),
            LayerKind::Text(text) => Some(&text.channels),
            LayerKind::Group(group) => Some(&group.channels),
            LayerKind::SectionDivider(_) => None,
        }
    }

    /// Mutable form of [`channels`](Self::channels).
    pub fn channels_mut(&mut self) -> Option<&mut ChannelStore<T>> {
        match &mut self.kind {
            LayerKind::Image(image) => Some(&mut image.channels),
            LayerKind::Text(text) => Some(&mut text.channels),
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
    pub fn effects(&self) -> Result<Vec<LayerEffectsBlock>> {
        self.blocks
            .blocks
            .iter()
            .filter_map(|block| match LayerEffectsBlock::read(block) {
                Ok(Some(effects)) => Some(Ok(effects)),
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    /// Whether the layer carries an adjustment or fill-layer settings block
    /// (`levl`, `curv`, `SoCo`, ...; see [`AdjustmentKind`]). Upstream detects
    /// the same keys to build its opaque `AdjustmentLayer`. Photoshop also
    /// stores a shape layer's fill in `SoCo`/`GdFl`/`PtFl`, so shape layers
    /// report `true` as well.
    pub fn is_adjustment_layer(&self) -> bool {
        self.blocks.blocks.iter().any(|block| {
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
        let mut mask = false;
        let mut fill = false;
        for block in &self.blocks.blocks {
            mask |= is_vector_mask_key(block.key);
            fill |= block.key.as_bytes() == *b"vscg"
                || AdjustmentKind::from_key(block.key).is_some_and(AdjustmentKind::is_fill);
        }
        mask && fill
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
    pub fn is_locked(&self) -> bool {
        self.protection_flags() & LOCK_ALL != 0
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
    /// The pixel mask moves with the layer (Photoshop's default linked-mask
    /// behavior); upstream's `center_x`/`center_y` setters leave the mask
    /// behind. Text layers also move their `TySh`
    /// transform so Photoshop re-renders the text at the new position.
    /// Smart objects must be moved through the document so their warp is
    /// re-rendered (see [`LayeredFile::move_smart_object`](crate::LayeredFile::move_smart_object)).
    pub fn translate(&mut self, dx: i32, dy: i32) -> Result<()> {
        if self.is_smart_object() {
            return Err(invalid(
                "smart-object layers move through LayeredFile::move_smart_object",
            ));
        }
        let bounds = self.bounds.translated(dx, dy)?;
        let mut mask = self.mask.clone();
        if let Some(data) = mask.as_mut() {
            for record in [data.pixel_mask.as_mut(), data.vector_mask.as_mut()]
                .into_iter()
                .flatten()
            {
                let moved = Rect::new(record.top, record.left, record.bottom, record.right)
                    .translated(dx, dy)?;
                (record.top, record.left, record.bottom, record.right) =
                    (moved.top, moved.left, moved.bottom, moved.right);
            }
        }
        if self.is_text_layer() {
            if let Some((x, y)) = self.text_position() {
                self.set_text_position(x + f64::from(dx), y + f64::from(dy))?;
            }
        }
        self.bounds = bounds;
        self.mask = mask;
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
    /// Group layer: child layers, the open/closed state, and mask channels.
    Group(GroupLayer<T>),
    /// Section divider marker (the `</Layer group>` records Photoshop writes).
    SectionDivider(SectionDivider),
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
        assert_eq!(group.mask_rect(), Some(Rect::new(0, 0, 2, 4)));
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
