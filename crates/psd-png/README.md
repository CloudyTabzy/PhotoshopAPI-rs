# psd-png

A streaming PNG decoder with portable SIMD kernels, built to decode smart-object rasters for
PhotoshopAPI-rs, the Rust port of PhotoshopAPI. Its only dependencies are `fearless_simd` and the
build-time `#[simd]` macro crate that goes with it, and `crc32fast` for the chunk checksum.

`decode_to` hands out one reconstructed scanline at a time while holding only DEFLATE's 32 KiB
match window, a segment of filtered rows, and a few rows of headroom — a few hundred kilobytes
for ordinary images, however tall, since the stage follows the row width and not the height.
That is the whole point of the crate: a raster larger than the whole-image path's 512 MiB
ceiling, or larger than the memory you want to spend on someone else's pixels, still decodes
row by row straight into your own buffer. The ceiling bounds the stage instead, so a header
naming rows gigabytes wide is still refused.

```rust
use psd_png::Decoder;

let png = std::fs::read("smart-object.png")?;
Decoder::new().decode_to(&png, |row: psd_png::Row<'_>| {
    // row.index counts from zero; row.bytes is one scanline, filters already reversed,
    // in the file's own layout — planar, one channel after another, at its native depth.
    planar.push(row.bytes);
    Ok::<(), psd_png::Error>(())
})?;
```

Rows arrive in file order, each borrowed only for the duration of the call. A sink error aborts
the decode and is returned; rows already delivered stay delivered.

## Converting while streaming

`decode_to_rgba8`, `decode_to_rgb8`, `decode_to_rgba16` and `decode_to_rgb16` deliver the same
rows already converted, interleaved, at the sample width you name — so a caller never holds a
converted image it did not ask for, and the whole-image buffer the port would otherwise
allocate and then deinterleave never exists:

```rust
// 16-bit RGBA, big-endian samples: a 16-bit source untouched, 8-bit scaled by 257,
// sub-byte greys across the full range, palette resolved, tRNS turned into alpha.
Decoder::new().decode_to_rgba16(&png, |row: psd_png::Row<'_>| {
    planar.push(row.bytes);
    Ok::<(), psd_png::Error>(())
})?;
```

This is the same conversion as `to_rgba8`/`to_rgb8`, expressed per row: the palette and `tRNS`
key are resolved once per image, only the current row is live, and a stream whose layout already
matches the file's own — RGBA to RGBA, or RGB to RGB at the file's sample width — hands each row
straight to the sink without converting it. The whole-image methods are implemented on top of
the same per-row code, so the two paths cannot drift.

## Status and provenance

This crate is **not published to a registry**. It is vendored into PhotoshopAPI-rs as
`crates/psd-png`, where it drives the smart-object PNG raster path. It carries no
`repository` URL of its own while its home is this repository.

It began as **[png-spark](https://github.com/stephenberry/png-spark) 0.2.0 by Stephen Berry**,
which contributed:

- the complete PNG format layer — every colour type, every bit depth, Adam7 on read, chunk
  parsing, ancillary-chunk retention, the 512 MiB ceiling and the fallible allocation behind it;
- the DEFLATE codec, the CRC-32 and Adler-32 implementations, and the scanline filters,
  forward and reverse. (The checksums have since been replaced; see below.)

The PhotoshopAPI-rs port team added:

- **fused reconstruction** — the scanline filters are reversed as inflation writes, lagging the
  output cursor by the match window, instead of walking the whole image a second time afterwards;
- **`decode_to`** — the resumable, budget-paused streaming decoder above;
- **portable SIMD kernels** — a `Paeth` filter kernel (one pixel per register, for 3- and
  4-byte strides) and the row-conversion kernels, written once against `fearless_simd`'s
  portable vectors and compiled for SSE2/SSE4.2/AVX2/AVX-512, NEON or wasm SIMD at run time,
  with the scalar paths as the fallback.

Both upstream licences (`MIT OR Apache-2.0`) and the original copyright
(`Copyright (c) 2026 Stephen Berry`) are retained unchanged; see [Licence](#licence).

## What it decodes

- Colour types: grayscale, RGB, indexed, grayscale+alpha, RGBA
- Bit depths: 1, 2, 4, 8, 16
- Interlacing: Adam7 on read; non-interlaced images stream, interlaced ones are decoded into a
  buffer first and emitted from there
- Chunks: `IHDR`, `PLTE`, `tRNS`, `IDAT`, `IEND`, plus any ancillary chunks you ask to keep;
  an unrecognised *critical* chunk is an error
- Conversion to 8-bit RGB or RGBA, resolving palettes, `tRNS`, and sub-byte depths
- The whole-file path: `decode` returns pixels in the file's own format, which is the only
  representation that is always correct and always free; `read_info` stops at the image data

Not supported: APNG, and writing interlaced files.

## Platforms

64-bit `x86_64` and `aarch64` are the supported targets, and the only ones CI builds and tests.
The SIMD kernels are portable: one source compiled for whichever backend the CPU reports at run
time — SSE2, SSE4.2, AVX2 or AVX-512 on x86-64, NEON on aarch64, wasm SIMD on the web — with the
scalar paths as the fallback on anything else. Two backends deliberately stay scalar: the plain
scalar fallback, where the generic kernel code runs a lane at a time, and — for the
shuffle-built conversion kernels — the bare SSE2 level, where a dynamic byte shuffle is emulated
per lane.

32-bit x86 is not a target. The decoder is correct there and refuses an over-wide header earlier
than it does on 64-bit — with `ImageTooLarge` instead of a size-limit refusal, having allocated
nothing — but three of the size-limit tests in `tests/limits.rs` assume a 64-bit address space and
fail on `i686`. They are test expectations, not decoder behaviour: they build a 17 GB header,
which a 32-bit target cannot represent, and expect the size limit rather than the addressability
ceiling to be what refuses it.

## Performance

Measured against png-spark 0.2.0, interleaved best-of-N in a single session because the
measurement machine thermally throttles; the noise floor measured the same way is ±0.00%. Full
methodology, tables and caveats are in [`docs/benchmarks.md`](docs/benchmarks.md).

| | result |
| --- | --- |
| Fused reconstruction, 9 large zlib-generated fixtures (0.1–36.9 MB filtered) | **1.8 % to 8.9 % faster, 9/9** |
| Small files around and below the 32 KiB window | unchanged; the gate sends them down the inherited two-pass path |
| `decode_to` vs `decode`, sink copying into a presized buffer | within **5 %** on 8 of 9 classes (16-bit gradients +27 %, an absolute 0.5 ms) |
| `decode_to` vs `decode`, sink discarding rows | **faster** — it never allocates or first-touches the whole image |
| A 16384×16384 RGBA image (1.07 GB filtered) | refused by `decode` under the default ceiling; streams through `decode_to` in a ~300 KB stage |
| Full RGBA8 deliverable vs png-spark 0.2.0 (`decode` + `to_rgba8` there, `decode_to_rgba8` here) | **4.6 % to 25.1 % faster, 9/9, mean 14.7 %** |
| Cost of the fused conversion | **≤ 1.6 %** on an 8-bit RGBA source, measured before pass-through; a source already in the requested layout now skips conversion entirely |
| SIMD `Paeth` vs the scalar two-row wavefront (`PSD_PNG_FORCE_SCALAR=1` vs default) | **8 % to 22 % faster** on `Paeth`-bearing images, unchanged where there is no `Paeth` |

Every PNG in the port's own fixture corpus (12 files, 512×512 and 200×108 RGBA) clears the
fusion threshold, so the fused path is the one that runs in production rather than a benchmark
shape.

## What the fork removed

The PNG **encoder** and the DEFLATE compressor under it came with the fork and are gone, along
with the ancillary-chunk retention (`Keep`, `Info::metadata`). The port decodes rasters and
never writes one, and every retained chunk is a copy of attacker-controlled bytes, so the
crate now carries only what a reader needs. The decode suite — the corpus sweep, the zlib
vectors, the streaming bounds — is unchanged, and its fixtures are built by hand rather than
by the encoder that used to sit opposite it.

## Design notes

**Reconstruction is fused into inflation.** Reversing a PNG filter is serial along a row, so the
inherited decoder did it in a second pass over the whole image — reading every row long after
inflation had evicted it. The streaming decoder instead reverses each row in place as the output
cursor passes it, held back by DEFLATE's maximum match distance (32 KiB) so that no future match
can read a byte reconstruction has rewritten. Streams no larger than one window plus one row can
never trigger this and take the original two-pass path, so small decodes pay nothing.

**`Paeth` is reversed two rows at a time, or one pixel per register.** The serial dependency
along a row is why `Paeth` is the filter that costs: on its own it reconstructs at around
800 MB/s, where `Up` manages 28 GB/s. Two schemes fill those idle slots, and this crate has
both. The scalar one takes two rows offset by a pixel — row *r* pixel *x* and row *r+1* pixel
*x-1* depend only on values settled before the step, so the two predictions issue together. The
register one follows libpng's SSE2 filters and takes the four channels of a single pixel per
iteration, which is independent of the left neighbour by construction. SIMD was preferred for
3- and 4-byte strides on measurement (8–22% where `Paeth` rows exist, nothing where they do
not), and the scalar pair path keeps every other stride. The multi-row and anti-diagonal
arrangements from Wuffs issue #157 would need several rows in flight at once, which the
reconstruction frontier and the streaming stage's sizing would both have to grow for; that is
the next step if the mixed-filter case ever justifies it, not this one. Two details differ from
libpng, both forced by reconstructing in place: its rows live in padded buffers and may write a
whole pixel at the last position, where here the bytes after a row are the next row's
still-filtered data, so this kernel touches only bytes inside its own row; and its stride is a
runtime value here, so the kernel is chosen per call. The kernel is written once against
`fearless_simd`'s portable vectors and compiled for the CPU's backend, so the acceleration is
no longer x86-64-only. Built with the `scalar-override` feature, `PSD_PNG_FORCE_SCALAR=1`
falls back to the scalar paths, which is how the two are measured against each other; without
it the environment is not read.

**Inflation is pausable, not restartable.** A segment boundary can land mid-block, so resuming
requires the bit position, the output cursor, and the start of the symbol being processed — a
match the decoder refused to take before the pause is rewound and retried. Huffman tables are
rebuilt per block regardless, which is what makes any pause point recoverable. A single-block
stream therefore obeys the same memory bound as a many-block one.

**The decompressor is not a state machine.** The one-shot path decodes a whole stream in one call
against one buffer, which removes the per-symbol state checks a resumable decoder needs and
leaves the bit buffer and output cursor in registers. Resumability costs nothing on the path that
does not use it: the segmented loop is a const-generic instantiation, and the ordinary decode
never enters it.

**Checksums use the hardware.** CRC-32 is `crc32fast`'s: carry-less multiplication (PCLMULQDQ,
PMULL) where the CPU has it, at 60-80 GB/s, and a slice-by-16 table walk elsewhere. Adler-32 is a
portable vector kernel written against `fearless_simd`, run on every target that has a vector
unit, at about 19 GB/s on AVX2. Both fall back to portable code, and every implementation is
tested against the same reference.

**Chunk CRCs are checked; the Adler-32 is not, by default.** The Adler-32 inside the compressed
stream covers the same bytes the chunk CRC already covered, so checking one catches the same
corruption for one pass instead of two. `Decoder::checks(Checks::Full)` turns both on, and
streaming verifies it incrementally per segment.

**A bad CRC on an ancillary chunk drops the chunk, not the file.** Nothing a decode returns is
built from a colour profile or a text comment, and files exist whose metadata was rewritten
without recomputing its checksum.

## Safety

Safe Rust apart from one `unsafe` block: the sixteen-byte pass loop of the inflate match copy.
It sits inside a function that checks the whole range it can touch before the loop starts, so the
function is sound for any arguments, and a bug in its caller is a panic and not an out-of-bounds
write. The crate root denies `unsafe_code` and that one function allows it by name, so a second
block anywhere fails the build. The filter, conversion and Adler-32 kernels are written against
`fearless_simd`'s portable vectors and contain no `unsafe`; the CRC-32's carry-less multiply is
`crc32fast`'s. Everything else — the literal stores of the inflate loop, chunk parsing,
filtering, conversion — is bounds-checked. The decoder's buffers are allocated with a probe followed by `vec!`, which is not
the abort-free guarantee a direct `alloc_zeroed` gave: see the changelog. Malformed input is a
tested case: the suite feeds truncated files, single-bit corruptions at every byte, and thousands
of random byte strings through the decoder, and requires errors rather than panics.

## Testing

```sh
python3 tools/gen_testdata.py       # reference corpus, generated not committed
cargo test
cargo clippy --all-targets
cargo fmt --all --check
```

Rust 1.96 or newer, edition 2024. CI runs the suite three ways — as built, in release, and once
more with the `scalar-override` feature and `PSD_PNG_FORCE_SCALAR=1`, so the scalar filters are
checked on x86-64, where the kernel would otherwise always run instead. The inherited suite covers the format; the added tests cover
what is new here:

- `tests/fused_reconstruction.rs` — fused output byte-identical to the two-pass path on
  zlib-generated fixtures up to 36.9 MB, including rows fired mid-inflation
- `tests/streaming_decode.rs` — row-exact parity with `decode` across large fixtures, corpus
  files and interlaced images; sink-error abort; a 1.07 GB image past the ceiling; corrupt
  streams agreeing between the two paths
- `tests/streaming_bounds.rs` — hand-built streams that end a match on every position a segment
  can pause at, for rows narrow enough to test the stage's headroom; the ceiling applied to the
  stage, to a converted row and to the interlaced fallback
- unit tests in `filter.rs`, `inflate.rs` and `simd.rs` — the frontier's lag invariant, the slide
  rule, segmented-versus-one-shot parity over 224 reference zlib vectors at a stage barely larger than
  the window, and the SIMD `Paeth` kernel against the scalar predictor at every stride it claims
  and every length including the tails and the extremes where a lane-wise port diverges. The whole
  suite passes with the kernel in force and with the scalar override, so the two paths are
  held to the same bytes.

The corpus is produced by a reference implementation rather than by this crate, so the tests
check the format and not just self-consistency. Three fuzz targets (`decode`, `inflate`,
`roundtrip`) and a Miri run over the unsafe code are wired in CI. Miri cannot cover the
segmented inflate path on Windows — it is covered by the Linux CI run instead.

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT licence ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

`Copyright (c) 2026 Stephen Berry` is retained from png-spark, which this crate is derived from.
