# Design proposal: banded decode — fuse reconstruction into inflation, then stream

**Provenance:** written by the PhotoshopAPI-rs port team. This document is **ours**, not upstream
png-spark; it lives in this working checkout so the implementing agent has everything in one place.
**Target:** this checkout (`png-spark`, main at `6d256fc`, the 0.2.0-era code).
**Status:** Phases 1 and 2 implemented on `fused-decode-frontier` with measured evidence (see
`BENCHMARKS.md`); Phase 3 (fused conversion) not started; no upstream contact has been made.

---

## 0. TL;DR

png-spark's **encoder streams in bands** (`Encoder::encode_to`, `src/encoder.rs:166`), but the
**decoder is two-pass**: inflate the whole filtered stream into one buffer, then unfilter that
buffer in a second pass — and Adam7 unfilters into a *second* buffer entirely
(`deinterlace`, `src/decoder.rs:593`).

Proposal: **make decode a banded pipeline too.**

1. Keep the one-shot, register-resident inflate core exactly as it is. Add a *reconstruction
   frontier*: after each batch of output writes, unfilter the rows whose bytes are now complete —
   strictly behind the write cursor, in place. This removes the second full pass over the image
   and reconstructs rows while they are still cache-hot.
2. Expose the same machinery as a **streaming decoder** (`decode_to` / `decode_rows`), built on a
   **block-granular resumable inflate**: resume at DEFLATE *block* boundaries (where the format
   already has seams and where dynamic Huffman tables are rebuilt anyway), never at symbol
   boundaries. The fast inner loop is untouched; the resumable state is just a bit position, the
   32 KiB match window, and the output cursor.
3. Fuse conversion into the same pass (row callback carrying the file's native layout), so a
   consumer that wants planar channels (this port) goes filtered-bytes → planes in one pass, with
   no interleaved buffer and no de-interleave copy.

Expected: **5–15% faster decode on large images**, **O(width) memory streaming**, one full pass
saved per consumer, and no API breakage. There is no 2× here — see §5 for why — but this is the
structural improvement that matters.

---

## 1. Context and evidence

### 1.1 What the port needs

The port decodes PNG only for **smart-object linked/embedded rasters**. A scan of the port's
document corpus (11 reference renders + the two PNGs embedded in
`SmartObjects/smart_object_file_no_warp.psd`) found **13 of 13 PNGs are RGBA 8-bit,
non-interlaced**. RGBA8 is the case that matters; RGB8/palette are the cases png-spark loses on
(§1.4), and interlaced files are rare but must stay correct.

### 1.2 Decode stage profile (measured on this machine)

`bench/src/bin/profile.rs` (added in this checkout) reports best-of-N per stage. First run:

| fixture | KB | decode | crc | inflate | unfilter | parts | other |
|---|---:|---:|---:|---:|---:|---:|---:|
| gray8_512 | 78.5 | 0.47 | 0.02 | 0.30 | 0.22 | 0.54 | −0.07 |
| gray16_512 | 112.6 | 0.93 | 0.04 | 0.48 | 0.52 | 1.04 | −0.11 |
| palette8_512 | 72.9 | 0.35 | 0.02 | 0.35 | 0.00 | 0.37 | −0.02 |
| rgb8_512 | 221.2 | 1.40 | 0.07 | 0.89 | 0.65 | 1.61 | −0.21 |
| rgba8_512 | 247.6 | 2.04 | 0.08 | 1.24 | 1.00 | 2.32 | −0.28 |
| rgb16_512 | 297.0 | 2.13 | 0.09 | 1.66 | 0.83 | 2.58 | −0.46 |
| rgb8_2048 | 2597.3 | 23.17 | 0.82 | 14.56 | 10.24 | 25.63 | −2.45 |
| rgba8_3840 | 5103.5 | 69.35 | 1.62 | 34.94 | 40.35 | 76.90 | −7.55 |

**Reading it:** inflate is **~50–65%** of decode; unfilter is **~25–40%**; CRC is **2–5%**;
everything else (chunk walk, zeroed allocation, compaction copies) is small. The `parts` column
exceeds `decode` because the unfilter stage is measured in isolation with a cold buffer (pessimistic);
in situ it reads rows that inflate has just written.

**Noise warning (important for the implementing agent):** this machine thermally throttles on
sustained benchmarks. A second profile run reported `rgba8_3840` decode at 93.32 ms vs 69.35 ms in
the first; the port's interleaved harness reports best 52.71 ms / mean 70.09 ms for the same file.
**All A/B comparisons must be interleaved, best-of-N, and in one session**; never compare numbers
from separate runs.

### 1.3 Filter distribution in the port's fixtures

`bench/tools/filters.py`:

| fixture | rows | filter histogram |
|---|---:|---|
| rgba8_3840 | 2400 | Paeth 91%, Sub 4%, Up 3% |
| rgba8_512 | 512 | Paeth 85%, Up 13%, Sub 1% |
| rgb8_512 | 512 | Paeth 72%, Up 25%, Sub 2% |
| rgb8_2048 | 2048 | Paeth 67%, Up 27%, Sub 5% |
| rgb16_512 | 512 | Paeth 70%, Up 24%, Avg 2%, Sub 1% |
| gray8_512 | 512 | Paeth 66%, Up 29%, Sub 3% |
| gray16_512 | 512 | Paeth 66%, Up 29%, Sub 3% |
| palette8_512 | 512 | None 100% |
| gray1_512 | 512 | Sub 66%, None 29%, Paeth 3% |

**Paeth dominates**, and the two-row Paeth wavefront (`unfilter_paeth_pair`, `src/filter.rs:218`;
gated in `unfilter_image_bpp`, `src/filter.rs:319`) already covers it. Extending the wavefront to
Sub/Average would touch 1–5% of rows — not worth the code. Unfilter is effectively at its practical
optimum; the remaining inefficiency is that it runs as a **separate pass**.

### 1.4 Cross-library decode (port harness, interleaved best-of-30)

| fixture | `png` crate | png-spark | spark vs png |
|---|---:|---:|---:|
| gray8 512² | 0.71 ms | 0.57 ms | **−20%** |
| gray16 512² | 1.12 | 0.98 | **−12%** |
| gray1 512² | 0.27 | 0.37 | +27% |
| palette8 512² | 0.69 | 0.77 | +11% |
| rgb8 512² | 1.57 | 1.69 | +8% |
| rgba8 512² | 1.85 | 1.72 | **−7%** |
| rgb16 512² | 2.23 | 2.13 | **−4%** |
| rgb8 2048² | 21.33 | 24.17 | +13% |
| rgba8 3840×2400 | 55.95 | 52.20 | **−7%** |

Decode is within ±10% of the `png`+`fdeflate` stack on every class; png-spark already wins the
port's dominant class. The remaining headroom is **structural**, not micro.

### 1.5 Maturity evidence (context for adoption; not part of this design)

- **Byte-exactness:** 15-fixture matrix vs Pillow/libpng (incl. Adam7, palette, sub-byte, 16-bit):
  0.00% bytes differ, max Δ 0.
- **Adversarial:** 310,304 malformed inputs (truncations at spread offsets, single-bit flips,
  chunk-length corruption including `0x7FFFFFFF`/`0xFFFFFFFF`) through `decode`, `to_rgba8`,
  `to_rgb8` and `read_info`: **0 panics**.

---

## 2. The architectural gap

### 2.1 What `decode()` does today

```
Decoder::decode                       src/decoder.rs:218
 ├─ parse()                          src/decoder.rs:253   chunk walk, PLTE/tRNS/metadata, IDAT borrowed or joined
 ├─ zeroed_vec(decompressed+slack)   src/common.rs:366, :313; slack src/inflate.rs:27
 ├─ inflater.zlib(idat, &mut buffer) src/inflate.rs:532   ONE-SHOT whole stream; the output buffer IS the match window
 ├─ Interlacing::None:
 │    unfilter_image(buffer, …)      src/filter.rs:284    SECOND PASS, in place, then truncate  (decoder.rs:230–241)
 │  Interlacing::Adam7:
 │    deinterlace(info, buffer)      src/decoder.rs:593   pass-by-pass unfilter into a SECOND buffer + scatter
 └─ validate_palette_indices         src/decoder.rs:559
```

The inflate core is deliberately **not** a state machine — the README design notes and
`src/inflate.rs` explain why: PNG states the decompressed size up front, so the whole stream is
decoded in one call against one buffer, keeping the bit buffer and output cursor in registers and
removing per-symbol state checks and output clamping.

### 2.2 Consequences

1. **Two passes over the image buffer.** Inflate writes N bytes; unfilter reads N and writes N.
   For a 4K RGBA8 image (36.9 MB) that is ~110 MB of DRAM traffic where ~74 MB would do.
2. **Locality.** Reconstruction reads rows long after they left L2/L3 (36.9 MB ≫ typical caches).
3. **Adam7 holds ~2× the limit** at once — the decoder's own documentation says so
   (`src/decoder.rs:193–195`).
4. **No streaming.** The encoder can write to a sink band by band; the decoder requires the whole
   image resident and enforces `DEFAULT_MAX_DECOMPRESSED_SIZE` (512 MiB) as a hard ceiling.
5. **No fused conversion.** `decode()` then `to_rgba8()` is another full pass; a consumer wanting
   planar channels (this port) adds a third (de-interleave interleaved → planes).
6. **The doctrine applies to the inflate core, not to reconstruction.** Fusing the unfilter pass
   needs no resumable decoder at symbol granularity. That is the opening this proposal takes.

---

## 3. Proposal

### 3.1 Core mechanism — the reconstruction frontier (fused in-place unfilter)

Keep `Inflater::zlib` one-shot and register-resident. Add a hook called at **batch boundaries** in
the inflate loop — after a literal run, a match copy, or a stored block, wherever the output cursor
jumps (`src/inflate.rs`, main loop around `:851`) — with `(buffer, cursor)`.

The hook maintains `reconstructed_rows` and, while the cursor is at or past the end of the next
complete row, reconstructs that row **in place, behind the cursor**:

- compaction is the same forward `copy_within` the existing code already performs
  (`unfilter_image_bpp`, `src/filter.rs:301–347`; the "always a forward move" property is relied on
  there already, `src/filter.rs:324–325`);
- reconstruction is the existing per-row code (`unfilter_row` / `unfilter_paeth_pair`,
  `src/filter.rs:128/218`), against the previous **reconstructed** row;
- stop at `height * (1 + row_bytes)` — the last filter byte — never reconstruct from
  `OUTPUT_SLACK` (`src/inflate.rs:27`).

**The lag condition (mandatory, added during implementation):** inflate's match copies read
the output buffer at `pos - distance`, and the format caps `distance` at exactly 32768
(RFC 1951: largest distance code 24577 + 13 extra bits). A row — or a two-row `Paeth` pair —
may therefore be reconstructed only when

```text
cursor >= (r + take) * (1 + row_bytes) + 32768
```

which places everything the reconstruction writes at least one match window behind the
cursor, outside every future match's reach. The original text said "strictly behind the
write cursor"; that is necessary but not sufficient, and an implementation following it
corrupts any stream whose matches reach into recently-reconstructed rows. The final
`<= 32768/(1+row_bytes)`-ish rows are drained after inflation ends (at that point no match
can read anything, so the lag drops to zero), keeping `decode()` byte-identical to
`unfilter_image`. Streams no larger than one window plus one row can never trigger
mid-inflation reconstruction and take the plain two-pass path unchanged, so small decodes
pay nothing for the machinery.

Invariants: the cursor never moves backwards; reconstruction writes strictly behind it, by
at least the 32768-byte match window; a row is only touched when its bytes are final. All
three hold by construction.

**Cost:** one predictable branch per batch. The hook only does work when a row boundary has been
crossed, so the check is O(rows) — 2,400 calls for a 4K image — not O(symbols). The per-symbol
path gains no work.

**Adam7:** v1 keeps `deinterlace` as-is (correctness first). v2 can reconstruct pass rows as they
complete and call the existing scatter (`src/decoder.rs:631`) per row; the scatter target still
needs the full image buffer, so interlaced streaming is only possible with a caller-owned scatter
target (see 3.3).

### 3.2 Streaming — block-granular resumable inflate

The design choice that keeps the speed doctrine intact:

> **Resume at DEFLATE block boundaries, not at symbol boundaries.**

Dynamic Huffman tables are rebuilt per block anyway, so the entire resumable state is a bit
position (≤ 64 bits), the 32 KiB match window, and the output cursor. No per-symbol state machine;
the inner loop is unchanged.

**Implemented revision (Phase 2, supersedes the sketch below):** pure block-boundary resume leaves
peak memory bounded by one block's *output*, which is unbounded (a hostile stream can be a single
block), so the shipped seam is an **output-budget pause** — `zlib_segment` stops at a caller-set
budget anywhere, including mid-block, and returns a `SegmentPause` (bit position, output cursor,
mid-block flag). The pause is cheap because the loop's existing output-limit checks become the pause
sites, and the match-refusal path rewinds to the symbol start so resume never loses a match. The
decoder drives it with a bounded stage (window + ~256 KiB segment + four rows of headroom), a
`StreamingFrontier` that reverses rows in place and emits them through a sink under the §3.1 lag,
and a slide that relocates the stage window and keeps one reconstructed row beyond the frontier for
the lookback. Interlaced images take the buffered path (v1). Measured cost vs `decode()`: ≤ 5 % on
eight of nine fixture classes (see BENCHMARKS.md section 6).

The original sketch, kept for the API shape:

Internal API shape:

```rust
pub enum BlockOutcome { More, Final }

impl Inflater {
    /// Inflates until the current DEFLATE block ends, appending to the window.
    /// State persists across calls: bit position, window, cursor.
    pub fn next_block(&mut self, input: &[u8], window: &mut Window) -> Result<BlockOutcome, InflateError>;
}
```

`Window` is a ring of at least DEFLATE's 32 KiB history for the **filtered** stream (inflate
references), plus row assembly for reconstruction. Note what is *not* needed: the reconstructed
rows are not part of inflate's history, so they can be emitted and dropped as soon as the next
row's reconstruction is done. The reconstruction lookback is one row (two for the Paeth pair
path), so the reconstructed side is O(row_bytes).

Public API (names to be settled with the maintainer):

```rust
impl Decoder {
    /// Decode to a sink, one reconstructed row at a time, in the file's native layout.
    pub fn decode_to(&mut self, png: &[u8], sink: impl FnMut(Row<'_>) -> Result<(), E>)
        -> Result<(), E>;
}

pub struct Row<'a> {
    pub index: usize,               // row in the output image
    pub layout: NativeLayout,       // colour type + bit depth (file's own format)
    pub bytes: &'a [u8],            // one scanline, filter byte removed
}
```

With this, decode needs **O(32 KiB + 3 × row_bytes)** instead of O(W × H). For interlaced images
the sink carries `(pass, x, y)` for a caller-owned scatter target (v2), or the buffered path is
used (v1). The size ceiling becomes a per-buffer cap rather than a hard limit: a 100k × 100k header
with `max_decompressed_size(None)` decodes in O(width) memory under streaming — a capability the
crate does not have today.

### 3.3 Fused conversion and the row-callback form

- The callback hands out rows in the **file's native layout** — the crate's stated doctrine
  ("Decoding gives you the file's own format").
- Optional adapters `decode_to_rgba8(sink)` / `decode_to_rgb8(sink)` mirror
  `encode_rgba8`/`encode_rgb8` and reuse the already row-oriented conversion helpers in
  `src/transform.rs` (`grey_row`, `rgb_row`, `rgba_row`, `indexed_row`, `:128–214`): one pass
  instead of decode + convert.
- **Consumer benefit (this port):** a callback that writes straight into planar channel planes
  turns the current pipeline — decode (interleaved) → de-interleave (planes) — into one pass from
  filtered bytes to planes, eliminating the interleaved buffer and a full copy. This is the part
  that makes the port faster, not just png-spark.

### 3.4 Compatibility

- `decode()` keeps its signature and output; internally it becomes the fused path (strictly less
  work). Its peak stays O(W × H) because it returns the whole image.
- New entry points are additive: `decode_to`, `decode_rows`, `decode_to_rgba8`/`decode_to_rgb8`.
- `read_info`, `Keep`, `Checks` (including `Checks::None`, `src/decoder.rs:343`),
  `max_decompressed_size` unchanged.
- No new dependencies, no threads, no `unsafe` outside the existing inflate core; the rolling
  unfilter is safe code.

---

## 4. Why this aligns with png-spark

- **It is the mirror of the crate's own encoder design.** The README's design notes: "Encoding
  works in bands… what is held grows with the image's width but not with its height."
  `Encoder::encode_to` (`src/encoder.rs:166`) delivers that. The read side is asymmetric today;
  this proposal closes the asymmetry with the same idea.
- **It preserves the performance doctrine.** The one-shot register-resident inflate core stays;
  the resumable seam is placed where DEFLATE already has seams (blocks) and where state is
  naturally rebuilt, not per symbol.
- **Zero dependencies, no background threads, spec-compliant, byte-exact** — the crate's identity
  is untouched.
- **It turns the 512 MiB ceiling from a policy limit into a non-issue** for streaming callers,
  and it makes the decoder usable in pipelines that process rows (image converters, ports) without
  a whole-image buffer.
- It is a design the author's own encoder already argues for — which is the strongest case for
  upstream acceptance.

---

## 5. Expected impact (honest estimates)

| Effect | Estimate | Basis |
|---|---|---|
| Fused reconstruction (one less pass) | **5–15%** on large images | unfilter share 25–40%, mostly compute-bound; fusion saves the DRAM read and restores locality |
| Streaming decode memory | **O(32 KiB + rows)** vs O(W × H) | design |
| Fused conversion / planar callback | **one pass saved** per consumer | port today: decode + de-interleave |
| Adam7 peak | 2× limit → 1× buffered; O(rows) with caller scatter | design |
| x86 hardware CRC (PCLMULQDQ) | 2–4% | measured CRC share; **separate, easy, orthogonal** |
| Inflate micro (wider match copies, etc.) | 2–8%, unmeasured | needs match/literal instrumentation; separate track |

**There is no 2× in this design, and the document should say so plainly.** Decode is DEFLATE-bound;
the inflate core already beats `fdeflate` by ~18%; the state of the art beyond it (libdeflate-class)
is years of C micro-work, not a patch. "Much more" comes from *not doing work twice* (fusion),
*not allocating the whole image* (streaming), and *not copying between layouts* (callback) — not
from a faster Huffman decoder.

---

## 6. Alternatives considered

1. **Symbol-level resumable decoder.** Rejected: per-symbol state checks and output clamping are
   exactly what the current design removed for speed (README design notes; `src/inflate.rs`).
2. **Threaded pipeline** (inflate thread + reconstruction thread). Could overlap the ~60% and
   ~35% stages for up to ~1.4× on two cores for large images, but adds threads to a zero-thread
   library, needs a handoff buffer with backpressure, and duplicates memory. Worth a separate
   discussion; not proposed here.
3. **SIMD unfilter.** Paeth is a serial predictor; the two-row wavefront already extracts the ILP
   (`src/filter.rs:218`); filters are not the bottleneck.
4. **Extending the wavefront to Sub/Average.** 1–5% of rows; not worth the code.
5. **x86 hardware CRC-32 (PCLMULQDQ).** Real but small (2–4%); orthogonal; can land on its own.
6. **Wider match copies in inflate.** Micro; measure match/literal composition first.

---

## 7. Implementation plan

### Phase 1 — fused in-place reconstruction (no API change) — DONE (`ec53075`, `0dbc3dc`)

1. Add a progress hook to the inflate loop (`src/inflate.rs`, main loop ~`:851`), called at batch
   boundaries when the cursor has advanced. Keep it out of the per-symbol path; one predictable
   branch per batch.
2. Implement the frontier in `src/decoder.rs` (or `src/filter.rs`) using the existing per-row code
   from `unfilter_image_bpp` (`src/filter.rs:301`): track `reconstructed_rows`; while
   `cursor >= (row + 1) * (1 + row_bytes)`, compact + reconstruct row `row`, `row += 1`.
3. Stop at `height * (1 + row_bytes)`; never reconstruct from `OUTPUT_SLACK`. Keep the existing
   `deinterlace` path for Adam7.
4. **Gates (extended during implementation):** `cargo test` (74 tests); byte-exact output on the
   generated 180-file corpus, the PngSuite interlaced samples, and the port's 14 fixtures; Miri;
   all three fuzz targets; A/B with
   `cargo run --release -p png-spark-bench -- decode` **and** the port's interleaved 3-way harness
   (best-of-N; this machine throttles).
   **The existing gates are blind to this change.** Every corpus image is smaller than the 32 KiB
   match window, so a frontier whose lag is wrong passes all of them: mid-inflation reconstruction
   simply never engages. Worse, png-spark's *own encoder* emits only zero-run (distance-1)
   matches, so encoder-generated fixtures cannot detect a broken lag either — the fixtures must
   come from a real match finder. Three additions close the gap:
   - `tools/gen_large_fixtures.py` generates nine non-interlaced PNGs up to 36.9 MB filtered,
     compressed by CPython's zlib (full LZ77), each asserted to exceed the window; skipping when
     absent, matching the corpus tests.
   - `tests/fused_reconstruction.rs` requires `decode()` to be byte-identical to the legacy
     two-pass path (inflate everything, then `unfilter_image`) over those fixtures, and asserts
     the set is large enough to force mid-inflation reconstruction.
   - unit tests pin frontier == `unfilter_image` across every filter, stride, `Paeth`-run
     alignment, invalid-filter row reporting, and assert the frontier actually reconstructs rows
     mid-stream (including the two-row wavefront) once the cursor passes the window.

### Phase 2 — block-granular resumable inflate + streaming API — DONE, revised (`2a44c37`)

See §3.2's implemented revision: shipped as an output-budget mid-block pause (`zlib_segment` +
`SegmentPause`) rather than pure block-boundary resume, closing the unbounded-block-output hole.

5. Refactor `Inflater` to expose `next_block` (state: bit position + window + cursor); implement
   `zlib()` on top of it so the one-shot path is literally the same code.
6. Add `Window` (32 KiB ring + row assembly) and `decode_to` / `decode_rows`; interlaced uses the
   buffered path in v1.
7. **Tests:** streaming roundtrip vs the existing decoder over the whole corpus; a streaming fuzz
   target; a memory-bound test (large header + `max_decompressed_size(None)` decodes in O(width)).

### Phase 3 — fused conversion + callback metadata

8. Row callback carrying native layout (and `(pass, x, y)` for Adam7); `decode_to_rgba8` /
   `decode_to_rgb8` adapters reusing `src/transform.rs` row helpers.
9. **Consumer validation:** wire the port's `decode_source_raster` to the callback form (one pass
   to planar channels) and measure end-to-end.

### Phase 4 — upstream contact

10. Only after the port team agrees. See §9.

---

## 8. Risks

- **Hot-loop perturbation — the one real risk.** Any extra work in the inflate loop can cost more
  than fusion saves. Mitigate: hook only at batch boundaries; keep the branch out of the per-symbol
  path; measure every step; consider `#[inline(always)]` on the hook check and `#[cold]` on the
  reconstruction body.
- **Block-boundary resumability.** Care needed with `bfinal`, stored blocks, and carrying the bit
  buffer across calls; the window must be exactly DEFLATE's 32 KiB. Test with the 224 zlib vectors
  (`tests/inflate_vectors.rs`) plus the `inflate` fuzz target.
- **Adam7 streaming.** Scatter needs a caller-owned image; keep buffered in v1.
- **API surface.** Additive; names settled with the maintainer.
- **Doctrine perception.** State the case explicitly: the one-shot core is preserved; the seam is
  at the format's own block boundaries.

---

## 9. Upstreaming — do not act without the port team's approval

- The repo has merged a PR before (`6d256fc`, PR #1, the streaming encoder), so contributions are
  plausible; **issue creation is restricted**, so the channel is a PR or direct contact.
- Recommended sequence: implement and benchmark in this checkout first (it is our working fork);
  decide with the team whether to PR or vendor. If upstream doesn't take it, **this checkout is the
  vendor**.
- Keep our additions (`bench/src/bin/`, `bench/tools/`, this document) out of any PR unless wanted;
  `bench/` is excluded from the published package.

---

## 10. Measurement appendix

**Tools in this checkout**

| Tool | Path | Purpose |
|---|---|---|
| Stage profiler | `bench/src/bin/profile.rs` | decode / crc / inflate / unfilter breakdown on a fixtures directory |
| Filter histogram | `bench/tools/filters.py` | per-fixture scanline filter distribution |
| Corpus color-type scan | `bench/tools/scan_png_mix.py` | PNG signatures + IHDR colour types in a document corpus |
| Their own bench | `cargo run --release -p png-spark-bench -- decode` | whole-PNG decode vs the `png` crate |

**Commands**

```bash
cargo run --release -p png-spark-bench --bin profile -- <fixtures dir>
python bench/tools/filters.py <fixtures dir>
cargo run --release -p png-spark-bench -- decode
```

**Port harness (external, may not persist):** `%TEMP%\opencode\png-bench` — interleaved 3-way
decode (png / zune-png / png-spark) plus a mutation-campaign binary (`src/bin/mutate.rs`).

**Methodology notes**

- Best-of-N only; interleave arms; one session per comparison. Thermal throttling on this machine
  swings sustained decode by >30%.
- The unfilter figure measured in isolation is pessimistic (cold buffer); in situ it is lower.
- `Checks::None` already exists to skip CRC verification (2–5%) when a caller doesn't want it.
