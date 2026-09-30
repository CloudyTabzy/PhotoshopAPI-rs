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
//! (overlays, satin, inner glow, inner shadow).
//!
//! Documented gaps: bevel/emboss, pattern overlay, noise and jitter, contour
//! shaping beyond Linear, the "Precise" glow techniques, stroke overprint
//! knockout, and `layerConceals` (the shadow is drawn under the layer's own
//! transparency either way, which is its default shape).

use psd_core::effect_enums::{StrokeFill, StrokePosition};
use psd_core::{BlendMode, Color, Gradient, GradientKind, LayerEffects, Shadow, StopSource};

use super::blend;
use super::{Content, Rect};

/// A resolved effect color and how strongly it draws.
struct EffectPaint {
    color: [f32; 3],
    alpha: f32,
    blend_mode: BlendMode,
}

fn color_rgb(color: &Color) -> [f32; 3] {
    match color {
        Color::Rgb { red, green, blue } => [
            (*red as f32 / 255.0).clamp(0.0, 1.0),
            (*green as f32 / 255.0).clamp(0.0, 1.0),
            (*blue as f32 / 255.0).clamp(0.0, 1.0),
        ],
        Color::Gray { gray } => {
            let value = (*gray as f32 / 255.0).clamp(0.0, 1.0);
            [value, value, value]
        }
        Color::Cmyk {
            cyan,
            magenta,
            yellow,
            black,
        } => {
            let c = *cyan as f32 / 255.0;
            let m = *magenta as f32 / 255.0;
            let y = *yellow as f32 / 255.0;
            let k = *black as f32 / 255.0;
            [
                (1.0 - c) * (1.0 - k),
                (1.0 - m) * (1.0 - k),
                (1.0 - y) * (1.0 - k),
            ]
        }
        Color::Hsb {
            hue,
            saturation,
            brightness,
        } => {
            let (r, g, b) = hsv_to_rgb(*hue as f32 / 360.0, *saturation as f32, *brightness as f32);
            [r, g, b]
        }
        _ => [0.0, 0.0, 0.0],
    }
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    match (i as i32).rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

/// Sample a gradient at `t` in 0..=1 (linear interpolation between stops).
pub(crate) fn gradient_sample(gradient: &Gradient, t: f32) -> [f32; 3] {
    let GradientKind::Solid(solid) = &gradient.kind else {
        return [0.0, 0.0, 0.0];
    };
    if solid.color_stops.is_empty() {
        return [0.0, 0.0, 0.0];
    }
    let position = (t.clamp(0.0, 1.0) * 4096.0).clamp(0.0, 4096.0);
    let stops = &solid.color_stops;
    let mut previous = &stops[0];
    for stop in stops {
        if (stop.location as f32) >= position {
            let next = stop;
            let span = (next.location - previous.location).max(1) as f32;
            let local = ((position - previous.location as f32) / span).clamp(0.0, 1.0);
            let a = stop_color(&previous.source);
            let b = stop_color(&next.source);
            return [
                a[0] + (b[0] - a[0]) * local,
                a[1] + (b[1] - a[1]) * local,
                a[2] + (b[2] - a[2]) * local,
            ];
        }
        previous = stop;
    }
    stop_color(&stops[stops.len() - 1].source)
}

fn stop_color(source: &StopSource) -> [f32; 3] {
    match source {
        StopSource::User(color) => color_rgb(color),
        _ => [0.0, 0.0, 0.0],
    }
}

/// The rect a layer's effects can reach beyond the layer's own bounds.
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
    let mut scratch = vec![0.0f32; plane.len()];
    // Two horizontal box passes.
    box_pass(plane, &mut scratch, width, height, n, true);
    box_pass(&scratch, plane, width, height, n, true);
    // Two vertical box passes.
    box_pass(plane, &mut scratch, width, height, n, false);
    box_pass(&scratch, plane, width, height, n, false);
}

/// One normalized box pass of width `n`, as a sliding window: each step adds
/// the entering tap and drops the leaving one, so the cost is O(pixels).
fn box_pass(
    source: &[f32],
    target: &mut [f32],
    width: usize,
    height: usize,
    n: usize,
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
        let mut sum = 0.0f32;
        for position in 0..n.min(inner) {
            sum += source[index_of(line, position)];
        }
        for position in 0..inner {
            target[index_of(line, position)] = sum * scale;
            sum -= source[index_of(line, position)];
            let entering = position + n;
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
                // Within the disc: take the neighbourhood maximum. Sampling the
                // disc exactly keeps the grayscale (anti-aliased) edge, which
                // binarizing would lose.
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

/// Interior overlays fold into the layer's straight color.
pub(crate) fn fold_interior_overlays(content: &mut Content, effects: &LayerEffects) {
    let width = content.width();
    let height = content.height();
    if width == 0 || height == 0 {
        return;
    }
    let mut paints: Vec<(EffectPaint, Option<&Gradient>)> = Vec::new();
    for overlay in effects.color_overlays.iter().filter(|o| enabled(o.enabled)) {
        paints.push((
            EffectPaint {
                color: overlay.color.as_ref().map(color_rgb).unwrap_or([0.0; 3]),
                alpha: opacity_scale(overlay.opacity),
                blend_mode: overlay.blend_mode.unwrap_or(BlendMode::NORMAL),
            },
            None,
        ));
    }
    for overlay in effects
        .gradient_overlays
        .iter()
        .filter(|o| enabled(o.enabled))
    {
        paints.push((
            EffectPaint {
                color: [0.0; 3],
                alpha: opacity_scale(overlay.opacity),
                blend_mode: overlay.blend_mode.unwrap_or(BlendMode::NORMAL),
            },
            overlay.gradient.as_ref(),
        ));
    }
    if paints.is_empty() {
        return;
    }
    for index in 0..width * height {
        let current = [
            content.color[0][index],
            content.color[1][index],
            content.color[2][index],
        ];
        let x = (index % width) as f32 / width.max(1) as f32;
        let y = (index / width) as f32 / height.max(1) as f32;
        let mut color = current;
        for (paint, gradient) in &paints {
            let paint_color = match gradient {
                Some(gradient) => gradient_sample(gradient, (x + y) * 0.5),
                None => paint.color,
            };
            let blended = blend::blend(paint.blend_mode, color, paint_color, false);
            for channel in 0..3 {
                color[channel] += (blended[channel] - color[channel]) * paint.alpha;
            }
        }
        content.color[0][index] = color[0];
        content.color[1][index] = color[1];
        content.color[2][index] = color[2];
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
        let mut field = vec![0.0f32; width * height];
        for y in 0..height {
            for x in 0..width {
                let a = sample(coverage, width, height, x as i64 + dx, y as i64 + dy);
                let b = sample(coverage, width, height, x as i64 - dx, y as i64 - dy);
                field[y * width + x] = (a - b).abs();
            }
        }
        let size = satin.size.unwrap_or(0.0);
        if size > 0.0 {
            blur_tent(&mut field, width, height, size.round().max(2.0) as usize);
        }
        if satin.invert.unwrap_or(false) {
            for value in &mut field {
                *value = 1.0 - *value;
            }
        }
        let paint = EffectPaint {
            color: satin.color.as_ref().map(color_rgb).unwrap_or([0.0; 3]),
            alpha: opacity_scale(satin.opacity),
            blend_mode: satin.blend_mode.unwrap_or(BlendMode::NORMAL),
        };
        apply_interior(content, coverage, &field, &paint);
    }

    if let Some(glow) = effects.inner_glow.as_ref().filter(|g| enabled(g.enabled)) {
        let size = glow.size.unwrap_or(0.0);
        let choke = glow.spread.unwrap_or(0.0);
        let mut field = interior_mask(coverage, width, height, size, choke);
        let range = glow.range.unwrap_or(100.0);
        if glow.source == Some(psd_core::effect_enums::GlowSource::Center) {
            // Center source is the exact complement of the gained edge field.
            range_gain(&mut field, range);
            for value in &mut field {
                *value = 1.0 - *value;
            }
        } else {
            range_gain(&mut field, range);
        }
        let paint = EffectPaint {
            color: glow
                .color
                .as_ref()
                .map(color_rgb)
                .or_else(|| glow.gradient.as_ref().map(|g| gradient_sample(g, 0.5)))
                .unwrap_or([0.0; 3]),
            alpha: opacity_scale(glow.opacity),
            blend_mode: glow.blend_mode.unwrap_or(BlendMode::NORMAL),
        };
        apply_interior(content, coverage, &field, &paint);
    }

    for shadow in effects.inner_shadows.iter().filter(|s| enabled(s.enabled)) {
        let distance = shadow.distance.unwrap_or(0.0);
        let angle = shadow.angle.unwrap_or(0.0).to_radians();
        let dx = (distance * angle.cos()).round() as i64;
        let dy = (-distance * angle.sin()).round() as i64;
        // The shadow's offset shifts the inverse matte.
        let mut shifted = vec![0.0f32; width * height];
        for y in 0..height {
            for x in 0..width {
                shifted[y * width + x] =
                    sample(coverage, width, height, x as i64 - dx, y as i64 - dy);
            }
        }
        let mut field = interior_mask(
            &shifted,
            width,
            height,
            shadow.size.unwrap_or(0.0),
            shadow.spread.unwrap_or(0.0),
        );
        for value in &mut field {
            *value *= 1.0 - sample(coverage, width, height, 0, 0).min(1.0);
        }
        let paint = EffectPaint {
            color: shadow.color.as_ref().map(color_rgb).unwrap_or([0.0; 3]),
            alpha: opacity_scale(shadow.opacity),
            blend_mode: shadow.blend_mode.unwrap_or(BlendMode::MULTIPLY),
        };
        apply_interior(content, coverage, &field, &paint);
    }
    paint_strokes(content, coverage, effects);
}

/// The stroke effect: an exact Euclidean distance band around the contour,
/// painted above the interior effects (Photoshop keeps the stroke effect above
/// a shape's vector stroke too). The overprint knockout is not modeled, so the
/// band draws over the content instead of knocking it out.
fn paint_strokes(content: &mut Content, coverage: &[f32], effects: &LayerEffects) {
    let width = content.width();
    let height = content.height();
    for stroke in effects.strokes.iter().filter(|s| enabled(s.enabled)) {
        let size = stroke.size.unwrap_or(0.0);
        if size <= 0.0 {
            continue;
        }
        let position = stroke.position.unwrap_or(StrokePosition::Outside);
        let field = stroke_field(coverage, width, height, size, position);
        let color = match stroke.fill.unwrap_or(StrokeFill::Color) {
            StrokeFill::Gradient => stroke
                .gradient
                .as_ref()
                .map(|gradient| gradient_sample(gradient, 0.5))
                .or_else(|| stroke.color.as_ref().map(color_rgb))
                .unwrap_or([0.0; 3]),
            _ => stroke.color.as_ref().map(color_rgb).unwrap_or([0.0; 3]),
        };
        let paint = EffectPaint {
            color,
            alpha: opacity_scale(stroke.opacity),
            blend_mode: stroke.blend_mode.unwrap_or(BlendMode::NORMAL),
        };
        apply_interior(content, coverage, &field, &paint);
    }
}

fn apply_interior(content: &mut Content, coverage: &[f32], field: &[f32], paint: &EffectPaint) {
    for index in 0..content.alpha.len() {
        let strength = field[index] * paint.alpha * coverage[index];
        if strength <= 0.0 {
            continue;
        }
        let current = [
            content.color[0][index],
            content.color[1][index],
            content.color[2][index],
        ];
        let blended = blend::blend(paint.blend_mode, current, paint.color, false);
        for channel in 0..3 {
            content.color[channel][index] =
                current[channel] + (blended[channel] - current[channel]) * strength;
        }
    }
}

fn sample(plane: &[f32], width: usize, height: usize, x: i64, y: i64) -> f32 {
    if x < 0 || y < 0 || x >= width as i64 || y >= height as i64 {
        return 0.0;
    }
    plane[y as usize * width + x as usize]
}

/// Exterior effects: drop shadows and outer glows, drawn below the layer.
pub(crate) fn build_outer(
    content: &Content,
    coverage: &[f32],
    effects: &LayerEffects,
    canvas: Rect,
) -> Option<Content> {
    let width = content.width();
    let height = content.height();
    if width == 0 || height == 0 || coverage.len() < width * height {
        return None;
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
        return None;
    }
    let (out_width, out_height) = (rect.width().max(0) as usize, rect.height().max(0) as usize);
    let mut outer = Content::new(rect);
    let offset_x = (content.rect.left - rect.left) as usize;
    let offset_y = (content.rect.top - rect.top) as usize;

    for shadow in effects.drop_shadows.iter().filter(|s| enabled(s.enabled)) {
        let distance = shadow.distance.unwrap_or(0.0);
        let angle = shadow.angle.unwrap_or(0.0).to_radians();
        let dx = (distance * angle.cos()).round() as i64;
        let dy = (-distance * angle.sin()).round() as i64;
        let mut matte = vec![0.0f32; out_width * out_height];
        for y in 0..height {
            for x in 0..width {
                let target_x = x as i64 + offset_x as i64 + dx;
                let target_y = y as i64 + offset_y as i64 + dy;
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
        let mut field = exterior_mask(
            &matte,
            out_width,
            out_height,
            shadow.size.unwrap_or(0.0),
            shadow.spread.unwrap_or(0.0),
        );
        // The layer's own transparency hides its shadow where it is opaque,
        // which is Photoshop's "Layer Knocks Out Drop Shadow" default.
        for y in 0..height {
            for x in 0..width {
                let target_x = x as i64 + offset_x as i64;
                let target_y = y as i64 + offset_y as i64;
                if target_x < 0
                    || target_y < 0
                    || target_x >= out_width as i64
                    || target_y >= out_height as i64
                {
                    continue;
                }
                let index = target_y as usize * out_width + target_x as usize;
                field[index] *= 1.0 - coverage[y * width + x];
            }
        }
        composite_outer(&mut outer, &field, &shadow_paint(shadow));
    }

    if let Some(glow) = effects.outer_glow.as_ref().filter(|g| enabled(g.enabled)) {
        let mut matte = vec![0.0f32; out_width * out_height];
        for y in 0..height {
            for x in 0..width {
                matte[(y + offset_y) * out_width + x + offset_x] = coverage[y * width + x];
            }
        }
        let mut field = exterior_mask(
            &matte,
            out_width,
            out_height,
            glow.size.unwrap_or(0.0),
            glow.spread.unwrap_or(0.0),
        );
        range_gain(&mut field, glow.range.unwrap_or(100.0));
        let paint = EffectPaint {
            color: glow
                .color
                .as_ref()
                .map(color_rgb)
                .or_else(|| glow.gradient.as_ref().map(|g| gradient_sample(g, 0.5)))
                .unwrap_or([0.0; 3]),
            alpha: opacity_scale(glow.opacity),
            blend_mode: glow.blend_mode.unwrap_or(BlendMode::NORMAL),
        };
        composite_outer(&mut outer, &field, &paint);
    }

    if outer.alpha.iter().all(|value| *value <= 0.0) {
        return None;
    }
    Some(outer)
}

fn shadow_paint(shadow: &Shadow) -> EffectPaint {
    EffectPaint {
        color: shadow.color.as_ref().map(color_rgb).unwrap_or([0.0; 3]),
        alpha: opacity_scale(shadow.opacity),
        blend_mode: shadow.blend_mode.unwrap_or(BlendMode::MULTIPLY),
    }
}

/// Paint one exterior effect plane into the outer content.
fn composite_outer(outer: &mut Content, field: &[f32], paint: &EffectPaint) {
    // Burn and dodge modes fold their alpha into the color instead of scaling
    // the draw, which is how Photoshop keeps a 50% black linear burn an
    // absolute subtraction.
    let folds = matches!(
        paint.blend_mode,
        BlendMode::LINEAR_BURN | BlendMode::COLOR_BURN | BlendMode::COLOR_DODGE
    );
    for (index, entry) in outer.alpha.iter_mut().enumerate() {
        let strength = field[index] * paint.alpha;
        if strength <= 0.0 {
            continue;
        }
        let mut color = paint.color;
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
        }
        let existing_alpha = *entry;
        let draw_alpha = strength;
        let out_alpha = draw_alpha + existing_alpha * (1.0 - draw_alpha);
        if out_alpha <= 0.0 {
            continue;
        }
        let current = [
            outer.color[0][index],
            outer.color[1][index],
            outer.color[2][index],
        ];
        let blended = blend::blend(paint.blend_mode, current, color, false);
        let mut result = [0.0; 3];
        for (channel, value) in result.iter_mut().enumerate() {
            *value = (current[channel] * existing_alpha * (1.0 - draw_alpha)
                + blended[channel] * draw_alpha)
                / out_alpha;
        }
        for (channel, value) in outer.color.iter_mut().enumerate() {
            value[index] = result[channel];
        }
        *entry = out_alpha;
    }
}

/// The stroke effect: an exact Euclidean distance band around the contour.
pub(crate) fn stroke_field(
    coverage: &[f32],
    width: usize,
    height: usize,
    size: f64,
    position: StrokePosition,
) -> Vec<f32> {
    let binary: Vec<f32> = coverage
        .iter()
        .map(|value| if *value > 0.0 { 1.0 } else { 0.0 })
        .collect();
    let outside = distance_transform(&binary, width, height);
    let inside_binary: Vec<f32> = binary.iter().map(|value| 1.0 - *value).collect();
    let inside = distance_transform(&inside_binary, width, height);
    let size = size.max(0.0) as f32;
    let (outer, inner) = match position {
        StrokePosition::Outside => (size, 0.0),
        StrokePosition::Inside => (0.0, size),
        StrokePosition::Center => (size / 2.0, size / 2.0),
    };
    let mut field = vec![0.0f32; coverage.len()];
    for (index, entry) in field.iter_mut().enumerate() {
        let mut value = 0.0f32;
        if outer > 0.0 {
            // A 1 px anti-aliased ramp at the band's outer edge.
            let d = outside[index];
            if d > 0.0 && d <= outer {
                value = (outer - d + 1.0).min(1.0);
            }
        }
        if inner > 0.0 {
            let d = inside[index];
            if d > 0.0 && d <= inner {
                let ramp = (inner - d + 1.0).min(1.0);
                value = value.max(ramp);
            }
        }
        *entry = value.clamp(0.0, 1.0);
    }
    field
}
