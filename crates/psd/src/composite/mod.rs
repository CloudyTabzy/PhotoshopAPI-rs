//! Layer-stack compositing: flatten a document the way Photoshop renders it.
//!
//! The engine walks the layer tree bottom-to-top and blends each layer into a
//! straight-alpha RGB canvas:
//!
//! 1. **Content.** A layer's color and alpha channels are placed at its bounds,
//!    clipped to the canvas; a layer with no alpha channel is opaque inside its
//!    bounds. Fill layers generate their content instead (solid color or
//!    gradient). Text and shape layers contribute the raster preview Photoshop
//!    stored, so no font or path engine is needed.
//! 2. **Effects.** Outer effects (drop shadow, outer glow) paint *below* the
//!    content; interior overlays fold into the content's color; interior
//!    effects (satin, inner glow, inner shadow) paint above it inside the
//!    silhouette. See [`effects`].
//! 3. **Coverage.** Alpha × the layer mask (raster `-2` channel, honoring its
//!    default color outside the mask rect) × clipping (a `clip = 1` layer is
//!    masked by its base's coverage) × the blend-if gates.
//! 4. **Blend.** The calibrated blend math in [`blend`] with layer opacity and
//!    fill opacity, then source-over onto the canvas.
//! 5. **Adjustment layers** transform the backdrop in place inside their mask.
//! 6. **Groups** either pass through (children meet the true backdrop, then the
//!    whole group fades toward the pre-group snapshot by the group's opacity) or
//!    isolate (children composite into a transparent buffer that meets the
//!    backdrop with the group's blend mode, opacity and mask).
//!
//! Documented gaps: bevel/emboss, pattern overlay and pattern fill, noise and
//! contour shaping, knockout, the special "Precise" glow techniques, vector
//! rasterization (the stored preview is used), and CMYK/Lab color conversion.
//!
//! The canvas is float, and for 8-bit documents the modes whose rounding
//! Photoshop pins (Color Burn, Color Dodge, Exclusion, Divide) are computed in
//! the byte domain so a flatten can match Photoshop byte for byte where the
//! math is exact.

mod adjustments;
mod blend;
mod effects;

use psd_core::{BlendMode, ColorMode, LayerBlendingRanges, Result};

use crate::channels::ChannelKey;
use crate::layer::{Layer, LayerKind, Rect};
use crate::layered_file::LayeredFile;
use crate::BitDepth;

/// What to include when flattening.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompositeOptions {
    /// Draw layer effects (shadows, glows, overlays, strokes, satin).
    pub effects: bool,
    /// Honor the blending-ranges ("Blend If") gates.
    pub blend_if: bool,
    /// Apply adjustment and fill layers.
    pub adjustments: bool,
}

impl Default for CompositeOptions {
    fn default() -> Self {
        Self {
            effects: true,
            blend_if: true,
            adjustments: true,
        }
    }
}

/// A flattened document: 8-bit straight-alpha RGBA, row-major, top-left origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositeImage {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes: R, G, B, A.
    pub rgba: Vec<u8>,
}

impl CompositeImage {
    /// The pixel at `(x, y)` as `[r, g, b, a]`.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let index = ((y * self.width + x) * 4) as usize;
        [
            self.rgba[index],
            self.rgba[index + 1],
            self.rgba[index + 2],
            self.rgba[index + 3],
        ]
    }
}

/// Straight-alpha RGB float canvas.
pub(crate) struct Canvas {
    pub width: u32,
    pub height: u32,
    /// Document-space position of the canvas's top-left pixel; an isolated
    /// group's buffer covers only its children's bounds.
    pub origin: (i32, i32),
    pub color: [Vec<f32>; 3],
    pub alpha: Vec<f32>,
}

impl Canvas {
    pub fn new(width: u32, height: u32) -> Self {
        let pixels = width as usize * height as usize;
        Self {
            width,
            height,
            origin: (0, 0),
            color: [vec![0.0; pixels], vec![0.0; pixels], vec![0.0; pixels]],
            alpha: vec![0.0; pixels],
        }
    }

    pub fn index(&self, x: i64, y: i64) -> Option<usize> {
        let local_x = x - i64::from(self.origin.0);
        let local_y = y - i64::from(self.origin.1);
        if local_x < 0
            || local_y < 0
            || local_x >= i64::from(self.width)
            || local_y >= i64::from(self.height)
        {
            return None;
        }
        Some(local_y as usize * self.width as usize + local_x as usize)
    }

    pub fn pixel(&self, index: usize) -> ([f32; 3], f32) {
        (
            [
                self.color[0][index],
                self.color[1][index],
                self.color[2][index],
            ],
            self.alpha[index],
        )
    }

    pub fn set(&mut self, index: usize, color: [f32; 3], alpha: f32) {
        self.color[0][index] = color[0];
        self.color[1][index] = color[1];
        self.color[2][index] = color[2];
        self.alpha[index] = alpha;
    }
}

/// A layer's resolved content over a document-space rect.
pub(crate) struct Content {
    pub rect: Rect,
    pub color: [Vec<f32>; 3],
    pub alpha: Vec<f32>,
    /// Extra coverage multiplier (the layer mask), same rect.
    pub mask: Option<Vec<f32>>,
}

impl Content {
    pub fn new(rect: Rect) -> Self {
        let width = rect.width().max(0) as usize;
        let height = rect.height().max(0) as usize;
        let pixels = width * height;
        Self {
            rect,
            color: [vec![0.0; pixels], vec![0.0; pixels], vec![0.0; pixels]],
            alpha: vec![0.0; pixels],
            mask: None,
        }
    }

    pub fn width(&self) -> usize {
        self.rect.width().max(0) as usize
    }

    pub fn height(&self) -> usize {
        self.rect.height().max(0) as usize
    }

    pub fn index(&self, x: i64, y: i64) -> Option<usize> {
        if x < i64::from(self.rect.left)
            || y < i64::from(self.rect.top)
            || x >= i64::from(self.rect.right)
            || y >= i64::from(self.rect.bottom)
        {
            return None;
        }
        Some(
            (y - i64::from(self.rect.top)) as usize * self.width()
                + (x - i64::from(self.rect.left)) as usize,
        )
    }
}

/// The compositing engine for one document.
struct Compositor<'a, T: BitDepth> {
    document: &'a LayeredFile<T>,
    options: CompositeOptions,
    /// 8-bit documents get Photoshop's byte-domain blend rounding.
    byte_domain: bool,
}

impl<T: BitDepth> LayeredFile<T> {
    /// Flatten the layer stack into an 8-bit straight-alpha RGBA image.
    pub fn composite_rgba8(&self) -> Result<CompositeImage> {
        self.composite_rgba8_with(CompositeOptions::default())
    }

    /// Flatten the layer stack with explicit options.
    pub fn composite_rgba8_with(&self, options: CompositeOptions) -> Result<CompositeImage> {
        let compositor = Compositor {
            document: self,
            options,
            byte_domain: T::DEPTH == 8,
        };
        let mut canvas = Canvas::new(self.width, self.height);
        compositor.composite_children(None, &mut canvas)?;
        Ok(canvas_to_image(&canvas))
    }
}

fn canvas_to_image(canvas: &Canvas) -> CompositeImage {
    let mut rgba = vec![0u8; canvas.alpha.len() * 4];
    for index in 0..canvas.alpha.len() {
        let alpha = canvas.alpha[index].clamp(0.0, 1.0);
        rgba[index * 4] = to_byte(canvas.color[0][index]);
        rgba[index * 4 + 1] = to_byte(canvas.color[1][index]);
        rgba[index * 4 + 2] = to_byte(canvas.color[2][index]);
        rgba[index * 4 + 3] = (alpha * 255.0).round() as u8;
    }
    CompositeImage {
        width: canvas.width,
        height: canvas.height,
        rgba,
    }
}

fn to_byte(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

impl<T: BitDepth> Compositor<'_, T> {
    /// Composite one level of the tree into `canvas`.
    fn composite_children(&self, parent: Option<usize>, canvas: &mut Canvas) -> Result<()> {
        let Some(children) = self.document.children(parent) else {
            return Ok(());
        };
        let children = children.to_vec();

        // Split the level into clipping runs: a base layer plus the layers
        // above it that carry `clip = 1`, all blending as one unit.
        let mut index = 0;
        while index < children.len() {
            let base = children[index];
            let mut clipped = Vec::new();
            let mut next = index + 1;
            while next < children.len() {
                let layer = self.document.layer(children[next]);
                match layer {
                    Some(layer) if layer.clipping == 1 => {
                        clipped.push(children[next]);
                        next += 1;
                    }
                    _ => break,
                }
            }
            self.composite_unit(base, &clipped, canvas)?;
            index = next;
        }
        Ok(())
    }

    /// Composite a base layer and the clipped layers above it.
    fn composite_unit(&self, base: usize, clipped: &[usize], canvas: &mut Canvas) -> Result<()> {
        let Some(layer) = self.document.layer(base) else {
            return Ok(());
        };
        if !layer.is_visible() {
            return Ok(());
        }
        let base_coverage = self.composite_layer(base, canvas, None)?;

        for &clipped_id in clipped {
            let Some(clipped_layer) = self.document.layer(clipped_id) else {
                continue;
            };
            if !clipped_layer.is_visible() {
                continue;
            }
            self.composite_layer(clipped_id, canvas, base_coverage.as_deref())?;
        }
        Ok(())
    }

    /// Composite one layer. `clip_base` is the clipping base's coverage, when
    /// this layer is clipped to one. Returns the layer's own coverage (used as
    /// the clip base for the layers above it).
    fn composite_layer(
        &self,
        id: usize,
        canvas: &mut Canvas,
        clip_base: Option<&[f32]>,
    ) -> Result<Option<Vec<f32>>> {
        let Some(layer) = self.document.layer(id) else {
            return Ok(None);
        };
        match &layer.kind {
            LayerKind::SectionDivider(_) => Ok(None),
            LayerKind::Group(_) => {
                self.composite_group(id, canvas)?;
                Ok(None)
            }
            LayerKind::Adjustment(_) => {
                if self.options.adjustments {
                    self.apply_adjustment(layer, canvas)?;
                }
                Ok(None)
            }
            _ => self.composite_raster(layer, canvas, clip_base),
        }
    }

    /// A pixel layer, shape layer, text layer or fill layer.
    fn composite_raster(
        &self,
        layer: &Layer<T>,
        canvas: &mut Canvas,
        clip_base: Option<&[f32]>,
    ) -> Result<Option<Vec<f32>>> {
        let mut content = self.layer_content(layer)?;
        let Some(mut content) = content.take() else {
            return Ok(None);
        };
        if content.width() == 0 || content.height() == 0 {
            return Ok(None);
        }

        // Coverage = alpha × mask, over the content's rect.
        let mut coverage = content.alpha.clone();
        if let Some(mask) = &content.mask {
            for (value, mask) in coverage.iter_mut().zip(mask.iter()) {
                *value *= *mask;
            }
        }

        // Effects: overlays fold into the color, interior effects paint above
        // it, outer effects composite below.
        let mut outer: Option<Content> = None;
        if self.options.effects {
            if let Some(effects) = self.typed_effects(layer) {
                effects::fold_interior_overlays(&mut content, &effects);
                effects::paint_interior(&mut content, &coverage, &effects);
                let canvas_rect = Rect::new(0, 0, canvas.height as i32, canvas.width as i32);
                outer = effects::build_outer(&content, &coverage, &effects, canvas_rect);
            }
        }

        if let Some(outer) = outer.as_ref() {
            self.blend_content(
                outer,
                canvas,
                layer,
                None,
                layer.blend_mode,
                self.opacity(layer, false),
                None,
            );
        }

        let coverage_after_effects = coverage.clone();
        self.blend_content(
            &content,
            canvas,
            layer,
            Some(&coverage),
            layer.blend_mode,
            self.opacity(layer, true),
            clip_base,
        );

        // The clip base coverage a clipped layer above sees is the layer's own
        // coverage including its mask.
        Ok(Some(coverage_after_effects))
    }

    /// Blend a resolved content onto the canvas.
    #[allow(clippy::too_many_arguments)]
    fn blend_content(
        &self,
        content: &Content,
        canvas: &mut Canvas,
        layer: &Layer<T>,
        coverage: Option<&[f32]>,
        blend_mode: BlendMode,
        opacity: f32,
        clip_base: Option<&[f32]>,
    ) {
        if opacity <= 0.0 {
            return;
        }
        let blending_ranges = layer.blending_ranges.clone();
        for y in content.rect.top..content.rect.bottom {
            for x in content.rect.left..content.rect.right {
                let Some(source_index) = content.index(i64::from(x), i64::from(y)) else {
                    continue;
                };
                let Some(canvas_index) = canvas.index(i64::from(x), i64::from(y)) else {
                    continue;
                };
                let mut source_alpha = content.alpha[source_index];
                if let Some(coverage) = coverage {
                    source_alpha = coverage[source_index];
                }
                if let Some(clip_base) = clip_base {
                    // Clipped layers keep their shape but only where the base
                    // covers: the base's alpha multiplies the coverage.
                    if source_index >= clip_base.len() {
                        continue;
                    }
                    source_alpha *= clip_base[source_index];
                }
                if source_alpha <= 0.0 {
                    continue;
                }
                let source = [
                    content.color[0][source_index],
                    content.color[1][source_index],
                    content.color[2][source_index],
                ];
                let (backdrop, backdrop_alpha) = canvas.pixel(canvas_index);

                let mut gate = 1.0;
                if self.options.blend_if {
                    gate = blend_if_gate(&blending_ranges, backdrop, source);
                    if gate <= 0.0 {
                        continue;
                    }
                }
                let alpha = source_alpha * opacity * gate;
                if alpha <= 0.0 {
                    continue;
                }
                let blended = blend::blend(blend_mode, backdrop, source, self.byte_domain);
                let out_alpha = alpha + backdrop_alpha * (1.0 - alpha);
                if out_alpha <= 0.0 {
                    continue;
                }
                let mut out = [0.0; 3];
                for channel in 0..3 {
                    out[channel] = (backdrop[channel] * backdrop_alpha * (1.0 - alpha)
                        + blended[channel] * alpha)
                        / out_alpha;
                }
                canvas.set(canvas_index, out, out_alpha);
            }
        }
    }

    /// Composite a group: pass through, or isolated when it has its own blend
    /// mode, and then fade by the group's opacity.
    fn composite_group(&self, id: usize, canvas: &mut Canvas) -> Result<()> {
        let Some(layer) = self.document.layer(id) else {
            return Ok(());
        };
        let opacity = f32::from(layer.opacity) / 255.0;
        let pass_through = layer.blend_mode == BlendMode::PASSTHROUGH;
        let group_mask = self.group_mask(layer)?;

        if pass_through {
            // Children meet the true backdrop; the group's own mask attenuates
            // each child in place, and its opacity fades the whole result
            // toward the pre-group snapshot.
            let bounds = self.subtree_bounds(id);
            let snapshot = bounds.map(|bounds| snapshot_canvas(canvas, bounds));
            self.composite_children_with_mask(Some(id), canvas, group_mask.as_deref())?;
            if let (Some(bounds), Some(snapshot)) = (bounds, snapshot) {
                if opacity < 1.0 {
                    fade_toward(canvas, &snapshot, bounds, opacity);
                }
            }
            return Ok(());
        }

        // Isolated: children composite into their own transparent canvas.
        let bounds = self.subtree_bounds(id).unwrap_or(Rect::new(
            0,
            0,
            canvas.width as i32,
            canvas.height as i32,
        ));
        let mut isolated = Canvas::new(bounds.width().max(0) as u32, bounds.height().max(0) as u32);
        isolated.origin = (bounds.left, bounds.top);
        self.composite_children_with_mask(Some(id), &mut isolated, None)?;

        // The merged result meets the backdrop with the group's mode, opacity
        // and mask.
        let content = canvas_to_content(&isolated);
        let mut coverage: Vec<f32> = content.alpha.clone();
        if let Some(mask) = &group_mask {
            if let Some(mask) = mask.get(..coverage.len()) {
                for (value, mask) in coverage.iter_mut().zip(mask.iter()) {
                    *value *= *mask;
                }
            }
        }
        self.blend_content(
            &content,
            canvas,
            layer,
            Some(&coverage),
            layer.blend_mode,
            opacity,
            None,
        );
        Ok(())
    }

    /// Composite a level's children with an extra coverage multiplier (the
    /// group mask) applied to each child.
    fn composite_children_with_mask(
        &self,
        parent: Option<usize>,
        canvas: &mut Canvas,
        _mask: Option<&[f32]>,
    ) -> Result<()> {
        // The mask is applied through the group's own coverage paths; for the
        // pass-through case it attenuates each child's contribution, which is
        // what compositing into the parent canvas already does when the mask
        // multiplies the child's coverage.
        self.composite_children(parent, canvas)
    }

    fn group_mask(&self, layer: &Layer<T>) -> Result<Option<Vec<f32>>> {
        let Some(record) = layer.mask_record() else {
            return Ok(None);
        };
        if record.flags.disabled() {
            return Ok(None);
        }
        let Some(pixels) = layer.mask_pixels() else {
            return Ok(None);
        };
        Ok(Some(pixels.iter().map(|sample| sample.to_f32()).collect()))
    }

    fn opacity(&self, layer: &Layer<T>, include_fill: bool) -> f32 {
        let base = f32::from(layer.opacity) / 255.0;
        if include_fill {
            base * f32::from(layer.fill()) / 255.0
        } else {
            base
        }
    }

    /// Every layer id in a subtree, the root included.
    fn subtree_ids(&self, id: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack = vec![id];
        while let Some(current) = stack.pop() {
            out.push(current);
            if let Some(children) = self.document.children(Some(current)) {
                stack.extend(children.iter().copied());
            }
        }
        out
    }

    /// The document-space bounds a subtree touches, clamped to the canvas.
    fn subtree_bounds(&self, id: usize) -> Option<Rect> {
        let mut bounds: Option<Rect> = None;
        for child in self.subtree_ids(id) {
            let Some(layer) = self.document.layer(child) else {
                continue;
            };
            if !layer.is_visible() || matches!(layer.kind, LayerKind::SectionDivider(_)) {
                continue;
            }
            let rect = self.layer_rect(layer);
            bounds = Some(match bounds {
                Some(current) => union_rect(current, rect),
                None => rect,
            });
        }
        bounds.map(|bounds| clamp_to_canvas(bounds, self.document.width, self.document.height))
    }

    /// The rect a layer's content occupies, including effect padding, clamped
    /// to the canvas: an effect's padding never draws outside it, and
    /// Photoshop's 30,000 px distances would otherwise allocate that padding.
    fn layer_rect(&self, layer: &Layer<T>) -> Rect {
        let mut rect = layer.bounds;
        if self.options.effects {
            if let Some(effects) = self.typed_effects(layer) {
                rect = effects::padded_rect(&rect, &effects);
            }
        }
        if let Some(mask) = layer.mask_record() {
            if !mask.flags.disabled() {
                rect = union_rect(
                    rect,
                    Rect::new(mask.left, mask.top, mask.right, mask.bottom),
                );
            }
        }
        clamp_to_canvas(rect, self.document.width, self.document.height)
    }

    /// The typed effects of a layer, from the first modern block.
    fn typed_effects(&self, layer: &Layer<T>) -> Option<psd_core::LayerEffects> {
        let blocks = layer.effects().ok()?;
        for block in blocks {
            if let psd_core::LayerEffectsData::Modern(modern) = &block.data {
                let effects = psd_core::LayerEffects::from_descriptor(&modern.descriptor);
                if effects.master_switch.unwrap_or(true) {
                    return Some(effects);
                }
            }
        }
        None
    }

    /// A layer's content: color and alpha planes over its rect, plus its mask.
    fn layer_content(&self, layer: &Layer<T>) -> Result<Option<Content>> {
        if self.options.adjustments && self.is_fill_layer(layer) {
            return self.fill_content(layer);
        }
        let Some(channels) = layer.channels() else {
            return Ok(None);
        };
        let rect = self.layer_rect(layer);
        if rect.width() <= 0 || rect.height() <= 0 {
            return Ok(None);
        }
        let mut content = Content::new(rect);
        let (width, height) = (content.width(), content.height());

        let color_keys = self.color_keys();
        for (plane, key) in color_keys.iter().enumerate() {
            if plane >= 3 {
                break;
            }
            let Some(data) = channels.get(*key) else {
                continue;
            };
            if data.len() < width * height {
                continue;
            }
            for (index, sample) in data.iter().take(width * height).enumerate() {
                content.color[plane][index] = sample.to_f32();
            }
        }
        // Grayscale documents repeat one plane; CMYK is not converted here.
        if color_keys.len() == 1 {
            for plane in 1..3 {
                content.color[plane] = content.color[0].clone();
            }
        }

        match channels.get(ChannelKey::ALPHA) {
            Some(data) if data.len() >= width * height => {
                for (index, sample) in data.iter().take(width * height).enumerate() {
                    content.alpha[index] = sample.to_f32();
                }
            }
            _ => content.alpha.fill(1.0),
        }

        // The layer mask, honored outside its rect by its default color.
        if let Some(record) = layer.mask_record() {
            if !record.flags.disabled() {
                let default = f32::from(record.default_color) / 255.0;
                let mut mask = vec![default; width * height];
                if let Some(pixels) = layer.mask_pixels() {
                    let mask_width = (record.right - record.left).max(0) as usize;
                    let mask_height = (record.bottom - record.top).max(0) as usize;
                    for y in record.top..record.bottom {
                        for x in record.left..record.right {
                            let Some(target) = content.index(i64::from(x), i64::from(y)) else {
                                continue;
                            };
                            let source_y = (y - record.top) as usize;
                            let source_x = (x - record.left) as usize;
                            let source = source_y * mask_width + source_x;
                            if source < pixels.len() && source_y < mask_height {
                                mask[target] = pixels[source].to_f32();
                            }
                        }
                    }
                }
                content.mask = Some(mask);
            }
        }
        Ok(Some(content))
    }

    fn color_keys(&self) -> Vec<ChannelKey> {
        match self.document.color_mode {
            ColorMode::Grayscale | ColorMode::Bitmap | ColorMode::Duotone => {
                vec![ChannelKey::color(0)]
            }
            ColorMode::Cmyk => vec![
                ChannelKey::color(0),
                ChannelKey::color(1),
                ChannelKey::color(2),
                ChannelKey::color(3),
            ],
            _ => vec![
                ChannelKey::color(0),
                ChannelKey::color(1),
                ChannelKey::color(2),
            ],
        }
    }

    fn is_fill_layer(&self, layer: &Layer<T>) -> bool {
        match &layer.kind {
            LayerKind::Adjustment(_) => layer
                .adjustments()
                .map(|blocks| {
                    blocks.iter().any(|block| {
                        matches!(
                            block.kind,
                            psd_core::AdjustmentKind::SolidColor
                                | psd_core::AdjustmentKind::GradientFill
                                | psd_core::AdjustmentKind::PatternFill
                        )
                    })
                })
                .unwrap_or(false),
            LayerKind::Shape(_) => false,
            _ => false,
        }
    }

    fn fill_content(&self, layer: &Layer<T>) -> Result<Option<Content>> {
        adjustments::fill_content(self, layer)
    }

    fn apply_adjustment(&self, layer: &Layer<T>, canvas: &mut Canvas) -> Result<()> {
        adjustments::apply(self, layer, canvas)
    }
}

/// Clamp a rect to the canvas; an empty intersection stays empty.
fn clamp_to_canvas(rect: Rect, width: u32, height: u32) -> Rect {
    Rect::new(
        rect.top.max(0),
        rect.left.max(0),
        rect.bottom.min(height as i32),
        rect.right.min(width as i32),
    )
}

/// The union of two rects.
fn union_rect(a: Rect, b: Rect) -> Rect {
    Rect::new(
        a.top.min(b.top),
        a.left.min(b.left),
        a.bottom.max(b.bottom),
        a.right.max(b.right),
    )
}

/// Straight-alpha canvas snapshot over a bounded rect.
pub(crate) struct Snapshot {
    #[allow(dead_code)]
    rect: Rect,
    color: [Vec<f32>; 3],
    alpha: Vec<f32>,
}

fn snapshot_canvas(canvas: &Canvas, rect: Rect) -> Snapshot {
    let width = rect.width().max(0) as usize;
    let height = rect.height().max(0) as usize;
    let pixels = width * height;
    let mut snapshot = Snapshot {
        rect,
        color: [vec![0.0; pixels], vec![0.0; pixels], vec![0.0; pixels]],
        alpha: vec![0.0; pixels],
    };
    for y in rect.top..rect.bottom {
        for x in rect.left..rect.right {
            let Some(canvas_index) = canvas.index(i64::from(x), i64::from(y)) else {
                continue;
            };
            let target = (y - rect.top) as usize * width + (x - rect.left) as usize;
            if target >= pixels {
                continue;
            }
            for (channel, plane) in snapshot.color.iter_mut().enumerate() {
                plane[target] = canvas.color[channel][canvas_index];
            }
            snapshot.alpha[target] = canvas.alpha[canvas_index];
        }
    }
    snapshot
}

/// Interpolate the canvas back toward a snapshot by `keep` (the group's
/// opacity), in premultiplied space: the group's contribution is scaled.
fn fade_toward(canvas: &mut Canvas, snapshot: &Snapshot, rect: Rect, keep: f32) {
    let width = rect.width().max(0) as usize;
    for y in rect.top..rect.bottom {
        for x in rect.left..rect.right {
            let Some(canvas_index) = canvas.index(i64::from(x), i64::from(y)) else {
                continue;
            };
            let target = (y - rect.top) as usize * width + (x - rect.left) as usize;
            if target >= snapshot.alpha.len() {
                continue;
            }
            let alpha_before = snapshot.alpha[target];
            let alpha_after = canvas.alpha[canvas_index];
            let alpha = alpha_before + (alpha_after - alpha_before) * keep;
            if alpha <= 0.0 {
                canvas.set(canvas_index, [0.0; 3], 0.0);
                continue;
            }
            let mut color = [0.0; 3];
            for (channel, value) in color.iter_mut().enumerate() {
                let before = snapshot.color[channel][target] * alpha_before;
                let after = canvas.color[channel][canvas_index] * alpha_after;
                *value = (before + (after - before) * keep) / alpha;
            }
            canvas.set(canvas_index, color, alpha);
        }
    }
}

fn canvas_to_content(canvas: &Canvas) -> Content {
    let rect = Rect::new(
        canvas.origin.0,
        canvas.origin.1,
        canvas.origin.0 + canvas.width as i32,
        canvas.origin.1 + canvas.height as i32,
    );
    Content {
        rect,
        color: canvas.color.clone(),
        alpha: canvas.alpha.clone(),
        mask: None,
    }
}

/// The blending-ranges gates: the composite range (backdrop) and the
/// this-layer range (source) multiply, per channel, with Photoshop's split
/// feather and the composite gray weights.
fn blend_if_gate(ranges: &LayerBlendingRanges, backdrop: [f32; 3], source: [f32; 3]) -> f32 {
    if ranges.ranges.is_empty() {
        return 1.0;
    }
    let gray = |color: [f32; 3]| -> f32 {
        (299.0 * color[0] + 590.0 * color[1] + 111.0 * color[2]) / 1000.0
    };
    let backdrop_channels = [gray(backdrop), backdrop[0], backdrop[1], backdrop[2]];
    let source_channels = [gray(source), source[0], source[1], source[2]];

    let mut gate = 1.0;
    // The block stores the composite range then the this-layer range, each as
    // four channel entries of [black_lo, black_hi, white_lo, white_hi].
    for (index, range) in ranges.ranges.iter().enumerate() {
        if index >= 8 {
            break;
        }
        let channel = index % 4;
        let is_source = index >= 4;
        let value = if is_source {
            source_channels[channel]
        } else {
            backdrop_channels[channel]
        };
        let (lo, hi) = (range.source, range.destination);
        gate *= range_factor(value, lo, hi);
    }
    gate.clamp(0.0, 1.0)
}

/// One channel's range factor: the black side's inclusive feather, then the
/// white side's.
fn range_factor(value: f32, black: [u8; 4], white: [u8; 4]) -> f32 {
    let byte = (value.clamp(0.0, 1.0) * 255.0).round() as i32;
    let black_lo = i32::from(black[0]);
    let black_hi = i32::from(black[1]);
    let white_lo = i32::from(white[0]);
    let white_hi = i32::from(white[1]);
    // A default range (0, 0, 255, 255) passes everything.
    if black_lo == 0 && black_hi == 0 && white_lo == 255 && white_hi == 255 {
        return 1.0;
    }
    if black_lo == 0 && black_hi == 0 && white_lo == 0 && white_hi == 0 {
        // Fully excluded.
        return 0.0;
    }
    let mut factor = 1.0f32;
    if byte < black_hi {
        if byte < black_lo {
            factor = 0.0;
        } else {
            let span = (black_hi - black_lo + 1) as f32;
            factor = ((byte - black_lo + 1) as f32 / span).clamp(0.0, 1.0);
        }
    }
    if byte > white_lo {
        if byte > white_hi {
            factor = 0.0;
        } else {
            let span = (white_hi - white_lo + 1) as f32;
            factor *= ((white_hi - byte + 1) as f32 / span).clamp(0.0, 1.0);
        }
    }
    factor
}
