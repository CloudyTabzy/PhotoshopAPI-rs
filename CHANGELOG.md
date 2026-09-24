# Changelog

All notable changes to PhotoshopAPI-rs are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html).
Until 1.0.0, a breaking change to the public Rust or Python API bumps the minor
version and any other change bumps the patch version.

"Upstream" means the C++ [PhotoshopAPI](https://github.com/EmilDohne/PhotoshopAPI)
v0.9.1 that this project ports.

## [Unreleased]

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
