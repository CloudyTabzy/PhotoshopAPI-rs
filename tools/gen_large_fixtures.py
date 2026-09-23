#!/usr/bin/env python3
"""Generate large PNG fixtures whose zlib streams carry a real LZ77 match finder.

png-spark's own encoder emits zero-run (distance-1) matches only, so its output cannot
detect a reconstruction frontier whose match-window lag is wrong. These fixtures are
filtered and compressed by CPython's zlib — a full match finder — so their streams hold
matches at arbitrary distances up to 32 KiB, crossing rows the decoder reconstructs
mid-inflation. Tests in tests/fused_reconstruction.rs compare the fused decode path
against the legacy inflate-then-unfilter path over these.

Run from the repository root:  python3 tools/gen_large_fixtures.py
Output: tmp/large/*.png  (gitignored, like the rest of tmp/)

Pure stdlib; the per-byte Python filtering is slow, so this is a one-time ~few-minute
generation. Sizes are chosen so every fixture's filtered stream comfortably exceeds
DEFLATE's 32768-byte match window.
"""

import os
import random
import struct
import zlib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "tmp", "large")

PAETH, AVG, SUB, UP, NONE = 4, 3, 1, 2, 0


def chunk(kind: bytes, payload: bytes) -> bytes:
    crc = zlib.crc32(kind + payload) & 0xFFFFFFFF
    return struct.pack(">I", len(payload)) + kind + payload + struct.pack(">I", crc)


def paeth(a: int, b: int, c: int) -> int:
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    if pb <= pc:
        return b
    return c


def filter_row(ftype: int, prev: bytes, row: bytes, bpp: int) -> bytes:
    out = bytearray(len(row))
    if ftype == NONE:
        out[:] = row
    elif ftype == SUB:
        for i in range(len(row)):
            out[i] = (row[i] - (row[i - bpp] if i >= bpp else 0)) & 0xFF
    elif ftype == UP:
        for i in range(len(row)):
            out[i] = (row[i] - prev[i]) & 0xFF
    elif ftype == AVG:
        for i in range(len(row)):
            a = row[i - bpp] if i >= bpp else 0
            out[i] = (row[i] - ((a + prev[i]) >> 1)) & 0xFF
    else:
        for i in range(len(row)):
            a = row[i - bpp] if i >= bpp else 0
            c = prev[i - bpp] if i >= bpp else 0
            out[i] = (row[i] - paeth(a, prev[i], c)) & 0xFF
    return bytes(out)


def filter_image(rows, bpp: int, filters) -> bytes:
    prev = bytes(len(rows[0]))
    out = bytearray()
    for i, row in enumerate(rows):
        f = filters[i % len(filters)]
        out.append(f)
        out += filter_row(f, prev, row, bpp)
        prev = row
    return bytes(out)


def write_png(name, width, height, channels, bit_depth, rows, filters, level=6):
    colour_type = {1: 0, 2: 4, 3: 2, 4: 6}[channels]
    ihdr = struct.pack(">IIBBBBB", width, height, bit_depth, colour_type, 0, 0, 0)
    raw = filter_image(rows, channels * (bit_depth // 8), filters)
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(raw, level))
        + chunk(b"IEND", b"")
    )
    with open(os.path.join(OUT, name), "wb") as fh:
        fh.write(png)
    print(f"{name}: {width}x{height}x{channels} bd={bit_depth} "
          f"filtered={len(raw) / 1e6:.1f} MB png={len(png) / 1e6:.2f} MB")


def rows_gradient(w, h, ch, bd=8):
    rows = []
    for y in range(h):
        row = bytearray()
        for x in range(w):
            hi = bd // 8
            vals = [
                (x * 65535) // max(w - 1, 1),
                (y * 65535) // max(h - 1, 1),
                ((x + y) * 65535) // max(w + h - 2, 1),
                65535,
            ][:ch]
            for v in vals:
                row += v.to_bytes(hi, "big") if hi == 2 else bytes([v & 0xFF])
        rows.append(bytes(row))
    return rows


def rows_pattern(w, h, ch, bd=8):
    """A smooth integer pattern: low entropy, highly self-similar across rows, so zlib's
    match finder produces long matches at scanline-ish distances."""
    rows = []
    for y in range(h):
        row = bytearray()
        for x in range(w):
            r = (x // 3 + y // 5) & 0xFF
            g = (x // 7 + y // 11 + 40) & 0xFF
            b = (abs(x - y) // 13 + 80) & 0xFF
            a = 255 - ((x // 17 + y // 19) & 0x7F)
            row += bytes([r, g, b, a][:ch])
        rows.append(bytes(row))
    return rows


def rows_noise(w, h, ch, seed=7):
    rng = random.Random(seed)
    return [bytes(rng.randrange(256) for _ in range(w * ch)) for _ in range(h)]


def main():
    os.makedirs(OUT, exist_ok=True)

    write_png("rgba8_gradient_1024.png", 1024, 1024, 4, 8,
              rows_gradient(1024, 1024, 4), [PAETH])
    write_png("rgba8_mixed_1024.png", 1024, 1024, 4, 8,
              rows_pattern(1024, 1024, 4), [NONE, SUB, UP, AVG, PAETH, PAETH, PAETH])
    write_png("rgba8_noise_1024.png", 1024, 1024, 4, 8,
              rows_noise(1024, 1024, 4), [UP])
    write_png("rgb8_photo_2048.png", 2048, 2048, 3, 8,
              rows_pattern(2048, 2048, 3), [PAETH])
    write_png("gray8_gradient_1600.png", 1600, 1600, 1, 8,
              rows_gradient(1600, 1600, 1), [SUB])
    write_png("rgba16_gradient_1024.png", 1024, 1024, 4, 16,
              rows_gradient(1024, 1024, 4, 16), [UP])
    write_png("rgba8_tall_64x4096.png", 64, 4096, 4, 8,
              rows_gradient(64, 4096, 4), [PAETH])
    write_png("rgba8_wide_4096x96.png", 4096, 96, 4, 8,
              rows_pattern(4096, 96, 4), [PAETH])
    write_png("rgba8_photo_3840x2400.png", 3840, 2400, 4, 8,
              rows_pattern(3840, 2400, 4), [PAETH])


if __name__ == "__main__":
    main()
