# Benchmarks — Rust port vs C++ upstream

Head-to-head measurements of this port against the C++20 upstream
([EmilDohne/PhotoshopAPI](https://github.com/EmilDohne/PhotoshopAPI) v0.9.1),
phase by phase, with the wins *and* the losses recorded. The summary tables
also appear in the project README; everything below that is the detail behind
them.

## Test setup

| | |
|---|---|
| Machine | 13th Gen Intel Core i7-13620H, 16 GB RAM, Samsung NVMe SSD |
| OS | Windows 11 |
| C++ build | Upstream v0.9.1, MSVC Release, system vcpkg deps (OpenImageIO, libdeflate, Eigen3) |
| Rust build | `cargo build --release`, 0.13.32 (post-hardening master) |
| Protocol | Five runs per implementation and document, warm file cache, alternating implementation and file order, 1.5 s pause between runs |
| Measured | 2026-10-04 |
| Reported | Median milliseconds; write ranges given where they matter |

**Phase mapping.** Upstream decodes PSD compression during `read` and stores
pixels internally compressed; `get_image_data()` extracts that storage into
typed buffers. Rust's lazy read retains the PSD streams;
`decode_all_layer_pixels()` decodes them. The fair comparison is therefore
**read + extract** against **read + extract**:

- C++: `LayeredFile<T>::read` → `get_image_data()` per `ImageLayer` → `write`
- Rust: `LayeredFile::read_with_options(raw)` → `decode_all_layer_pixels()` → `write`

Asymmetries, stated plainly: upstream's extract visits only `ImageLayer`
objects while Rust materializes other layer kinds too (more work on the Rust
side), and upstream re-embeds smart-object payloads through OpenImageIO on
write while Rust preserves the linked bytes verbatim (less work, higher
fidelity, on the Rust side).

## Documents

| Document | Size | Depth / mode | Contents | Origin |
|---|---:|---|---|---|
| `example.psd` | 0.28 MB | 8-bit RGB | Text layers, CJK/emoji, effects | NAVER WEBTOON fixture |
| `Compression_Mixed_8bit.psd` | 0.64 MB | 8-bit RGB | Mixed RLE/ZIP channels | Upstream test corpus |
| `CMYK_16.psd` | 2.4 MB | 16-bit CMYK | CMYK channels | Upstream test corpus |
| `smart_object_file_no_warp.psd` | 5.3 MB | 8-bit RGB | Smart object, no warp | Upstream test corpus |
| `big8.psd` | 435 MB | 8-bit RGBA | 12 layers @ 4000×3000, opaque α | Generated (see README) |
| `big16.psd` | 252 MB | 16-bit RGBA | 10 layers @ 2000×3000 | Generated (see README) |

The synthetic generators are deterministic (seeded LCG) and documented in the
main README so the inputs can be reproduced.

## Results — five-run medians

Corpus documents (ms, lower is better):

| Document | Read + extract C++ | Read + extract Rust | Write C++ | Write Rust | Total C++ | Total Rust |
|---|---:|---:|---:|---:|---:|---:|
| `Compression_Mixed_8bit.psd` | 10.93 | **1.12** | 27.55 | **1.74** | 38.10 | **3.02** |
| `CMYK_16.psd` | 85.34 | **4.99** | 560.07 | **9.87** | 646.03 | **15.20** |
| `example.psd` | 36.71 | **1.62** | 17.52 | **1.56** | 53.98 | **3.39** |
| `smart_object_file_no_warp.psd` | 33.85 | **5.52** | 230.48 | **3.65** | 264.50 | **9.35** |

Synthetic large documents (ms):

| Document | Read + extract C++ | Read + extract Rust | Write C++ median (range) | Write Rust median (range) | Total C++ | Total Rust |
|---|---:|---:|---:|---:|---:|---:|
| `big8.psd` (435 MB) | 349.41 | **156.75** | **330.71** (285.8–922.2) | 652.95 (166.0–1820.1) | **717.39** | 815.24 |
| `big16.psd` (252 MB) | 276.76 | **199.86** | 600.42 (592.9–619.0) | **360.04** (355.7–1020.0) | 884.48 | **560.14** |

**Read the write medians with care.** Neither side `fsync`s, so wall clock
includes OS dirty-page writeback, and the faster byte producer hits the
throttle more often. On this run `big16` already favors Rust on the median
(360 vs 600 ms). `big8` favors C++ on the median (331 vs 653 ms) — but Rust's
median rode a single 1.8 s stall; its best run (166.0 ms) sits far below
C++'s best (285.8 ms). The honest summary: Rust's write floor is lower on
both files; `big8`'s median remains throttle-sensitive.

## Phase-level detail

Same five-run session, median per phase — where the time actually goes.
Rows marked *earlier session* come from an identical-protocol run before the
write hardening and are included for breadth, not precision.

`big8.psd` (medians):

| Phase | C++ | Rust |
|---|---:|---:|
| Raw read (lazy, compressed payloads retained) | n/a | **118.7** |
| Read | 204.8 | (eager read 118.8) |
| Extract / decode all layers | 144.6 | **38.9** |
| Write | 330.7 (285.8–922.2) | 652.9 (166.0–1820.1) |

`big16.psd` (medians):

| Phase | C++ | Rust |
|---|---:|---:|
| Raw read (lazy) | n/a | **56.1** |
| Read | 208.4 | (eager read 177.8) |
| Extract / decode all layers | **68.4** | 143.9 |
| Write | 600.4 (592.9–619.0) | **360.0** (355.7–1020.0) |

Small/medium documents, read side (C++ carries a ~30 ms floor per `read` that
Rust does not):

| Document | C++ read | Rust eager read |
|---|---:|---:|
| `Compression_Mixed_8bit.psd` | 10.5 | **0.75** |
| `CMYK_16.psd` | 84.7 | **4.12** |
| `example.psd` | 35.6 | **1.04** |
| `smart_object_file_no_warp.psd` | 31.0 | **4.94** |
| `qual_rca_pinout.psd` (6 MB, earlier session) | 35.7 | **3.22** |
| `SmartObject.psd` (5.8 MB, earlier session) | 115.5 | **3.58** |

## Codec-level decode — the remaining gap

Isolated 16-bit ZIP-prediction channel decode is the one phase where upstream
still leads: **~68.4 ms vs ~143.9 ms** on `big16` (≈2.1×). On 8-bit RLE the
positions invert hard — **144.6 ms C++ vs 38.9 ms Rust** (≈3.7× our way). The
`big16` gap is codec-level (their parallel libdeflate path vs our linflate +
rayon job scheduling), not architectural: end-to-end `big16` read+extract
still favors Rust (199.9 vs 276.8 ms) because parsing dominates the remaining
time. It is the honest first item on any future performance list.

## In-memory serialization (disk removed from the equation)

`to_bytes` on eager-read documents — the pure encoder cost:

| Document | Rust `to_bytes` |
|---|---:|
| `big8.psd` | ~125 ms |
| `big16.psd` | ~320 ms |

Stable across runs. This is the control experiment behind "the write variance
is storage, not encoding": the same encode that produced 134 ms–6.4 s saves to
disk produces ~125 ms in memory.

## Heap allocation peaks

MiB, synthetic documents, source file cache and thread stacks excluded:

| Operation | Rust |
|---|---:|
| Eager read, `big8` | 549.5 |
| Write, `big8` | 551.6 |
| `to_bytes`, `big8` | 965.8 |
| Eager read, `big16` | 458.4 |
| Write, `big16` | 537.7 |
| `to_bytes`, `big16` | 788.0 |

Bulk extraction runs under an estimated 128 MiB codec workspace budget (256 MiB
default for writes); a lazy read needs no memory beyond the document itself.

## Scorecard

| Front | Faster | By how much |
|---|---|---|
| Small/medium read+extract | **Rust** | 6.1×–22.7× |
| Large 8-bit read+extract | **Rust** | 2.2× |
| Large 16-bit read+extract | **Rust** | 1.4× |
| Small/medium write | **Rust** | 11×–63× |
| Large write (best run) | **Rust** | 1.7×–1.8× (166 vs 286, 356 vs 593 ms) |
| Large write (median) | split | C++ on `big8` (331 vs 653), Rust on `big16` (360 vs 600) |
| Isolated 8-bit RLE channel decode | **Rust** | ~3.7× |
| Isolated 16-bit ZIP-pred decode | **C++** | ~2.1× |
| Smart-object save | **Rust** | ~370× (OIIO re-embed vs byte passthrough) |

## Coverage and fidelity asymmetries

Not speed, but worth recording alongside it:

- Upstream **rejects** `4901393.psd` (real-world file with an RLE size quirk)
  and `smart-filters/src.psd` (`PlacedLayerTaggedBlock` unimplemented); this
  port reads both.
- Upstream's write path re-embeds smart-object sources through OpenImageIO —
  1953 ms on `SmartObject.psd` — where this port preserves the linked bytes
  verbatim in 4.7 ms.
- Upstream extract skips smart-object layers (not `ImageLayer`s); Rust
  materializes them, so its extracted pixel counts are higher at equal
  document.

## Charts

`make_plots.py` regenerates all images in this directory from the headline
medians:

```text
py make_plots.py
```

Per-document `*_combined_plot.png` files plus `corpus_graphs.png` and
`synthetic_graphs.png` overviews. Write bars on the synthetic charts show the
best run as the solid bar with a min..max band and a median notch, for the
reasons above.

## Reproducing

```text
cargo run -p psd --release --example rw_bench -- --out <dir> <file.psd> ...
```

The C++ side was an argv-driven Release harness timing
`LayeredFile<T>::read`, `get_image_data()` per `ImageLayer`, and
`LayeredFile<T>::write`, built against the upstream tree with vcpkg deps. The
corpus documents are in `fixtures/documents/` and the upstream
`PhotoshopTest/documents/` tree; the synthetic generators are documented in
the main README.
