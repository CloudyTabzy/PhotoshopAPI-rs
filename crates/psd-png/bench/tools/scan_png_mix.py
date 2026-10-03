import json
import struct
import sys
from collections import Counter
from pathlib import Path

ROOT = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("fixtures/documents")
PNG_SIG = b"\x89PNG\r\n\x1a\n"
NAMES = {0: "Gray", 2: "RGB", 3: "Palette", 4: "GrayAlpha", 6: "RGBA"}


def ihdr(data: bytes, off: int):
    """Parse IHDR fields at a PNG signature offset; None if truncated/invalid."""
    try:
        w, h = struct.unpack(">II", data[off + 16 : off + 24])
        depth = data[off + 24]
        ctype = data[off + 25]
        interlace = data[off + 28]
        if ctype not in NAMES or depth not in (1, 2, 4, 8, 16):
            return None
        return (w, h, depth, ctype, interlace)
    except Exception:
        return None


rows = []

# 1) standalone .png files
for p in sorted(ROOT.rglob("*.png")):
    data = p.read_bytes()
    info = ihdr(data, 0) if data[:8] == PNG_SIG else None
    rows.append({"path": str(p.relative_to(ROOT)), "kind": "file", "ihdr": info})

# 2) PNG signatures embedded in other containers (PSD/PSB lnkD/lnkE data, etc.)
for p in sorted(ROOT.rglob("*")):
    if not p.is_file() or p.suffix.lower() == ".png":
        continue
    data = p.read_bytes()
    start = 0
    while True:
        i = data.find(PNG_SIG, start)
        if i < 0:
            break
        info = ihdr(data, i)
        if info:
            rows.append(
                {
                    "path": str(p.relative_to(ROOT)),
                    "kind": f"embedded@{i}",
                    "ihdr": info,
                }
            )
        start = i + 8

summary = Counter()
for row in rows:
    if row["ihdr"]:
        w, h, depth, ctype, interlace = row["ihdr"]
        summary[f"{NAMES[ctype]} {depth}-bit {'interlaced' if interlace else 'non-interlaced'}"] += 1

out = {"summary": dict(summary), "rows": rows}
out_path = Path(sys.argv[2]) if len(sys.argv) > 2 else Path("scan.json")
out_path.write_text(json.dumps(out, indent=1), encoding="utf-8")

print(f"scanned {len(rows)} entries ({sum(1 for r in rows if r['ihdr'])} valid PNGs)")
for key, count in summary.most_common():
    print(f"  {count:3d}  {key}")
print()
for row in rows:
    info = row["ihdr"]
    if info:
        w, h, depth, ctype, interlace = info
        print(
            f"{row['path']}  [{row['kind']}]  {w}x{h} {NAMES[ctype]} {depth}-bit"
            f"{' interlaced' if interlace else ''}"
        )
