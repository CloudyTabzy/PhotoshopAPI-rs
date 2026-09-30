//! Layer-stack compositing: flatten a document the way Photoshop renders it.
//!
//! The engine walks the layer tree bottom-to-top and blends each layer into a
//! straight-alpha RGB canvas:
//!
//! 1. **Content.** A layer's color and alpha channels are placed at its bounds,
//!    clipped to the canvas; a layer with no alpha channel is opaque inside its
//!    bounds. Fill layers generate their content instead (solid color or
//!    gradient or pattern). Shape layers render from their vector paths and
//!    strokes; text layers contribute the raster preview Photoshop stored.
//! 2. **Effects.** Outer effects (drop shadow, outer glow) paint *below* the
//!    content; interior overlays fold into the content's color, or paint
//!    independently when the fill and effects are separated; interior
//!    effects (satin, inner glow, inner shadow) paint above it inside the
//!    silhouette. The renderer evaluates effect geometry and blending in
//!    separate internal modules.
//! 3. **Coverage.** Alpha × the layer mask (raster `-2` channel, honoring its
//!    default color outside the mask rect) × clipping (a `clip = 1` layer is
//!    masked by its base's coverage) × the blend-if gates.
//! 4. **Blend.** The calibrated blend math with layer opacity and
//!    fill opacity, then source-over onto the canvas.
//! 5. **Adjustment layers** transform the backdrop in place inside their mask.
//! 6. **Groups** either pass through (children meet the true backdrop, then the
//!    whole group fades toward the pre-group snapshot by the group's opacity) or
//!    isolate (children composite into a transparent buffer that meets the
//!    backdrop with the group's blend mode, opacity and mask).
//!
//! Documented gaps: noise and jitter, soft-effect contour shaping, knockout,
//! the "Precise" glow techniques, special fill-opacity blend kernels, per-range
//! Hue/Saturation adjustments, and profile-aware CMYK/Lab document conversion.
//!
//! The canvas is float, and for 8-bit documents the modes whose rounding
//! Photoshop pins (Color Burn, Color Dodge, Exclusion, Divide) are computed in
//! the byte domain so a flatten can match Photoshop byte for byte where the
//! math is exact.

mod adjustments;
mod bevel;
mod blend;
mod contour;
mod effects;
mod masks;
mod paths;
mod ramp;
mod shapes;
mod stroke;
mod tile;

use psd_core::{BlendMode, ColorMode, LayerBlendingRanges, PsdError, Result};

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
    /// The silhouette layer effects follow when it differs from the alpha (a
    /// shape's path, whatever its fill's transparency), same rect.
    pub shape: Option<Vec<f32>>,
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
            shape: None,
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

/// A coverage plane over a document-space rect; zero outside it.
pub(crate) struct Plane {
    pub rect: Rect,
    pub data: Vec<f32>,
}

impl Plane {
    pub fn at(&self, x: i32, y: i32) -> f32 {
        if x < self.rect.left || y < self.rect.top || x >= self.rect.right || y >= self.rect.bottom
        {
            return 0.0;
        }
        let width = self.rect.width() as usize;
        self.data
            .get((y - self.rect.top) as usize * width + (x - self.rect.left) as usize)
            .copied()
            .unwrap_or(0.0)
    }
}

/// The compositing engine for one document.
struct Compositor<'a, T: BitDepth> {
    document: &'a LayeredFile<T>,
    options: CompositeOptions,
    /// 8-bit documents get Photoshop's byte-domain blend rounding.
    byte_domain: bool,
    /// The document's pattern tiles, decoded once.
    patterns: Vec<psd_core::pattern::Pattern>,
}

impl<T: BitDepth> LayeredFile<T> {
    /// Flatten the layer stack into an 8-bit straight-alpha RGBA image.
    ///
    /// Visible layers must have decoded channels. After a lazy read, call
    /// [`decode_layer_pixels`](Self::decode_layer_pixels) for those layers
    /// first; otherwise this returns [`PsdError::InvalidData`]. Decoding stays
    /// explicit so the document's bitmap memory budget is honored.
    pub fn composite_rgba8(&self) -> Result<CompositeImage> {
        self.composite_rgba8_with(CompositeOptions::default())
    }

    /// Flatten the layer stack with explicit options.
    ///
    /// Like [`composite_rgba8`](Self::composite_rgba8), this requires decoded
    /// channels for visible layers that participate in rendering.
    pub fn composite_rgba8_with(&self, options: CompositeOptions) -> Result<CompositeImage> {
        let compositor = Compositor {
            document: self,
            options,
            byte_domain: T::DEPTH == 8,
            patterns: if options.effects || options.adjustments {
                self.patterns()
            } else {
                Vec::new()
            },
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
        let mut base_coverage = self.composite_layer(base, canvas, None)?;
        // A group or adjustment base has no pixels of its own to report, but
        // the layers clipped to it still need its footprint.
        if base_coverage.is_none() && !clipped.is_empty() {
            base_coverage = self.footprint(base, canvas);
        }

        for &clipped_id in clipped {
            let Some(clipped_layer) = self.document.layer(clipped_id) else {
                continue;
            };
            if !clipped_layer.is_visible() {
                continue;
            }
            self.composite_layer(clipped_id, canvas, base_coverage.as_ref())?;
        }
        Ok(())
    }

    /// The coverage layers clipped to a group or an adjustment layer see: the
    /// union silhouette of a group's children, or an adjustment's mask, scaled
    /// by the base's opacity (and fill, for an adjustment).
    fn footprint(&self, id: usize, canvas: &Canvas) -> Option<Plane> {
        let layer = self.document.layer(id)?;
        let canvas_rect = Rect::new(
            canvas.origin.1,
            canvas.origin.0,
            canvas.origin.1 + canvas.height as i32,
            canvas.origin.0 + canvas.width as i32,
        );
        match &layer.kind {
            LayerKind::Group(_) => {
                let rect = self.subtree_bounds(id)?;
                let mut isolated =
                    Canvas::new(rect.width().max(0) as u32, rect.height().max(0) as u32);
                isolated.origin = (rect.left, rect.top);
                self.composite_children(Some(id), &mut isolated).ok()?;
                let mask = self.mask_plane(layer, rect);
                let strength = self.opacity(layer, false);
                let data = isolated
                    .alpha
                    .iter()
                    .enumerate()
                    .map(|(index, alpha)| {
                        alpha * mask.as_ref().map_or(1.0, |mask| mask[index]) * strength
                    })
                    .collect();
                Some(Plane { rect, data })
            }
            LayerKind::Adjustment(_) => {
                let strength = self.opacity(layer, true);
                let mask = self.mask_plane(layer, canvas_rect);
                let pixels =
                    canvas_rect.width().max(0) as usize * canvas_rect.height().max(0) as usize;
                let data = (0..pixels)
                    .map(|index| mask.as_ref().map_or(1.0, |mask| mask[index]) * strength)
                    .collect();
                Some(Plane {
                    rect: canvas_rect,
                    data,
                })
            }
            _ => None,
        }
    }

    /// Composite one layer. `clip_base` is the clipping base's coverage, when
    /// this layer is clipped to one. Returns the layer's own coverage (used as
    /// the clip base for the layers above it).
    fn composite_layer(
        &self,
        id: usize,
        canvas: &mut Canvas,
        clip_base: Option<&Plane>,
    ) -> Result<Option<Plane>> {
        let Some(layer) = self.document.layer(id) else {
            return Ok(None);
        };
        if matches!(layer.kind, LayerKind::Adjustment(_))
            && !self.is_fill_layer(layer)
            && !self.options.adjustments
        {
            return Ok(None);
        }
        if layer
            .channels()
            .is_some_and(|channels| channels.raw_channels().next().is_some())
        {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "compositing requires decoded channels; call decode_layer_pixels for visible layers after a lazy read",
            });
        }
        match &layer.kind {
            LayerKind::SectionDivider(_) => Ok(None),
            LayerKind::Group(_) => {
                self.composite_group(id, canvas, clip_base)?;
                Ok(None)
            }
            // A fill layer is content; every other adjustment transforms what
            // is below it.
            LayerKind::Adjustment(_) if !self.is_fill_layer(layer) => {
                if self.options.adjustments {
                    self.apply_adjustment(layer, canvas, clip_base)?;
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
        clip_base: Option<&Plane>,
    ) -> Result<Option<Plane>> {
        let mut content = self.layer_content(layer)?;
        let Some(content) = content.take() else {
            return Ok(None);
        };
        self.composite_resolved(layer, content, canvas, clip_base, true, layer.blend_mode)
    }

    /// Run a resolved content through the layer pipeline: effects, coverage,
    /// blend. `with_fill` scales the content by the layer's fill opacity (a
    /// group ignores it); `mode` is the blend mode the content meets the
    /// backdrop with.
    fn composite_resolved(
        &self,
        layer: &Layer<T>,
        mut content: Content,
        canvas: &mut Canvas,
        clip_base: Option<&Plane>,
        with_fill: bool,
        mode: BlendMode,
    ) -> Result<Option<Plane>> {
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

        // The matte effects follow: the layer's silhouette (a shape's path
        // even where its fill is transparent) times the mask.
        let mut matte = content
            .shape
            .clone()
            .unwrap_or_else(|| content.alpha.clone());
        if let Some(mask) = &content.mask {
            for (value, mask) in matte.iter_mut().zip(mask.iter()) {
                *value *= *mask;
            }
        }

        // Effects: overlays fold into the color, interior effects paint above
        // it, outer effects composite below.
        let mut outer: Vec<effects::OuterPlane> = Vec::new();
        let mut strokes: Vec<effects::StrokePlane> = Vec::new();
        let mut bevel: Vec<effects::OuterPlane> = Vec::new();
        // Interior effects paint over the canvas after the layer, with their
        // own blend modes, when the layer's mode or fill opacity would
        // otherwise change them (Blend Interior Effects as Group is off).
        let mut interior_after: Vec<effects::OuterPlane> = Vec::new();
        if self.options.effects {
            if let Some(effects) = self.typed_effects(layer) {
                let canvas_rect = Rect::new(
                    canvas.origin.1,
                    canvas.origin.0,
                    canvas.origin.1 + canvas.height as i32,
                    canvas.origin.0 + canvas.width as i32,
                );
                let as_group = self.interior_as_group(layer, mode, with_fill);
                if as_group {
                    let context = self.effect_context(layer, canvas_rect);
                    effects::fold_interior_overlays(&mut content, &matte, &effects, &context);
                    effects::paint_interior(&mut content, &matte, &effects);
                } else if effects::has_interior_effects(&effects) {
                    let context = self.effect_context(layer, canvas_rect);
                    interior_after =
                        effects::build_interior(content.rect, &matte, &effects, &context);
                }
                outer = effects::build_outer(&content, &matte, &effects, canvas_rect);
                // A stroke follows the pixel shape, reshaped by the mask only
                // where the mask is fully black.
                let mut shape = content
                    .shape
                    .clone()
                    .unwrap_or_else(|| content.alpha.clone());
                if let Some(mask) = &content.mask {
                    for (value, mask) in shape.iter_mut().zip(mask.iter()) {
                        if *mask <= 0.0 {
                            *value = 0.0;
                        }
                    }
                }
                let stroke_context = self.effect_context(layer, canvas_rect);
                strokes = effects::build_strokes(&content, &shape, &effects, &stroke_context);
                // Bevel and emboss shade from the layer's own matte, before any
                // stroke knockout, and composite over everything else.
                let context = self.effect_context(layer, canvas_rect);
                bevel = bevel::build(&content, &matte, &effects, &context);
                // Without overprint a stroke's band knocks the layer's own
                // content out.
                for plane in &strokes {
                    for (index, value) in coverage.iter_mut().enumerate() {
                        *value *= plane.content_factor(index);
                    }
                }
            }
        }

        // Exterior effects draw below the layer, each with its own blend mode,
        // at the layer's master opacity. Blend If does not gate effects.
        for plane in &outer {
            self.blend_content(
                &plane.content,
                canvas,
                layer,
                None,
                plane.blend_mode,
                self.opacity(layer, false),
                None,
                false,
            );
        }
        // Strokes in a mode other than Normal blend against the backdrop
        // below the content.
        for plane in strokes.iter().filter(|plane| !plane.above_content()) {
            self.draw_stroke(plane, canvas, layer);
        }

        let mut clip_plane = Plane {
            rect: content.rect,
            data: coverage.clone(),
        };
        // A clipped layer fades with its base: opacity and fill scale the
        // whole clipping group.
        let group_strength = self.opacity(layer, with_fill);
        for value in &mut clip_plane.data {
            *value *= group_strength;
        }
        if interior_after.is_empty() {
            self.blend_content(
                &content,
                canvas,
                layer,
                Some(&coverage),
                mode,
                group_strength,
                clip_base,
                true,
            );
        } else {
            // Resolve content and independent effects at their intrinsic
            // opacity over the actual backdrop, then apply the layer's
            // silhouette, mask, clip coverage and master opacity once to the
            // complete result. This preserves translucent backdrops without
            // exposing hidden fill color through the effects.
            let snapshot = snapshot_canvas(canvas, content.rect);
            let mut content_coverage: Vec<f32> = coverage
                .iter()
                .zip(&matte)
                .map(|(&value, &matte)| {
                    if matte > f32::EPSILON {
                        value / matte
                    } else {
                        0.0
                    }
                })
                .collect();
            for value in &mut content_coverage {
                *value = value.clamp(0.0, 1.0);
            }
            let fill_strength = if with_fill {
                f32::from(layer.fill()) / 255.0
            } else {
                1.0
            };
            self.blend_content(
                &content,
                canvas,
                layer,
                Some(&content_coverage),
                mode,
                fill_strength,
                None,
                true,
            );
            for plane in &mut interior_after {
                for (effect_alpha, &matte) in plane.content.alpha.iter_mut().zip(&matte) {
                    *effect_alpha = if matte > f32::EPSILON {
                        (*effect_alpha / matte).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                }
                self.blend_content(
                    &plane.content,
                    canvas,
                    layer,
                    None,
                    plane.blend_mode,
                    1.0,
                    None,
                    false,
                );
            }
            let mut contribution = matte;
            if let Some(clip_base) = clip_base {
                for (index, value) in contribution.iter_mut().enumerate() {
                    let x = content.rect.left + (index % content.width()) as i32;
                    let y = content.rect.top + (index / content.width()) as i32;
                    *value *= clip_base.at(x, y);
                }
            }
            fade_toward(
                canvas,
                &snapshot,
                self.opacity(layer, false),
                Some(&contribution),
            );
        }
        for plane in strokes.iter().filter(|plane| plane.above_content()) {
            self.draw_stroke(plane, canvas, layer);
        }
        for plane in &bevel {
            self.blend_content(
                &plane.content,
                canvas,
                layer,
                None,
                plane.blend_mode,
                self.opacity(layer, false),
                None,
                false,
            );
        }

        // The clip base coverage a clipped layer above sees is the layer's own
        // coverage including its mask.
        Ok(Some(clip_plane))
    }

    /// Whether the layer's interior effects blend together with its content:
    /// always with "Blend Interior Effects as Group" (`infx`), and in any case
    /// for a Normal layer at full fill, where the two agree.
    fn interior_as_group(&self, layer: &Layer<T>, mode: BlendMode, with_fill: bool) -> bool {
        let key = psd_core::TaggedBlockKey::new(*b"infx");
        let flagged = layer
            .blocks
            .get(key)
            .and_then(|block| block.data.first().copied())
            .is_some_and(|flag| flag != 0);
        let full_fill = !with_fill || layer.fill() == 255;
        flagged || (mode == BlendMode::NORMAL && full_fill)
    }

    /// Composite a stroke plane at the layer's master opacity; Blend If does
    /// not gate effects.
    fn draw_stroke(&self, plane: &effects::StrokePlane, canvas: &mut Canvas, layer: &Layer<T>) {
        self.draw_stroke_with(plane, canvas, layer, self.opacity(layer, false));
    }

    /// [`draw_stroke`](Self::draw_stroke) at an explicit master opacity.
    fn draw_stroke_with(
        &self,
        plane: &effects::StrokePlane,
        canvas: &mut Canvas,
        layer: &Layer<T>,
        opacity: f32,
    ) {
        self.blend_content(
            &plane.content,
            canvas,
            layer,
            None,
            plane.blend_mode,
            opacity * plane.opacity,
            None,
            false,
        );
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
        clip_base: Option<&Plane>,
        gated: bool,
    ) {
        if opacity <= 0.0 {
            return;
        }
        let restricted = self.channel_restrictions(layer);
        if restricted == [true; 3] {
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
                    // covers: the base's coverage multiplies theirs.
                    source_alpha *= clip_base.at(x, y);
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
                if gated && self.options.blend_if {
                    gate = blend_if_gate(&blending_ranges, backdrop, backdrop_alpha, source);
                    if gate <= 0.0 {
                        continue;
                    }
                }
                let alpha = source_alpha * opacity * gate;
                if alpha <= 0.0 {
                    continue;
                }
                let mut blended = blend::blend(blend_mode, backdrop, source, self.byte_domain);
                if backdrop_alpha < 1.0 {
                    // Over a partly transparent backdrop the source keeps its
                    // own colour in proportion to the missing backdrop.
                    for channel in 0..3 {
                        blended[channel] = source[channel] * (1.0 - backdrop_alpha)
                            + blended[channel] * backdrop_alpha;
                    }
                }
                let out_alpha = alpha + backdrop_alpha * (1.0 - alpha);
                if out_alpha <= 0.0 {
                    continue;
                }
                let mut out = [0.0; 3];
                for channel in 0..3 {
                    out[channel] = if restricted[channel] {
                        // An excluded channel keeps the backdrop's
                        // premultiplied value.
                        backdrop[channel] * backdrop_alpha / out_alpha
                    } else {
                        (backdrop[channel] * backdrop_alpha * (1.0 - alpha)
                            + blended[channel] * alpha)
                            / out_alpha
                    };
                }
                canvas.set(canvas_index, out, out_alpha);
            }
        }
    }

    /// Composite a group: pass through, or isolated when it has its own blend
    /// mode, and then fade by the group's opacity and mask.
    fn composite_group(
        &self,
        id: usize,
        canvas: &mut Canvas,
        clip_base: Option<&Plane>,
    ) -> Result<()> {
        let Some(clip_base) = clip_base else {
            return self.composite_group_unclipped(id, canvas);
        };
        let canvas_rect = Rect::new(
            canvas.origin.1,
            canvas.origin.0,
            canvas.origin.1 + canvas.height as i32,
            canvas.origin.0 + canvas.width as i32,
        );
        let mut rect = self.subtree_bounds(id).unwrap_or(canvas_rect);
        if let Some(layer) = self.document.layer(id) {
            if self.options.effects {
                if let Some(effects) = self.typed_effects(layer) {
                    rect = effects::padded_rect(&rect, &effects);
                }
            }
            if let Some(board) = layer.artboard()?.and_then(|board| board.rect()) {
                rect = union_rect(
                    rect,
                    Rect::new(
                        board.top.round() as i32,
                        board.left.round() as i32,
                        board.bottom.round() as i32,
                        board.right.round() as i32,
                    ),
                );
            }
        }
        rect = clamp_to_canvas_rect(rect, canvas_rect);
        // Preserve the true backdrop for pass-through children, then gate the
        // whole group's contribution (effects included) in premultiplied space.
        let snapshot = snapshot_canvas(canvas, rect);
        self.composite_group_unclipped(id, canvas)?;
        let mut coverage = Vec::with_capacity(snapshot.alpha.len());
        for y in rect.top..rect.bottom {
            for x in rect.left..rect.right {
                coverage.push(clip_base.at(x, y));
            }
        }
        fade_toward(canvas, &snapshot, 1.0, Some(&coverage));
        Ok(())
    }

    fn composite_group_unclipped(&self, id: usize, canvas: &mut Canvas) -> Result<()> {
        let Some(layer) = self.document.layer(id) else {
            return Ok(());
        };
        let opacity = f32::from(layer.opacity) / 255.0;
        let bounds = self.subtree_bounds(id);
        let effects = if self.options.effects {
            self.typed_effects(layer)
        } else {
            None
        };
        // A group with a non-default Blend If range always isolates, and so
        // does a group with a fill opacity: its adjustments then reach only
        // its own content, and the fill scales the merged result like the
        // opacity does (a styled group ignores its fill).
        let group_fill = effects.is_none() && layer.fill() != 255;
        // An artboard clips its content to its own rectangle and paints its
        // background first, so it composites as an isolated unit.
        let artboard = layer.artboard().ok().flatten();
        let pass_through = layer.blend_mode == BlendMode::PASSTHROUGH
            && !(self.options.blend_if && blend_if_active(&layer.blending_ranges))
            && !group_fill
            && artboard.is_none();
        let canvas_rect = Rect::new(
            canvas.origin.1,
            canvas.origin.0,
            canvas.origin.1 + canvas.height as i32,
            canvas.origin.0 + canvas.width as i32,
        );

        if pass_through {
            // Children meet the true backdrop; the group's opacity and mask
            // then fade the whole result toward the pre-group snapshot.
            let Some(bounds) = bounds else {
                return Ok(());
            };
            let styled = effects.filter(effects::has_effects);
            let silhouette = styled
                .as_ref()
                .map(|effects| self.group_silhouette(id, layer, bounds, effects, canvas_rect))
                .transpose()?;
            // Exterior effects paint first, from the children's silhouette.
            if let (Some(effects), Some((content, coverage))) = (&styled, &silhouette) {
                for plane in effects::build_outer(content, coverage, effects, canvas_rect) {
                    self.blend_content(
                        &plane.content,
                        canvas,
                        layer,
                        None,
                        plane.blend_mode,
                        opacity,
                        None,
                        false,
                    );
                }
            }
            let snapshot = snapshot_canvas(canvas, bounds);
            self.composite_children(Some(id), canvas)?;
            let mask = self.mask_plane(layer, bounds);
            if opacity < 1.0 || mask.is_some() {
                fade_toward(canvas, &snapshot, opacity, mask.as_deref());
            }
            // Interior effects paint above the children, inside the silhouette.
            if let (Some(effects), Some((content, coverage))) = (&styled, &silhouette) {
                self.paint_group_interior(
                    canvas,
                    layer,
                    content,
                    coverage,
                    effects,
                    canvas_rect,
                    opacity,
                );
            }
            return Ok(());
        }

        // Isolated: children composite into their own transparent canvas,
        // wide enough for the group's own effects.
        let mut rect = bounds.unwrap_or(canvas_rect);
        if let Some(board) = artboard.as_ref().and_then(|board| board.rect()) {
            rect = clamp_to_canvas_rect(
                Rect::new(
                    board.top.round() as i32,
                    board.left.round() as i32,
                    board.bottom.round() as i32,
                    board.right.round() as i32,
                ),
                canvas_rect,
            );
        }
        if let Some(effects) = &effects {
            rect = clamp_to_canvas_rect(effects::padded_rect(&rect, effects), canvas_rect);
        }
        let mut isolated = Canvas::new(rect.width().max(0) as u32, rect.height().max(0) as u32);
        isolated.origin = (rect.left, rect.top);
        if let Some(board) = &artboard {
            let background = match board.background() {
                Some(psd_core::artboard::ArtboardBackground::Black) => Some([0.0; 3]),
                Some(psd_core::artboard::ArtboardBackground::Transparent) => None,
                Some(psd_core::artboard::ArtboardBackground::Custom) => Some(
                    board
                        .background_color()
                        .and_then(psd_core::Color::from_descriptor)
                        .map_or([1.0; 3], |color| ramp::color_rgb(&color)),
                ),
                _ => Some([1.0; 3]),
            };
            if let Some(color) = background {
                for index in 0..isolated.alpha.len() {
                    isolated.set(index, color, 1.0);
                }
            }
        }
        self.composite_children(Some(id), &mut isolated)?;

        // The merged result plays the layer's role: effects, then it meets the
        // backdrop with the group's mode, opacity and mask (the group's fill
        // opacity is ignored).
        let mut content = canvas_to_content(&isolated);
        content.mask = self.mask_plane(layer, rect);
        let mode = if layer.blend_mode == BlendMode::PASSTHROUGH {
            BlendMode::NORMAL
        } else {
            layer.blend_mode
        };
        self.composite_resolved(layer, content, canvas, None, group_fill, mode)?;
        Ok(())
    }

    /// A pass-through group's silhouette: the children composited on their own,
    /// as a content and its coverage (alpha × the group's mask), over the
    /// group's bounds padded for its effects.
    fn group_silhouette(
        &self,
        id: usize,
        layer: &Layer<T>,
        bounds: Rect,
        effects: &psd_core::LayerEffects,
        canvas_rect: Rect,
    ) -> Result<(Content, Vec<f32>)> {
        let rect = clamp_to_canvas_rect(effects::padded_rect(&bounds, effects), canvas_rect);
        let mut isolated = Canvas::new(rect.width().max(0) as u32, rect.height().max(0) as u32);
        isolated.origin = (rect.left, rect.top);
        self.composite_children(Some(id), &mut isolated)?;
        let mut content = canvas_to_content(&isolated);
        content.mask = self.mask_plane(layer, rect);
        let mut coverage = content.alpha.clone();
        if let Some(mask) = &content.mask {
            for (value, mask) in coverage.iter_mut().zip(mask) {
                *value *= *mask;
            }
        }
        Ok((content, coverage))
    }

    /// Interior effects of a pass-through group, painted over the canvas
    /// inside the group's silhouette, each with its own blend mode.
    #[allow(clippy::too_many_arguments)]
    fn paint_group_interior(
        &self,
        canvas: &mut Canvas,
        layer: &Layer<T>,
        silhouette: &Content,
        coverage: &[f32],
        effects: &psd_core::LayerEffects,
        canvas_rect: Rect,
        opacity: f32,
    ) {
        let scaled: Vec<f32> = coverage.iter().map(|value| value * opacity).collect();
        let copy_canvas = |canvas: &Canvas| -> Content {
            let mut content = Content::new(silhouette.rect);
            for y in silhouette.rect.top..silhouette.rect.bottom {
                for x in silhouette.rect.left..silhouette.rect.right {
                    if let (Some(source), Some(target)) = (
                        canvas.index(i64::from(x), i64::from(y)),
                        content.index(i64::from(x), i64::from(y)),
                    ) {
                        for channel in 0..3 {
                            content.color[channel][target] = canvas.color[channel][source];
                        }
                        content.alpha[target] = canvas.alpha[source];
                    }
                }
            }
            content
        };
        let write_back = |canvas: &mut Canvas, content: &Content, weights: Option<&[f32]>| {
            for y in silhouette.rect.top..silhouette.rect.bottom {
                for x in silhouette.rect.left..silhouette.rect.right {
                    let (Some(target), Some(source)) = (
                        canvas.index(i64::from(x), i64::from(y)),
                        content.index(i64::from(x), i64::from(y)),
                    ) else {
                        continue;
                    };
                    let weight = weights.map_or(1.0, |weights| weights[source]);
                    if weight <= 0.0 {
                        continue;
                    }
                    for channel in 0..3 {
                        let old = canvas.color[channel][target];
                        canvas.color[channel][target] =
                            old + (content.color[channel][source] - old) * weight;
                    }
                }
            }
        };

        // Overlays fold into the colours inside the silhouette.
        let mut folded = copy_canvas(canvas);
        let context = self.effect_context(layer, canvas_rect);
        effects::fold_interior_overlays(&mut folded, coverage, effects, &context);
        write_back(canvas, &folded, Some(&scaled));
        // Soft interior effects then paint above.
        let mut painted = copy_canvas(canvas);
        effects::paint_interior(&mut painted, &scaled, effects);
        write_back(canvas, &painted, None);
        // Strokes draw above, at the group's opacity.
        for plane in effects::build_strokes(silhouette, coverage, effects, &context) {
            self.draw_stroke_with(&plane, canvas, layer, opacity);
        }
    }

    /// The effect renderers' view of the document around a layer.
    fn effect_context(&self, layer: &Layer<T>, canvas: Rect) -> effects::EffectContext<'_> {
        // `fxrp`: the effects reference point, two big-endian doubles.
        let key = psd_core::TaggedBlockKey::new(*b"fxrp");
        let reference = layer
            .blocks
            .get(key)
            .filter(|block| block.data.len() >= 16)
            .map_or((0.0, 0.0), |block| {
                let read = |at: usize| {
                    f64::from_be_bytes(block.data[at..at + 8].try_into().unwrap_or([0; 8]))
                };
                (read(0), read(8))
            });
        effects::EffectContext {
            canvas,
            patterns: &self.patterns,
            reference,
        }
    }

    /// The colour channels Advanced Blending excludes (`brst` lists them).
    fn channel_restrictions(&self, layer: &Layer<T>) -> [bool; 3] {
        let mut restricted = [false; 3];
        let key = psd_core::TaggedBlockKey::new(*b"brst");
        if let Some(block) = layer.blocks.get(key) {
            for chunk in block.data.chunks_exact(4) {
                let channel = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                if let Some(slot) = restricted.get_mut(channel as usize) {
                    *slot = true;
                }
            }
        }
        restricted
    }

    fn opacity(&self, layer: &Layer<T>, include_fill: bool) -> f32 {
        let base = f32::from(layer.opacity) / 255.0;
        if include_fill {
            base * f32::from(layer.fill()) / 255.0
        } else {
            base
        }
    }

    /// The document-space bounds a subtree touches, clamped to the canvas.
    fn subtree_bounds(&self, id: usize) -> Option<Rect> {
        let mut bounds: Option<Rect> = None;
        // The group itself has no pixels of its own; its descendants do.
        for &child in self.document.children(Some(id))? {
            let Some(layer) = self.document.layer(child) else {
                continue;
            };
            if !layer.is_visible() || matches!(layer.kind, LayerKind::SectionDivider(_)) {
                continue;
            }
            let rect = if matches!(layer.kind, LayerKind::Group(_)) {
                // A nested group's effects follow its merged children, not
                // the often-empty rectangle in its layer record. Carry those
                // bounds upward so snapshots include every rendered pixel.
                let mut rect = self.subtree_bounds(child);
                if let Some(board) = layer
                    .artboard()
                    .ok()
                    .flatten()
                    .and_then(|board| board.rect())
                {
                    let board = Rect::new(
                        board.top.round() as i32,
                        board.left.round() as i32,
                        board.bottom.round() as i32,
                        board.right.round() as i32,
                    );
                    rect = Some(rect.map_or(board, |rect| union_rect(rect, board)));
                }
                let Some(mut rect) = rect else {
                    continue;
                };
                if self.options.effects {
                    if let Some(effects) = self.typed_effects(layer) {
                        rect = effects::padded_rect(&rect, &effects);
                    }
                }
                rect
            } else if matches!(layer.kind, LayerKind::Adjustment(_))
                || (self.options.adjustments && self.is_fill_layer(layer))
            {
                // An adjustment reaches every pixel beneath it, and a fill
                // layer covers the whole canvas.
                Rect::new(
                    0,
                    0,
                    self.document.height as i32,
                    self.document.width as i32,
                )
            } else {
                self.layer_rect(layer)
            };
            if rect.width() <= 0 || rect.height() <= 0 {
                continue;
            }
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
                    Rect::new(mask.top, mask.left, mask.bottom, mask.right),
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
        let width = content.width();

        // The channel planes cover the layer's own bounds; the content rect may
        // be padded (effects, mask) or clamped to the canvas, so place them by
        // coordinates, never by linear index.
        let bounds = layer.bounds;
        let (bounds_width, bounds_height) = (
            bounds.width().max(0) as usize,
            bounds.height().max(0) as usize,
        );
        let place = |plane: &mut [f32], data: &[T]| {
            if data.len() < bounds_width * bounds_height {
                return;
            }
            for y in rect.top.max(bounds.top)..rect.bottom.min(bounds.bottom) {
                let source_row = (y - bounds.top) as usize * bounds_width;
                let target_row = (y - rect.top) as usize * width;
                for x in rect.left.max(bounds.left)..rect.right.min(bounds.right) {
                    plane[target_row + (x - rect.left) as usize] =
                        data[source_row + (x - bounds.left) as usize].to_f32();
                }
            }
        };

        let color_keys = self.color_keys();
        for (plane, key) in color_keys.iter().enumerate() {
            if plane >= 3 {
                break;
            }
            if let Some(data) = channels.get(*key) {
                place(&mut content.color[plane], data);
            }
        }
        // Grayscale documents repeat one plane.
        if color_keys.len() == 1 {
            for plane in 1..3 {
                content.color[plane] = content.color[0].clone();
            }
        }
        // CMYK planes are stored inverted (255 is no ink), so the naive
        // device conversion is a product with the black plane. Photoshop
        // converts through an ICC profile; this is the profile-free
        // approximation.
        if color_keys.len() == 4 {
            let mut black = vec![1.0f32; content.color[0].len()];
            if let Some(data) = channels.get(color_keys[3]) {
                black.fill(0.0);
                place(&mut black, data);
            }
            for plane in &mut content.color {
                for (value, k) in plane.iter_mut().zip(&black) {
                    *value *= *k;
                }
            }
        }

        match channels.get(ChannelKey::ALPHA) {
            Some(data) if data.len() >= bounds_width * bounds_height => {
                place(&mut content.alpha, data);
            }
            _ => {
                // No transparency channel: opaque over the layer's bounds only.
                for y in rect.top.max(bounds.top)..rect.bottom.min(bounds.bottom) {
                    let target_row = (y - rect.top) as usize * width;
                    for x in rect.left.max(bounds.left)..rect.right.min(bounds.right) {
                        content.alpha[target_row + (x - rect.left) as usize] = 1.0;
                    }
                }
            }
        }

        content.mask = self.mask_plane(layer, rect);
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
        // A solid, gradient or pattern fill layer, or a shape layer, which
        // Photoshop stores as a fill layer with a vector mask and no pixels.
        match &layer.kind {
            LayerKind::Adjustment(_) | LayerKind::Shape(_) => layer
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
            _ => false,
        }
    }

    fn fill_content(&self, layer: &Layer<T>) -> Result<Option<Content>> {
        adjustments::fill_content(self, layer)
    }

    fn apply_adjustment(
        &self,
        layer: &Layer<T>,
        canvas: &mut Canvas,
        clip_base: Option<&Plane>,
    ) -> Result<()> {
        adjustments::apply(self, layer, canvas, clip_base)
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

/// Clamp a rect to another (the canvas' document-space rect).
fn clamp_to_canvas_rect(rect: Rect, canvas: Rect) -> Rect {
    Rect::new(
        rect.top.max(canvas.top),
        rect.left.max(canvas.left),
        rect.bottom.min(canvas.bottom),
        rect.right.min(canvas.right),
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

/// Interpolate the canvas back toward a snapshot by the group's opacity and
/// coverage, in premultiplied space: the group's contribution is scaled.
fn fade_toward(canvas: &mut Canvas, snapshot: &Snapshot, opacity: f32, mask: Option<&[f32]>) {
    let rect = snapshot.rect;
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
            let keep = opacity
                * mask
                    .and_then(|mask| mask.get(target))
                    .copied()
                    .unwrap_or(1.0);
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
        canvas.origin.1,
        canvas.origin.0,
        canvas.origin.1 + canvas.height as i32,
        canvas.origin.0 + canvas.width as i32,
    );
    Content {
        rect,
        color: canvas.color.clone(),
        alpha: canvas.alpha.clone(),
        mask: None,
        shape: None,
    }
}

/// Whether any blending range differs from the identity.
pub(crate) fn blend_if_active(ranges: &LayerBlendingRanges) -> bool {
    let identity = [0, 0, 255, 255];
    ranges
        .ranges
        .iter()
        .take(4)
        .any(|range| range.source != identity || range.destination != identity)
}

/// The blending-ranges gates. The record holds one entry per channel (Gray,
/// then R, G, B); each carries a This Layer pair and an Underlying Layer pair.
/// The four active gates multiply, and This Layer multiplies Underlying Layer.
/// Underlying coverage is `(1 - backdrop alpha) + backdrop alpha * gate`:
/// transparent backdrop pixels always pass.
pub(crate) fn blend_if_gate(
    ranges: &LayerBlendingRanges,
    backdrop: [f32; 3],
    backdrop_alpha: f32,
    source: [f32; 3],
) -> f32 {
    if ranges.ranges.is_empty() {
        return 1.0;
    }
    let gray = |color: [f32; 3]| -> f32 {
        let weighted = 299.0 * color[0].clamp(0.0, 1.0) * 255.0
            + 590.0 * color[1].clamp(0.0, 1.0) * 255.0
            + 111.0 * color[2].clamp(0.0, 1.0) * 255.0;
        (weighted / 1000.0).round() / 255.0
    };
    let backdrop_channels = [gray(backdrop), backdrop[0], backdrop[1], backdrop[2]];
    let source_channels = [gray(source), source[0], source[1], source[2]];

    let mut this_gate = 1.0f32;
    let mut underlying_gate = 1.0f32;
    for (channel, range) in ranges.ranges.iter().take(4).enumerate() {
        this_gate *= range_factor(source_channels[channel], range.source);
        underlying_gate *= range_factor(backdrop_channels[channel], range.destination);
    }
    let underlying = (1.0 - backdrop_alpha) + backdrop_alpha * underlying_gate;
    (this_gate * underlying).clamp(0.0, 1.0)
}

/// One channel's range factor: the black side's inclusive feather, then the
/// white side's. `range` is `[black_low, black_high, white_low, white_high]`;
/// equal low/high values are a hard cutoff.
fn range_factor(value: f32, range: [u8; 4]) -> f32 {
    let byte = (value.clamp(0.0, 1.0) * 255.0).round() as i32;
    let black_lo = i32::from(range[0]);
    let black_hi = i32::from(range[1]);
    let white_lo = i32::from(range[2]);
    let white_hi = i32::from(range[3]);
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
