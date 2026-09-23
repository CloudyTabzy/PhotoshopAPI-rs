import glob
import os
import struct
import sys
import zlib
from collections import Counter

CHANNELS = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}
FILTER_NAMES = ["None", "Sub", "Up", "Avg", "Paeth"]


def chunks(data):
    pos = 8
    while pos + 8 <= len(data):
        (length,) = struct.unpack(">I", data[pos : pos + 4])
        kind = data[pos + 4 : pos + 8]
        yield kind, data[pos + 8 : pos + 8 + length]
        pos += 12 + length


def main(directory):
    for path in sorted(glob.glob(os.path.join(directory, "*.png"))):
        name = os.path.basename(path)
        if any(marker in name for marker in (".enc-", ".pngcrate", ".zune", ".pngspark")):
            continue
        data = open(path, "rb").read()
        ihdr = next(body for kind, body in chunks(data) if kind == b"IHDR")
        width, height, depth, color, _comp, _filt, interlace = struct.unpack(
            ">IIBBBBB", ihdr
        )
        idat = b"".join(body for kind, body in chunks(data) if kind == b"IDAT")
        raw = zlib.decompress(idat)
        if interlace:
            print(f"{name}: interlaced (skipped)")
            continue
        row_bytes = (width * CHANNELS[color] * depth + 7) // 8
        expected = height * (1 + row_bytes)
        if len(raw) != expected:
            print(f"{name}: inflated {len(raw)} != expected {expected}")
            continue
        counts = Counter(
            raw[y * (1 + row_bytes)] for y in range(height)
        )
        total = sum(counts.values())
        dist = "  ".join(
            f"{FILTER_NAMES[k]}:{100 * v // total}%"
            for k, v in sorted(counts.items())
        )
        print(f"{name:<26} rows={height:<5} {dist}")


if __name__ == "__main__":
    main(sys.argv[1])
