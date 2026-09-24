"""Two small RGBA fixtures bracketing the 32 KiB match window, for small-file A/B timing."""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from gen_large_fixtures import chunk, filter_image, write_png, rows_pattern, ROOT

OUT = os.path.join(ROOT, "tmp", "small")
os.makedirs(OUT, exist_ok=True)

# 64 x 92  -> filtered 23.7 KB, under the window: the frontier never fires mid-inflation.
# 64 x 184 -> filtered 47.3 KB, over it: it fires a handful of times near the end.
for h in (92, 184):
    rows = rows_pattern(64, h, 4)
    name = f"rgba8_small_64x{h}.png"
    write_png(name, 64, h, 4, 8, rows, [4])
    os.replace(os.path.join(ROOT, "tmp", "large", name), os.path.join(OUT, name))
