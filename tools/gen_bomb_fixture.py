#!/usr/bin/env python3
"""Generate the size-ceiling fixture: a 16384x16384 RGBA8 image of zeros.

Filtered size is ~1.07 GB, far over DEFAULT_MAX_DECOMPRESSED_SIZE (512 MiB), so
`decode` refuses it while `decode_to` must stream it. Zlib compresses a gigabyte of
zeros to almost nothing, so the file stays tiny.

Output: tmp/bomb/zeros_16384.png  (gitignored, like the rest of tmp/)
"""

import os
import struct
import zlib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "tmp", "bomb")

WIDTH = HEIGHT = 16384
CHANNELS = 4


def chunk(kind: bytes, payload: bytes) -> bytes:
    crc = zlib.crc32(kind + payload) & 0xFFFFFFFF
    return struct.pack(">I", len(payload)) + kind + payload + struct.pack(">I", crc)


def main():
    os.makedirs(OUT, exist_ok=True)
    pitch = 1 + WIDTH * CHANNELS
    # Filter byte 0 (None) per row, then a row of zeros. Bytes, not a per-pixel loop.
    filtered = bytearray(pitch * HEIGHT)
    ihdr = struct.pack(">IIBBBBB", WIDTH, HEIGHT, 8, 6, 0, 0, 0)
    compressed = zlib.compress(bytes(filtered), 1)
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", compressed)
        + chunk(b"IEND", b"")
    )
    path = os.path.join(OUT, "zeros_16384.png")
    with open(path, "wb") as fh:
        fh.write(png)
    print(f"zeros_16384.png: {WIDTH}x{HEIGHT} filtered={len(filtered) / 1e9:.2f} GB "
          f"png={len(png) / 1e3:.1f} KB")


if __name__ == "__main__":
    main()
