#!/usr/bin/env python3
"""Generates the conversion benchmark set in tmp/convert.

The shapes the converting decode actually does work on. The main bench set
(tmp/bench) is RGB-family at 8 bits, where a conversion is either a pass-through or a
small expansion; these fixtures are the shapes whose conversion is proportional work:
grey at 16 bits (scale + expand), RGB and RGBA at 16 bits, palette at 8 bits with and
without per-entry `tRNS` alpha, and the sub-byte depths.

Every file is written twice: once with stored DEFLATE blocks under `tmp/convert`
(no compressor involved, so whatever the converting arms cost beyond the plain decode
is conversion, not inflate differences) and once deflate-compressed under
`tmp/convertz` (the same pixels at a realistic inflate cost, so the conversion share
can be read under both regimes).
"""
import os
import zlib

import numpy as np

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "tmp", "convert")
OUTZ = os.path.join(ROOT, "tmp", "convertz")

W, H = 1920, 1080


def stored_zlib(data: bytes) -> bytes:
    out = bytearray(b"\x78\x01")
    if not data:
        out += b"\x01\x00\x00\xff\xff"
    rest = data
    while rest:
        block, rest = rest[:65535], rest[65535:]
        out += bytes([1 if not rest else 0])
        out += len(block).to_bytes(2, "little") + (~len(block) & 0xFFFF).to_bytes(2, "little")
        out += block
    out += zlib.adler32(data).to_bytes(4, "big")
    return bytes(out)


def chunk(png: bytearray, kind: bytes, payload: bytes) -> None:
    png += len(payload).to_bytes(4, "big") + kind + payload
    png += zlib.crc32(kind + payload).to_bytes(4, "big")


CHANNELS = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}


def write(out_dir: str, name: str, width: int, height: int, depth: int, color_type: int,
          raw_rows: bytes, palette: bytes | None = None, trns: bytes | None = None,
          deflate: bool = False) -> None:
    """`raw_rows` is the unfiltered raster; each row gains its filter byte 0 on the way
    into the stream."""
    row_bytes = (width * CHANNELS[color_type] * depth + 7) // 8
    assert len(raw_rows) == row_bytes * height, name
    filtered = b"".join(
        b"\x00" + raw_rows[y * row_bytes:(y + 1) * row_bytes] for y in range(height))
    png = bytearray(b"\x89PNG\r\n\x1a\n")
    ihdr = (width.to_bytes(4, "big") + height.to_bytes(4, "big")
            + bytes([depth, color_type, 0, 0, 0]))
    chunk(png, b"IHDR", ihdr)
    if palette is not None:
        chunk(png, b"PLTE", palette)
    if trns is not None:
        chunk(png, b"tRNS", trns)
    chunk(png, b"IDAT", zlib.compress(filtered, 6) if deflate else stored_zlib(filtered))
    chunk(png, b"IEND", b"")
    path = os.path.join(out_dir, name)
    with open(path, "wb") as fh:
        fh.write(png)
    print(out_dir, name, f"{len(raw_rows) / (1024 * 1024):.1f} MB raw ->",
          os.path.getsize(path), "B png")


def specs(rng) -> list:
    """The fixture set, as (name, width, height, depth, color_type, raw, palette, trns)."""
    out = []

    # Grey at 16 bits: the widest grey conversion, every sample scaled from 16 to 8.
    raw = rng.integers(0, 65536, (H, W, 1), dtype=np.uint16).astype(">u2").tobytes()
    out.append(("gray16.png", W, H, 16, 0, raw, None, None))

    # RGB at 16 bits: six bytes per pixel, three scaled samples per pixel.
    raw = rng.integers(0, 65536, (H, W, 3), dtype=np.uint16).astype(">u2").tobytes()
    out.append(("rgb16.png", W, H, 16, 2, raw, None, None))

    # RGBA at 16 bits: passes through at its own width, converts hard at 8.
    raw = rng.integers(0, 65536, (H, W, 4), dtype=np.uint16).astype(">u2").tobytes()
    out.append(("rgba16.png", W, H, 16, 6, raw, None, None))

    # Palette at 8 bits, and the same with per-entry `tRNS` alpha.
    indices = rng.integers(0, 256, (H, W, 1), dtype=np.uint8).tobytes()
    palette = np.arange(768, dtype=np.uint8).tobytes()
    alphas = np.arange(256, dtype=np.uint8).tobytes()
    out.append(("palette8.png", W, H, 8, 3, indices, palette, None))
    out.append(("palette8_trns.png", W, H, 8, 3, indices, palette, alphas))

    # Sub-byte grey: the unpacking path, two bits per sample.
    raw = rng.integers(0, 4, (H, W // 4, 1), dtype=np.uint8).tobytes()
    out.append(("gray2.png", W, H, 2, 0, raw, None, None))

    # Indexed at 4 bits, sixteen entries.
    raw = rng.integers(0, 16, (H, W // 2), dtype=np.uint8).tobytes()
    out.append(("palette4.png", W, H, 4, 3, raw, np.arange(48, dtype=np.uint8).tobytes(), None))
    return out


def main() -> None:
    all_specs = specs(np.random.default_rng(11))
    for out_dir, deflate in ((OUT, False), (OUTZ, True)):
        os.makedirs(out_dir, exist_ok=True)
        for name, w, h, depth, color_type, raw, palette, trns in all_specs:
            write(out_dir, name, w, h, depth, color_type, raw,
                  palette=palette, trns=trns, deflate=deflate)


if __name__ == "__main__":
    main()
