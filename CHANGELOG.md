# Changelog

All notable changes to PhotoshopAPI-rs are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html).
Until 1.0.0, a breaking change to the public Rust or Python API bumps the minor
version and any other change bumps the patch version.

"Upstream" means the C++ [PhotoshopAPI](https://github.com/EmilDohne/PhotoshopAPI)
v0.9.1 that this project ports.

## [Unreleased]

## [0.13.11] - 2026-10-03

### Fixed

- **Interior glows, shadows and satin in Color Dodge, Color Burn and the linear dodge/burn modes
  scale their source by the effect's strength.** An interior effect folded into a layer's colour
  used to ease the *result* of the blend toward the layer by the strength, which is how Normal
  and Screen behave; the dodge and burn modes instead move the source toward their neutral colour
  and apply in full, as a separate effect plane already did. A 77 % Color Dodge inner glow on a
  grey layer now reads 196, 174, 157, 143... from the edge inward where Photoshop's flatten reads
  194, 173, 156, 143..., instead of 210, 197, 184, 172.... `photoshop-inner-glow-range` fell from
  mean 0.76 to **0.04** and `photoshop-gradient-overlay-geometry` from 1.16 to **0.06**; in the
  stored-merge set only `smart_object_file_no_warp` moved (0.86 → 0.54).

## [0.13.10] - 2026-10-03

### Fixed

- **A clipping base's overlays can sit under its members.** With "Blend Clipped Layers as Group"
  off and "Blend Interior Effects as Group" on, a colour, gradient or pattern overlay on the base
  is part of the base the clipped layers sit on, so an opaque member stays its own colour; the
  base's other effects (shadows, glows, strokes, bevel) still paint over the members. Every other
  flag combination keeps the overlay over the finished group.
- **A partly opaque Normal base composites as a unit even with the group option off.** A
  half-opaque base with a Multiply member rendered darker than Photoshop's flatten, which is the
  same as with the option on; a soft-edged base keeps the layer-by-layer path (unverified).
  `photoshop-clip-base-effects` fell from mean 1.59 (max 128) to **0.41**, with no change
  elsewhere in the Photoshop-export or stored-merge sets; what remains there is the inner-shadow
  edge of the `A` cells.

## [0.13.9] - 2026-10-03

### Fixed

- **Two-stop gradient overlays, glows and strokes now ease like fill layers do.** A gradient with
  smoothness 4096 eases every segment, including a lone one, and the transparency ramp eases the
  same way; only fill layers did, so an overlay of two colours came out linear while Photoshop's
  flatten starts slowly and flattens out toward both ends (green channel 0, 5, 11, 18, 25... where
  a linear ramp gave 0, 9, 18, 27...). `Ramp` no longer carries an end-point-smoothing switch; a
  smoothness of zero is still linear. `photoshop-overlay-zorder` fell from mean 0.67 to **0.01**,
  `photoshop-stroke-shapeburst` 0.58 to **0.06** and `photoshop-stroke-aa-matte` 0.95 to 0.84 in
  the Photoshop-export set, and the stored-merge set improved on a dozen documents
  (`clipping-mask2` 1.77 → 1.51, `fill_adjustments` 2.58 → 2.44, `layer_effects` 0.97 → 0.87)
  with no regression.

## [0.13.8] - 2026-10-03

### Fixed

- **Rectangle shapes and vector masks now have crisp edges.** A closed axis-aligned rectangle path
  is rasterised with its edges rounded to whole pixels, the way Photoshop draws it and stores its
  pixels: a path whose right edge is 31.959 px fills its last column whole and a top edge at
  23.471 starts on row 23, where this port used to anti-alias both (coverage 0.96 and 0.53). Curves
  and slanted outlines keep anti-aliased edges. This was the cause of the "mask density" residual:
  `mask-density-layermask` fell from mean 1.94 to **0.00**, and the same rule took
  `rgb-blend-modes` 1.68 → 0.14, `gray-blend-modes` 1.85 → 0.76 and `gradient-sizes` 0.64 → 0.18
  in the stored-merge set, with no regression in it or in the Photoshop-export set. The density
  curve itself was right all along.

## [0.13.7] - 2026-10-03

### Fixed

- **A Linear gradient fill now runs between whole-pixel end points.** Photoshop truncates the two
  ends of the fill's axis (the chord through the bounds' centre, `centre ∓ half the chord`) to
  whole pixels and projects each pixel's *integer* coordinate onto that segment, so a slanted fill
  is a little steeper than its stored angle and sits at a fractional offset from the exact centre
  line; along a pixel axis the rule reduces to sampling at the pixel's corner instead of its
  middle. Measured on Photoshop flattens: `photoshop-shape-gradient` fell from mean 1.22 to
  **0.39** (dither noise sets the floor), and in the stored-merge set the 90/180° fills of
  `masks3` (2.06 → 0.20), `knockout-isolated-groups` (1.18 → 0.09) and `vector-mask2`
  (2.00 → 0.16) and the 150° `rgb-blend-modes` (2.11 → 1.68) improved, with no regression.
  Radial, Angle, Reflected and Diamond fills and layer-effect gradients keep their geometry. The
  rule is fitted to one slanted fill and several axis-aligned ones; how a negative coordinate or
  a scaled axis rounds is not verified.

## [0.13.6] - 2026-10-03

### Fixed

- **Mask, vector-mask and shape feather now use Photoshop's three-box approximation instead of a
  true Gaussian.** The reference editor pins the rule it calibrated against Photoshop
  (`mask_feather_box_radii`): Kutskir's three-box approximation with the narrow/wide pass split —
  ideal box width `sqrt(12 sigma^2 / 3 + 1)` floored to an odd width, the passes that fit the
  narrower width taking it — and a padded plane whose reach is the *sum of the box radii*, not a
  `3 * sigma` guess. Our feather was a separable Gaussian with a `ceil(3 * sigma)` radius, which
  is a different kernel. Measured on the Photoshop-export oracle: `photoshop-vector-mask-feather`
  went from mean 0.60 / 5.76 % of pixels off by more than 2 to **0.52 / 4.82 %**, with no
  regression anywhere (stored-merge median mean error unchanged at 0.120; the mask-density
  residuals are density, not feather, and are untouched). A unit test pins the radii mapping for
  `sigma = 3` (`[2, 2, 3]`, reach 7) and that the kernel conserves a constant plane.


## [0.13.5] - 2026-10-03

### Fixed

- **A freshly regenerated `SoLd` now matches a Photoshop-authored block field for field.**
  Compared against a Photoshop-authored `soLD` v4 block for an unwarped placed layer: twelve keys
  are byte-identical (`PgNm`, `totalPages`, `Crop`, `frameStep`, `duration`, `frameCount`, `Annt`,
  `Type`, `Rslt`, `comp`, `compInfo`, `ClMg`), the warp descriptor matches except its bounds, and
  every other difference is a value that must vary per document (the two uuids, the placement
  quads, the two size fields). Two fixes made that true:
  - the missing trailing **`ClMg`** key — `placedLayerOCIOConversion` →
    `placedLayerOCIOConvertEmbedded`, in a descriptor whose class id is `ClMg` and not `null`;
  - a smart object created **without** a warp now writes `warpStyle = warpNone`; the
    `warpCustom` from `generate_default` was leaking into the descriptor, and `warpCustom`
    belongs to a warp the caller actually set.
- A new test pins the field list, the key order and the `ClMg` encodings; the per-document
  values are the only ones it does not pin.


## [0.13.4] - 2026-10-03

### Fixed

- **Per-layer tagged blocks tolerate the even pad being declared outside the length.**
  Photoshop counts that pad inside an even declared length — every block in the 691-document
  corpus reads that way — but some writers declare the unpadded length and put the pad byte
  after it, which this port rejected with a signature error at the next block. The reader now
  rounds the declared length up to even, accepting both shapes; the writer keeps Photoshop's
  shape, so such a file's bytes are unchanged on a round trip and only the declared length
  reads one larger. A generated fixture from a MoonBit parser's suite exposed it; the
  Photoshop-authored corpora still round-trip byte-exactly.


## [0.13.3] - 2026-10-03

### Fixed

- **The PSB eight-byte-length key list now carries the whole linked-layer family** (`lnkD`,
  `lnk3`, `lnkE`) and `Ink2` from the specification's large-block list. Corpus evidence (687
  documents): a PSB's `lnk2` under an `8BIM` signature reads 0 as a u32 and 17584 as a u64,
  while the PSD of the same document reads 17576 as a u32 — the width really is version-based,
  so a key the list omits is misparsed in a PSB. A PSB with `8BIM lnkE` and a 64-bit length now
  round-trips; the same key in a PSD still reads 32-bit. Found by comparing against a
  BSD-licensed Python writer that shares this project's lineage.


## [0.13.2] - 2026-10-03

### Changed

- **The merged-image placeholder is a white fill, not a black one.** Photoshop stores a
  transparent merge already matted over white (`[255, 255, 255, 0]`) and a document saved
  without a merge carries a solid fill of that shape; a three-channel document has no alpha to
  hide behind, so white is also what a reader that only shows the merged image expects. The
  RLE framing is unchanged (`write_fill_scanline` takes the fill byte), and `MERGED_FILL`
  names it.


## [0.13.1] - 2026-10-03

### Fixed

- **Interior effects are no longer scaled by the pixel's own coverage.** The interior field
  (inner shadow, inner glow, satin) already describes the effect's falloff inside the shape, so
  multiplying it by coverage as well double-attenuated anti-aliased edges — exactly where a
  hard 1 px inner shadow lives. This matches the calibration record's own model: interior
  effects fold into the layer's straight colour inside the base pass, so the layer's alpha,
  masks and opacity apply once at the composite.

  Measured on the stored-merge oracle with a 2012-era corpus's fixtures: `test.psd` and its
  group variants went from mean 0.26 / max 48 / **0.29 % of pixels off by more than 2** to
  mean 0.21 / max 19 / **0.02 %** — a 15x reduction in the worst-pixel share, driven by a white
  75 % normal inner shadow on a text layer. The Photoshop-export oracle is unchanged where the
  registry pins it (Hue/Saturation 0.07-0.12, interior soft effects mean <= 1), and the full
  207-document merge oracle's median mean error is 0.120.


## [0.13.0] - 2026-10-03

### Changed (breaking)

- `Layer::translate` no longer rewrites mask rects, and **refuses** a layer that carries vector
  geometry instead of moving it halfway. A mask's stored rect is relative to the layer when its
  link flag is set and absolute otherwise, so a move must leave it alone: Photoshop's Move tool
  carries a *linked* mask implicitly and leaves an *unlinked* one where it is. Vector geometry
  (a shape's path, or any `vmsk`/`vsms` vector mask) is stored as fractions of the document, so
  a layer on its own cannot move it — moving only the bounds would leave the path behind and
  Photoshop would snap the layer back on its next re-render.
- `Layer::translate_with_path_delta` is the primitive that takes the document-relative delta;
  `Layer::translate` delegates to it with `None`.

### Added

- `LayeredFile::translate_layer`: the document-level move, the way Photoshop's Move tool works.
  It walks the subtree (a group takes its children and grandchildren), converts the pixel delta
  into the 8.24 fixed-point unit paths are stored in, moves each layer's pixel bounds, a text
  layer's transform and its vector geometry, and routes smart objects through
  `move_smart_object`.
- `psd-core`: `VectorPath::translate`, `VectorMask::translate` and `PathPoint::translated` —
  checked geometry moves over the knots and the clipboard record, leaving records that carry no
  coordinates alone.
- The Python bindings' layer `move` uses the document-level move when the layer belongs to a
  document, so groups and vector-bearing layers behave like Photoshop's Move tool; a detached
  layer still uses the primitive (and refuses vector geometry, which needs the document).

### Tests

- New `crates/psd/tests/layer_move.rs`: mask rects never move in either link state, a shape
  path moves by the document-relative delta while the layer-level move refuses it, a group
  carries its children and grandchildren, and a text layer's transform moves.
- Two existing tests asserted the old mask-moving behavior; they now assert the pinned rule.

## [0.12.19] - 2026-10-02

### Added

- PyO3 documents now expose `composite_rgba8()` and `composite_rgba8_with()`, returning a
  NumPy `uint8` RGBA array, and `convert_bit_depth()`, returning the matching typed document
  class. The generated native stubs include these methods.
- The README now marks the compositor as best-effort and identifies the remaining gaps for
  users who need Photoshop-matched flattened output.

## [0.12.18] - 2026-10-02

### Added

- `LayeredFile::convert_bit_depth` converts channel samples between 8-, 16- and 32-bit
  documents while preserving layer structure, masks, profiles, metadata and layerless merged
  pixels. It decodes lazy channels in the output copy and charges the copy to the remaining
  bitmap memory budget. Unsupported mode/depth pairs are rejected. Float-to-integer conversion
  clips HDR and negative samples; widening integer data does not restore lost precision.

## [0.12.17] - 2026-10-02

### Fixed

- Scaled patterns use rounded mip reductions and byte-quantized bilinear interpolation,
  reproducing Photoshop's minified and magnified reference pixels exactly. Unrotated tiles
  with odd dimensions retain footprint averaging to preserve their repeat period; the native mip
  convention and higher-precision source patterns remain unverified.
- Anti-aliased bevel profile contours now use three-tap gradient sums and average the nine
  clipped lighting samples before gloss and highlight/shadow splitting. Disjoint shading
  contributions preserve their covered area instead of attenuating one another through
  source-over compositing. Plain smooth bevels retain their existing calibrated path.
- RGB Channel Mixer adjustments convert slider percentages to truncated Q10 coefficients
  and round integer output in the document's sample domain. The float kernel retains HDR
  values and uses its native constant scale of 1/2048. Coefficients are resolved once per
  layer; wide accumulators prevent overflow on unusual stored percentages.
- Pattern sampling rejects nonfinite placement parameters and overflowing tile dimensions,
  and safely wraps extreme pixel positions.

## [0.12.16] - 2026-10-02

### Fixed

- Black & White adjustments now use fixed-point slider weights and round in the document's
  integer sample domain for 8- and 16-bit images. Eight-bit tinting uses rounded integer
  luminance, including channel clipping, instead of carrying fractional tint offsets.
- Color Lookup adjustments use trilinear interpolation for 32-bit documents, retaining
  tetrahedral interpolation at integer depths. Malformed embedded cubes with nonfinite
  values or excess colour components are left unrendered.
- Hard chisel bevels now build a quantized signed chamfer field directly from fractional
  alpha, with Soften applied before the distance transform. Lighting, soft chisel and
  some contour and texture combinations remain approximations.
- Layerless multichannel documents now render a grayscale preview of their first channel.
  All ink planes remain preserved when saving or materializing the merged image; a single
  ink plane is no longer mistaken for transparency. Spot-colour display and overprint
  simulation remain unsupported.
- The corpus regression suite now actually runs its advertised compositor check and verifies
  the output dimensions and pixel count. Rendering errors can no longer pass unnoticed
  merely because reading and round-tripping succeed.

## [0.12.15] - 2026-10-02

### Fixed

- A shape layer used as a clipping base now blends as a unit with its clipped layers, like a pixel
  layer: the base's own colour no longer bleeds through the soft anti-aliased edge of an opaque
  clipped layer (an orange ellipse with a blue gradient clipped to it used to show an orange
  fringe).
- An adjustment layer that is the base of clipped pixel layers no longer transforms the layers
  below it; it acts on its clip group alone. Clipped adjustment layers do not change this: a
  base with only clipped adjustments still transforms the backdrop. This follows a Photoshop
  render of a masked Hue/Saturation base with a clipped gradient layer and is the least certain
  part of this release.

## [0.12.14] - 2026-10-02

### Fixed

- A stroke effect on a soft layer (a brush dab with no opaque plateau) now recolours the whole soft
  region, keeping the layer's alpha for Inside strokes and painting opaque for Center strokes,
  where the stroke band used to leave the layer's own colour in the middle. Pixels with alpha below
  one and a half 8-bit steps no longer count as part of the shape the stroke follows.
- A centred vector stroke in a document last saved by Photoshop CS6 sits half a pixel right and down
  of its path, as that version draws it: a 3 px stroke on a path at integer coordinates covers whole
  pixel rows, and a dash pattern starts at the first knot. Documents from later versions draw the
  stroke on the path, as before.

## [0.12.13] - 2026-10-02

### Fixed

- A stroked shape layer is no longer clipped to its own path. A stroke that sits on or outside the
  path put half its ring beyond the outline, and the composite cut that half away, leaving a
  ring about half as wide. Stored pixels of stroked shape layers now composite whole, which brings
  the shape documents of one reference set, and a stroke-only shape saved by an older Photoshop
  at 182.88 ppi, to an exact match (merged-reference mean error over the 202-document set
  1.252 → 1.153).
- Shape layers that keep their paint in a `vscg` content block (the form newer Photoshop versions
  write, in place of a `SoCo`/`GdFl`/`PtFl` fill block) are recognised as fill layers. They render
  from their path when the pixels stored beside them are missing or hold clearly less ink than the
  stroke should; otherwise the stored pixels stay in use, which agrees better with every
  reference on hand.
- The live stroke of a shape with several subpaths (combine, subtract, intersect, exclude) now
  follows the outline of the combined shape: arcs of one subpath that end up inside another are no
  longer stroked.


## [0.12.12] - 2026-10-02

### Added

- The Noise slider of shadows and glows now adds grain: each painted pixel's strength moves by its
  signed byte from the fixed noise table times the slider, clamped to the valid range, and pixels
  the effect does not reach stay clear. The grain follows document position, so it is repeatable.
  No Photoshop render of a noisy effect was available to check the scaling against.

## [0.12.11] - 2026-10-02

### Changed

- Dissolve draws from a fixed 16384-byte noise table (generated by a Park-Miller generator from a
  fixed seed) indexed by position: a pixel survives when its coverage byte reaches the table byte,
  so the same pixel always draws the same noise. It does not yet reproduce Photoshop's pattern
  position for position.
- A Center stroke over the soft, translucent part of a raster layer now also paints behind the
  layer there, leaving near-solid stroke colour across the soft interior (an Inside stroke still
  recolours it and keeps the layer's alpha).

## [0.12.10] - 2026-10-01

### Fixed

- Linear Dodge and Linear Burn scale their source by the strength before the saturating add or
  subtract, instead of easing the blended result toward the backdrop. A bevel's highlight or
  shadow in those modes, a shadow or glow in them, and a layer's fill opacity now add or subtract
  the scaled source at full white or black; the layer's own opacity still eases the result. A
  reference with a Linear Dodge highlight and Linear Burn shadow improves from 4.1 to 1.1 mean
  error.

## [0.12.9] - 2026-10-01

### Changed

- The compositor's approximate models (Color Balance, Photo Filter, Vibrance, Black & White,
  Color Lookup, bevel contour/texture, stroke handling of soft layers, noise gradients, Dissolve,
  Precise glow, effect contours and the layer-by-layer clipping path) are now marked in their
  documentation as not yet pixel-exact, with what each was checked against. No rendering changes.

## [0.12.8] - 2026-10-01

### Added

- Glows with the Precise technique follow the exact distance from the layer's edge instead of a
  blur: full out to the spread's share of the size, then a linear fall to nothing at the size.
  This is an approximation drawn from how the technique is described; no Photoshop render of it was
  available to check against.

## [0.12.7] - 2026-10-01

### Added

- The contour of a drop shadow, inner shadow, outer glow, inner glow or satin now reshapes the
  effect's falloff: it maps the soft field's strength (after the blur and, for glows, the range
  gain) to the strength painted. A linear contour changes nothing. A reference using shaped shadows
  and glows improves from 1.46 to 0.57 mean error.

## [0.12.6] - 2026-10-01

### Fixed

- Effects that follow the global light (drop and inner shadows, Bevel & Emboss) now take the
  document's global angle and altitude instead of the copy stored in the layer, which goes stale
  when the global light is changed after the effect was made. A reference with a stale shadow angle
  improves from 6.39 to 1.46 mean error.
- A shallow knockout inside an isolated group now reaches back to the group's own starting canvas
  instead of the document's backdrop. The reference with knockout in isolated groups improves from
  13.9 to 1.2.
- An Outside stroke on a layer with soft alpha (a raster fade or glow) now sits behind the layer
  across its whole footprint and shows through the translucent part, as Photoshop draws it, instead
  of fading toward the backdrop. An Inside or Center stroke over the soft interior of such a layer
  recolours it and keeps its alpha.
- A pass-through or isolated group that is the base of a clipping group now blends as a unit with
  its clipped layers when everything inside it blends Normally, so its soft edges keep their alpha
  (a reference with a clipped shape on a soft group goes from 4.1 to 0.01).

### Added

- Noise gradients render. Photoshop's random sequence cannot be reproduced, so a gradient is drawn
  from a deterministic sequence seeded by the stored seed and shaped by its roughness, colour model
  and ranges; fills, overlays, strokes and gradient maps all use it. The result resembles the
  original but does not match it pixel for pixel.

### Changed

- The stored-merge probe in the test harness composites a merge that keeps straight colour (black
  under a clear alpha) over white before comparing.

## [0.12.5] - 2026-10-01

### Changed

- A Bevel & Emboss profile contour with the anti-aliased option on is now lit at nine sub-pixel
  positions (0, 0.33 and 0.66 px in both axes) instead of once per pixel: the raw height is
  interpolated, the contour is applied per sample, each position is shaded separately and the nine
  shadings are averaged. The Photoshop-rendered contour reference improves from 2.81 to 1.93 mean
  error (worst pixel 127 to 67).
- Chisel bevels build their height from the signed distance to the 50 % coverage contour,
  corrected for each pixel's own coverage and scaled over `size + 1` pixels, instead of a distance
  to the nearest fully painted or clear pixel. A gloss-contour reference improves from 0.82 to
  0.78.

## [0.12.4] - 2026-10-01

### Changed

- Bevel textures on Inner, Outer and Stroke bevels now add a one-sided, unsmoothed relief
  (bright texture pixels sink the surface, or lift it when inverted) instead of a centred relief
  smoothed and faded toward the flat parts of the bevel, and their shading is capped near the
  bevel's own footprint. Emboss and Pillow Emboss keep the earlier centred relief.
- A texture that is not linked to the layer ignores its stored phase offset and sits at the
  document origin; the offset only applies to a linked texture.
- The Photoshop-rendered bevel references with textures improve from 1.23 / 0.37 to 0.79 / 0.30
  mean error, and the document mixing a contour and a texture sub-option from 6.13 to 2.81.

## [0.12.3] - 2026-10-01

### Added

- Color Lookup adjustment layers that embed an Iridas `.cube` table now render, using
  tetrahedral interpolation and honouring a custom `DOMAIN_MIN`/`DOMAIN_MAX`. Layers that embed
  another table format, a 1D table or a malformed file are still left unrendered.

## [0.12.2] - 2026-10-01

### Changed

- Color Balance renders as one transfer curve per channel instead of tonal-range weights: the
  shadow sliders move a channel's black point, the highlight sliders its white point and the
  midtone sliders bend it. With Preserve Luminosity the sliders act relative to the largest
  shadow, smallest highlight and midpoint of the midtone sliders, so moving all three of a range
  together changes nothing. Checked against two Photoshop-authored documents (stored merges within
  0.6 levels) and a chained adjustment reference, which falls from 9.8 to 2.6 mean error.
- Photo Filter multiplies in the D50 connection space (tristimulus values against the filter colour
  relative to the white point) instead of in gamma-encoded RGB, then restores luma when Preserve
  Luminosity is on.
- Vibrance follows a hue/saturation/value model: positive amounts favour muted pixels, spare the
  red-to-orange band and leave very dark pixels alone; negative amounts pull saturated pixels toward
  gray. The Saturation slider scales channels about a fixed gray that leans on red and green.
- A Black & White tint shifts the tint colour so its luma becomes the gray.
- A clipping group whose base is a plain pixel or text layer without effects, Blend If or knockout
  now blends as a unit when "Blend Clipped Layers as Group" is on (the default): members meet the
  base colour as if it were opaque, the base's alpha decides where the group shows, and the result
  merges with the base's blend mode. A soft base edge no longer picks up the clipped colour over the
  backdrop. A Photoshop-authored clipping fixture (Multiply and Levels members over a translucent
  base) now matches exactly. With the option off, members still blend one by one.

## [0.12.1] - 2026-10-01

### Fixed

- Black & White adjustment layers now render from their stored slider weights. The compositor
  looked up the wrong descriptor keys, so every layer used the default weights, and it dropped the
  gray floor of pale colours (a pale red rendered as a dark gray). A pixel now keeps its minimum
  channel and scales the rest by the slider weight at its hue, interpolated between neighbouring
  sliders. A tinted layer renders the tint colour at the gray's lightness (not yet checked against
  a Photoshop render).
- Monochrome Channel Mixer layers mix the single gray output stored in the first record instead of
  taking the luma of the three colour outputs.

## [0.12.0] - 2026-10-01

### Added

- Hue/Saturation now uses all six range bands, and Selective Color applies its absolute and
  relative channel corrections. `LayeredFile::materialize_merged_image` promotes a retained
  layerless composite to an editable background layer.
- CMYK and Lab document colors use their embedded ICC profiles through `moxcms`, with bounded
  batches and defined fallbacks for missing or mismatched profiles.
- Layer knockout modes and deterministic Dissolve coverage are included in compositing.

### Changed

- Untouched layerless documents preserve and render their merged `ImageData`. Transparent merged
  colors are un-matted before the compositor exposes straight RGBA; Indexed layerless images use
  their document palette. Empty authored documents retain transparent output.
- A clipping base's style paints above its clipped members. Pass-through groups used as clipping
  bases confine child adjustments to pixels already contributed inside the group.
- Color Balance, Vibrance and Photo Filter use revised Photoshop-oriented approximations. The
  adjustment-chain reference mean error fell from roughly 13 to 9.8 levels.
- `psd-core::ImageData` can own a retained merged section and no longer implements `Copy`.

### Fixed

- One-bit white samples expand to full-scale 8-bit values. One-bit and layerless merged images
  can be rendered, preserved, and materialized within the compositor's byte limits.
- Disabled effect blocks no longer suppress group fill opacity. Layerless merged-image sections
  that exceed the retention budget return a typed memory-limit error rather than silently losing
  their image on save.

## [0.11.5] - 2026-09-30

### Fixed

- Compositing a visible layer whose channels are still compressed after a lazy read returns
  an actionable error instead of treating its colors, transparency and masks as absent.
  Explicit `decode_layer_pixels` calls retain the document's configured memory budget.
- Groups clipped to another layer honor its coverage, including group opacity, masks and
  effects. Pass-through groups retain the true backdrop, and nested group effects contribute
  to the bounds used for clipping and opacity snapshots.
- Adjustment layers honor their blend mode while preserving backdrop alpha. Companion
  `CgEd` metadata no longer suppresses an adjustment when stored before its settings block.
- Independent interior effects keep their own paint, transparency and blend modes when fill
  opacity is reduced. They render over the true backdrop before the layer's mask, clip
  coverage and master opacity apply once to the combined result.
- Curves uses extended `Crv ` channel records when present and excludes non-color channels
  from RGB adjustment. Its lookup tables are resolved once per layer instead of rebuilding
  splines and allocating temporary buffers for every pixel.
- Turning layer effects off preserves pattern fills and pattern-painted shapes.
- Strict compositor oracle checks reject incomplete comparisons and empty selections in
  addition to excessive pixel error. The stored-merge probe supports the same strict limit;
  diagnostic sweeps continue reporting known rendering limitations without enforcing it.

### Changed

- Corrected public documentation links for the compositor and typed image resources.

## [0.11.4] - 2026-09-30

### Fixed

- Hue/Saturation runs Photoshop's integer pipeline instead of a float HSL model: the lightness
  slider per channel, then a saturation ratio on the half chroma and a hue turn in whole steps
  of a 1530-step wheel (master within 2/255 of Photoshop); colorize rebuilds from the source's
  lightness. The six per-range adjustments are still not applied.
- Exposure on a grayscale document works in gamma 1.8, its working space.
- A Normal layer with a fill opacity below 100% still shows its pattern, gradient and color
  overlays, glows and inner shadows at the layer's opacity (they ignore the fill); an overlay
  pattern overlay counts as an interior effect.

## [0.11.3] - 2026-09-30

The compositor is now also checked against the merged image Photoshop stored in real documents
(a second env-gated probe in `tests/composite_oracle.rs`). Across 200 documents the median
mean error is 0.2 levels, and this pass fixed the real-world failures it exposed.

### Fixed

- **Fill layers never rendered**: a solid, gradient or pattern fill layer was routed to the
  adjustment path, which skips fills.
- Modern Brightness/Contrast read its values from the wrong block (they live in the content
  generator's descriptor, the `brit` record is a legacy placeholder) and applied the legacy
  formula to 8-bit documents; its brightness Hermite used the wrong start tangent.
- Exposure worked in the encoded domain with a bogus -0.5 offset; it now applies gain and offset
  in linear light, then gamma, then re-encodes (within one level of Photoshop).
- Curves applied the composite curve before the per-channel curves; Levels and Curves on a
  grayscale document read the wrong channel (its single channel uses the record that a colour
  document keeps for red).
- Descriptor colours: gray is a percentage of black, CMYK and HSB saturation/brightness are
  percentages (they were read as 0-255), and Lab colours now convert to sRGB.
- A pass-through group with a fill opacity isolates (its adjustments reach only its own
  content) and the fill scales the merged result like the opacity does.
- Effects on a shape follow the path, not its fill's transparency, so a gradient fill that fades
  out keeps a full outline; a stroke promotes a faint flat region to the shape instead of
  stroking only the solid part.
- Clipping to a group or an adjustment layer: the clipped layers see the group's silhouette or
  the adjustment's mask instead of nothing.

### Added

- **Artboards** composite as isolated units clipped to their rectangle over their background
  (white, black, transparent or a custom colour).
- CMYK documents convert to RGB with the profile-free device formula instead of reading the
  first three planes as RGB.
- Pattern strokes (layer-effect Stroke with a pattern fill).

## [0.11.2] - 2026-09-30

Bevel and emboss, and shape layers. On the reference flattens every shape fixture now
matches (fills, pattern and gradient fills, boolean combines, strokes, feather) and the
bevel and emboss fixtures match to a few /255.

### Added

- **Shape layers**: a fill layer with a vector mask (how Photoshop stores a shape, with no
  pixels) is rendered from its path. The fill is a solid colour, a gradient (geometry aligned
  to the path's own bounds) or a pattern; `vstk` strokes render with their width, alignment
  (inside, centre, outside), caps, joins, miter limit, dashes and paint (solid, gradient or
  pattern) and opacity; the path's feather blurs the whole shape, stroke included, without
  clamping at the canvas edge, and its density shows the fill at `1 - density` outside the
  path. Subpath combines follow Photoshop: the first shape on an empty path is exactly itself.
- **Bevel and emboss**: Inner and Outer Bevel, Emboss and Pillow Emboss; Smooth, Chisel Hard
  and Chisel Soft; Soften; Depth, Direction, Size, angle and altitude; highlight and shadow
  colours, modes and opacities; the Contour sub-option (a Linear contour steepens the slope by
  100 / Range, any other curve reshapes the profile); the Gloss Contour; and Texture (the
  pattern's luminance perturbs the height). Stroke Emboss is not rendered yet.
- `composite` contours: natural-cubic curves with corner points, baked into a 256-entry table.
- The corpus harness gained a `composite` check: every document flattens to the canvas size
  without panicking.

### Changed

- The stroker unions its pieces under the non-zero fill rule.

## [0.11.1] - 2026-09-30

The compositor is now checked against Photoshop's own flattens of 50 small test documents
(an env-gated probe, `tests/composite_oracle.rs`). Most of the gap to Photoshop turned out
to be a handful of real bugs, all fixed here; masks, Blend If, channel restrictions, group
styles, inner and outer glows, inner shadows, stroke knockout and pattern overlays now match
within 0-2/255 on the reference set.

### Fixed

- Every effect, mask union or canvas clamp displaced or blacked out the layer's pixels:
  channel planes were copied linearly into the padded rect instead of being placed at the
  layer's bounds.
- Blend If read the wrong record layout. The first four blending ranges are Gray, R, G and B,
  each carrying a This Layer pair and an Underlying Layer pair; the gate now multiplies them
  and lets transparent backdrop pixels always pass the underlying side. Adjustment layers
  gate on the adjusted output, and a group with a non-default range isolates.
- Blend modes ignored the backdrop's alpha: over a partly transparent backdrop the source
  keeps its own colour in proportion to the missing backdrop (an isolated group's Multiply
  child no longer multiplies against nothing).
- Three `Rect::new` calls had their arguments swapped (masks, isolated groups, fill layers).
- The tent blur used a forward-only window, which shifted every shadow and glow up and left
  by about the blur size; the two box passes are now centred. Drop and inner shadows fall
  away from the light (120 degrees throws the shadow down and right).
- Inner shadows, inner glows and satin never saw the world beyond the layer's edge and drew
  nothing there; their fields now run over the matte padded with transparent pixels.
- Levels applied the per-channel records over the composite record instead of composing with
  it.
- A group's bounds always included the canvas origin, and adjustment layers inside a group
  were clipped to the children's bounds.

### Added

- **Masks**: layer and group masks are resolved to one coverage plane with their density and
  feather. A raster mask's feather is a Gaussian (sigma = the feather radius) that replicates
  the canvas edge; density lerps toward white. **Vector masks** (`vmsk`/`vsms`) are
  rasterized from their Bezier paths: contours sharing a shape group fill even-odd together,
  groups combine by their lead contour's operation (add, subtract, intersect, exclude) over
  the running coverage, and the vector feather and density apply unclamped at the canvas edge.
  Adjustment layers honor masks, opacity, fill opacity, clipping and Blend If.
- **Layer effects**: gradient overlays with Photoshop's calibrated geometry (Linear,
  Radial, Angle, Reflected, Diamond; angle, scale, offset, reverse, Align with Layer, colour
  and transparency stops, midpoints and Classic easing), pattern overlays (anchored at the
  effects reference point or the document origin, with phase, scale and rotation), strokes
  as real planes (distance band, gradient and Shape Burst fills, and the overprint-off
  knockout that removes the layer's own content under the band even at 0% opacity),
  exterior effects composited with their own blend modes, "Layer Knocks Out Drop Shadow",
  and "Blend Interior Effects as Group" (`infx`): with it off, interior effects of a
  non-Normal layer paint over the result with their own modes.
- **Styled groups**: a group with effects runs the layer pipeline on its flattened content
  (isolated groups), or draws its exterior effects before and its interior effects after its
  children (pass-through groups).
- **Advanced Blending channel restrictions** (`brst`).
- Gradient fill layers use the fill-layer geometry instead of a horizontal ramp.
- `LayeredFile::patterns()` decodes the document's `Patt`, `Pat2` and `Pat3` blocks into RGBA8
  tiles. The decoder moved to `psd_core::pattern` (`psd-companion` re-exports it).
- Tests: synthetic documents with known flattened pixels (`tests/composite.rs`), and unit
  tests for the path rasterizer, the gradient ramps and their geometry.

### Changed

- Clipped layers fade with their base: the base's opacity and fill scale the whole clipping
  group, and a clipped layer samples its base's coverage by position.

## [0.11.0] - 2026-09-30

### Added

- **A compositor** (`LayeredFile::composite_rgba8`, `composite_rgba8_with`): flattens the layer
  stack into 8-bit straight-alpha RGBA the way Photoshop renders it. The engine places each
  layer's channels at its bounds, applies effects, resolves coverage (alpha x raster mask x
  clipping x blend-if), blends with Photoshop's modes and opacity, applies adjustment and fill
  layers, and composites groups (pass-through children meet the true backdrop and the whole
  group then fades toward the pre-group snapshot by the group's opacity; a group with its own
  blend mode isolates instead).
  - **Blend modes**: all of Photoshop's, including the four non-separable ones (PDF
    `set_lum`/`set_sat`). For 8-bit documents the modes whose rounding Photoshop pins are
    computed in the byte domain: Color Burn and Color Dodge round to nearest half-up and
    resolve their division corner by the destination, Exclusion rounds the product before
    doubling, Divide rounds to nearest.
  - **Blend If**: the composite and this-layer ranges gate per channel with Photoshop's split
    feather and its composite-gray weights (299/590/111 at 1/1000).
  - **Adjustments**: Brightness/Contrast (legacy hybrid order and the modern gain-ray curve),
    Levels, Curves (natural cubic, Photoshop's own interpolation), Exposure, Hue/Saturation,
    Color Balance, Black & White, Photo Filter, Channel Mixer, Invert, Posterize, Threshold,
    Gradient Map, Vibrance; solid-color and gradient fills.
  - **Effects**: color and gradient overlays (folded into the layer's straight color),
    satin, inner glow (Edge and Center), inner shadow, drop shadow and outer glow (spread
    dilation, then a tent blur applied as two sliding box passes), and strokes (an exact
    Euclidean distance band around the contour). The soft-effect pipeline follows the
    calibration records: spread expands the matte by `round(spread% x size)`, the remaining
    size blurs with `N = max(2, round(size)) - spread_radius`, Range gains multiply after the
    blur, and an omitted range renders at 100.
- `CompositeOptions` turns effects, blend-if or adjustments off for diagnostics.
- The compositor is memory-conscious: canvases are bounded to the layer or subtree bounds and
  clamped to the document, effect padding never allocates past the canvas, and the tent blur
  and distance transform are O(pixels).

### Not yet matching Photoshop

The compositor's structure is complete, but its output is **not yet validated against
Photoshop**: on the corpus it runs over every document (494 of them) without panicking and in
milliseconds, and it matches the stored merged image closely on simple documents, but the
stored composites themselves are usually Photoshop's *placeholder* fills (a solid white or
black image written when the merge was skipped), so they cannot serve as ground truth for
most files. Accuracy work continues against the files that do carry a real merge.

Known gaps, all deliberate for this pass: bevel/emboss, pattern overlay and pattern fill,
noise and jitter, contour shaping beyond Linear, the "Precise" glow techniques, stroke
overprint knockout, knockout (shallow/deep), dissolve, CMYK/Lab color conversion (RGB and
grayscale documents only), vector rasterization (the stored preview is used), and Selective
Color, Color Lookup and Content Generator.

## [0.10.0] - 2026-09-30

### Changed (breaking)

- `SectionDivider::from_raw` returns `SectionDivider` rather than `Option<_>`, and the enum
  gained `Unknown(u32)`: a record that carries an `lsct` block is a divider record whatever
  its type value says. `as_raw` keeps an unknown value byte for byte, and `is_known` reports
  whether the value is one of the four the format defines.

### Fixed

- A divider whose `lsct` type value this build does not know was read as a pixel layer, so its
  group lost its pair and the next save synthesized a second divider — one extra layer record
  per group. Such a record now keeps its pairing, its place in the tree and its bytes. No file
  in the ~600-document sweep carries an unknown value today (real files use 0-3), so this
  closes a latent class rather than a live bug: a newer Photoshop, or a patched file, would
  have hit it.

### Tests

- A round-trip test patches every bounding divider of a group fixture to `0xcafebabe` and
  requires the tree to keep its shape, the unknown value to survive the write, and the re-read
  document to hold the same records.
- The corpus harness now takes a directory-prefix entry with a `*` check, so a corpus that
  keeps deliberately malformed files can be swept cleanly instead of pinning each file.

## [0.9.6] - 2026-09-30

### Added

- `Layer::set_rich_text`: replace a text layer's body **and** re-split it into runs of the
  given UTF-16 lengths. The text goes in exactly as `set_text` writes it (the EngineData
  literal, the `Txt` descriptor string, the legacy ranges), then the run structure is
  rebuilt: every run starts from the layer's first run's style — Photoshop's own
  inheritance — so the differences are set afterwards with `style_run_mut`, which indexes
  runs in order. Paragraph runs are split on the text's `
`-terminated lines, and a
  trailing `
` is appended when the text does not end with one, since Photoshop's
  `RunLengthArray` covers it. Lengths that are zero or do not cover the whole text are
  refused before anything changes.

### Tests

- A rebuild splits a one-run layer into two runs that both inherit its size, styles the
  second run differently, re-splits a two-line text into two paragraph runs, survives a
  save, and leaves the document byte-identical when a call is refused.

## [0.9.5] - 2026-09-30

### Added

- `LayeredFile::duplicate_layer`: copy a layer — with its subtree, group divider, mask,
  effects, text and every preserved block — directly above the original, the way Photoshop's
  Duplicate Layer does. The copy gets a fresh `lyid` when the original's is taken.
- `LayeredFile::copy_layer_from`: copy a layer from another document into this one, on top.
  The copy owns its data, so the source document does not have to outlive it, and layer ids
  are made unique against the destination. Document-level data a layer only *references* (a
  smart object's linked file, a pattern) is not copied — the same shape as a Photoshop
  duplicate, which also leaves the reference pointing at the original.
- The layer-id guard now sits in the one place every detached-tree insertion passes through
  (`allocate_tree`), so a tree re-inserted after its id was taken again is covered too.

### Tests

- A duplicate lands directly above its original, in the same parent, with the pixels and a
  fresh id, and both survive a write; a duplicated group brings its subtree; a layer and a
  group copied between documents arrive on top, leave the source untouched, and keep the
  destination's ids unique.

## [0.9.4] - 2026-09-30

### Added

- Typed views for three document resources that were only preserved raw before. Each parses
  on demand from its block — nothing is retained until a caller asks — and each has a
  `set_*` that replaces the payload, while a save still writes the raw block, so an
  untouched document keeps its own bytes exactly:
  - **Grid and guides (`1032`)** — `ImageResources::grid_and_guides`. Positions and the grid
    cycle are in Photoshop's 1/32-pixel units, which `Guide::position_px` and
    `GridGuides::grid_px` convert (the default cycle, 576, is 18px = a quarter inch at
    72 dpi).
  - **Slices (`1050`)** — `ImageResources::slices`. Version 6's record list (name, bounds,
    URL, target, message, alt tag, cell text, alignment, colour) plus the descriptor that
    versions 7 and 8 use, and the optional trailing descriptor a v6 payload may carry.
  - **Layer comps (`1065`)** — `ImageResources::layer_comps`, the comp list with its capture
    flags and the last applied comp, and `Layer::comp_states` for the per-layer `cmls`
    block. A comp whose entry does not name a layer leaves that layer's own visibility in
    force, which is what the states report.

### Tests

- A corpus sweep parses each of the three resources from every document that carries one and
  re-serializes it, requiring the bytes to match: **531 guides, 529 slices (versions 6 and 8)
  and 5 layer-comp resources round-trip byte for byte across 555 files**. A self-contained
  test builds a document with a comp list and a layer's comp state and pins the accessors,
  including the inherited-visibility case.

## [0.9.3] - 2026-09-30

### Fixed

- A text layer built for a document that is not 72 dpi now renders at the size asked for.
  The run's `FontSize` is stored in document pixels, not points, while the builder took its
  argument as points and wrote it unchanged: at 300 dpi a 12pt request came out four times
  too small, and the estimated text box was scaled the same way. `TextLayerBuilder` gained
  `dpi` (72 by default, so nothing changes there) and converts points to pixels for the run,
  its leading, and the box metrics; the default style sheet keeps the nominal point value,
  which is the unit it is in. `CharacterStyle::font_size` and the builder now document the
  units, since the two differ on the wire.
- Adding a layer whose `lyid` another layer already has assigns a fresh id, so a clone or a
  copy from another document cannot repeat an id that is meant to be unique within a
  document. A document read from disk never passes through the add methods, so its own ids
  round-trip untouched.
- `LayeredFile::write` writes through a sibling temporary file and replaces the target only
  once the whole document is on disk. It used to create the target directly and delete it if
  the write failed part way, which destroyed the previous file — including a document saved
  over its own source.

### Added

- `Layer::layer_id` and `Layer::set_layer_id`, and `TaggedBlockKey::LYID` with them.

### Tests

- A builder round trip at 72, 150 and 300 dpi pins the run size, the default sheet's nominal
  size, and the box scale; a cloned layer with a taken id; a write that fails before the
  bytes land leaving the target byte-identical with no temporary file left behind; and that a
  section divider cannot be removed on its own, since that is the half-a-pair shape a group
  linker has to survive.

## [0.9.2] - 2026-09-30

### Fixed

- An unedited save no longer rewrites the resolution resource. `dpi` was written
  unconditionally, so a file with unequal X/Y resolutions or non-inch units was saved with
  both axes at the horizontal value in pixels per inch — silently losing the vertical axis
  and the units. The resource is now the authority: it is written only when `dpi` differs
  from what it says, and setting `dpi` still sets both axes to that many pixels per inch. A
  file that had no resolution resource keeps lacking it (the format's default is 72 ppi);
  `LayeredFile::new` now creates one, as Photoshop's own documents have.
- A layer whose name lives only in the legacy pascal record no longer gains a `luni` block
  on save. Adding one changed a file that round-tripped without it, so a second save
  differed from the first. The block is added only when the record cannot carry the name —
  a character outside Windows-1252, or a payload past the one-byte length marker — which is
  exactly when the pascal string alone would corrupt it. An existing `luni` is still
  refreshed when the name changes.

### Added

- `psd_core::PascalString::fits`, which reports whether a value survives a write/read round
  trip at a given alignment; it is what the name-block rule uses.

### Changed

- `PascalString::section_size` measures the encoded payload rather than the UTF-8 length,
  which differ for non-ASCII values.

### Tests

- The corpus harness compares document-level image resources (by id and payload, typed
  blocks by value) between the original and the re-read document. It previously checked
  only the code that reads them, which let the resolution rewrite above pass every sweep.

## [0.9.1] - 2026-09-30

### Changed

- A document read from disk holds its ICC profile once. `LayeredFile::icc_profile` was a copy of
  the profile in the `image_resources` ICC block, so both stayed in memory for the document's
  lifetime (and a save cloned both again). A read now moves the profile into `icc_profile` and
  leaves the block in `image_resources` as an empty placeholder that keeps its position, so a
  document that had an ICC block still writes it in the same place. `icc_profile` was already
  what a save writes; the resource block's copy could only go stale if it was edited.
  `ImageResources::take_icc_profile` does the move. Code that read the profile from
  `document.image_resources.icc_profile()` should read `document.icc_profile` instead.

## [0.9.0] - 2026-09-30

Saving a lazily read document no longer copies its compressed channels, and a 16/32-bit
document's layer data is never held twice while it is read. This changes a public type, so it
is a minor version.

### Changed (breaking)

- `psd::core::ChannelData` and `ChannelImageData` carry a lifetime, and `ChannelData::data` is a
  `Cow<'a, [u8]>` instead of a `Vec<u8>`. A parse still owns its payloads; a write stages the
  payloads a lazy document keeps (`ReadOptions::with_raw_data`) by borrowing them, where it used to
  clone every one. Code that only reads `channel.data` as a slice is unaffected. Code that builds
  a `ChannelData` writes `ChannelData::owned(compression, vec)` (or `borrowed`), or `vec.into()`
  for the field; code that needs a `Vec` calls `.into_owned()` or `.to_vec()`.
  `LayerInfo::channel_image_data` is now `Vec<ChannelImageData<'a>>`.
  Writing a lazy 8-bit document of 415 MB now peaks 2 MB above the document, where it peaked at
  twice its size; a lazy 16-bit document of 241 MB, 2 MB above it, where it peaked at 482 MB.
- `PhotoshopFile::read` returns the `Lr16`/`Lr32` block of a 16/32-bit document as an empty
  placeholder. Its layers are parsed where they lie in the file, so the block's bytes are never
  copied and the layer data exists once, parsed, instead of twice while reading. The block keeps
  its position, and writing regenerates its content from `layer_info` as before. A nested block
  that holds no layers is kept byte for byte.
  Reading a lazy 16-bit document of 241 MB now peaks at 240 MB of heap, where it peaked at 480 MB.
  `psd::LayeredFile::document_blocks` therefore has the block with empty data (0.8.6 did this
  after the read; it now never has the bytes).

### Fixed

- The buffered writer (`PhotoshopFile::write`) and the streaming one now agree on a 16/32-bit
  document that carries several `Lr16`/`Lr32` blocks: the layer data goes into the first and the
  repeats are dropped. The buffered writer used to write a full copy of the layer data into each,
  as upstream does, and only the streaming writer dropped them, so the two produced different files.

### Added

- `ChannelData::owned` and `ChannelData::borrowed`.
- Tests that pin the memory behavior: a lazy document stages only borrowed payloads and rewrites
  to identical bytes; a nested block is taken from the first `Lr16`/`Lr32` only; a block with no
  layers survives a round trip; the streaming and buffered writers agree.

## [0.8.6] - 2026-09-30

Less memory to write a 16-bit document, and to hold one after reading it.

### Changed

- A 16- or 32-bit document read from disk no longer keeps a second copy of its compressed
  layer data. The `Lr16`/`Lr32` block stays in `document_blocks`, which fixes where it is
  written, but its data is empty: the writer regenerates it from the layer tree, as it always
  did. On a 3000 x 2000, 10-layer, 16-bit document the decoded document takes 458 MB instead of
  698 MB, and the operating-system peak while reading falls from 966 MB to 729 MB. If every
  layer is later removed the empty block is not written.
- Compressed channels no longer carry unused capacity. The ZIP compressor sized its output for
  incompressible input and kept that capacity, so a compressible channel stayed at its raw size in
  memory until it was written.
- ZIP-with-prediction encoding of 8- and 16-bit channels no longer copies the channel at full
  size: rows are delta-coded a block at a time in a small scratch buffer and written out
  big-endian. The bytes are the same.
- Compressing an 8-bit channel no longer copies it to hand it to the RLE or ZIP codec
  (`psd_codecs::endian::be_bytes` borrows the samples when they are already bytes).
- Writing the layer data of a 16/32-bit document no longer clones the block it replaces, which
  had copied all of the layer data once more.
- Together with 0.8.5, the heap peak while writing that 16-bit document (decoded pixels plus
  compressed channels) falls from 2,133 MB to 713 MB, and the file is byte for byte the same.

## [0.8.5] - 2026-09-30

Writing a document no longer needs a second, whole-file copy of it in memory.

### Changed

- `LayeredFile::write` and `write_with_progress` stream the file to disk instead of building
  it in a buffer first, and each compressed channel is released as soon as it is written.
  On a 4000 x 3000, 12-layer, 8-bit document the heap peak while writing falls from 1,708 MB
  to 999 MB (the decoded pixels plus the compressed channels, nothing more), and the write
  takes 0.56 s instead of 1.57 s. A 16-bit document falls from 2,133 MB to 1,398 MB. A write
  that fails part way removes the partial file.
- `LayeredFile::to_bytes` fills a buffer sized up front, not one grown by doubling, and
  releases each channel as it goes. Its operating-system peak on the same 8-bit document falls
  from 1,724 MB to 1,006 MB of resident memory.
- For 16- and 32-bit documents the layer data is written straight into its `Lr16`/`Lr32`
  block. It used to be regenerated into a buffer, cloned into a replacement block and copied
  again, three or four copies of all the compressed channels at once.
- The output is byte for byte what it was, except in one case: a 16/32-bit document that
  carries several `Lr16`/`Lr32` blocks now has its layer data written into the first and the
  others dropped, where each used to receive its own copy of the layer data.

### Added

- `PhotoshopFile::write_to`, which streams the five sections to any `std::io::Write` and
  empties each channel payload as it is written, and `PhotoshopFile::size_hint`.
  `PhotoshopFile::write` to a `BeWriter` is unchanged.
- `TaggedBlock::write_to`, `TaggedBlock::encoded_len` and `TaggedBlock::header_bytes`, and
  `std::io::Write` for `BeWriter`.

### Verified

- The streamed bytes equal the buffered writer's on every document in the corpora, 8-, 16- and
  32-bit, PSD and PSB, and on synthetic documents of each depth.

## [0.8.4] - 2026-09-30

### Changed

- RLE channel compression packs its scanlines a block at a time into one buffer instead of
  allocating and growing a `Vec` per scanline. Writing a 4000 x 3000, 12-layer, 8-bit
  document made about 624,000 heap allocations before and about 23,000 now, and the output
  is byte for byte the same. `psd_codecs::rle::pack_bits_compress_into` appends one packed
  row (padding included) to a caller's buffer and returns its length; `pack_bits_compress`
  is unchanged in behavior.

## [0.8.3] - 2026-09-30

### Fixed

- Updated the vendored PNG decoder to 0.5.1, correcting a documentation link left behind
  when its encoder API was removed. The workspace documentation now builds with warnings denied.

## [0.8.2] - 2026-09-30

### Added

- `Layer::set_layer_effects_block` replaces a validated modern effects payload while preserving
  its complete descriptor and trailing bytes and refreshing the legacy mirror.
  `ModernLayerEffects::to_payload` serializes that complete descriptor view.

### Fixed

- Python effects payload replacement now retains incoming fields outside the typed effects
  model. Typed effects edits also retain existing trailing bytes and block signatures.
- Corrected an effects API documentation link rejected by strict rustdoc checks.
- Curve writers reject control-point and freehand-map layouts that disagree with the payload's
  map flag, including curves in the extension section.
- Artboard conversion rejects groups containing existing artboards. Python's artboard list
  follows the current stacking order, and no-op document-settings edits retain the source signature.
- Detached trees with children attached to nongroup layers are rejected before insertion,
  preventing unreachable arena entries. Rejected adjustment edits leave inconsistent text
  records unchanged.
- Adjustment and vector block replacement retain source signatures. Vector-block removal
  ignores unrelated metadata keys.
- Shape builders reject invalid bounds before adding a layer, and synthesized transparency
  validates rectangle extents and bitmap sizes before allocating, avoiding coordinate-overflow panics.

### Changed

- Promoting image or adjustment layers to shapes moves their existing channel buffers instead
  of copying them.

## [0.8.1] - 2026-09-29

Artboards can now be created and edited through Rust and Python while keeping their document
settings in step.

### Added

- `Artboard::new`, `set_rect`, `set_preset_name`, `set_background`, `set_guide_indices`, and
  `to_tagged_block` author the version-16 layer descriptor while preserving unknown fields on
  edits. `ArtboardSettings::new`, typed setters, and `to_tagged_block` write the document-level
  `artd` settings with the correct PSD/PSB signature.
- `LayeredFile::add_artboard`, `set_artboard`, `clear_artboard`, `add_layer_to_group`,
  `insert_layer_tree`, and `remove_layer` keep the document-level artboard count in sync.
  Artboards cannot be nested inside other artboards; direct `Layer::set_artboard` and
  `clear_artboard` edits are intended for detached layers.
- `set_artboard_settings` and `clear_artboard_settings` let callers manage the document-level
  artboard-tool settings directly.
- Python layers expose artboard block inspection and editing; Python documents expose their
  artboards, document settings, and an artboard builder with bounds, preset, background, and guide
  indices.

### Verified

- Typed artboard and document-settings writers round-trip the generated PSD/PSB fixtures byte for
  byte, including PSB's `8B64` document block signature. Corpus checks also verify no-op artboard
  edits preserve layer and document blocks.
- Rust and Python tests cover new artboards, child groups, background/preset data, document counts,
  nested-artboard rejection, and write/read behavior.

## [0.8.0] - 2026-09-29

Shape layers can now be authored from typed vector blocks, and Python can inspect and edit
adjustment and shape data.

### Added

- `VectorPath::to_payload`, `VectorMask::to_payload`, and `VectorBlock::to_tagged_block` write
  the vector path, mask, fill, stroke, origination, and path-name payloads while retaining
  unknown records and trailing bytes.
- `LayerKind::Shape` and `ShapeLayer` preserve shape preview and mask channels. Layers can add,
  replace, and clear typed vector blocks; `LayeredFile` can create modern shapes from vector
  blocks and legacy or modern shapes from tagged blocks, at the root or inside a group.
- Python layers expose `is_adjustment_layer`, `is_shape_layer`, adjustment/vector block payloads,
  and block replacement/removal. Python documents can create adjustment/fill and shape layers
  from validated block payloads.
- Python layers expose effects block payloads and a validated modern-effects setter that also
  refreshes the legacy `lrFX` mirror.

### Changed

- Reading a fill paired with a vector mask now classifies it as `LayerKind::Shape`; a pixel layer
  with a vector mask alone remains an image layer. Preview channels are preserved, and new shape
  records mark their pixel data as derived.
- Adding `LayerKind::Shape` is a breaking change for exhaustive matches on the public enum, so
  this release advances the pre-1.0 compatibility line to 0.8.0.

### Verified

- The corpus harness now checks byte-exact no-op effect edits and adjustment/vector payload
  serialization. Shape creation round-trips both legacy fill-plus-mask and modern vector-content
  blocks; the additional local corpora pass the same checks.
- The Python suite exercises adjustment creation/editing and legacy/modern shape creation.

## [0.7.0] - 2026-09-29

Adjustment and fill layers can now be built from typed settings and edited through the layer API.

### Added

- `AdjustmentBlock::new`, `AdjustmentData::to_payload`, and
  `AdjustmentBlock::to_tagged_block` serialize all recognized adjustment and fill payloads,
  including descriptor-backed settings, levels and curves extensions, and preserved trailing
  bytes. The block builder rejects mismatched kind/data pairs and layouts the typed model cannot
  encode.
- `LayerKind::Adjustment` and `AdjustmentLayer` give adjustment and fill layers their own layer
  kind while preserving preview and mask channels. `Layer::new_adjustment`,
  `Layer::adjustment`, `Layer::set_adjustment`, and `Layer::clear_adjustment` build and edit their
  settings blocks.
- `LayeredFile::add_adjustment_layer` and `add_adjustment_layer_to_group` create adjustment
  layers with empty bounds and fill layers spanning the canvas. CgEd companion blocks can be
  added through `Layer::set_adjustment`.
- The Python `Layer.kind` property reports `"adjustment"` for these records.

### Changed

- Reading a layer with a recognized adjustment or fill settings block now uses
  `LayerKind::Adjustment`. Its existing channels remain available and round-trip as before.
  New adjustment layers set Photoshop's pixel-data-irrelevant flag; their preview channels are
  not synthesized when absent.
- Adding `LayerKind::Adjustment` is a breaking change for exhaustive matches on the public
  `LayerKind` enum, so this release advances the pre-1.0 compatibility line to 0.7.0.

### Verified

- Typed payloads rebuild every block in the generated adjustment PSD/PSB documents byte for
  byte. The corpus checks now also enforce byte-exact serialization for each parsed adjustment
  block.
- New adjustment and fill layers survive write/read with their settings and bounds; adjustment
  edit tests verify replacement, companion ordering, and clearing.
- The legacy effects edit suite passes, including descriptor updates, mirror generation, and
  no-op preservation.

## [0.6.30] - 2026-09-29

Editing a layer's effects now keeps its legacy `lrFX` block in step.

### Added

- `legacy_effects_block_data`, which builds the legacy `lrFX` payload from a `LayerEffects`.
  Photoshop writes each layer's effects twice, as the descriptor block current versions read
  and as `lrFX`, the fixed 396-byte layout Photoshop 5 to CS5 read: seven records (common
  state, drop shadow, inner shadow, outer glow, inner glow, bevel, solid fill), always all of
  them. It holds one of each, so the first instance of each family is written, and an effect
  the layer lacks is written as Photoshop's default switched off. Satin, gradient and pattern
  overlays, strokes and further instances have no legacy record and are not mirrored.
  Colours are written in their own colour space (RGB, HSB, CMYK with inverted ink, Lab,
  gray), a gradient glow takes its first stop's colour, an emboss's size is halved and a
  bevel's strength is the depth applied to it within 1 to 20.

### Changed

- `Layer::set_layer_effects` regenerates the `lrFX` mirror after a real change, immediately
  after the descriptor block as in Photoshop-authored files, instead of dropping it. The
  mirror is derived from what the descriptor now says, so a field the set leaves out keeps
  the value the layer has. Setting a layer's own effects back still changes nothing and keeps
  its existing mirror byte for byte.

### Verified

- The generated block against the `lrFX` Photoshop wrote beside the descriptor block, on all
  300 layers that have both in about 420 Photoshop-authored documents. 166 match byte for
  byte; the other 134 differ only in rounding Photoshop does on values this model cannot see
  the low bits of (a colour component within two units of 65535, an opacity byte within one:
  current Photoshop truncates an exact half percent to `0x7f`, older versions rounded it up
  to `0x80`), except four records that three documents' Photoshop wrote unlike any other
  (an absent drop shadow with distance 0, an absent inner glow not inverted, and a 100 px
  bevel written as 50 px). A bevel's strength is truncated to 16.16, not rounded, and the
  common state stays visible even when the layer's effects are switched off as a whole.

## [0.6.29] - 2026-09-29

Layer effects can now be created and edited on a layer.

### Added

- `LayerEffects`, every effect on a layer as the typed models from 0.6.28: a scale, the master
  switch, drop and inner shadows, outer and inner glow, bevel, colour overlays, satin, gradient
  overlays, pattern overlay and strokes. It reads from, patches in place and builds the root
  descriptor of an `lfx2`, `lmfx` or `lfxs` block (`from_descriptor`, `apply_to`,
  `to_descriptor`). Unlike a single effect, the set is the whole truth about a layer: a family
  with no entries is removed from the descriptor and an entry with no counterpart is added.
  Effects that repeat keep the form the file used, a single key or a list (newer files write
  even one effect as a list), and a new set uses the single key for one and a list for several.
  `effects_block_data` frames a root descriptor as a block payload.
- `Layer::layer_effects`, `Layer::set_layer_effects` and `Layer::clear_layer_effects`. Reading
  uses the block Photoshop treats as authoritative (`lmfx`, then `lfx2`, then `lfxs`). Setting
  edits the existing block in place, or adds a block where Photoshop puts it: after the layer's
  content block and before its vector-mask and name blocks, as `lmfx` for several instances of a
  repeatable effect and `lfx2` otherwise. An existing block keeps its key, because
  Photoshop-authored files hold several instances in `lfx2` as well as `lmfx`.
  Setting a layer's own effects back to what it has changes nothing, including the legacy
  `lrFX` mirror beside the block. A real change rewrites the descriptor block and drops that
  mirror, which Photoshop ignores whenever a descriptor block exists; regenerating it from
  the set follows in a later release. A block that cannot be read is replaced.
- `numModifyingFX`, the tally newer files carry, moves by however much an edit changes the
  number of enabled effects and is never recomputed: Photoshop's own value is not always the
  plain count (a zero-width stroke is enabled and counted as 0).

### Verified

- On all 300 layers with effects in about 420 Photoshop-authored documents, setting a layer's
  own effects back leaves every block of the layer exactly as it was, and every effects block's
  length is a multiple of four, which is how a new block is padded. That sweep showed three
  things the design now accounts for: a no-op is decided on the descriptor, not the block's
  bytes (files pad their blocks differently); an existing `lfx2` may hold several instances; and
  `numModifyingFX` is not always the enabled count.

## [0.6.28] - 2026-09-29

Typed models of every layer effect, the second step towards authoring them. As before, nothing
that reads or writes a document changes: the models read from, patch and build the descriptors
in an effects block, and a later release connects them to layers.

### Added

- `Shadow` (drop and inner, chosen by `ShadowKind`), `Glow` (outer and inner, `GlowKind`),
  `Bevel`, `ColorOverlay`, `Satin`, `GradientOverlay`, `PatternOverlay` and `Stroke`, each a
  struct of `Option` fields that mirrors what its descriptor holds. `from_descriptor` reads the
  fields a file has and leaves `None` for the rest; `apply_to` writes only a `Some` field that
  differs from what the descriptor already says, placing a new item where Photoshop puts it
  and leaving unknown items, their order, their key encodings and any value spelled another
  way but equal exactly as they were; `to_descriptor` builds a fresh descriptor. `Default`
  (`Shadow::new`, `Glow::new` for the two that have variants) is Photoshop's default effect
  with every field set, so a new effect has the full layout Photoshop writes. Percentages are
  the file's 0-100, lengths pixels, angles degrees.
- The enumerators effects use, as public types with a `code()` and a `from_id()` that reads
  both the historical four-character value and the long spelling Photoshop 2026 writes:
  `StrokePosition`, `StrokeFill`, `BevelStyle`, `BevelTechnique`, `BevelDirection`,
  `GlowTechnique`, `GlowSource`, `GradientStyle` (with the stroke-only shape burst) and
  `GradientInterpolation`.

### Verified

- Against all 2,764 effects found in about 420 Photoshop-authored documents, in single form
  and in `*Multi` lists: for every one, a patch that changes nothing changes no bytes, and 2,757
  fresh builds from the fields read reproduce Photoshop's bytes exactly (compared with each
  file's blend-mode spelling normalised, since a model does not keep whether a file wrote
  `multiply` or `Mltp`). The other seven are pre-CS6 files that write items in an older order
  (a colour overlay with its colour last, an inner glow with two items swapped); a fresh build
  uses the modern order, and those two layouts are pinned by name in the test so that any other
  difference fails it. Getting there fixed two things the first run showed: an effect's
  descriptor carries its own key as its class ID (`DrSh`, `ebbl`, `patternFill`), and item
  order differs from that emitted by an independent PSD parser.

## [0.6.27] - 2026-09-29

The groundwork for writing layer effects and adjustment layers: typed values that read from,
patch and build the descriptors those blocks are made of. Nothing that reads or writes a
document changes.

### Added

- `Color`, `Gradient`, `Contour`, `Offset` and `PatternRef`, the values effects, gradient
  strokes and fill layers share. Each reads from a descriptor (`from_descriptor`, `None` when
  the shape is not one it models), builds a fresh one in the layout Photoshop writes
  (`to_descriptor`), and patches an existing one in place (`apply_to`), so unknown items, their
  positions and their key encodings survive an edit. `Color` covers `RGBC`, `CMYC`, `Grsc`,
  `HSBC` and `LbCl` and reads the `GRYC`/`LABC` spellings other libraries use; `Gradient`
  covers custom gradients (colour, foreground and background stops, transparency stops) and
  noise gradients.
- `DescriptorKey::id` spells a key the way Photoshop does (a four-byte ID as a zero-length
  character ID, longer ones explicit, and `warp`, `time`, `hold` and `list` explicit), and
  `Descriptor` gains `with_class`, `set`, `set_text`, `set_ordered`, `set_text_ordered` and
  `remove`. `set` replaces a value in place; `set_ordered` places a new item where Photoshop's
  order puts it, beside the items already there.
- `DescriptorValue::percent`, `pixels`, `angle`, `unit`, `text`, `enumerated`, `boolean`,
  `long` and `double` constructors, and the `UNIT_PERCENT`, `UNIT_PIXELS` and `UNIT_ANGLE`
  constants.
- `UnicodeString::terminated`, a string that ends in a null code unit on disk but not in its
  text, the form `UnicodeString::read` makes of every descriptor name and `TEXT` value.

### Verified

- Against every colour, contour, gradient, offset and pattern-reference descriptor found in
  the layer effects of about 420 Photoshop-authored documents (5,667 in all): all model, a
  patch that changes nothing changes no bytes, and a value built fresh from what was read
  reproduces Photoshop's bytes exactly. Two conventions the first run exposed are now built
  in: every descriptor name and string ends in a null unit (an empty name is one null, not
  none), and replacing a string with an equal one is a no-op. The sweep runs over `fixtures/`
  and, with `PSD_EXTRA_CORPUS`, over more directories.

## [0.6.26] - 2026-09-29

### Fixed

- `psd-png` decoded a 16-bit greyscale PNG with a `tRNS` key to 8-bit RGBA (`to_rgba8`,
  `decode_to_rgba8`) with the wrong pixels transparent: in each block of eight pixels a keyed
  pixel in the first half turned its neighbours transparent instead of itself, and one in the
  second half turned none. Only rows that contain the key were affected. The portable kernel
  introduced in 0.6.25 had it; the parity tests missed it because random rows almost never
  contain a given 16-bit key, and they now plant the key. The document read path never
  reached it: smart-object PNGs go through native RGBA rows or the 16-bit RGBA route, and the
  16-bit-output kernel was correct.

### Changed

- `psd-png` 0.5.x, subtree-synced (`263c5ac`). The decoder's `unsafe` went from 22 blocks to
  one, in the inflate match copy, inside a function that checks its whole range first and is
  sound for any arguments; `#![deny(unsafe_code)]` keeps it the only one. Measured against the
  previous build with alternating runs, inflate is at parity (summed -0.1%, every stream within
  about 2%) and against `fdeflate`, decoding into a preallocated buffer with checksums off, it
  is 1.13-2.2x faster on every fixture.
- The chunk CRC-32 is `crc32fast`'s: carry-less multiplication where the CPU has it, 60-80 GB/s
  against 3.2 GB/s for the crate's own slice-by-16. It runs over every compressed byte of a
  default decode, so it was most of the decode of poorly compressible PNGs: whole-image decode
  over the crate's 16 fixtures is 11.9% faster in total, with 16-bit RGBA -65%, 16-bit RGB -55%,
  noise -54% and 16-bit grey -40%, and well-compressed images unchanged.
  **`crc32fast` is now a permanent dependency of `psd-png`** (MIT OR Apache-2.0, depending only
  on `cfg-if`), which also removes the hand-written aarch64 CRC path that could not be tested
  off that architecture.
- The zlib Adler-32 is a portable vector kernel, 3.4 to 19.4 GB/s on AVX2, replacing a scalar
  loop and a hand-written aarch64 NEON module; it is only computed under `Checks::Full`.
- The decoder's buffer allocation is safe code: it probes with `try_reserve_exact` and then
  allocates with `vec![0; n]`. This is a weaker guarantee than the direct fallible allocation it
  replaces: a refusal the probe sees is still an `OutOfMemory` error, but if the memory is taken
  between the probe and the allocation the process aborts.

## [0.6.25] - 2026-09-29

### Changed

- `psd-png` 0.5.0, subtree-synced (`e8cf5f9`): its SIMD kernels are portable now. The
  hand-written SSE2 `Paeth` filter kernel and every SSE2 conversion kernel were replaced by
  one source written against `fearless_simd`'s portable vectors, compiled at run time for
  SSE2, SSE4.2, AVX2, AVX-512, NEON or wasm SIMD with the scalar paths as the fallback. This
  retires the crate's zero-dependency identity (with approval) and ends the kernels'
  x86-64-only status: the filter and conversion kernels now run on ARM and on the web, which
  never had them. Measured in one harness against the SSE2 kernels they replace: 3.190 vs
  3.215 ms at 3-byte strides, 3.132 vs 3.415 ms at 4-byte strides, 13.058 vs 13.207 ms and
  12.900 vs 13.922 ms at 2048², and 2.6% faster on a real fixture's own filtered bytes;
  against the scalar wavefront, 11% at 3-byte strides and 37% at 4-byte strides. Every kernel
  keeps the old contract (length checks, declines, exhaustive parity tests against the scalar
  helpers) and `PSD_PNG_FORCE_SCALAR=1` still selects the scalar paths in one build. A new
  `paeth_micro` bench bin measures the kernel alone. Note for developers: debug builds run
  the kernels unoptimised, since generic code only inlines under optimisation, so
  `cargo test --release` is the fast path.

## [0.6.24] - 2026-09-28

### Changed

- The ZIP-prediction kernels are now portable-vector (`fearless_simd`) and the
  f32 decode is fused. It used to copy the whole channel and allocate a second
  full-size buffer; it now scans each compressed row straight into a small
  reusable row buffer and transposes from there, so the only large allocation
  is the result. Measured on a 4 MB channel, best of 15: `decode_f32` 2.459 →
  0.411 ms (**5.98x**, now faster than the libdeflate inflate that feeds it),
  `decode::<u8>` 1.40x, `decode::<u16>` 1.10x, and a 32-bit document read end
  to end 11.35 → 8.64 ms (1.31x). The encode kernels measure 1.01–1.02x — the
  write path is deflate- and allocation-bound — and are kept for the code
  shape rather than for speed.
- `psd-codecs` gains `fearless_simd` (and its `#[simd]` attribute crate) as
  dependencies: the portable vectors dispatch at runtime across SSE2/SSE4.2/
  AVX2/AVX-512, NEON and WASM SIMD with a scalar backend, so every platform
  keeps working. The pre-SIMD chains stay behind a `scalar-override` feature
  with `PSD_CODECS_FORCE_SCALAR=1`, so one binary can measure and test the two
  against each other; parity is pinned across vector boundaries and all
  corpora re-verified.

## [0.6.23] - 2026-09-28

### Added

- Experimental: a `fearless` feature on `psd-codecs` that builds a
  portable-SIMD prototype of the f32 ZipPrediction decode
  (`prediction_fearless`), off by default so the shipping build is unchanged.
  Measured against the scalar code on a 4 MB channel: 1.98x on the byte-wise
  prefix sum, 1.47x on the four-plane transpose, 1.12x for the whole call
  (the rest is allocation traffic). The prototype exists to answer whether
  the portable API can express these loops without raw intrinsics — it can —
  and it records that the `#[simd]` attribute is mandatory in practice: the
  same kernel measured 0.42x without it. Parity with the scalar decode is
  pinned by tests across vector boundaries.

## [0.6.22] - 2026-09-28

### Fixed

- Python bindings: a 1-bit (bitmap mode) file reports `BitDepth.bd_1` from
  `PhotoshopFile.find_bitdepth` instead of the 32-bit fallback, and the
  `BitDepth` enum gains its `bd_1` member (upstream's `BD_1 = 0`).

### Added

- Python bindings: `LayeredFile_*.source_depth` reports the file's on-disk
  depth, which differs from `bit_depth` only for 1-bit documents (they read
  as 8-bit ones, and saving writes 8-bit).

## [0.6.21] - 2026-09-28

### Added

- 1-bit (bitmap mode) documents read as 8-bit ones: the packed pixels are
  expanded during channel decode — eight pixels per byte, MSB first, with the
  inked (set) bits black and a row stride of `ceil(width / 8)` — and saving
  writes 8-bit, since the port has no sample type for packed bits. The
  document reports its on-disk depth as `LayeredFile::source_depth`, so a
  caller can see that a conversion happened. All three bitmap fixtures in the
  reference corpus now pass the read, views, roundtrip and stable checks
  (302 of 302).

### Fixed

- A 1-bit channel body that ends mid-row degrades to black padding — all-ones
  bytes, since zero bytes would paint white — rather than failing the read.

## [0.6.20] - 2026-09-28

### Changed

- A channel whose stream does not decode is replaced with a zero-filled
  channel of its declared size instead of failing the document: one corrupt
  channel costs that channel, not the file. This is the recovery a mature
  reader has shipped for years (corrected once so the substitute matches the
  channel's byte count). An unknown compression marker still fails at the
  header, so only damaged data of a known codec is covered, and the raw
  bytes remain reachable through the raw-channel read option.
- An unreadable layer-effects block — or a single effect inside one whose
  value is not the object the format requires — is skipped with a warning
  rather than failing the call. The raw block stays the write source, so
  nothing is lost and a file renders without that one effect.

## [0.6.19] - 2026-09-28

### Fixed

- A layer-mask block's reverse-ordered second header now follows the record's
  `-3` (real user mask) channel instead of the block length. A parameter
  block that pushes the record past 36 bytes used to be read as that header —
  turning a 22.8 px feather into a bogus rectangle — or the parameters were
  dropped entirely; they are now read whenever a mask flag asks for them.
- A channel declaring a length of one — malformed, the spec allows zero or at
  least two — no longer fails the file: the stray byte is consumed so the
  channels after it stay aligned, and the channel reads as empty.
- Truncated mask parameters (a flag byte promising more than the block holds)
  keep the fields that fit and warn, instead of failing the whole read.
- A `ResolutionInfo` image resource smaller than its fixed payload is kept
  raw instead of failing the file.
- A layer whose channels all carry no payload reads with degenerate bounds —
  an empty gradient-fill layer with a `0 x -1` rectangle — instead of being
  rejected.
- A layer-and-mask tail shorter than a mask length no longer underflows; the
  padding is consumed and the section ends cleanly. This was a reachable
  panic.
- Group records keep whichever pass-through spelling the document uses:
  `pass` on the record with a type-only `lsct`, or `norm` on the record with
  the mode on a longer `lsct`. The writer used to upgrade the short form,
  changing files it round-tripped.

### Changed

- One-bit (bitmap mode) documents keep a clear error that names the reason:
  their pixels are packed per byte, a layout this port has no sample type
  for, and the C++ upstream has the same limitation. Tracked as a future
  feature.

## [0.6.18] - 2026-09-28

### Fixed

- Files whose layer-and-mask section declares length zero — written without a
  layer section — read again; the reader used to parse into the image data
  that follows. A section that ends right after the layer info (no global
  layer mask info and no document-level blocks) is accepted too, the mask
  info is optional, and a mask length that runs past the section is clamped
  rather than rejected. Some writers also declare a layer-info or
  record-extra length a couple of bytes longer than its content; the tail is
  then read from where the content actually ended — a non-zero gap means the
  next section starts there, padding is zeros — which is what makes those
  files' channel data and trailing tagged blocks line up. Found by
  cross-validating against an older third-party parser's fixture set, whose
  reader walks structurally instead of trusting declared lengths.

## [0.6.17] - 2026-09-28

### Fixed

- `psd-companion`: pattern records now read their channel slots
  positionally — the first slots are the colour planes and the slot after the
  declared channel count is the transparency plane, as the format defines it.
  The previous present-channel counting could mistake a present user-mask slot
  for the transparency plane. A present slot that is neither (the user-mask
  slot among them) is now length-checked and skipped without decoding, so a
  malformed unused plane can no longer fail the record or force a large
  decode.
- `psd-companion`: a multichannel pattern record (image mode 7, the CS-era
  bevel-texture presets) decodes as grayscale instead of being refused;
  Photoshop reads its single plane exactly like grayscale. A written slot
  declaring a zero or impossible length is also handled: zero means the slot
  carries nothing and a length below the header size is rejected.

## [0.6.16] - 2026-09-28

### Fixed

- `psd-companion`: a pattern record that carries a transparency plane now
  uses it as the pattern's alpha instead of writing every pixel opaque. Real
  Photoshop patterns can have transparent pixels, and the plane is the
  record's own fourth channel; the parser this reader was modelled on ignores
  it. Found by cross-validating the decoder against a real-world `.pat`
  fixture whose pixels are pinned by a second implementation — the three
  opaque pixels matched before the fix and the semi-transparent one did not.
  Pinned by a hand-built fixture test.

## [0.6.15] - 2026-09-28

### Fixed

- `psd-companion`: the brush-dynamics `bVTy` control table now follows
  Photoshop's own order (`0 off, 1 fade, 2 pen pressure, 3 pen tilt,
  4 stylus wheel, 5 rotation, 6 initial direction, 7 direction`). The table
  this reader inherited from a widely used parser library orders the values
  differently from index 5 on, which silently renamed the angle controls
  (`rotation` read as `initial direction` and so on). Pinned against a
  Photoshop export whose every dynamic was set to a distinct value, and
  confirmed by two independent format references. Cross-validated against
  that reference implementation's own test expectations, including a
  pixel-level brush-mask checksum, which now all match.
- A `-3` (real user mask) channel whose payload does not decode — the
  compression-marker-only records older Photoshop files carry — is kept raw
  instead of failing the read. Photoshop ignores that plane's payload; the
  `-2` rendered mask, which Photoshop does read, stays strict. Pinned by
  `empty_real_user_mask_channel_does_not_fail_the_read`.

## [0.6.14] - 2026-09-28

### Fixed

- Per-layer tagged blocks now declare an even length, with an odd payload's
  pad byte inside the declared length. Photoshop advances by the declared
  length rounded up to an even count, so an odd declared length made it walk
  one byte past the block and read the rest of the layer record as unknown
  data. Photoshop's own files never declare an odd length (a scan of 8,245
  per-layer blocks across the test corpora shows none), so every even-length
  block an input carries stays byte-identical; the change closes the hole for
  odd payloads an editor could introduce. The document-level block list keeps
  its four-byte alignment outside the declared length, which is what
  Photoshop does there.
- Image channels decode damaged RLE scanlines the way Photoshop does: an
  overrunning packet is clipped to its row, a stream that ends early leaves
  the rest of the row at zero, and each row is still positioned by its own
  declared size, so one damaged row cannot desync the rows after it.
  Previously a corrupt scanline failed the whole read, rejecting legacy files
  Photoshop opens. The strict exact-length decoder remains the contract for
  every other payload (patterns, brushes, this crate's own round trips).
- A zero-length channel — how old Photoshop writes the channels of an empty
  layer, with no payload and no compression marker — reads as an empty
  channel instead of an error.
- Authored pixel records match Photoshop's invariants. A new image layer
  carries layer-record flag bit 3 (every Photoshop pixel record does, and a
  record without it gets legacy semantics), and an image or text layer
  written without a transparency channel gains an all-opaque one unless it is
  the bottom record covering exactly the canvas. Photoshop reads a pixel
  record with no transparency channel as its Background layer — opaque over
  the whole canvas whatever its bounds say — so a floating RGB-only layer
  used to hide everything beneath it.

## [0.6.13] - 2026-09-28

### Fixed

- `luni` roundtrip on files that write the layer-name block unpadded.
  Photoshop pads the block to four bytes; some other editors do not. The
  name-comparison read assumed the padded form, so an unpadded block failed to
  parse, looked like a rename, and was rewritten two bytes longer on every
  save. The read now takes the code-unit count as the whole payload, and a
  changed name still writes Photoshop's padded spelling. Found by sweeping a
  third-party editor's public corpus of 172 real PSD/PSB files through the
  corpus harness (`PSD_EXTRA_CORPUS`): 169 passed before, all 172 now pass the
  read, views, roundtrip and stable checks.
- Float paragraph properties (`FirstLineIndent`, `StartIndent`, `EndIndent`,
  `SpaceBefore`, `SpaceAfter`, `Zone`, `AutoLeading`) always carry a decimal
  point when set through the typed setters. Photoshop reads a bare integer
  token for these keys as 16.16 fixed point — `SpaceBefore 24` reads back as
  0.000366 px — so setting a value on a file whose old token was an integer
  silently lost it. Pinned by
  `paragraph_float_setters_always_write_a_decimal_point`.
- `psd-companion`: the `.abr` sample rows are a `u16` length table followed by
  the row data, not interleaved rows, and the pattern channel rows follow the
  same framing; section padding rounds the length up to the next four-byte
  boundary instead of adding `size % 4`. Both were caught by real
  Photoshop-written brush files from a third-party corpus — the hand-built
  fixtures had encoded the reader's own mistakes, so they agreed with it. The new
  `tests/real_fixtures.rs` sweeps a directory named by
  `PSD_COMPANION_FIXTURES`, and the reader now asserts every sampled preset
  references a sample the file carries by UUID (Photoshop does not keep the
  two lists in a matching order).

## [0.6.12] - 2026-09-28

### Added

- The `psd-companion` crate: readers for the small Adobe formats that travel
  beside PSD files — `.abr` brush presets, `.csh` custom shapes and `.ase`
  swatch palettes. All three are read-only over a byte slice, following the
  workspace's ag-psd port as the format reference (none of the three has an
  official Adobe specification). The crate reuses `psd-core`'s machinery
  instead of carrying its own: `.csh` paths parse through the same
  `VectorPath` records a layer's vector mask does, and `.abr` presets are PSD
  action descriptors read through the shared descriptor parser, with a typed
  brush model (computed/sampled/tips/dynamic shapes, dynamics, texture, dual
  brush, tool options) over it. `.abr` major versions 1 and 2 — the pre-CS
  entry-stream layout, for which no fixture or reference exists here — are
  rejected with a named error rather than decoded from guesswork, as are
  16-bit run-length-encoded samples and run-length-encoded indexed patterns.
  Fixture suites are hand-built through `psd-core`'s own writers: 20 tests
  covering every colour model, group nesting, both sample compressions, the
  pattern channel list, and the malformed-input rejections. (The formats
  live in their own crate and do not expand the PSD ones.)

## [0.6.11] - 2026-09-28

### Changed

- The vendored `psd-png` is synced to its 0.4.0: SIMD conversion kernels behind the
  decoder's existing dispatch layer, each claimed only where it measured faster than the
  autovectorised scalar loop and pinned to it by exhaustive parity tests. The shapes the
  port's fallback path consumes benefit most: a palette PNG decoded through
  `decode_to_rgba16` pays 56–60 % less conversion, a 4-bit palette 30–33 % less, and a
  16-bit RGBA source narrowed to 8-bit output 15–19 % less. Shapes where the kernel lost to
  the scalar loop — sub-byte greyscale, gray8→rgba16, equal-width RGB — declined and stay on
  the unchanged scalar path. No public `psd` API changes.

## [0.6.10] - 2026-09-25

### Changed

- The vendored `psd-png` is synced to its 0.3.0: the crate is now a decoder only. Its PNG
  encoder, the DEFLATE compressor under it, `FilterStrategy`, `WriteError` and the
  ancillary-chunk retention (`Keep`, `Info::metadata`) are removed — the workspace consumes
  the decode path, and the vendored surface no longer carries what it never calls. Its
  decode suite, the corpus sweep and the zlib vectors are unchanged; its fixtures are now
  built by hand rather than by the encoder that used to sit opposite them.
- The PNG decode's decompressed-size ceiling is now stated as the planar raster budget
  (`MAX_RASTER_BYTES`) rather than the encoded-source cap. The two carry the same value
  today, so behaviour is unchanged; the decode now has one budget — the raster the port
  builds — instead of two coincidentally equal policies.

## [0.6.9] - 2026-09-25

### Changed

- PNG Smart Object rasters now decode through the vendored `psd-png` codec instead of the
  `image` crate. The decode streams rows straight into the planar channels — no whole decoded
  image is ever materialised — so a decode's largest allocation is a bounded ~0.29 MiB stage
  (independent of image size) plus the planar output, against the `image` crate's whole-image
  buffer. On this repository's corpus, interleaved benchmark sessions with a live control put
  the new path at **−55.0 %** wall time against the old one (2.2× faster). JPEG keeps decoding
  through the `image` crate, unchanged.
- The whole-image 8-bytes-per-pixel preflight no longer applies to PNG, because no whole-image
  decoder buffer exists to budget: the cap now applies to the planar raster the port builds
  (4 samples of `T` per pixel against the 512 MiB limit) and to the decoder's bounded stage.
  An 8-bit document therefore accepts PNGs up to twice the old pixel budget, and a 32-bit-float
  document is capped at its true 16 bytes per pixel, which the old path never enforced on its
  own output.
- `psd-png` is an optional dependency behind the `image` feature rather than a dev-dependency;
  a build without that feature compiles and reports the same error as before.
- `psd-png` validates PNG chunk CRCs by default, so a corrupted-CRC file is rejected by either
  decoder; the failure message wording now comes from `psd-png`.

## [0.6.8] - 2026-09-25

### Added

- `BitDepth::widen_eight` and `BitDepth::widen_sixteen`, the widening rules a PNG-native
  sample passes through on its way into a channel at each of the three depths. Their cheap
  exact forms (identity, ×257, and the `round(v / 257)` narrowing which is deliberately not
  the high byte) are pinned exhaustively — all 256 bytes and all 65536 `u16` values — against
  the `T::from_f32(sample.to_f32())` composition `interleaved_to_planar` computes, so a
  native-rows decode and the current conversion route provably produce the same bytes.
- The smart-object PNG streaming helper (dev-only, behind the `image` feature) now dispatches
  non-interlaced RGBA sources to `psd-png`'s `decode_to`: 8-bit sources take four plain
  samples per pixel and 16-bit sources four big-endian reads, instead of the previous route
  that widened every source to 16-bit RGBA and narrowed it back through a float round-trip.
  Gray, palette, RGB, sub-8-bit and interlaced sources keep that converted route, whose
  grayscale-as-neutral-RGB and `tRNS` semantics the differential tests pin. Nothing in the
  production read path calls this yet, so decoded output is unchanged; the route is measured
  by an ignored benchmark that now runs five arms — `image` / parity / streaming-native /
  streaming-rgba16 / control — with the native arm at **−54.4 %** against the `image` crate
  (2.2× faster, largest allocation 0.29 MiB against 1.00 MiB, control noise +1.6 %).

## [0.6.7] - 2026-09-25

### Added

- `crates/psd-png`, a vendored fork of the png-spark PNG codec, is now a workspace member. It
  is a **dev-dependency only** at this point: nothing in the read path uses it yet. What it
  brings is a differential test that decodes every PNG in the corpus through both it and the
  `image` crate, at `u8`, `u16` and `f32`, and requires the two to agree byte for byte, plus
  coverage for the greyscale and 16-bit cases the corpus does not contain.
- A test pinning how a 16-bit source is narrowed into an 8-bit document. The port narrows by
  `round(v / 257)`; `psd-png` keeps the high byte, and the two disagree by one
  least-significant bit across much of the range. Every PNG in `fixtures/` is 8-bit, so no other
  test would notice a decoder swap that changed this — the test fails if the swap is made.

### Changed

- The workspace's third-party code is now recorded in `LICENSE`, and the dependency ladder in
  `README.md` lists the vendored crate. `crates/psd-png` is MIT OR Apache-2.0 with its original
  copyright retained, so it is not covered by this project's BSD 3-Clause terms.

## [0.6.6] - 2026-09-24

### Added

- A corpus regression test runs every document under `fixtures/` through
  named checks:
  - read;
  - every on-demand view;
  - write-and-reread equality of header fields, layer metadata, tagged
    blocks, and decoded pixels;
  - byte stability across a second save.

  Failures are compared with a pinned list. A new failure and an unexpected
  pass both fail the suite by name. The list is empty today. Setting
  `PSD_EXTRA_CORPUS` sweeps another directory with the same checks.

### Fixed

- The section-divider record written for a new group now carries the `luni`
  name block that Photoshop writes. Before this fix, re-saving a document read
  back from such a save added the block, so the two saves differed.

## [0.6.5] - 2026-09-24

### Added

- Artboards can be read. `Layer::is_artboard` and `Layer::artboard` expose a
  group's artboard block (`artb`, `artd`, or `abdd`): its rectangle, preset
  name, background type and color, and guide indices.
  `LayeredFile::artboards` lists the artboard groups, and
  `LayeredFile::artboard_settings` reads the document-level `artd` tool
  settings.
- Artboards stay group layers in the layer tree. Every group operation applies
  to them, and saving writes their blocks unchanged. Upstream also round-trips
  artboards as groups with opaque data.
- A generated test corpus in `fixtures/generated/Artboards` covers two
  artboards, a nested group, a plain group, and the document settings, in
  8-bit PSD and PSB.

## [0.6.4] - 2026-09-24

### Added

- `BlendMode::from_descriptor_enum` decodes the descriptor blend modes that
  layer effects and vector strokes store. It accepts both the historical IDs
  (`Mltp`, `linearBurn`) and the string IDs that Photoshop 2026 writes instead
  (`multiply`, `colorBurn`). `BlendMode::to_descriptor_enum` returns the
  historical ID, which every Photoshop version reads. Effect and stroke views
  add `blend_mode_value()`.
- `Warp::is_vertical` reports a vertical warp in either spelling of the
  rotation.

### Fixed

- A text-warp rotation stored as the long-form string ID `horizontal` or
  `vertical` now reads as horizontal or vertical instead of an unknown value.
  The fix applies to Rust and to Python's `warp_rotation`.
- If a text layer's `Ornt` value used the long form, changing its orientation
  wrote `Hrzn`/`Vrtc` as an explicit-length string ID, which Photoshop does
  not define. It now always writes the zero-length char ID.
- The smart-object warp's `set_warp_rotate` accepts the long forms. A char ID
  that replaces a long-form value is written with the zero-length encoding.
  Warp descriptors built from scratch now write `Ornt` and `Hrzn`/`Vrtc` as
  zero-length char IDs, as upstream does, instead of as explicit-length string
  IDs.

## [0.6.3] - 2026-09-24

### Added

- Layers expose read-only typed views of vector data:
  - Vector masks (`vmsk`, and `vsms` from Photoshop CS6 on) expose their
    invert, unlink, and disable flags.
  - Paths expose typed records: subpaths with their shape operation and
    origination index, Bézier knots with pixel conversion, and fill-rule and
    clipboard records.
  - Shape stroke styles (`vstk`), CS6 shape fills (`vscg`), and live-shape
    parameters (`vogk`) expose shape type, bounds, corner radii, and
    transform.

  Unknown path records, descriptor fields, and trailing bytes remain
  available, and saving still writes the original blocks.
- `Layer::vector_blocks`, `Layer::vector_mask`, and `Layer::is_shape_layer`
  read a layer's vector data. A shape layer has both a vector mask and a fill,
  so a pixel layer with a vector mask is not a shape. Upstream classifies any
  layer with vector origination, mask, or stroke data as a shape, but only
  after its adjustment check has already claimed every `SoCo` layer.
- `LayeredFile::document_paths` reads the work path and the saved paths from
  the image resources, including each saved path's Unicode name from the
  document's `pths` block.
- A generated test corpus in `fixtures/generated/Vectors` covers legacy and
  CS6 shape layers, a compound path, an open path, a vector-masked pixel layer,
  and document paths, in 8-bit PSD and PSB.

### Changed

- The generated adjustment fixtures mark their pixel data as irrelevant, as
  Photoshop does for adjustment and fill layers.

## [0.6.2] - 2026-09-24

### Added

- Layers expose read-only typed views of adjustment and fill layer settings:
  brightness/contrast, levels, curves, exposure, vibrance, hue/saturation
  (current and Photoshop 4.0 keys), color balance, black & white, photo
  filter, channel mixer, color lookup, invert, posterize, threshold, gradient
  map, selective color, and solid, gradient, and pattern fills. The `CgEd`
  companion data exposes modern brightness/contrast values and preset names.
  Upstream only detects these layers and round-trips them as opaque data.
- `Layer::is_adjustment_layer` reports whether a layer carries one of these
  settings blocks. Shape layers also report `true`, because Photoshop stores
  their fill in the same blocks.
- Views keep what they do not interpret: newer descriptor fields, the extra
  records after the legacy levels and curves sections, and trailing payload
  bytes. Saving still writes the original tagged-block bytes. A malformed or
  unsupported payload returns an error only from the view that reads it.
- `RawColor`, the specification's 10-byte color structure, is shared by
  adjustment views and legacy effects. `LegacyEffectColor` is now an alias for
  it.
- A generated test corpus in `fixtures/generated/Adjustments` covers every
  adjustment and fill kind except pattern fill, in 8-bit PSD and PSB and 16-bit
  PSD. `cargo run -p psd --example generate_fixtures` regenerates it.

## [0.6.1] - 2026-09-24

### Added

- Layers expose read-only typed views of modern `lfx2`/`lmfx`/`lfxs` and legacy
  `lrFX` effects, including drop shadows, glows, bevels, overlays, satin, and
  strokes. Multi-effect lists and unknown descriptor fields remain accessible.
- Legacy effect records expose common settings and retain unknown record payloads.
  Saving still writes the original tagged-block bytes, including unsupported
  effect fields.

## [0.6.0] - 2026-09-24

### Added

- Rust readers can retain layer and mask channels in their original compressed
  form, decode one channel or one layer on demand, and write untouched payloads
  without decoding them.

### Changed

- `ReadOptions` adds `use_raw_data`. Existing eager reads remain the default;
  callers using exhaustive struct literals must now provide the new field or
  use struct update syntax with `ReadOptions::default()`.
- A channel store distinguishes decoded pixels from raw payloads. Pixel access
  returns no samples for a raw-backed key until that channel is decoded.

### Fixed

- Duplicate layer channel IDs now return a typed error instead of silently
  replacing an earlier payload, as the C++ layer map does.
- Raw 32-bit ZIP channels must be decoded before writing, so the writer can
  emit ZIP prediction as Photoshop expects.

## [0.5.3] - 2026-09-24

### Added

- Rust document readers accept a cumulative decoded-channel memory limit. Reads
  default to 2 GiB; callers can set a smaller limit or explicitly choose
  unlimited decoding.
- Python `read` and `from_bytes` accept `memory_limit`: omit it for the Rust
  default, pass `0` for unlimited decoding, or pass a positive byte limit.

### Fixed

- Invalid or over-limit layer and mask extents now return typed errors before
  channel allocation. Empty `0×−1` vector-mask placeholders stay zero-area to
  preserve compatibility with existing Photoshop documents.

## [0.5.2] - 2026-09-24

Repository hygiene ahead of the first public push. No behavior, API or output
change.

### Added

- A BSD-3-Clause `LICENSE`, retaining Emil Dohne's copyright for the original
  C++ library and extending it for this port.
- A `README.md` covering what the port does, how it differs from upstream, the
  supported feature set, and the full dependency list with licenses. Every
  dependency in the resolved graph (116 crates with `--all-features`) is
  permissively licensed; there is no GPL, AGPL, SSPL, EUPL or MPL.
- Continuous integration now also runs the all-features and no-default-features
  test configurations, `clippy` with `--all-features`, rustdoc with warnings
  denied, and the Python binding test suite (213 tests) via maturin and pytest.

### Changed

- Source comments no longer cite an internal operating manual. Where a comment
  explained a deliberate difference from upstream, the explanation is unchanged
  and now stands on its own; where the citation carried no information, it was
  removed. Comments reference upstream file names and the Adobe PSD/PSB
  specification only.
- Internal working records (the deferred-decision register, benchmark and
  dependency evaluations, and the implementation audit trail) and local
  agent-tooling state are no longer tracked. They remain on disk for the
  maintainers but are not published.
- Python bytecode caches, wheel output and local tool caches are no longer
  tracked.

## [0.5.1] - 2026-09-24

A review of the smart-object and render engine (Phase 2).

### Added

- Grayscale and grayscale+alpha JPEG/PNG smart-object sources (8- and 16-bit)
  are placed as neutral RGB, as Photoshop does. They were rejected before.
  Upstream keeps only the gray plane, as red.
- Linked PSD/PSB sources with more than four channels are accepted. Channels
  beyond RGB and transparency are ignored.

### Changed

- The Python distribution (`photoshopapi-rs`) takes its version from the Cargo
  workspace instead of a separate value in `pyproject.toml`.

### Fixed

- Warp renders were misregistered by up to ~0.75 px. Pixels were sampled
  relative to the fractional mesh bounds but placed at the rounded output
  origin (as upstream does). Renders now sample where the pixels are placed.
- Warp supersamples were anchored at each pixel's top-left corner, which
  shifted renders by `1/(2·supersample)` px up and left. They now sit at the
  centers of the subpixel grid. With the fix above, the error against
  Photoshop's reference renders drops by about 10%.
- `Raster::rescale` with bilinear or bicubic filtering shifted the image by
  half an output pixel (so even a same-size resize blurred) and darkened the
  top and left edges when enlarging. It now maps pixel centers and repeats edge
  pixels.
- `Homography::between` could return a singular transform for a degenerate
  destination quad. It now returns an error.
- `Homography::between` failed for a perspective whose vanishing line passes
  through the canvas origin. It now solves in centroid-relative coordinates.
- `Warp::no_op` reported every newly generated quilt warp as deformed, because
  Photoshop pads its default quilt slices by 0.6 px.
- A linked PSD/PSB's fourth composite channel was always read as
  transparency, so a saved selection cut holes in the placed image. It is now
  transparency only when the file marks it so (negative layer count).
- Created documents with transparent layers were written with an extra
  composite channel but a positive layer count, which Photoshop lists as an
  "Alpha 1" channel. The channel is now marked as merged transparency.

## [0.5.0] - 2026-09-24

First versioned state of the port: roadmap Phases 1–4 are
complete.

### Added

- `psd-core`: read/write of all five PSD/PSB file sections, with 2/4/8-byte
  length widths chosen by `Version`. Covers 8/16/32-bit data, including layer
  data nested in `Lr16`/`Lr32`, plus Pascal/Unicode strings and descriptors.
  Unknown tagged blocks and image resources are kept byte-exact, and typed
  views cover placed-layer (`PlLd`, `SoLd`/`SoLE`) and linked-layer
  (`lnk2`/`lnkD`/`lnkE`/`lnk3`) blocks.
- `psd-codecs`: PackBits RLE, ZIP (libdeflate) and ZIP-with-prediction
  (including 32-bit float byte de-interleaving), bulk endian swaps, and
  planar/interleaved shuffles, parallelized with rayon.
- `psd`: `LayeredFile<T>` for `u8`/`u16`/`f32` documents in RGB, CMYK and
  Grayscale.
  - The layer tree is an arena with stable `LayerId`s and Photoshop-style path
    lookup, and supports insert, remove and move with group dividers kept
    paired.
  - Image, group and section-divider layers carry planar channels, pixel and
    group masks, and blend mode, opacity, fill, lock, clipping and display
    color.
  - Also: ICC profile and DPI, per-document and per-layer write compression,
    and progress callbacks.
- Smart objects (Phase 2):
  - Warps: typed normal/quilt warps that read, edit and write back to
    `PlLd`/`SoLd`.
  - Geometry: Bézier surfaces, quad meshes and homographies.
  - Rendering: supersampled warp rendering, RGB compositing and raster
    resampling.
  - Sources: JPEG/PNG decoding (the `image` feature) and PSD/PSB (Raw/RLE)
    composites.
  - Documents: creating, replacing and transforming smart objects, with
    embedded or external storage.
- Text layers (Phase 3): an EngineData parser that edits by splicing raw bytes;
  text, run, font, orientation, shape, box, transform, warp, and
  style/paragraph property editing; range edits; building text layers from
  scratch; and a document text cache that detects staleness.
- Python bindings (Phase 4): the `photoshopapi` package (PyO3 + maturin)
  mirrors upstream's pybind11 surface, with NumPy interop and IDE stubs. All
  184 upstream `psapi-test` cases pass.

### Changed

Deliberate differences from upstream behavior:

- Integer samples round to the nearest value when converted from floats.
  Upstream truncates.
- Moving a layer moves its pixel mask and a text layer's `TySh` anchor.
  Upstream moves only the layer.
- A vector-only mask is not reported as a pixel mask. With both masks present,
  the pixel mask is channel `-3`, as Photoshop lays it out.
- `luni` is written without a trailing null, as Photoshop spells it.
- Python layer constructors default to the automatic per-depth compression
  instead of forcing ZIP-with-prediction. An explicit value is still honored.
- Smart-object edits re-render immediately. Upstream renders lazily.
- Text edits spanning several `TySh` blocks, and failed range edits, are
  all-or-nothing. Paragraph ranges widen to whole paragraphs, and runs are
  remapped per paragraph when a paragraph break is added or removed.
- Writing detects stale text caches automatically. Upstream requires an
  explicit `invalidate_text_cache()`.

### Fixed

Compared with upstream:

- `PlLd` blocks in PSB files use the 4-byte length that the reader expects, so
  placed layers survive a PSB round trip.
- Unknown image-resource blocks, unknown layer-flag bits and the negative
  layer count (merged transparency) are preserved instead of dropped.
- The merged channel count includes alpha. Upstream's `hasAlpha &=` never
  sets it.
- Mask channel `-2` takes its bounds from the vector mask when one exists.
- Smart-object `rotate` takes degrees as documented. Upstream passes the value
  to `cos`/`sin` as radians.
- `reset_transform` resets both corner quads, including after perspective
  edits.
- Replacing a smart-object source repeatedly at the same size no longer
  compounds the warp scaling.
- `Warp::no_op` also checks the Bézier control net, so a curved warp with
  untouched corners is not reported as a no-op.
- Python `ChannelID.Black` addresses CMYK channel index 3. Upstream maps it to
  index 2.
