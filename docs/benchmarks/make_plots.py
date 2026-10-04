"""Render the README benchmark table as bar charts, in the same style as the
upstream doxygen images (docs/doxygen/images/benchmarks).

Data: the medians published in README.md `## Performance` (measured from the
`d1328e5` build — five runs per implementation and document, warm cache,
alternating order). Synthetic-document writes carry a min..max whisker because
OS dirty-page throttling dominated those runs.

Usage: py make_plots.py            (writes PNGs next to this file)
"""

import matplotlib.pyplot as plt

# file -> (title, cpp_read_extract, cpp_write, rs_read_extract, rs_write,
#          write_range_cpp, write_range_rs)  — ms; ranges optional
REAL = {
    "Compression_Mixed_8bit.psd": ("Compression Mixed (8-bit)", 9.81, 22.84, 1.19, 1.71, None, None),
    "CMYK_16.psd": ("CMYK (16-bit)", 79.59, 548.67, 5.37, 15.10, None, None),
    "example.psd": ("Webtoon example (8-bit)", 29.03, 17.03, 1.80, 1.78, None, None),
    "smart_object_file_no_warp.psd": ("Smart object, no warp (8-bit)", 28.87, 222.13, 5.20, 3.92, None, None),
}
SYNTHETIC = {
    "big8.psd": ("big8 (435 MB, 8-bit RLE)", 334.1, 672.9, 157.5, 2670.9,
                 (286.2, 4052.0), (160.5, 3866.6)),
    "big16.psd": ("big16 (252 MB, 16-bit ZIP-pred.)", 267.6, 624.6, 195.0, 2375.2,
                  (595.3, 1019.7), (347.7, 2579.6)),
}

CPP_RE, CPP_W = "#7f7fff", "#0000ff"   # upstream's blue shades
RS_RE, RS_W = "#ff7f7f", "#ff0000"     # and its red shades


def combined_plot(name, title, cpp_re, cpp_w, rs_re, rs_w, rng_cpp, rng_rs):
    labels = ["cpp_read+extract", "cpp_write", "rs_read+extract", "rs_write"]
    vals = [cpp_re, cpp_w, rs_re, rs_w]
    colors = [CPP_RE, CPP_W, RS_RE, RS_W]
    fig, ax = plt.subplots(figsize=(6.4, 6.4))
    yerr = None
    if rng_cpp or rng_rs:
        lo = [0, cpp_w - rng_cpp[0], 0, rs_w - rng_rs[0]]
        hi = [0, rng_cpp[1] - cpp_w, 0, rng_rs[1] - rs_w]
        yerr = [lo, hi]
    bars = ax.bar(labels, vals, color=colors, yerr=yerr, capsize=5)
    for bar, v in zip(bars, vals):
        ax.text(bar.get_x() + bar.get_width() / 2, bar.get_height(),
                f"{v:,.2f}", ha="center", va="bottom", fontsize=9)
    ax.set_title(title)
    ax.set_xlabel("Benchmark")
    ax.set_ylabel("Median Time (ms)")
    ax.tick_params(axis="x", labelsize=8)
    fig.tight_layout()
    fig.savefig(name.replace(".psd", "").replace(".", "_") + "_combined_plot.png", dpi=100)
    plt.close(fig)


def overview(title, groups, path, ranged=False):
    """Grouped chart: read+extract and write per implementation across files.
    With `ranged`, write bars carry min..max whiskers (dirty-page throttle
    dominated the synthetic-document write medians — see README)."""
    import numpy as np
    names = list(groups)
    x = np.arange(len(names))
    w = 0.2
    fig, ax = plt.subplots(figsize=(11, 6.4))
    for offs, key, color, label, rng_idx in [
        (-1.5 * w, 1, CPP_RE, "C++ read+extract", None),
        (-0.5 * w, 2, CPP_W, "C++ write", 5),
        (0.5 * w, 3, RS_RE, "Rust read+extract", None),
        (1.5 * w, 4, RS_W, "Rust write", 6),
    ]:
        vals = [groups[n][key] for n in names]
        yerr = None
        if ranged and rng_idx:
            lo = [max(v - groups[n][rng_idx][0], 0) for n, v in zip(names, vals)]
            hi = [groups[n][rng_idx][1] - v for n, v in zip(names, vals)]
            yerr = [lo, hi]
        bars = ax.bar(x + offs, vals, w, color=color, label=label, yerr=yerr, capsize=4)
        for bar, v in zip(bars, vals):
            ax.text(bar.get_x() + bar.get_width() / 2, bar.get_height(),
                    f"{v:g}", ha="center", va="bottom", fontsize=7)
    ax.set_xticks(x, [groups[n][0].replace(" (", "\n(") for n in names], fontsize=8)
    ax.set_ylabel("Median Time (ms)")
    ax.set_title(title)
    if ranged:
        fig.text(0.5, 0.005,
                 "Write whiskers: 5-run min..max — medians dominated by OS dirty-page throttling",
                 ha="center", fontsize=8, style="italic")
    ax.legend()
    fig.tight_layout(rect=(0, 0.03, 1, 1))
    fig.savefig(path, dpi=100)
    plt.close(fig)


for name, (title, *vals) in REAL.items():
    combined_plot(name, title, *vals)
for name, (title, *vals) in SYNTHETIC.items():
    combined_plot(name, title, *vals)
overview("Read + extract and write — corpus documents", REAL, "corpus_graphs.png")
overview("Read + extract and write — synthetic large documents", SYNTHETIC, "synthetic_graphs.png", ranged=True)
print("wrote", len(REAL) + len(SYNTHETIC) + 2, "charts")
