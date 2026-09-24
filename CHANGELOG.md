# Changelog

Releases of `psd-png` follow after the fork point. The versions it inherited from png-spark are
listed under "Inherited from png-spark" at the end. The package is `publish = false` and carries
no `repository` URL, so no version headings carry compare links.

## [Unreleased]

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
