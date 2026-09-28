# Changelog

All notable changes to PhotoshopAPI-rs are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html).
Until 1.0.0, a breaking change to the public Rust or Python API bumps the minor
version and any other change bumps the patch version.

"Upstream" means the C++ [PhotoshopAPI](https://github.com/EmilDohne/PhotoshopAPI)
v0.9.1 that this project ports.

## [Unreleased]

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
  pattern channel list, and the malformed-input rejections. (Resolves the
  scope decision in TODO item 8: the formats live in their own crate and do
  not expand the PSD ones.)

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
