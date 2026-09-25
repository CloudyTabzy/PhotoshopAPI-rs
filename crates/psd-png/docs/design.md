# Design

How this decoder is put together, and why each piece is shaped the way it is.

**Provenance.** Written by the PhotoshopAPI-rs port team; not an upstream png-spark document.
This crate began as png-spark 0.2.0 by Stephen Berry (`MIT OR Apache-2.0`, copyright
retained), which contributed the format coverage, the DEFLATE codec, the checksums and the
scanline filters. The port team added the fused reconstruction, the streaming decoder, the
fused conversion, the SIMD filter kernel and the size accounting described here, and removed
the encoder, the compressor under it and the ancillary-chunk retention, none of which the
decode path ever touched. Measurements for every claim below are in
[`benchmarks.md`](benchmarks.md).

**Status.** Implemented and measured. The crate is unpublished and vendored into the
PhotoshopAPI-rs workspace as `crates/psd-png` when smart-object PNG decode lands there; that
integration has not happened yet.

## The shape of a decode

A decode is four stages, and the design's whole purpose is to overlap the middle two.

```
parse header ──► allocate ──► inflate ──► reconstruct ──► deliver
  IHDR, PLTE,      what the    DEFLATE →   Paeth/Sub/Up/   decode(): one buffer
  tRNS, IDAT       plan needs   filtered     Average,          decode_to(): a row at a time
                                bytes       in place
```

The inherited decoder did stages 3 and 4 as **two separate passes over the whole image**:
inflate wrote N bytes, then unfilter read N and wrote N. For a 4K RGBA8 image that is roughly
110 MB of memory traffic where 74 MB would do, and the second pass read rows long after they
had left cache. The encoder in the same crate already streamed in bands (since removed), so
the read side was the asymmetric half.

Everything below follows from closing that asymmetry.

## The reconstruction frontier

`ReconstructionFrontier` (`src/filter.rs`) reverses each row **in place** as inflation's output
cursor passes it, rather than in a later pass. The work is the same per-row code the old
second pass used — `unfilter_row`, and `unfilter_paeth_pair` for the two-row `Paeth`
wavefront — so the output is byte-identical by construction rather than by coincidence.

### The lag condition

This is the one invariant everything else rests on, and it is stricter than it first looks.

DEFLATE match copies read the output buffer at `pos - distance`, and the format caps
`distance` at exactly 32768. A row may therefore be reconstructed only once the cursor is a
full match window past the end of that row's bytes:

```text
cursor >= (row + take) * (1 + row_bytes) + 32768
```

Anything weaker — "strictly behind the write cursor" — is **necessary but not sufficient**, and
an implementation that stops there corrupts any stream whose matches reach back into
recently reconstructed rows. The original design proposal said only the weaker thing; the
stronger condition was added during implementation, after exactly that corruption.

The rows in the final window's worth of the image cannot satisfy the condition, so they are
drained after inflation ends, where no match can read anything and the lag drops to zero.

### Why it costs nothing on small images

A stream no larger than one match window plus one row can never trigger mid-inflation
reconstruction. Those take the plain two-pass path, unchanged and byte-identical, so the
machinery is compiled out entirely for them (`const ENABLED`) and the 180-file corpus — every
image of which is under 32 KiB — runs the legacy code with no frontier in it at all. Measured:
equal within the noise floor.

### Where the work is actually spent

The frontier runs reconstruction inline, between decode-loop iterations, so its cost is
**exposed, not hidden**. The saving against the old decoder is therefore not "the unfilter
disappeared" but cache locality: 44.5 ms cold against roughly 30 ms hot on the 4K fixture.
That is why fusion is worth 1.8–8.9% and not 25–40% — the unfilter share of decode is real,
and fusion does not delete it, it makes it cheaper.

Within that share, essentially all of it is `Paeth` (790 MB/s, against inflation's ~2000 MB/s),
and on `Up`/`Sub` images it is invisible. Every optimisation decision below follows from that
one measurement.

## Streaming

`decode_to` and its converting variants deliver one reconstructed row at a time through a sink,
in file order, in the file's own layout, holding only a bounded stage — a few hundred
kilobytes for an ordinary image, whatever its height.

### Pausing inflate without slowing it down

The inherited inflate core is deliberately **not** a state machine. PNG states the
decompressed size up front, so the whole stream is decoded in one call against one buffer,
which keeps the bit buffer and output cursor in registers and removes per-symbol state checks
and output clamping. That doctrine is the crate's main performance asset and streaming does
not give it up.

The seam is an **output-budget pause**. `zlib_segment` stops when it reaches a caller-set
output budget — anywhere, including mid-block — and returns a `SegmentPause`: bit position,
output cursor, and a mid-block flag. The loop's existing output-limit checks *become* the pause
sites, so the pause is nearly free, and the match-refusal path rewinds to the symbol start so
a resume never loses a match. Huffman tables are rebuilt per block regardless, so a pause
costs a bit position and nothing else.

The first design proposed resuming at DEFLATE *block* boundaries, which is the textbook seam.
That was abandoned: a single block's output is unbounded, so a hostile stream could force
unbounded peak memory, and a pure block-boundary seam inherits that hole. The shipped
mid-block pause closes it. `SEGMENTED` is a const generic so the one-shot `zlib` is literally
the same code with the seam compiled out.

### The stage

`StageLayout` (`src/decoder.rs`) computes it from the row width alone:

- **pitch** — one filtered row: `row_bytes + 1` for the filter byte.
- **cap** — one segment's worth of new filtered bytes, targeting 256 KiB (encoder-band-sized),
  but never less than one row, and never less than a whole stored block (65535 bytes) plus a
  row, because a stored block can arrive in one piece however narrow the rows are.
- **len** — the whole stage: `MAX_MATCH_DISTANCE` (the retained match window) `+ cap + 4 ×
  pitch + OUTPUT_SLACK`. The four rows of headroom cover the retained region — the match
  window plus less than two rows — and the segment extension.

Between segments the driver **slides**: it relocates the retained region to the front of the
stage and sets the base to just above the last reconstructed row, so the next row's
predecessor survives for lookback. When the frontier has not caught up, a segment is extended
by up to two rows. A segment that can make no progress at all is an error rather than a loop.

This is also where a real bug lived: progress was reported at the top of each decode
iteration, so a pause landing immediately after a match left the frontier up to 258 bytes
behind the cursor — more than the stage's headroom on a row of a few dozen bytes, which
panicked on valid images with rows under about 130 bytes. Segments now report their final
position before returning, and stop at the driver's budget for compressed blocks as well as
stored ones.

## Fused conversion

`decode_to_rgba8`, `_rgb8`, `_rgba16` and `_rgb16` convert each row as it is reconstructed, so
no converted image is ever materialised and a consumer can write straight into its own
storage. For the port that means filtered bytes → planar channels in one pass, with no
interleaved buffer and no de-interleave copy.

One `RowConverter` (`src/transform.rs`) backs all four variants *and* the whole-image
`to_rgba8`/`to_rgb8`, so the streaming and whole-image paths cannot drift. 8-bit output is
byte-identical to the pre-fusion implementation.

Two things make it cheaper than the obvious implementation:

- **Pass-through.** A stream whose requested layout is already the file's own — RGBA to RGBA,
  or RGB to RGB at the file's sample width — hands each row straight to the sink with no
  scratch row. RGB passes through even with a `tRNS` colour, because the alpha that key would
  imply is dropped from a three-channel row anyway.
- **Per-image resolution.** The palette `tRNS` alpha is folded once into a 256-entry RGBA
  table, so an indexed pixel costs one range check and one load instead of a lookup into
  `PLTE` and another into `tRNS`. The grey and RGB `tRNS` keys are likewise read once per
  image rather than once per row.

Sample-width rules worth stating: a 16-bit source passes through untouched, 8-bit scales by
257 on the way up to 16 bits, and 1/2/4-bit greys reach the ends of the output range.

## The SIMD filter kernel

An SSE2 `Paeth` kernel for 3- and 4-byte strides, following libpng's filters: one row at a
time, one pixel per 128-bit register, the four channels of the pixel as independent lanes.
Measured **mean −12%** on `Paeth`-bearing images, up to −22%, and unchanged where the filter
mix has none.

Three decisions are worth recording:

- **libpng's arrangement, not the multi-row one.** Wuffs issue #157 and Blend2D describe
  multi-row or anti-diagonal arrangements that extract more instruction-level parallelism.
  They were not taken: they need the frontier and the stage's sizing to grow to keep several
  rows in flight, and Wuffs' own numbers say the shuffling is not free. libpng's version is a
  drop-in that touches nothing structural, and the measurements say it already claims the
  available win at these row widths.
- **Reconstruction is in place, so the kernel writes only bytes inside its own row.** It never
  reads or writes a neighbouring row, which is what lets it be correct against a live frontier
  rather than a padded buffer.
- **Selection is compile-time, not runtime.** SSE2 is part of the x86-64 baseline, so
  detecting it at runtime bought nothing: every x86-64 machine has it and no other target has
  the kernel. The dispatch is a `cfg` on `target_arch = "x86_64"` plus a compile-time assertion
  that SSE2 is enabled; i686, AArch64 and everything else get the scalar wavefront. There is
  no AVX2 tier, because the measurements give no reason to want one at this width.

The `scalar-override` feature exists so both implementations can be run in one build and held
to the same bytes. It is off by default, and without it the process environment has no say in
which code runs.

## Checksums

The default is `Checks::Crc`: chunk CRCs are verified, and the zlib Adler-32 is not, matching
upstream policy. `Checks::Full` verifies both, `Checks::None` skips both.

Adler-32 is computed **incrementally per segment, inside the progress hook, ahead of each
reconstruction** — not over the output buffer once at the end. That is not an optimisation
choice, it is a correctness requirement: the fused path rewrites the buffer in place, so a
final pass over it would be checksumming data that has already been transformed. Getting this
wrong rejected every valid non-interlaced image past the match window, under full checks, in
both `decode` and `decode_to`, and was invisible because the default does not enable it.

AArch64 has hardware Adler-32 and CRC paths; elsewhere the checksums are portable
implementations.

## Interlaced images

Adam7 is correct and complete, and it is not streamed. An interlaced image is decoded into a
whole-image buffer and emitted from there in file order.

That is a deliberate scope decision. Adam7 unfiltering needs pass-by-pass scatter into a
full-size target, so a streaming path would require the caller to own that target — moving a
responsibility outward for images that are rare in practice. The port's corpus is 12/12
non-interlaced. The cost is that an interlaced decode holds the whole image, which is why the
size ceiling treats it as a whole-image decode (below).

## Untrusted input

`IHDR` is thirteen bytes and states the image's dimensions, and the buffer they imply is sized
before a single compressed byte has been read. Nothing downstream corroborates them, so the
header alone decides how much memory a decode asks for. The defences, in order:

- **`Info::validate` gates every sizing path.** No allocation proportional to the header's
  dimensions happens before it runs, which is what lets `read_info` answer "what does this
  file claim?" for a caller that wants to apply its own policy — a header the decoder would
  refuse can still be inspected.
- **Row arithmetic is computed wide.** A row's byte size is derived from a bit count that can
  exceed `usize` even when the byte count does not — 100 million RGBA16 pixels are 800 MB,
  which a 32-bit `usize` holds, but the 6.4-billion-bit count it comes from does not. The
  product is accumulated in `u64` and narrowed afterwards, saturating only for a width whose
  byte size genuinely cannot be represented. Done in `usize`, this aborted on an untrusted
  header in debug builds and **wrapped** in release, understating both the allocation and the
  ceiling it is checked against.
- **The ceiling is a per-buffer cap, not an image cap.** `Plan::largest_buffer` reports the
  largest single buffer a decode will actually allocate, and that is what
  `max_decompressed_size` bounds: the decompressed image for `decode`, the **stage** (plus one
  converted row) for `decode_to`, and the whole-image buffer for the interlaced fallback. So
  a header naming rows gigabytes wide is refused, and a tall image streams past the ceiling
  regardless of height.
- **Allocation failure is a value, not an abort.** `zeroed_vec` calls the allocator directly so
  an unsatisfiable request returns `None` rather than killing the process, which is not a
  library's decision to make.
- **Palette indices are range-checked** before a table lookup in `decode` and in the converting
  paths. Native `decode_to`, which hands back the file's own indexed rows untouched, does not
  check them — a known gap, since an out-of-range index reaches the caller rather than the
  table.

`Platforms`, in the README, records the supported targets: 64-bit x86-64 and AArch64.

## Trade-offs

What was considered and not taken, and what it would cost.

- **Symbol-level resumable inflate.** Rejected: per-symbol state checks and output clamping are
  exactly what the one-shot core removed for speed. The seam is placed where the format and the
  implementation both already have one.
- **A threaded pipeline** (inflate on one thread, reconstruction on another) could overlap the
  ~60% and ~35% stages for up to ~1.4× on two cores. Rejected: it adds threads and a
  backpressured handoff buffer to a crate whose identity is zero dependencies and no threads,
  and it duplicates memory. Worth revisiting if the profile ever stops being DEFLATE-bound.
- **Extending the scalar wavefront to `Sub`/`Average`.** 1–5% of rows. Not worth the code.
- **x86 hardware CRC.** Real but small, and it only applies to a caller that asks for
  `Checks::Full`, since chunk CRCs are off the hot path by default.
- **An AVX2 filter tier.** Not written; SSE2 already claims the win at these widths, and a
  wider register does not help a predictor that is serial along the row.
- **Multi-row `Paeth`.** Deferred, not rejected. It needs the frontier and the stage sizing to
  grow, so it is a separate step with its own measurements.
- **Faster inflate itself.** The remaining cost is the serial Huffman loop. Beating it is
  years of C-level micro-work, not a patch, and nothing in this design depends on it.

### One estimate that was wrong

The original proposal listed SIMD unfiltering among the alternatives **rejected**, on the
grounds that "filters are not the bottleneck". The stage profile says unfilter is 25–40% of
decode, `Paeth` dominates it, and it runs at 790 MB/s — which made it the single largest
remaining cost. The SSE2 kernel then turned out to be worth more on the 4K fixture than fused
reconstruction and fused conversion combined.

The reasoning error was assuming that a percentage of total time bounds the prize: 25–40% of a
DEFLATE-bound decode is a large absolute number, and the predictor is a much cheaper thing to
vectorise than a Huffman decoder. It is recorded here because the same reasoning — "X is a
small share, so X is not worth doing" — is the most likely way to leave the next 10% on the
table.

## Appendix: the proposal this was built from

The design began as a proposal written against png-spark 0.2.0 at `6d256fc`. Phases 1–3 were
implemented as proposed; Phase 4, an upstream pull request, was cancelled on 2026-09-24 in
favour of a hard fork. This record is kept because the reasoning is worth more than the
outcome, and because two of the estimates did not survive contact with measurement.

**Original estimates, and what was measured:**

| Effect | Estimated | Measured |
|---|---|---|
| Fused reconstruction | 5–15% on large images | 1.8–8.9%, 9/9 fixtures |
| Streaming memory | O(window + rows) instead of O(W × H) | as designed; ≤5% cost on 8 of 9 classes |
| Fused conversion | one pass saved per consumer | −4.6% to −25.1%, mean −14.7%, 9/9 |
| x86 hardware CRC | 2–4% | not implemented; moot while CRCs are off by default |
| Inflate micro-optimisation | 2–8%, unmeasured | not attempted |

The fused-reconstruction estimate was directionally right and numerically optimistic; the
document had already said there was no 2× in the design, and that held.

**API shapes that changed.** The proposal sketched `Inflater::next_block` returning
`BlockOutcome::{More, Final}`, a `decode_rows`, and a `Row` carrying a `NativeLayout` plus
`(pass, x, y)` for Adam7. What shipped: `zlib_segment` returning `SegmentPause`/`SegmentOutcome`
and pausing on an output budget rather than a block boundary; `decode_to` only, with no
`decode_rows`; and a `Row` carrying just `index` and `bytes`, because the layout is already on
`Info` from `read_info` and no new public layout type was needed. The Adam7 row metadata was
dropped entirely — interlaced images emit in file order, which is what a consumer wants.

The 16-bit converting variants (`decode_to_rgba16`, `_rgb16`) were added beyond the sketch,
because the port's `BitDepth` boundary refuses to widen a sample and a downconvert inside the
decoder would be unrecoverable.

**A correctness condition the proposal got wrong.** It said reconstruction must stay "strictly
behind the write cursor". The real condition is a full 32768-byte match window behind it, as
described above; the weaker form corrupts any stream whose matches reach into recently
reconstructed rows. This was the most valuable thing the implementation phase found.
