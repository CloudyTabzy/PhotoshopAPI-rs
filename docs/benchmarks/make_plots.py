"""Render the README benchmark table as bar charts, in the same style as the
upstream doxygen images (docs/doxygen/images/benchmarks).

Data: the medians published in README.md `## Performance` (measured from the
`d1328e5` build — five runs per implementation and document, warm cache,
alternating order). Synthetic-document write bars show the best run as the
solid bar (their medians are dominated by OS dirty-page throttling), with a
shaded min..max band and a notch at the median.

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


WRITE_BAR_NOTE = ("Reads: 5-run median. Writes: best run (solid bar); "
                  "band = min..max, notch = median — disk-flush noise")


def combined_plot(name, title, cpp_re, cpp_w, rs_re, rs_w, rng_cpp, rng_rs):
    labels = ["cpp_read+extract", "cpp_write", "rs_read+extract", "rs_write"]
    medians = [cpp_re, cpp_w, rs_re, rs_w]
    colors = [CPP_RE, CPP_W, RS_RE, RS_W]
    heights = list(medians)
    for i, rng in ((1, rng_cpp), (3, rng_rs)):
        if rng:
            heights[i] = rng[0]
    fig, ax = plt.subplots(figsize=(6.4, 6.4))
    bars = ax.bar(labels, heights, color=colors)
    for i, rng in ((1, rng_cpp), (3, rng_rs)):
        if rng:
            ax.bar(i, rng[1] - rng[0], 0.8, bottom=rng[0], color=colors[i], alpha=0.3)
            med = medians[i]
            ax.plot([i - 0.4, i + 0.4], [med, med], color="black", lw=1.5,
                    solid_capstyle="butt")
            ax.text(i, med, f"median {med:g}", ha="center", va="bottom", fontsize=8)
    for bar, v in zip(bars, heights):
        ax.text(bar.get_x() + bar.get_width() / 2, bar.get_height(),
                f"{v:,.2f}", ha="center", va="bottom", fontsize=9)
    ax.set_title(title)
    ax.set_xlabel("Benchmark")
    ax.set_ylabel("Time (ms) — lower is better")
    ax.tick_params(axis="x", labelsize=8)
    if rng_cpp or rng_rs:
        fig.text(0.5, 0.005, WRITE_BAR_NOTE, ha="center", fontsize=8, style="italic")
    fig.tight_layout(rect=(0, 0.03, 1, 1))
    fig.savefig(name.replace(".psd", "").replace(".", "_") + "_combined_plot.png", dpi=100)
    plt.close(fig)


def overview(title, groups, path, ranged=False):
    """Grouped chart: read+extract and write per implementation across files.
    With `ranged`, write bars show the best run as solid (dirty-page throttle
    dominated the synthetic-document write medians — see README), plus a
    shaded min..max band and a notch at the median."""
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
        heights = vals
        if ranged and rng_idx:
            heights = [groups[n][rng_idx][0] for n in names]
            for xi, n, med in zip(x + offs, names, vals):
                mn, mx = groups[n][rng_idx]
                ax.bar(xi, mx - mn, w, bottom=mn, color=color, alpha=0.3)
                ax.plot([xi - w / 2, xi + w / 2], [med, med], color="black",
                        lw=1, solid_capstyle="butt")
        bars = ax.bar(x + offs, heights, w, color=color, label=label)
        for bar, v in zip(bars, heights):
            ax.text(bar.get_x() + bar.get_width() / 2, bar.get_height(),
                    f"{v:g}", ha="center", va="bottom", fontsize=7)
    ax.set_xticks(x, [groups[n][0].replace(" (", "\n(") for n in names], fontsize=8)
    ax.set_ylabel("Time (ms) — lower is better")
    ax.set_title(title)
    if ranged:
        fig.text(0.5, 0.005, WRITE_BAR_NOTE, ha="center", fontsize=8, style="italic")
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
