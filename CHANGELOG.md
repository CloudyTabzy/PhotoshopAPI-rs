# Changelog

## 0.1.0 — in progress (unreleased)

Derived from png-spark 0.2.0 by Stephen Berry (`MIT OR Apache-2.0`, copyright retained); see the
README for what came from there and what is new here. Nothing here has been published: the
package is `publish = false` and carries no `repository` URL.

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

### Changed

- The package is now `psd-png`, unpublished by registry and without a `repository` URL, built
  for the PhotoshopAPI-rs port and vendored into its workspace.
- The conversion code is one implementation parameterised by output sample width, with
  `to_rgba8` and `to_rgb8` built on it, so the whole-image and streaming paths cannot drift.
  The 8-bit output is byte-identical to what the previous implementation produced.
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
  fallback allocates the whole image just as `decode` does.
- The streaming stage and the converted row had no size ceiling, so a header naming a very
  wide image could ask for tens of gigabytes. Both are now held to `max_decompressed_size`,
  and `SizeLimitExceeded` reports the buffer that was over it.
- The crate documentation credited png-spark to the wrong author; it is Stephen Berry's.
- The SIMD kernel's unit tests did not compile on any target but x86-64, so `cargo test`
  failed to build on AArch64. They are now compiled only where the kernel exists.

### Changed

- The SSE2 `Paeth` kernel is chosen at compile time. SSE2 is part of the x86-64 baseline, so
  x86-64 builds always carry it and other targets never do; the runtime feature detection and
  the table of function pointers are gone, and the kernel's `unsafe` shrinks to the one call
  into it, backed by a compile-time assertion that SSE2 is enabled.
- `PSD_PNG_FORCE_SCALAR=1` is read only when the crate is built with the new
  `scalar-override` feature, which the benchmark harness and the scalar CI run enable. A
  default build no longer lets the process environment choose which code runs.

### Performance

- `Paeth` scanline reversal has an SSE2 kernel for 3- and 4-byte strides, following libpng's
  filters: one row at a time, one pixel per 128-bit register, with the four channels of the
  pixel as independent lanes. It replaces the scalar two-row wavefront where it applies, which
  remains in use for every other stride. Measured against it with
  `PSD_PNG_FORCE_SCALAR=1`: 8–22% faster where images have `Paeth` rows (mean 12% on both the
  large fixtures and the port's own corpus), and unchanged where the filter mix has none. The
  two paths are byte-identical, and the suite passes with either in force.

## 0.2.0

- `Encoder::encode_to` writes a PNG to any `io::Write`, filtering and compressing a band at a time. Peak working memory grows with the image's width but not its height, against the whole file plus a filtered copy of every row before.
- `WriteError` carries either an encoding fault or the sink's `io::Error`. `Error` is unchanged and still `Clone + PartialEq + Eq`.
- `Deflater::zlib_start` and `zlib_push` compress a zlib stream in pieces.
- `Encoder::encode` now goes through the same banded path. Output is a fraction of a percent larger, and a large image is split across several `IDAT` chunks instead of one.

## 0.1.0

First release.

- Decodes and encodes every PNG colour type and bit depth, interlaced or not
- Its own DEFLATE, CRC-32, Adler-32 and filter code, so there are no dependencies
- Ancillary chunks carried through on both read and write
- A 512 MiB default ceiling on decompressed size, for input you did not write
- Requires Rust 1.96, edition 2024
