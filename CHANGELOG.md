# Changelog

Releases of `psd-png` follow after the fork point. The versions it inherited from png-spark are
listed under "Inherited from png-spark" at the end. The package is `publish = false` and carries
no `repository` URL, so no version headings carry compare links.

## [Unreleased]

### Fixed

- **16-bit greyscale with a `tRNS` key, decoded to RGBA8, made the wrong pixels
  transparent.** The portable kernel for that shape gathered each pixel's alpha from the
  compare mask one byte per pixel, but the 16-bit compare leaves two bytes per pixel, so
  output pixel *n* of every eight took the mask of pixel *n* / 2: a keyed pixel in the first
  half of a block turned pixels 2*j* and 2*j* + 1 transparent instead of itself, and one in
  the second half turned none. Only rows that contain the key were affected, and only where
  the kernel runs (not under `PSD_PNG_FORCE_SCALAR=1`, on bare SSE2, or on the scalar
  backend). The 0.5.0 port introduced it; the SSE2 kernel it replaced was correct. The
  parity tests missed it because random rows almost never contain a given 16-bit key; they
  now plant the key, and fail on the old code on every backend.

### Changed

- The parity tests run every kernel once per backend the machine supports, not only the
  one `dispatch!` reaches: on an AVX2 machine the SSE2 and SSE4.2 lowerings of the same
  source were never executed. A test cross-checks the backend list against the standard
  library's CPU detection, so a backend cannot drop out and leave its tests vacuous, and
  the conversion tests now expect the same backends the facade routes to.
- Big-endian targets take the scalar path. The kernels bitcast between lane widths and read
  a big-endian PNG sample as a native lane, which assumes little-endian lane layout.
- The `Rgb` → `Rgba` widening and the indexed copy decline, instead of reporting a row
  converted, when the target is not a whole number of pixels or (indexed) the channel count
  is not 3 or 4. Callers never pass either today.
- The facade and the kernels take the backend from one `Level::new()` value rather than a
  private cache beside `fearless_simd`'s own.
- Documentation states the crate's dependencies as they are: `fearless_simd` and the
  build-time `#[simd]` macro crate, not `fearless_simd` alone, and not "nothing outside the
  standard library".

## [0.5.0] - 2026-09-29

### Changed

- **The SIMD kernels are portable.** The hand-written SSE2 `Paeth` kernel and the SSE2
  conversion kernels were replaced by one source written against `fearless_simd`'s portable
  vectors, compiled at run time for SSE2, SSE4.2, AVX2, AVX-512, NEON or wasm SIMD with the
  scalar paths as the fallback. This gives the crate two dependencies, `fearless_simd` and
  its `#[simd]` macro crate, and it ends the
  kernels' x86-64-only status: the filter and conversion kernels now run on ARM and on the
  web, where every target previously took the scalar path. Measured in one harness against
  the SSE2 kernels they replace: 2–8% faster on `Paeth` workloads; against the scalar
  wavefront, 11% at 3-byte strides and 37% at 4-byte strides. Every kernel is still pinned to
  the scalar helpers by the same parity tests, and `PSD_PNG_FORCE_SCALAR=1` (with the
  `scalar-override` feature) still selects the scalar paths in one build. Two backends
  deliberately decline to the scalar path: the scalar fallback, where generic code runs a
  lane at a time, and the bare SSE2 level for the shuffle-built conversion kernels, where a
  dynamic byte shuffle is emulated per lane.

### Notes

- Debug builds run the kernels unoptimised: generic code only inlines under optimisation,
  where the old `core::arch` intrinsics emitted real instructions either way. The
  conversion-heavy test suites are correspondingly slower under a plain `cargo test`; every
  number in `docs/benchmarks.md`, and `cargo test --release`, are the fast ones.

## [0.4.0] - 2026-09-28

### Added

- SIMD conversion kernels behind the F1 dispatch layer (F2). `RowConverter::convert` now
  offers the accelerated paths a row before the scalar helpers run, and every decline falls
  through untouched. Claimed, each pinned to the scalar helpers by exhaustive parity tests
  and measured faster than the autovectorised scalar loop: palette expansion at depth 8
  (both output widths, `tRNS` alpha folded into the resolved table), palette sub-byte
  expansion for 16-bit output, greyscale 8→RGBA8 and 16→RGBA8/16 with `tRNS` keys,
  greyscale-alpha at both depths and output widths, RGBA 8→16 and 16→8, and keyless RGB
  widening as portable scalar. The 16-bit palette table is pre-expanded
  (`[r, r, g, g, b, b, a, a]` per entry), so a 16-bit conversion is one fixed-size copy per
  pixel. Measured, conversion-only, against the scalar helper each kernel replaces:
  palette8→rgba16 0.16×, rgba16→rgba8 0.25×, graya8→rgba16 0.38×, gray8→rgba8 0.48×,
  rgba8→rgba16 0.51×, palette8→rgba8 0.68×. End to end on the 1920×1080 fixtures:
  palette8 rgba16 −56…−60 %, palette4 rgba16 −30…−33 %, rgba16→rgba8 −15…−19 %.

### Changed

- Shapes where the kernel measured slower than the autovectorised scalar loop declined
  instead: every sub-byte greyscale target, gray8→rgba16, rgb8→rgba8, rgb16→rgba16 and
  palette sub-byte→rgba8. The scalar helpers keep them, unchanged. A first indexed kernel
  with a runtime output stride measured 2× slower than scalar — a runtime-length
  `copy_from_slice` is a `memcpy` call per pixel — and was replaced by a const-generic
  fixed-size copy before anything shipped.
- The `stream_decode` bench reuses its output buffers across timed runs. A fresh 8.3 MB
  `Vec` per iteration paid page faults scaled to the output size, which masqueraded as
  conversion cost and flipped the sign of the palette8 comparison.

## [0.3.0] - 2026-09-25

### Removed

- The PNG encoder, the DEFLATE compressor under it, `FilterStrategy`, `WriteError` and the
  ancillary-chunk retention (`Keep`, `Info::metadata`, `Info::chunk`, the public `Chunk`).
  The port decodes PNG rasters and never writes one, so the crate now carries only what a
  reader needs, and the public surface no longer names anything the decode path never
  touched. With it went the encode half of `filter.rs` (the forward filters live on as
  test-local helpers beside the round-trip pins that need them), the encode benches and the
  encode-side corpus mode of the bench harness, the encoder cross-check test, the
  `roundtrip` fuzz target, and the tests that built fixtures through the encoder — replaced
  by a hand-built fixture module that writes every scanline stored, which needs no
  compressor and keeps the decode tests honest about the format. The decode suite, the
  corpus sweep and the zlib vectors are unchanged.

## [0.2.2] - 2026-09-25

### Changed

- The two long-form documents moved into `docs/` and were rewritten as documents rather than
  development logs. `docs/design.md` describes the decoder as built — the pipeline, the
  reconstruction frontier and its lag invariant, the streaming stage, the fused conversion,
  the SIMD kernel, the size ceilings, the checksum policy, interlaced handling and the
  defences against a hostile header. `docs/benchmarks.md` carries the methodology and every
  measurement. Both were previously organised by implementation phase and mixed proposal with
  outcome, which made them hard to read as descriptions of the crate and easy to misread as
  current when they were not.

### Fixed

- The design document recorded predictions that measurement had since overtaken, most
  seriously a rejected SIMD-filtering option that became the single largest win. The
  appendix now sets every original estimate against what was measured and names the
  estimate that was wrong, instead of leaving a forward-looking table in place of a result.

## [0.2.1] - 2026-09-25

Documentation only. No code, API or measurement changed.

### Fixed

- The README described every converting row as being copied into a scratch buffer, and the
  benchmark notes priced the conversion of an 8-bit RGBA source at that copy. Since 0.2.0 a
  source already in the requested layout hands its rows to the sink untouched, so both
  descriptions were behind the code, and the quoted cost came from a measurement taken before
  the change.
- The benchmark notes gave the SIMD comparison as `PSD_PNG_FORCE_SCALAR=1` against a default
  build. That variable has been read only under the `scalar-override` feature since 0.2.0, so
  the command as written compared the kernel against itself.
- The benchmark notes' header still said the fused conversion had not started. It shipped in
  0.2.0.
- The design notes' harness commands named a benchmark package that no longer exists, and
  several notes referred to a test harness, a baseline checkout and a remaining-work list by
  local path or by name. None of those ship with the package, so a reader following them had
  nothing to follow; the benchmark section now states what its numbers depend on instead.

### Changed

- The checkout directory is now `psd-png/`, matching the crate. The package's own name,
  version, API and contents are unaffected.

## [0.2.0] - 2026-09-25

The first release of `psd-png`, the PhotoshopAPI-rs port's fork of png-spark 0.2.0 by Stephen
Berry (`MIT OR Apache-2.0`, copyright retained; see the README for what came from there and what
is new here). The package was numbered 0.1.0 while it was being built and was never published, so
this crate has no 0.1.0 release.

### Added

- `Decoder::decode_to` decodes a PNG one scanline at a time through a sink, in file order and
  the file's native layout, holding only the match window, a segment of filtered rows and a few
  rows of headroom — a few hundred kilobytes for ordinary images whatever their height.
  `max_decompressed_size` bounds that stage (and, when converting, the one converted row)
  rather than the image, so images `decode` must refuse stream row by row while a header
  naming rows gigabytes wide is refused on both paths. Interlaced images stream from a
  whole-image buffer and are limited exactly as `decode` limits them. Within a few percent of `decode()` in
  speed, and faster than it against a sink that does not retain the rows. Under
  `Checks::Full` the Adler-32 is verified over the *filtered* bytes, hashed ahead of each
  in-place reconstruction rather than over a buffer that has already been rewritten.
- `Decoder::decode_to_rgba8`, `decode_to_rgb8`, `decode_to_rgba16` and `decode_to_rgb16`:
  the same streaming decode with the conversion folded in, so no converted image is ever
  materialised. Rows are interleaved and big-endian at 16 bits. A 16-bit source passes through
  untouched; 8-bit keeps its high byte going down to `to_rgba8` and is scaled by 257 going up
  to the 16-bit forms; 1-, 2- and 4-bit greys reach the ends of the output range exactly;
  palettes and `tRNS` resolve as they do in the whole-image conversions. Against png-spark
  0.2.0's two-pass `decode` + `to_rgba8`, the full RGBA8 deliverable is 4.6 % to 25.1 % faster
  across nine fixtures (mean 14.7 %), and the conversion itself costs at most 1.6 % on an
  8-bit RGBA source.
- `Row`, carrying a scanline's index and bytes.
- Non-interlaced decoding reverses the scanline filters as inflation writes, lagging the output
  cursor by DEFLATE's 32 KiB match window, instead of walking the whole image a second time after
  inflation. Large decodes are 2–9% faster (measured on zlib-generated fixtures up to 36.9 MB,
  interleaved best-of-N), small ones take the unchanged two-pass path.
- An SSE2 `Paeth` kernel for 3- and 4-byte strides, following libpng's filters: one row at a
  time, one pixel per 128-bit register, with the four channels of the pixel as independent
  lanes. It replaces the scalar two-row wavefront where it applies, which remains in use for
  every other stride.
- A `scalar-override` feature, off by default, under which the crate reads
  `PSD_PNG_FORCE_SCALAR=1`. The benchmark harness enables it, and CI runs the whole suite once
  more with it, so the scalar two-row wavefront is checked on machines whose build would
  otherwise only ever run the kernel.

### Changed

- The package is now `psd-png`, unpublished by registry and without a `repository` URL, built
  for the PhotoshopAPI-rs port and vendored into its workspace.
- The conversion code is one implementation parameterised by output sample width, with
  `to_rgba8` and `to_rgb8` built on it, so the whole-image and streaming paths cannot drift.
  The 8-bit output is byte-identical to what the previous implementation produced.
- The SSE2 `Paeth` kernel is chosen at compile time. SSE2 is part of the x86-64 baseline, so
  x86-64 builds always carry it and other targets never do; the runtime feature detection and
  the table of function pointers are gone, and the kernel's `unsafe` shrinks to the one call
  into it, backed by a compile-time assertion that SSE2 is enabled.
- `PSD_PNG_FORCE_SCALAR=1` is read only when the crate is built with the `scalar-override`
  feature. **A default build no longer lets the process environment choose which filter code
  runs**, so a caller relying on the variable to force the scalar path must enable the feature.
- Crate documentation, README and CI describe a decoder; the encoder inherited from png-spark
  is retained unchanged for one step and then removed.

### Fixed

- `decode_to` and its converting variants could panic ("budget must leave OUTPUT_SLACK") on
  valid non-interlaced images with rows narrower than about 130 bytes — greyscale under 128
  pixels wide, RGBA under 32 — when a DEFLATE match ended exactly where a segment paused. The
  reconstruction frontier was only told of progress at the top of each decode iteration, so
  at such a pause it trailed the cursor by up to a whole match, and the stage slide kept more
  than its headroom. A paused segment now reports its final position before returning, and
  segments pause at the driver's budget for compressed blocks as well as stored ones, as the
  segment interface already documented. A segment that can never make progress is reported
  as an error instead of looping.
- `decode_to` on an interlaced image ignored `max_decompressed_size`, although its buffered
  fallback allocates the whole image just as `decode` does. Such a decode that previously
  succeeded on a large interlaced image now fails with `SizeLimitExceeded` instead.
- The streaming stage and the converted row had no size ceiling, so a header naming a very
  wide image could ask for tens of gigabytes. Both are now held to `max_decompressed_size`,
  and `SizeLimitExceeded` reports the buffer that was over it.
- A row's byte size was computed in `usize` from a bit count that can be wider than `usize`
  even when the byte count is not, so a header naming a very wide image overflowed that
  multiplication: a 100-million-pixel RGBA16 row is 800 MB, which a 32-bit `usize` holds, but
  the 6.4-billion-bit count it comes from does not. Debug builds aborted on the untrusted
  header, and release builds wrapped to a smaller size, understating the buffers and the
  ceiling they are checked against. The bit count is now accumulated in `u64` and narrowed
  afterwards, saturating for a width whose byte size cannot be represented at all.
- The crate documentation credited png-spark to the wrong author; it is Stephen Berry's.
- The SIMD kernel's unit tests did not compile on any target but x86-64, so `cargo test`
  failed to build on AArch64. They are now compiled only where the kernel exists.

### Performance

- `Paeth` scanline reversal measures 8–22% faster than the scalar two-row wavefront where
  images have `Paeth` rows (mean 12% on both the large fixtures and the port's own corpus), and
  unchanged within noise where the filter mix has none. The two paths are byte-identical, and
  the suite passes with either in force.
- A converting stream whose requested layout is the file's own — RGBA to RGBA or RGB to RGB,
  at the file's sample width — hands each row straight to the sink instead of copying it into
  a scratch row first: 1–4% off `decode_to_rgba8` on 8-bit RGBA sources.
- Palettes are resolved once per image into a 256-entry RGBA table with the `tRNS` alpha
  folded in, so a pixel costs one range check and one load instead of a lookup into `PLTE`
  and another into `tRNS`: 10–12% off `decode_to_rgba8` on a 2048×2048 palette image, and
  the same table serves `to_rgba8`. The `tRNS` key of a greyscale or RGB image is likewise
  read once per image rather than once per row.

---

## Inherited from png-spark

These entries describe png-spark itself and are kept for continuity. They are not releases of
`psd-png`; the fork point is png-spark 0.2.0.

### 0.2.0

- `Encoder::encode_to` writes a PNG to any `io::Write`, filtering and compressing a band at a time. Peak working memory grows with the image's width but not its height, against the whole file plus a filtered copy of every row before.
- `WriteError` carries either an encoding fault or the sink's `io::Error`. `Error` is unchanged and still `Clone + PartialEq + Eq`.
- `Deflater::zlib_start` and `zlib_push` compress a zlib stream in pieces.
- `Encoder::encode` now goes through the same banded path. Output is a fraction of a percent larger, and a large image is split across several `IDAT` chunks instead of one.

### 0.1.0

First release.

- Decodes and encodes every PNG colour type and bit depth, interlaced or not
- Its own DEFLATE, CRC-32, Adler-32 and filter code, so there are no dependencies
- Ancillary chunks carried through on both read and write
- A 512 MiB default ceiling on decompressed size, for input you did not write
- Requires Rust 1.96, edition 2024
