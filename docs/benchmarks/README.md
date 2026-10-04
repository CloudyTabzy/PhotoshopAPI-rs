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
| Rust build | `cargo build --release` at `d1328e5` (hardened as 0.13.32 — no perf-relevant changes since) |
| Protocol | Five runs per implementation and document, warm file cache, alternating implementation and file order, 1.5 s pause between runs |
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
| `Compression_Mixed_8bit.psd` | 9.81 | **1.19** | 22.84 | **1.71** | 33.31 | **2.95** |
| `CMYK_16.psd` | 79.59 | **5.37** | 548.67 | **15.10** | 627.44 | **20.58** |
| `example.psd` | 29.03 | **1.80** | 17.03 | **1.78** | 46.18 | **3.75** |
| `smart_object_file_no_warp.psd` | 28.87 | **5.20** | 222.13 | **3.92** | 251.00 | **9.02** |

Synthetic large documents (ms):

| Document | Read + extract C++ | Read + extract Rust | Write C++ median (range) | Write Rust median (range) |
|---|---:|---:|---:|---:|
| `big8.psd` (435 MB) | 334.1 | **157.5** | 672.9 (286.2–4052.0) | 2670.9 (160.5–3866.6) |
| `big16.psd` (252 MB) | 267.6 | **195.0** | 624.6 (595.3–1019.7) | 2375.2 (347.7–2579.6) |

**Read the write medians with care.** On a nearly-full disk both writers hit
multi-second stalls from OS dirty-page throttling; the *ranges* are the honest
part. Rust's write floor (160.5 / 347.7 ms) sits below C++'s (286.2 / 595.3 ms),
and a later quieter-session re-run of `big8` produced write times of
161.8 / 282.0 / 337.4 / 632.2 / 1159.7 ms — median 337 ms. Neither side
`fsync`s; the measured wall clock is mostly the OS accepting and draining
pages, and the faster byte producer hits the throttle more often.

## Phase-level detail (indicative single-session)

Same machine, warm cache, ranges where runs differed. The canonical numbers
are the medians above; this split shows *where* the time goes.

`big8.psd`:

| Phase | C++ | Rust |
|---|---:|---:|
| Raw read (lazy, compressed payloads retained) | n/a | 118–214 |
| Read (eager decode) + extract | ~206–235 read + 139–144 extract | 120–160 total |
| Channel decode only | ~142 | **77.5** |
| Write | 268–319 | **160.5–449** |

`big16.psd`:

| Phase | C++ | Rust |
|---|---:|---:|
| Raw read (lazy) | n/a | 57–114 |
| Read + extract | ~197–210 read + 64–66 extract | 173–260 total |
| Channel decode only | **~65** | ~137–159 |
| Write | 552–1093 | **347.7–480** |

Small/medium documents, read side (C++ carries a ~30 ms floor per `read` that
Rust does not):

| Document | C++ read | Rust eager read |
|---|---:|---:|
| `example.psd` | 32.5 | **0.80** |
| `qual_rca_pinout.psd` (6 MB) | 35.7 | **3.22** |
| `smart_object_file_no_warp.psd` | 34.3 | **5.19** |
| `SmartObject.psd` (5.8 MB) | 115.5 | **3.58** |

## Codec-level decode — the remaining gap

Isolated 16-bit ZIP-prediction channel decode is the one phase where upstream
still leads: **~65 ms vs ~137–159 ms** on `big16`. That is a codec-level
difference (their parallel libdeflate path vs our linflate + rayon job
scheduling), not an architectural one — end-to-end `big16` read+extract still
favors Rust (195 vs 268 ms) because parsing dominates the remaining time.
It is the honest first item on any future performance list.

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
| Small/medium read+extract | **Rust** | 5.6×–42× |
| Large 8-bit read+extract | **Rust** | 2.1× |
| Large 16-bit read+extract | **Rust** | 1.4× |
| Small/medium write | **Rust** | 1.7×–52× |
| Large write (floor, disk-OK) | **Rust** | ~1.8× (160 vs 286 ms) |
| Large write (median, throttled disk) | C++ | see caveat above |
| Isolated 16-bit ZIP-pred decode | **C++** | ~2.2× |
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
