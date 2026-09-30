//! Layer effects: overlays, shadows, glows, satin and strokes.
//!
//! The shapes follow Photoshop's own pipeline as recorded by the calibration
//! references: a spread expands the matte by `round(spread% × size)` as a
//! grayscale dilation, the remaining size blurs with a separable **tent**
//! kernel `(N − |k|) / N²` where `N = max(2, round(size)) − spread_radius` (not
//! a triple box), and the interior pipeline mirrors it on the inverse matte
//! with the choke in the spread's role. Range gains multiply *after* the blur
//! (`min(1, field × 100 / range)`), and a descriptor that omits the range
//! renders at 100 for both glows.
//!
//! Interior overlays fold into the layer's straight color inside the base pass,
//! so alpha, masks, fill/layer opacity and the backdrop apply once — which is
//! also what makes a 100% Normal overlay hide the pixels outright. Satin and
//! the interior soft effects paint above that, in Photoshop's stacking order
//! (overlays, satin, inner glow, inner shadow, stroke).
//!
//! Exterior effects (drop shadows, the outer glow) are separate planes, each
//! composited onto the backdrop with its own blend mode before the layer.
//!
//! Every soft computation runs over the matte padded with transparent pixels,
//! so the blur sees the empty world beyond the layer's edge.
//!
//! Documented gaps: bevel/emboss, pattern overlay, noise and jitter, contour
//! shaping beyond Linear, the "Precise" glow techniques and stroke overprint
//! knockout.

use psd_core::effect_enums::{GlowSource, StrokeFill, StrokePosition};
use psd_core::{BlendMode, Gradient, LayerEffects, Shadow};

use super::blend;
use super::ramp::{self, color_rgb, Placement, Ramp, Span, SpanBasis};
use super::tile::TileSampler;
use super::{Content, Rect};
use psd_core::pattern::Pattern;

/// A resolved effect paint: a flat colour, or a gradient sampled per pixel.
struct EffectPaint {
    color: [f32; 3],
    /// Master strength, `0..=1`.
    alpha: f32,
    blend_mode: BlendMode,
    ramp: Option<Ramp>,
}

impl EffectPaint {
    fn flat(color: [f32; 3], alpha: f32, blend_mode: BlendMode) -> Self {
        Self {
            color,
            alpha,
            blend_mode,
            ramp: None,
        }
    }

    /// The colour and strength for a soft field value. A gradient glow maps
    /// the field so the gradient's start sits at full intensity.
    fn at(&self, field: f32) -> ([f32; 3], f32) {
        match &self.ramp {
            Some(ramp) => {
                let (color, alpha) = ramp.sample(1.0 - field);
                (color, alpha * if field > 0.0 { 1.0 } else { 0.0 })
            }
            None => (self.color, field),
        }
    }
}

/// Sample a gradient at `t` in `0..=1` as straight colour (no transparency).
pub(crate) fn gradient_sample(gradient: &Gradient, t: f32) -> [f32; 3] {
    Ramp::new(gradient, None, false).map_or([0.0; 3], |ramp| ramp.sample(t).0)
}

/// Whether any effect of the set draws.
pub(crate) fn has_effects(effects: &LayerEffects) -> bool {
    effects.drop_shadows.iter().any(|s| enabled(s.enabled))
        || effects.inner_shadows.iter().any(|s| enabled(s.enabled))
        || effects
            .outer_glow
            .as_ref()
            .is_some_and(|g| enabled(g.enabled))
        || effects
            .inner_glow
            .as_ref()
            .is_some_and(|g| enabled(g.enabled))
        || effects.satin.as_ref().is_some_and(|s| enabled(s.enabled))
        || effects.color_overlays.iter().any(|o| enabled(o.enabled))
        || effects.gradient_overlays.iter().any(|o| enabled(o.enabled))
        || effects.strokes.iter().any(|s| enabled(s.enabled))
}

/// Whether any interior effect (overlay, satin, inner glow or shadow) draws.
pub(crate) fn has_interior_effects(effects: &LayerEffects) -> bool {
    effects.inner_shadows.iter().any(|s| enabled(s.enabled))
        || effects
            .inner_glow
            .as_ref()
            .is_some_and(|g| enabled(g.enabled))
        || effects.satin.as_ref().is_some_and(|s| enabled(s.enabled))
        || effects.color_overlays.iter().any(|o| enabled(o.enabled))
        || effects.gradient_overlays.iter().any(|o| enabled(o.enabled))
}

/// The rect a layer's exterior effects can reach beyond the layer's bounds.
pub fn padded_rect(rect: &Rect, effects: &LayerEffects) -> Rect {
    let mut out = *rect;
    let mut grow = |amount: f64| {
        let amount = amount.ceil().max(0.0) as i32;
        out.left -= amount;
        out.top -= amount;
        out.right += amount;
        out.bottom += amount;
    };
    for shadow in effects.drop_shadows.iter().filter(|s| enabled(s.enabled)) {
        let distance = shadow.distance.unwrap_or(0.0).abs();
        grow(distance + shadow.size.unwrap_or(0.0) + shadow.spread.unwrap_or(0.0));
    }
    if let Some(glow) = effects.outer_glow.as_ref().filter(|g| enabled(g.enabled)) {
        grow(glow.size.unwrap_or(0.0) + glow.spread.unwrap_or(0.0));
    }
    for stroke in effects.strokes.iter().filter(|s| enabled(s.enabled)) {
        grow(stroke.size.unwrap_or(0.0));
    }
    if let Some(bevel) = effects.bevel.as_ref().filter(|b| enabled(b.enabled)) {
        grow(super::bevel::padding(bevel));
    }
    out
}

fn enabled(flag: Option<bool>) -> bool {
    flag.unwrap_or(true)
}

fn opacity_scale(value: Option<f64>) -> f32 {
    (value.unwrap_or(100.0) / 100.0).clamp(0.0, 1.0) as f32
}

// ---------------------------------------------------------------------------
// Soft-effect pipeline
// ---------------------------------------------------------------------------

/// A separable tent blur with `(N − |k|) / N²` weights, applied as two
/// sliding box passes of width `N` (a triangle is a box convolved with
/// itself), which is the same kernel in O(pixels) instead of O(pixels × N).
/// Edges truncate rather than renormalize, like Photoshop's own window.
pub(crate) fn blur_tent(plane: &mut [f32], width: usize, height: usize, radius: usize) {
    if radius == 0 || width == 0 || height == 0 {
        return;
    }
    let n = radius.min(width.max(height)).max(1);
    // Two boxes of width `n` convolve to a triangle centred `n - 1` samples
    // late; splitting that lead between the passes keeps the blur centred.
    let first = (n - 1) / 2;
    let second = n - 1 - first;
    let mut scratch = vec![0.0f32; plane.len()];
    box_pass(plane, &mut scratch, width, height, n, first, true);
    box_pass(&scratch, plane, width, height, n, second, true);
    box_pass(plane, &mut scratch, width, height, n, first, false);
    box_pass(&scratch, plane, width, height, n, second, false);
}

/// One normalized box pass of width `n`, as a sliding window covering
/// `position - lead .. position - lead + n`: each step adds the entering tap
/// and drops the leaving one, so the cost is O(pixels).
fn box_pass(
    source: &[f32],
    target: &mut [f32],
    width: usize,
    height: usize,
    n: usize,
    lead: usize,
    horizontal: bool,
) {
    let scale = 1.0 / n as f32;
    let (outer, inner) = if horizontal {
        (height, width)
    } else {
        (width, height)
    };
    let index_of = |line: usize, position: usize| -> usize {
        if horizontal {
            line * width + position
        } else {
            position * width + line
        }
    };
    for line in 0..outer {
        // The window for position 0 is `-lead .. n - lead`, clipped to the line.
        let mut sum = 0.0f32;
        for position in 0..(n - lead).min(inner) {
            sum += source[index_of(line, position)];
        }
        for position in 0..inner {
            target[index_of(line, position)] = sum * scale;
            if position >= lead {
                sum -= source[index_of(line, position - lead)];
            }
            let entering = position + n - lead;
            if entering < inner {
                sum += source[index_of(line, entering)];
            }
        }
    }
}

/// Exact Euclidean distance transform (Felzenszwalb–Huttenlocher): the
/// distance from every pixel to the nearest set pixel, in O(pixels).
///
/// The envelope runs in f64: the squared terms reach `(w + h)²`, which f32
/// cannot hold alongside the "no pixel" sentinel without losing the low bits.
pub(crate) fn distance_transform(matte: &[f32], width: usize, height: usize) -> Vec<f32> {
    if width == 0 || height == 0 || matte.len() < width * height {
        return vec![0.0; matte.len()];
    }
    let sentinel = ((width + height) * (width + height)) as f64 + 1.0;
    let mut grid: Vec<f64> = matte
        .iter()
        .take(width * height)
        .map(|value| if *value > 0.0 { 0.0 } else { sentinel })
        .collect();
    let mut line = vec![0.0f64; height.max(width)];
    for x in 0..width {
        for y in 0..height {
            line[y] = grid[y * width + x];
        }
        distance_1d(&mut line[..height]);
        for y in 0..height {
            grid[y * width + x] = line[y];
        }
    }
    for y in 0..height {
        for x in 0..width {
            line[x] = grid[y * width + x];
        }
        distance_1d(&mut line[..width]);
        for x in 0..width {
            grid[y * width + x] = line[x];
        }
    }
    grid.iter().map(|value| value.sqrt() as f32).collect()
}

/// The 1-D lower envelope of parabolas, in place.
fn distance_1d(f: &mut [f64]) {
    let n = f.len();
    if n == 0 {
        return;
    }
    let mut d = vec![0.0f64; n];
    let mut v = vec![0usize; n];
    let mut z = vec![0.0f64; n + 1];
    let mut k = 0usize;
    v[0] = 0;
    z[0] = f64::NEG_INFINITY;
    z[1] = f64::INFINITY;
    for q in 1..n {
        #[allow(unused_assignments)]
        let mut s = 0.0f64;
        loop {
            s = ((f[q] + (q * q) as f64) - (f[v[k]] + (v[k] * v[k]) as f64))
                / (2.0 * q as f64 - 2.0 * v[k] as f64);
            if s <= z[k] && k > 0 {
                k -= 1;
            } else {
                break;
            }
        }
        k += 1;
        v[k] = q;
        z[k] = s;
        z[k + 1] = f64::INFINITY;
    }
    k = 0;
    for (q, value) in d.iter_mut().enumerate() {
        while k + 1 < n && z[k + 1] < q as f64 {
            k += 1;
        }
        let delta = q as f64 - v[k] as f64;
        *value = delta * delta + f[v[k]];
    }
    f.copy_from_slice(&d);
}

/// Grayscale dilation by a radius, as Photoshop's spread: every pixel takes the
/// largest value within the disc.
pub(crate) fn dilate(matte: &[f32], width: usize, height: usize, radius: usize) -> Vec<f32> {
    if radius == 0 {
        return matte.to_vec();
    }
    let distance = distance_transform(matte, width, height);
    matte
        .iter()
        .zip(distance.iter())
        .map(|(value, distance)| {
            if *distance <= radius as f32 {
                1.0f32.max(*value)
            } else {
                *value
            }
        })
        .collect()
}

/// The soft matte of an exterior effect: spread dilation then tent blur.
fn exterior_mask(
    matte: &[f32],
    width: usize,
    height: usize,
    size: f64,
    spread_percent: f64,
) -> Vec<f32> {
    let size = size.max(0.0).round() as usize;
    let spread_radius = ((spread_percent / 100.0) * size as f64).round() as usize;
    let mut field = dilate(matte, width, height, spread_radius);
    let blur = size.saturating_sub(spread_radius);
    let radius = if size == 0 { 0 } else { blur.max(2) };
    blur_tent(&mut field, width, height, radius);
    field
}

/// The interior mirror: the inverse matte dilates by the choke, then blurs.
fn interior_mask(
    matte: &[f32],
    width: usize,
    height: usize,
    size: f64,
    choke_percent: f64,
) -> Vec<f32> {
    let inverse: Vec<f32> = matte.iter().map(|value| 1.0 - value).collect();
    exterior_mask(&inverse, width, height, size, choke_percent)
}

fn range_gain(field: &mut [f32], range: f64) {
    let range = if range <= 0.0 { 100.0 } else { range };
    let gain = (100.0 / range) as f32;
    for value in field {
        *value = (*value * gain).min(1.0);
    }
}

/// Run a soft-effect computation over the matte padded with transparent
/// pixels, so the blur sees the empty world beyond the layer's edge, and crop
/// the resulting field back to the layer.
fn with_padding(
    matte: &[f32],
    width: usize,
    height: usize,
    pad: f64,
    compute: impl FnOnce(&[f32], usize, usize) -> Vec<f32>,
) -> Vec<f32> {
    // Offsets can be enormous; past the layer's extent plus the widest blur
    // (250 px) padding adds nothing.
    let pad = pad.clamp(0.0, width.max(height) as f64 + 256.0).ceil() as usize + 2;
    let (padded_width, padded_height) = (width + 2 * pad, height + 2 * pad);
    let mut padded = vec![0.0f32; padded_width * padded_height];
    for y in 0..height {
        let target = (y + pad) * padded_width + pad;
        padded[target..target + width].copy_from_slice(&matte[y * width..(y + 1) * width]);
    }
    let field = compute(&padded, padded_width, padded_height);
    let mut out = vec![0.0f32; width * height];
    for y in 0..height {
        let source = (y + pad) * padded_width + pad;
        out[y * width..(y + 1) * width].copy_from_slice(&field[source..source + width]);
    }
    out
}

fn sample(plane: &[f32], width: usize, height: usize, x: i64, y: i64) -> f32 {
    if x < 0 || y < 0 || x >= width as i64 || y >= height as i64 {
        return 0.0;
    }
    plane[y as usize * width + x as usize]
}

/// The pixel offset of a shadow: the angle is where the light comes from, so
/// the shadow falls the other way.
fn shadow_offset(angle: f64, distance: f64) -> (i64, i64) {
    let angle = angle.to_radians();
    (
        (-distance * angle.cos()).round() as i64,
        (distance * angle.sin()).round() as i64,
    )
}

// ---------------------------------------------------------------------------
// Interior effects
// ---------------------------------------------------------------------------

/// The bounds of the pixels with any coverage, in document coordinates.
fn visible_span(content: &Content, coverage: &[f32]) -> Option<Span> {
    let (width, height) = (content.width(), content.height());
    let (mut left, mut top, mut right, mut bottom) = (usize::MAX, usize::MAX, 0, 0);
    for y in 0..height {
        for x in 0..width {
            if coverage[y * width + x] > 0.0 {
                left = left.min(x);
                right = right.max(x + 1);
                top = top.min(y);
                bottom = bottom.max(y + 1);
            }
        }
    }
    if left == usize::MAX {
        return None;
    }
    Some(Span {
        left: (content.rect.left + left as i32) as f32,
        top: (content.rect.top + top as i32) as f32,
        width: (right - left) as f32,
        height: (bottom - top) as f32,
    })
}

/// What the effect renderers need beyond the layer: where the canvas is, the
/// document's pattern tiles, and the layer's effects reference point.
pub(crate) struct EffectContext<'a> {
    pub canvas: Rect,
    pub patterns: &'a [Pattern],
    /// The anchor "Link with Layer" patterns hang from (`fxrp`).
    pub reference: (f64, f64),
}

impl EffectContext<'_> {
    /// The tile sampler for a pattern reference, placed.
    pub(super) fn sampler(
        &self,
        pattern: Option<&psd_core::PatternRef>,
        scale: Option<f64>,
        angle: Option<f64>,
        linked: Option<bool>,
        phase: Option<psd_core::Offset>,
    ) -> Option<TileSampler<'_>> {
        let reference = pattern?;
        let tile = self.patterns.iter().find(|p| p.id == reference.id)?;
        let mut anchor = phase.map_or((0.0, 0.0), |o| (o.horizontal, o.vertical));
        if linked.unwrap_or(true) {
            anchor.0 += self.reference.0;
            anchor.1 += self.reference.1;
        }
        TileSampler::new(tile, scale.unwrap_or(100.0), angle.unwrap_or(0.0), anchor)
    }
}

/// Interior overlays fold into the layer's straight color, bottom to top:
/// pattern, gradient, then color overlays.
pub(crate) fn fold_interior_overlays(
    content: &mut Content,
    coverage: &[f32],
    effects: &LayerEffects,
    context: &EffectContext<'_>,
) {
    let width = content.width();
    let height = content.height();
    let canvas = context.canvas;
    if width == 0 || height == 0 {
        return;
    }
    if let Some(overlay) = effects
        .pattern_overlay
        .as_ref()
        .filter(|o| enabled(o.enabled))
    {
        if let Some(sampler) = context.sampler(
            overlay.pattern.as_ref(),
            overlay.scale,
            overlay.angle,
            overlay.linked,
            overlay.phase,
        ) {
            let strength = opacity_scale(overlay.opacity);
            let mode = overlay.blend_mode.unwrap_or(BlendMode::NORMAL);
            for index in 0..width * height {
                let x = content.rect.left + (index % width) as i32;
                let y = content.rect.top + (index / width) as i32;
                let (color, alpha) = sampler.sample(x, y);
                fold_pixel(content, index, color, alpha * strength, mode);
            }
        }
    }
    for overlay in effects
        .gradient_overlays
        .iter()
        .filter(|o| enabled(o.enabled))
    {
        let Some(gradient) = overlay.gradient.as_ref() else {
            continue;
        };
        let Some(ramp) = Ramp::new(gradient, overlay.interpolation, false) else {
            continue;
        };
        let span = if overlay.align.unwrap_or(true) {
            visible_span(content, coverage)
        } else {
            Some(Span {
                left: canvas.left as f32,
                top: canvas.top as f32,
                width: canvas.width() as f32,
                height: canvas.height() as f32,
            })
        };
        let Some(span) = span else {
            continue;
        };
        let offset = overlay
            .offset
            .map_or((0.0, 0.0), |o| (o.horizontal as f32, o.vertical as f32));
        let placement = Placement {
            style: overlay
                .style
                .unwrap_or(psd_core::effect_enums::GradientStyle::Linear),
            angle: overlay.angle.unwrap_or(90.0) as f32,
            scale: overlay.scale.unwrap_or(100.0) as f32,
            reverse: overlay.reverse.unwrap_or(false),
            offset,
        };
        let strength = opacity_scale(overlay.opacity);
        let mode = overlay.blend_mode.unwrap_or(BlendMode::NORMAL);
        for index in 0..width * height {
            let x = content.rect.left + (index % width) as i32;
            let y = content.rect.top + (index / width) as i32;
            let position = ramp::position(&placement, span, SpanBasis::Projection, x, y);
            let (color, alpha) = ramp.sample(position);
            fold_pixel(content, index, color, alpha * strength, mode);
        }
    }

    for overlay in effects.color_overlays.iter().filter(|o| enabled(o.enabled)) {
        let color = overlay.color.as_ref().map(color_rgb).unwrap_or([0.0; 3]);
        let strength = opacity_scale(overlay.opacity);
        let mode = overlay.blend_mode.unwrap_or(BlendMode::NORMAL);
        for index in 0..width * height {
            fold_pixel(content, index, color, strength, mode);
        }
    }
}

fn fold_pixel(
    content: &mut Content,
    index: usize,
    color: [f32; 3],
    strength: f32,
    mode: BlendMode,
) {
    if strength <= 0.0 {
        return;
    }
    let current = [
        content.color[0][index],
        content.color[1][index],
        content.color[2][index],
    ];
    let blended = blend::blend(mode, current, color, false);
    for channel in 0..3 {
        content.color[channel][index] =
            current[channel] + (blended[channel] - current[channel]) * strength;
    }
}

/// Interior soft effects paint above the folded color, inside the silhouette.
pub(crate) fn paint_interior(content: &mut Content, coverage: &[f32], effects: &LayerEffects) {
    let width = content.width();
    let height = content.height();
    if width == 0 || height == 0 || coverage.len() < width * height {
        return;
    }

    // Satin, then inner glow, then inner shadow (Photoshop's stacking order).
    if let Some(satin) = effects.satin.as_ref().filter(|s| enabled(s.enabled)) {
        let distance = satin.distance.unwrap_or(0.0).max(1.0);
        let angle = satin.angle.unwrap_or(0.0).to_radians();
        let dx = (distance * angle.cos()).round() as i64;
        let dy = (-distance * angle.sin()).round() as i64;
        let size = satin.size.unwrap_or(0.0);
        let mut field = with_padding(coverage, width, height, size + distance, |matte, w, h| {
            let mut field = vec![0.0f32; w * h];
            for y in 0..h {
                for x in 0..w {
                    let a = sample(matte, w, h, x as i64 + dx, y as i64 + dy);
                    let b = sample(matte, w, h, x as i64 - dx, y as i64 - dy);
                    field[y * w + x] = (a - b).abs();
                }
            }
            if size > 0.0 {
                blur_tent(&mut field, w, h, size.round().max(2.0) as usize);
            }
            field
        });
        if satin.invert.unwrap_or(false) {
            for value in &mut field {
                *value = 1.0 - *value;
            }
        }
        let paint = EffectPaint::flat(
            satin.color.as_ref().map(color_rgb).unwrap_or([0.0; 3]),
            opacity_scale(satin.opacity),
            satin.blend_mode.unwrap_or(BlendMode::NORMAL),
        );
        apply_interior(content, coverage, &field, &paint);
    }

    if let Some(glow) = effects.inner_glow.as_ref().filter(|g| enabled(g.enabled)) {
        let size = glow.size.unwrap_or(0.0);
        let choke = glow.spread.unwrap_or(0.0);
        let mut field = with_padding(coverage, width, height, size, |matte, w, h| {
            interior_mask(matte, w, h, size, choke)
        });
        range_gain(&mut field, glow.range.unwrap_or(100.0));
        if glow.source == Some(GlowSource::Center) {
            // Center source is the exact complement of the gained edge field.
            for value in &mut field {
                *value = 1.0 - *value;
            }
        }
        let paint = glow_paint(glow, BlendMode::SCREEN);
        apply_interior(content, coverage, &field, &paint);
    }

    for shadow in effects.inner_shadows.iter().filter(|s| enabled(s.enabled)) {
        let (dx, dy) = shadow_offset(shadow.angle.unwrap_or(0.0), shadow.distance.unwrap_or(0.0));
        // The shadow's offset shifts the matte; the field is the soft inverse
        // of the shifted silhouette. An offset past the layer's extent (plus
        // the blur) already shows no silhouette, so clamp it to keep planes
        // small.
        let size = shadow.size.unwrap_or(0.0);
        let limit = (width.max(height) as f64 + size + 2.0) as i64;
        let (dx, dy) = (dx.clamp(-limit, limit), dy.clamp(-limit, limit));
        let field = with_padding(coverage, width, height, size, |matte, w, h| {
            let mut shifted = vec![0.0f32; w * h];
            for y in 0..h {
                for x in 0..w {
                    shifted[y * w + x] = sample(matte, w, h, x as i64 - dx, y as i64 - dy);
                }
            }
            interior_mask(&shifted, w, h, size, shadow.spread.unwrap_or(0.0))
        });
        let paint = EffectPaint::flat(
            shadow.color.as_ref().map(color_rgb).unwrap_or([0.0; 3]),
            opacity_scale(shadow.opacity),
            shadow.blend_mode.unwrap_or(BlendMode::MULTIPLY),
        );
        apply_interior(content, coverage, &field, &paint);
    }
}

fn glow_paint(glow: &psd_core::Glow, default_mode: BlendMode) -> EffectPaint {
    let ramp = glow
        .gradient
        .as_ref()
        .and_then(|gradient| Ramp::new(gradient, None, false));
    EffectPaint {
        color: glow.color.as_ref().map(color_rgb).unwrap_or([1.0; 3]),
        alpha: opacity_scale(glow.opacity),
        blend_mode: glow.blend_mode.unwrap_or(default_mode),
        // A glow filled with a gradient ignores the flat colour.
        ramp: if glow.color.is_some() && glow.gradient.is_none() {
            None
        } else {
            ramp
        },
    }
}

fn apply_interior(content: &mut Content, coverage: &[f32], field: &[f32], paint: &EffectPaint) {
    for index in 0..content.alpha.len() {
        let (color, field_alpha) = paint.at(field[index]);
        let strength = field_alpha * paint.alpha * coverage[index];
        if strength <= 0.0 {
            continue;
        }
        fold_pixel(content, index, color, strength, paint.blend_mode);
    }
}

// ---------------------------------------------------------------------------
// Exterior effects
// ---------------------------------------------------------------------------

/// One exterior effect plane: straight colour and alpha over a rect, drawn
/// onto the backdrop with its own blend mode.
pub(crate) struct OuterPlane {
    pub content: Content,
    pub blend_mode: BlendMode,
}

/// Exterior effects: drop shadows and the outer glow, drawn below the layer.
pub(crate) fn build_outer(
    content: &Content,
    coverage: &[f32],
    effects: &LayerEffects,
    canvas: Rect,
) -> Vec<OuterPlane> {
    let width = content.width();
    let height = content.height();
    let mut planes = Vec::new();
    if width == 0 || height == 0 || coverage.len() < width * height {
        return planes;
    }
    // Padding never draws outside the canvas, and Photoshop's distances reach
    // 30,000 px: clamp before allocating.
    let padded = padded_rect(&content.rect, effects);
    let rect = Rect::new(
        padded.top.max(canvas.top),
        padded.left.max(canvas.left),
        padded.bottom.min(canvas.bottom),
        padded.right.min(canvas.right),
    );
    if rect.width() <= 0 || rect.height() <= 0 {
        return planes;
    }
    let (out_width, out_height) = (rect.width().max(0) as usize, rect.height().max(0) as usize);
    let offset_x = (content.rect.left - rect.left) as i64;
    let offset_y = (content.rect.top - rect.top) as i64;

    // The matte placed on the padded plane, shifted by `(dx, dy)`.
    let place = |dx: i64, dy: i64| -> Vec<f32> {
        let mut matte = vec![0.0f32; out_width * out_height];
        for y in 0..height {
            for x in 0..width {
                let target_x = x as i64 + offset_x + dx;
                let target_y = y as i64 + offset_y + dy;
                if target_x < 0
                    || target_y < 0
                    || target_x >= out_width as i64
                    || target_y >= out_height as i64
                {
                    continue;
                }
                matte[target_y as usize * out_width + target_x as usize] = coverage[y * width + x];
            }
        }
        matte
    };
    // The layer's own silhouette on the padded plane, for knockouts.
    let own = place(0, 0);

    for shadow in effects.drop_shadows.iter().filter(|s| enabled(s.enabled)) {
        let (dx, dy) = shadow_offset(shadow.angle.unwrap_or(0.0), shadow.distance.unwrap_or(0.0));
        let matte = place(dx, dy);
        let mut field = exterior_mask(
            &matte,
            out_width,
            out_height,
            shadow.size.unwrap_or(0.0),
            shadow.spread.unwrap_or(0.0),
        );
        // "Layer Knocks Out Drop Shadow" (on by default): the layer's own
        // transparency shape removes its shadow.
        if shadow.layer_knocks_out.unwrap_or(true) {
            for (value, matte) in field.iter_mut().zip(&own) {
                *value *= 1.0 - *matte;
            }
        }
        planes.push(plane_from_field(rect, &field, &shadow_paint(shadow)));
    }

    if let Some(glow) = effects.outer_glow.as_ref().filter(|g| enabled(g.enabled)) {
        let mut field = exterior_mask(
            &own,
            out_width,
            out_height,
            glow.size.unwrap_or(0.0),
            glow.spread.unwrap_or(0.0),
        );
        range_gain(&mut field, glow.range.unwrap_or(100.0));
        planes.push(plane_from_field(
            rect,
            &field,
            &glow_paint(glow, BlendMode::SCREEN),
        ));
    }
    planes
}

fn shadow_paint(shadow: &Shadow) -> EffectPaint {
    EffectPaint::flat(
        shadow.color.as_ref().map(color_rgb).unwrap_or([0.0; 3]),
        opacity_scale(shadow.opacity),
        shadow.blend_mode.unwrap_or(BlendMode::MULTIPLY),
    )
}

/// Turn a soft field into a colour/alpha plane. Burn and dodge modes fold
/// their alpha into the colour instead of scaling the draw, which is how
/// Photoshop keeps a 50% black linear burn an absolute subtraction.
fn plane_from_field(rect: Rect, field: &[f32], paint: &EffectPaint) -> OuterPlane {
    let mut content = Content::new(rect);
    let folds = matches!(
        paint.blend_mode,
        BlendMode::LINEAR_BURN | BlendMode::COLOR_BURN | BlendMode::COLOR_DODGE
    );
    for (index, value) in field.iter().enumerate() {
        let (mut color, field_alpha) = paint.at(*value);
        let strength = field_alpha * paint.alpha;
        if strength <= 0.0 {
            continue;
        }
        let mut alpha = strength;
        if folds {
            match paint.blend_mode {
                BlendMode::COLOR_DODGE => {
                    for channel in &mut color {
                        *channel *= strength;
                    }
                }
                _ => {
                    for channel in &mut color {
                        *channel += (1.0 - *channel) * (1.0 - strength);
                    }
                }
            }
            alpha = 1.0;
        }
        content.color[0][index] = color[0];
        content.color[1][index] = color[1];
        content.color[2][index] = color[2];
        content.alpha[index] = alpha;
    }
    OuterPlane {
        content,
        blend_mode: paint.blend_mode,
    }
}

/// The band of a stroke effect over a matte: its coverage, and the Shape Burst
/// position (0 at the band's outer limit, 1 at its inner limit).
///
/// The band is an exact Euclidean distance field from the binary contour (a
/// pixel is painted when its coverage reaches one half, or any coverage when
/// nothing does). Outside reaches `size` px out, Inside `size` px in, Center
/// `size / 2` each way. A band limit's coverage ramps over one more pixel,
/// `clamp(band + 1 − distance)`, and the sum `alpha × inside + (1 − alpha) ×
/// outside` keeps the band seamless across anti-aliased contour pixels.
fn stroke_band(
    matte: &[f32],
    width: usize,
    height: usize,
    size: f64,
    position: StrokePosition,
) -> (Vec<f32>, Vec<f32>) {
    let size = size.max(0.0) as f32;
    let (band_out, band_in) = match position {
        StrokePosition::Outside => (size, 0.0),
        StrokePosition::Inside => (0.0, size),
        StrokePosition::Center => (size / 2.0, size / 2.0),
    };
    let pad = size + 2.0;
    // Computed over the matte padded with empty pixels, so a layer touching
    // its plane's edge still has a world outside it.
    let both = with_padding_pair(matte, width, height, f64::from(pad), |matte, w, h| {
        let any_solid = matte.iter().any(|value| *value >= 0.5);
        let mut contour: Vec<f32> = matte
            .iter()
            .map(|value| {
                let painted = if any_solid {
                    *value >= 0.5
                } else {
                    *value > 0.0
                };
                if painted {
                    1.0
                } else {
                    0.0
                }
            })
            .collect();
        if any_solid {
            // An anti-aliased fringe hugs solid pixels and stays outside the
            // contour, but a faint flat region (a wash, a faded gradient) is
            // stroked whole: a painted pixel farther than two pixels from
            // every solid one joins the shape.
            let to_solid = distance_transform(&contour, w, h);
            for (index, value) in contour.iter_mut().enumerate() {
                if *value == 0.0 && matte[index] > 0.0 && to_solid[index] > 2.0 {
                    *value = 1.0;
                }
            }
        }
        let outside = if band_out > 0.0 {
            distance_transform(&contour, w, h)
        } else {
            Vec::new()
        };
        let inverse: Vec<f32> = contour.iter().map(|value| 1.0 - value).collect();
        let inside = if band_in > 0.0 {
            distance_transform(&inverse, w, h)
        } else {
            Vec::new()
        };
        let mut coverage = vec![0.0f32; w * h];
        let mut burst = vec![0.0f32; w * h];
        let outer_reach = if band_out > 0.0 { band_out + 1.0 } else { 0.0 };
        let inner_reach = if band_in > 0.0 { band_in + 1.0 } else { 0.0 };
        let span = (outer_reach + inner_reach).max(1.0);
        for index in 0..w * h {
            let alpha = matte[index];
            let outside_coverage = if outside.is_empty() {
                0.0
            } else {
                (band_out + 1.0 - outside[index]).clamp(0.0, 1.0)
            };
            let inside_coverage = if inside.is_empty() {
                0.0
            } else {
                (band_in + 1.0 - inside[index]).clamp(0.0, 1.0)
            };
            coverage[index] =
                (alpha * inside_coverage + (1.0 - alpha) * outside_coverage).clamp(0.0, 1.0);
            let along = if contour[index] > 0.0 {
                if inside.is_empty() {
                    span
                } else {
                    band_out + inside[index]
                }
            } else if outside.is_empty() {
                0.0
            } else {
                outer_reach - outside[index]
            };
            burst[index] = (along / span).clamp(0.0, 1.0);
        }
        (coverage, burst)
    });
    both
}

/// [`with_padding`] for a computation producing two fields.
fn with_padding_pair(
    matte: &[f32],
    width: usize,
    height: usize,
    pad: f64,
    compute: impl FnOnce(&[f32], usize, usize) -> (Vec<f32>, Vec<f32>),
) -> (Vec<f32>, Vec<f32>) {
    let pad = pad.clamp(0.0, width.max(height) as f64 + 256.0).ceil() as usize + 2;
    let (padded_width, padded_height) = (width + 2 * pad, height + 2 * pad);
    let mut padded = vec![0.0f32; padded_width * padded_height];
    for y in 0..height {
        let target = (y + pad) * padded_width + pad;
        padded[target..target + width].copy_from_slice(&matte[y * width..(y + 1) * width]);
    }
    let (first, second) = compute(&padded, padded_width, padded_height);
    let crop = |field: &[f32]| {
        let mut out = vec![0.0f32; width * height];
        for y in 0..height {
            let source = (y + pad) * padded_width + pad;
            out[y * width..(y + 1) * width].copy_from_slice(&field[source..source + width]);
        }
        out
    };
    (crop(&first), crop(&second))
}

/// One stroke effect resolved to planes.
pub(crate) struct StrokePlane {
    /// Band coverage before any opacity, over the content's rect.
    pub band: Vec<f32>,
    /// Straight colour, with the band (times the gradient's transparency) as
    /// alpha, over the same rect.
    pub content: Content,
    /// The stroke's own opacity, `0..=1`.
    pub opacity: f32,
    pub blend_mode: BlendMode,
    /// `overprint`: blend over the layer's own content instead of knocking it
    /// out.
    pub overprint: bool,
}

impl StrokePlane {
    /// How much of the layer's own content survives under this stroke at a
    /// pixel. Without overprint the band knocks content out at full band
    /// coverage whatever the stroke opacity; a Normal stroke compensates by
    /// `(1 − band) / (1 − strength)` so that composited above the knocked-out
    /// content it equals a plain over-composite (a no-op for an opaque solid
    /// stroke).
    pub fn content_factor(&self, index: usize) -> f32 {
        if self.overprint {
            return 1.0;
        }
        let band = self.band[index];
        if band <= 0.0 {
            return 1.0;
        }
        if self.blend_mode == BlendMode::NORMAL {
            let strength = (self.content.alpha[index] * self.opacity).clamp(0.0, 1.0);
            if strength >= 1.0 {
                return 1.0;
            }
            ((1.0 - band) / (1.0 - strength)).clamp(0.0, 1.0)
        } else {
            1.0 - band
        }
    }

    /// Whether the stroke draws above the layer's content (Normal, or
    /// overprint); other modes blend against the backdrop below it.
    pub fn above_content(&self) -> bool {
        self.blend_mode == BlendMode::NORMAL || self.overprint
    }
}

/// Resolve the layer's stroke effects to planes over the content's rect.
pub(crate) fn build_strokes(
    content: &Content,
    coverage: &[f32],
    effects: &LayerEffects,
    context: &EffectContext<'_>,
) -> Vec<StrokePlane> {
    let canvas = context.canvas;
    let (width, height) = (content.width(), content.height());
    let mut planes = Vec::new();
    if width == 0 || height == 0 || coverage.len() < width * height {
        return planes;
    }
    for stroke in effects.strokes.iter().filter(|s| enabled(s.enabled)) {
        let size = stroke.size.unwrap_or(0.0);
        if size <= 0.0 {
            continue;
        }
        let position = stroke.position.unwrap_or(StrokePosition::Outside);
        let (band, burst) = stroke_band(coverage, width, height, size, position);
        let mut plane = Content::new(content.rect);
        match stroke.fill.unwrap_or(StrokeFill::Color) {
            StrokeFill::Gradient => {
                let ramp = stroke
                    .gradient
                    .as_ref()
                    .and_then(|gradient| Ramp::new(gradient, stroke.interpolation, false));
                let Some(ramp) = ramp else {
                    continue;
                };
                let style = stroke
                    .style
                    .unwrap_or(psd_core::effect_enums::GradientStyle::Linear);
                let span = if stroke.align.unwrap_or(true) {
                    visible_span(content, coverage)
                } else {
                    Some(Span {
                        left: canvas.left as f32,
                        top: canvas.top as f32,
                        width: canvas.width() as f32,
                        height: canvas.height() as f32,
                    })
                };
                let placement = Placement {
                    style,
                    angle: stroke.angle.unwrap_or(90.0) as f32,
                    scale: stroke.scale.unwrap_or(100.0) as f32,
                    reverse: stroke.reverse.unwrap_or(false),
                    offset: stroke
                        .offset
                        .map_or((0.0, 0.0), |o| (o.horizontal as f32, o.vertical as f32)),
                };
                for index in 0..width * height {
                    if band[index] <= 0.0 {
                        continue;
                    }
                    let position = if style == psd_core::effect_enums::GradientStyle::ShapeBurst {
                        let burst = burst[index];
                        if placement.reverse {
                            1.0 - burst
                        } else {
                            burst
                        }
                    } else if let Some(span) = span {
                        let x = content.rect.left + (index % width) as i32;
                        let y = content.rect.top + (index / width) as i32;
                        ramp::position(&placement, span, SpanBasis::Projection, x, y)
                    } else {
                        0.0
                    };
                    let (color, alpha) = ramp.sample(position);
                    for (channel, value) in color.iter().enumerate() {
                        plane.color[channel][index] = *value;
                    }
                    plane.alpha[index] = band[index] * alpha;
                }
            }
            StrokeFill::Pattern => {
                let Some(sampler) = context.sampler(
                    stroke.pattern.as_ref(),
                    stroke.scale,
                    stroke.angle,
                    stroke.linked,
                    stroke.phase,
                ) else {
                    continue;
                };
                for (index, coverage) in band.iter().enumerate() {
                    if *coverage <= 0.0 {
                        continue;
                    }
                    let x = content.rect.left + (index % width) as i32;
                    let y = content.rect.top + (index / width) as i32;
                    let (color, alpha) = sampler.sample(x, y);
                    for (channel, value) in color.iter().enumerate() {
                        plane.color[channel][index] = *value;
                    }
                    plane.alpha[index] = *coverage * alpha;
                }
            }
            StrokeFill::Color => {
                let color = stroke.color.as_ref().map(color_rgb).unwrap_or([0.0; 3]);
                for (index, coverage) in band.iter().enumerate() {
                    if *coverage <= 0.0 {
                        continue;
                    }
                    for (channel, value) in color.iter().enumerate() {
                        plane.color[channel][index] = *value;
                    }
                    plane.alpha[index] = *coverage;
                }
            }
        }
        planes.push(StrokePlane {
            band,
            content: plane,
            opacity: opacity_scale(stroke.opacity),
            blend_mode: stroke.blend_mode.unwrap_or(BlendMode::NORMAL),
            overprint: stroke.overprint.unwrap_or(false),
        });
    }
    planes
}
