# PhotoshopAPI-rs

[![CI](https://github.com/CloudyTabzy/PhotoshopAPI-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/CloudyTabzy/PhotoshopAPI-rs/actions/workflows/ci.yml)
[![License: BSD-3-Clause](https://img.shields.io/badge/license-BSD--3--Clause-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.96%2B-orange.svg)](https://www.rust-lang.org)
[![Crates.io](https://img.shields.io/crates/v/psd.svg)](https://crates.io/crates/psd)

> [!NOTE]
> **PhotoshopAPI-rs is a young port.** The on-disk format work is broad and
> round-trip tested, but the API is still settling and bugs are likely. Please
> report anything you find on the issues page.

---

## About

**PhotoshopAPI-rs** is a pure-Rust library for reading, writing and editing
Photoshop documents (`.psd` / `.psb`), with Python bindings. It is a
from-scratch port of the C++20 [EmilDohne/PhotoshopAPI](https://github.com/EmilDohne/PhotoshopAPI)
v0.9.1, which is in turn based on a C++ PSD SDK, a Python PSD writer, and the official
[Photoshop File Format Specification](https://web.archive.org/web/20231122064257/https://www.adobe.com/devnet-apps/photoshop/fileformatashtml/).

Like upstream, the goal is layer editing as a first-class citizen across **all
three bit depths** (8, 16 and 32-bit) — not just reading pixels out of a
flattened composite. Unlike upstream, you do not need a C++ toolchain, CMake,
vcpkg, or a C++ dependency tree to use it.

## Why a Rust port?

The original request was narrow: *port this C++ library so it can be embedded in
a Rust service.* Once the port existed, the interesting question became what a
port is actually **for** — because a transliteration would have preserved
upstream's C++ architecture bugs along with its features. So this port follows
one rule:

> **Reshape, don't transliterate.**
> Idiomatic Rust wins over upstream fidelity — *except* where bytes matter. If
> Photoshop parses it, it must be byte-exact.

Everything below follows from that split. The format layer is deliberately
conservative, because there the only correct implementation is the one
Photoshop accepts. The document API is deliberately aggressive, because
`shared_ptr` graphs and `dynamic_pointer_cast` are not how Rust expresses
"a layer that is sometimes an image and sometimes a group".

## What this port does differently

This is the substance of the port, not a feature checkbox.

### A clean dependency ladder

Five crates, each with a strictly narrower job than its neighbour:

| Crate | Responsibility | Depends on |
|---|---|---|
| `psd-core` | Raw on-disk sections, tagged blocks, descriptors | *(nothing)* |
| `psd-codecs` | RLE / ZIP / ZIP-prediction / endian / interleave | *(nothing)* |
| `psd-png` | Streaming, memory-bounded PNG decoder (vendored fork) | *(nothing)* |
| `psd` | The `LayeredFile<T>` document API | `psd-core`, `psd-codecs` |
| `psd-py` | Python bindings (PyO3) | `psd` |

`psd-core` knows nothing about pixels and `psd-codecs` knows nothing about
Photoshop, so each is testable in isolation — the format layer has no codec
dependency at all, and synthesizes the merged composite's bytes itself.

`psd-png` is a fork of [png-spark](https://github.com/stephenberry/png-spark)
0.2.0 by Stephen Berry, vendored here whole. It decodes a PNG one scanline at
a time while holding a few hundred kilobytes, whatever the image's height, and
its design and measurements are in `crates/psd-png/docs/`. It is MIT OR
Apache-2.0 rather than BSD 3-Clause; see the third-party section of `LICENSE`.
It is currently a **dev-dependency only**: a differential test decodes every
corpus PNG through both it and the `image` crate and requires byte equality, but
nothing in the read path uses it yet.

### An arena layer tree, addressed by identity and by path

Upstream builds a `shared_ptr` graph and recovers layer type with
`dynamic_pointer_cast`. Here a document owns a `Vec<Layer<T>>` with stable,
never-reused `LayerId`s, layer kinds are an enum match, and nesting is an index
list. Layers are also addressable the way a person thinks about them:

```rust
let layer = document.find_layer("Effects/Drop Shadow")?;
```

### Typed errors instead of throw-on-log

Upstream logs at `Error` severity and *then* throws `std::runtime_error`.
Everything here returns `Result<_, PsdError>`, and recoverable anomalies are
`tracing::warn!` + continue. A corrupt descriptor nesting depth is a bounded
error, not a stack overflow.

### Byte-exact passthrough, and then some

Unknown tagged blocks round-trip byte-for-byte (upstream parity). Unknown
**image resources** also round-trip here — upstream silently drops them, which
loses XMP metadata, thumbnails and guides on every save.

### Text edits splice bytes instead of re-serializing

Text layers are edited by patching the raw `EngineData` payload at recorded
byte spans. Unknown keys, formatting and numeric spellings outside the edited
range survive untouched, and multi-block (`TySh`) edits are all-or-nothing. A
`Txt2` text cache is invalidated automatically when text changes, rather than
requiring the caller to remember.

### Descriptor key encoding is observed, not guessed

Photoshop writes some descriptor keys with a zero-length marker and others with
an explicit length, deciding via a large hardcoded list. This port records how
each key was encoded on read and reproduces it on write — byte-exact without
maintaining the list. Keys the port creates follow the one rule real files show
(a four-byte ID is a zero-length character ID, anything longer is explicit),
which held for every descriptor in about 420 Photoshop-authored documents.

### Smart objects are typed, not descriptor soup

Normal and quilt warps are real types. Geometry is Bézier surfaces, quad meshes
and homographies; rendering is supersampled and verified against Photoshop's own
reference renders. Linked sources decode from JPEG/PNG or a linked PSD/PSB
composite, and replacement is transactional.

## Feature support

**Supported**

- Read and write `.psd` and `.psb`
- Nested groups, layer insert / move / remove, group dividers kept paired
- Editable text layers: create, style, inspect, range-edit, remap on reflow
- Smart objects: create, replace, transform, warp, extract
- Layer effects: read as typed models, and create or edit drop and inner shadows, glows,
  bevel and emboss, colour, gradient and pattern overlays, satin and strokes (several
  instances of each repeatable effect included). An edit changes what it names and keeps
  every other byte, and refreshes the legacy `lrFX` block beside the descriptor
- Adjustment and fill layers: typed settings for every recognized block kind, byte-exact
  payload writing, layer creation with adjustment or canvas-sized fill bounds, and block edits
- Shape layers: create legacy and modern fills from typed vector masks, strokes, and live-shape
  blocks; vector records and unknown path bytes are preserved
- Artboards: create artboard groups, edit their bounds and backgrounds, and maintain document
  artboard settings through Rust and Python
- Python access to effects, adjustment, vector, and artboard blocks, including adjustment/fill,
  shape, and artboard creation
- Pixel and group masks
- Layer attributes: name, blend mode, opacity, fill, lock, clipping, display color
- ICC profile and DPI
- 8-, 16- and 32-bit documents
- RGB, CMYK and Grayscale color modes
- Raw, RLE, ZIP and ZIP-with-prediction compression
- Optional lazy layer and mask channels with compressed-payload passthrough
- Unknown tagged blocks and image resources preserved byte-exactly
- Photoshop-style layer path lookup

**Not supported**

- Lab and Multichannel color modes
- A valid merged composite image. Saves write a zeroed RLE composite, matching
  upstream: it keeps files 20–50% smaller, but third-party readers that require
  a real flattened image (Lightroom, for example) will not have one.

## Requirements

- Rust 1.96 or newer
- A 64-bit system; Linux, Windows or macOS

The codecs are scalar code that LLVM auto-vectorizes, parallelized across
scanlines with `rayon`; there is no hand-written AVX2 path. The vendored
`psd-png` codec is the one exception: it carries a compile-time-selected SSE2
`Paeth` reconstruction kernel, which is x86-64 baseline, and aarch64 NEON
kernels for checksums — no runtime CPU feature detection anywhere.

## Install

### Rust

```bash
cargo add psd
```

The `image` feature enables JPEG/PNG decoding for smart-object sources.

### Python

```bash
pip install photoshopapi-rs
```

Requires Python 3.9 or newer. Wheels are built with
[maturin](https://github.com/PyO3/maturin).

## Quickstart

The primary type is `LayeredFile<T>`, where `T` is `u8`, `u16` or `f32` for the
three bit depths. Layers are addressed by `LayerId` or by path.

### Rust

```rust
use psd::core::ColorMode;
use psd::{ChannelKey, Layer, LayeredFile, Rect};

fn main() -> psd::core::Result<()> {
    let (width, height) = (64u32, 64u32);

    // 8-bit RGB document; use `u16` or `f32` for 16-/32-bit.
    let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, width, height)?;

    // Rect::new takes (top, left, bottom, right).
    let mut layer = Layer::new_image("Layer Red", Rect::new(0, 0, height as i32, width as i32));

    // Channels are planar, one buffer per channel, keyed by ChannelKey.
    let samples = (width * height) as usize;
    let pixels = layer.image_mut().unwrap();
    for channel in 0..3u8 {
        let value = if channel == 0 { 255 } else { 0 };
        pixels.set_channel(ChannelKey::color(channel), vec![value; samples]);
    }

    // Layer settings are plain fields, and can still be changed after
    // insertion: the on-disk record is only finalized on write.
    layer.opacity = 128;
    document.add_layer(layer);

    document.write("WriteSimpleFile.psd")
}
```

Reading is just as direct, and any path in the tree resolves:

```rust
use psd::LayeredFile;

fn main() -> psd::core::Result<()> {
    let document = LayeredFile::<u8>::read("input.psd")?;
    if let Some(shadow) = document.layer_by_path("Effects/Drop Shadow") {
        println!("{} @ {}% opacity", shadow.name, shadow.opacity);
    }
    Ok(())
}
```

To inspect a document before loading its pixels, retain layer and mask
channels in compressed form. Decode only the channels or layers you use:

```rust
use psd::{LayeredFile, ReadOptions};

fn main() -> psd::core::Result<()> {
    let options = ReadOptions::default().with_raw_data(true);
    let mut document = LayeredFile::<u8>::read_with_options("input.psd", options)?;
    if let Some(id) = document.find_layer("Effects/Drop Shadow") {
        document.decode_layer_pixels(id)?;
    }
    document.write("output.psd")
}
```

The default reader decodes eagerly. A lazy read keeps each compressed payload
until its channel is decoded or replaced, and writes untouched payloads with
their original compression, straight from where the document holds them: saving
a lazy document needs no memory beyond the document itself. `ChannelStore::get` returns `None` for a raw channel;
`is_raw` distinguishes it from an absent channel. The default 2 GiB decoded
channel budget also applies when lazy channels are decoded later. The merged
composite remains outside this read path.

### Python

```python
import numpy as np
import photoshopapi as psapi

width, height = 64, 64
document = psapi.LayeredFile_8bit(psapi.enum.ColorMode.rgb, width, height)

pixels = np.zeros((3, height, width), np.uint8)
pixels[0] = 255
layer = psapi.ImageLayer_8bit(pixels, "Layer Red", width=width, height=height)

# Adjust properties any time before writing.
layer.opacity = 0.5
document.add_layer(layer)

document.write("WriteSimpleFile.psd")
```

## Workspace layout

```
crates/psd-core/     raw format: sections, tagged blocks, descriptors, strings
crates/psd-codecs/   pure byte codecs: PackBits, ZIP, prediction, endian, interleave
crates/psd/          the document API, smart objects, warps, rendering, text
crates/psd-py/       PyO3 bindings
fixtures/            vendored test corpus
```

## Dependencies

This port does use dependencies — it is not dependency-free. The point of the
crate split is that most of them are confined to one layer, and the core format
and codec crates pull in almost nothing.

### Direct dependencies

| Crate | Dependency | License | Why |
|---|---|---|---|
| `psd-core` | [`thiserror`](https://crates.io/crates/thiserror) 2.0 | MIT OR Apache-2.0 | Derive the `PsdError` enum |
| `psd-core` | [`tracing`](https://crates.io/crates/tracing) 0.1 | MIT | `warn!` for recoverable anomalies |
| `psd-core` | [`serde`](https://crates.io/crates/serde) 1.0 | MIT OR Apache-2.0 | Optional (`serde` feature) descriptor views |
| `psd-codecs` | [`libdeflater`](https://crates.io/crates/libdeflater) 1.26 | Apache-2.0 | The only codec with byte-exact ZIP round trips |
| `psd-codecs` | [`rayon`](https://crates.io/crates/rayon) 1.12 | MIT OR Apache-2.0 | Parallelism across scanlines and channels |
| `psd-codecs` | [`thiserror`](https://crates.io/crates/thiserror) 2.0 | MIT OR Apache-2.0 | `CodecError` |
| `psd` | [`memmap2`](https://crates.io/crates/memmap2) 0.9 | MIT OR Apache-2.0 | Memory-mapped reads |
| `psd` | [`nalgebra`](https://crates.io/crates/nalgebra) 0.34 | Apache-2.0 | Homographies for smart-object placement |
| `psd` | [`rayon`](https://crates.io/crates/rayon) 1.12 | MIT OR Apache-2.0 | Parallel channel decode and compositing |
| `psd` | [`image`](https://crates.io/crates/image) 0.25 | MIT OR Apache-2.0 | JPEG/PNG smart-object sources (default feature) |
| `psd` | [`uuid`](https://crates.io/crates/uuid) 1.26 | Apache-2.0 OR MIT | Smart-object linked-data identities |
| `psd-py` | [`pyo3`](https://crates.io/crates/pyo3) 0.29 | MIT OR Apache-2.0 | Python bindings |
| `psd-py` | [`numpy`](https://crates.io/crates/numpy) 0.29 | BSD-2-Clause | Zero-copy NumPy array interop |

`serde_json` (MIT OR Apache-2.0) is a dev-dependency of `psd-core`, used only to
assert descriptor serialization in tests.

### Transitive dependencies

With `--all-features` the resolved graph is **116 crates**, and every one of them
is permissively licensed: MIT, Apache-2.0, BSD-2-Clause / BSD-3-Clause, Zlib,
0BSD, Unlicense, Unicode-3.0, and Apache-2.0-with-LLVM-exception. There is **no
GPL, AGPL, SSPL, EUPL or MPL** anywhere in the tree, so linking this library
into a proprietary product does not oblige you to open-source anything.

The one license expression mentioning LGPL is `r-efi` (a UEFI-target crate,
not part of a normal desktop build), and it is `MIT OR Apache-2.0 OR
LGPL-2.1-or-later` — an either/or choice, so the LGPL terms never apply.

## Deliberate differences from upstream

Where upstream has a bug or an accident, this port fixes it and says so in a
comment at the site. A sample:

- Integer samples **round** to nearest when converted from floats; upstream
  truncates (`0.5 → 128` here, `127` upstream).
- The merged channel count includes alpha. Upstream's `hasAlpha &=` never sets
  it, so Photoshop shows a spurious "Alpha 1" channel.
- `PlLd` blocks in PSB files use the length width the reader expects, so placed
  layers survive a PSB round trip.
- Smart-object `rotate` takes degrees as documented; upstream passes the value
  to `cos`/`sin` as radians, so `rotate(45)` turns by about 58°.
- `ChannelID.Black` addresses CMYK channel index 3; upstream maps it to 2.

The full list is in [CHANGELOG.md](CHANGELOG.md) under *Changed* and *Fixed*.

## Testing

- **840 Rust tests** across 56 suites (2 ignored), plus **227 Python tests** — of which 184
  are ported one-for-one from upstream's `psapi-test` suite, keeping upstream's
  own assertions.
- A vendored corpus of **71 PSD/PSB documents** covering bit depth × color mode ×
  container × compression, all written by Photoshop 2022.
- Byte-exact codec vectors, and read → write → read structural plus pixel
  round-trip assertions.
- CI on Windows, Linux and macOS: build, tests, `clippy -D warnings`, `fmt`, and
  rustdoc with warnings denied.

## Roadmap

- Indexed and Duotone color modes
- Performance benchmarks and documentation (images and benchmark data are not
  published yet, so no speed claims are made here)

## License

BSD-3-Clause. See [LICENSE](LICENSE) — copyright is retained for Emil Dohne's
original C++ library and extended for this port.
