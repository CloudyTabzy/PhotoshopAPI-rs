Vendored from the psd-webtoon integration corpus
(`packages/psd/tests/integration/fixtures/`, MIT License,
(c) 2021-present NAVER WEBTOON).

- `example.psd` / `example.psb` — an authored 400x800 RGB document with 14
  content records: nested group, CJK and emoji layer names, text layers, and
  layers extending past the canvas. `example.layer0..13` are raw decoded RGBA
  goldens per content record (top-first, their `composite(false, false)`);
  `example.imageData` is Photoshop's stored merged composite. Compared by
  `crates/psd/tests/webtoon_goldens.rs` — note the file's own merge and
  per-layer previews disagree on text antialiasing (Photoshop rasterized the
  text twice), which the composite tolerance pins.
- `CJK.psd`, `engineData.psd`, `pattern.psd` — authored single-feature
  documents (CJK names, a text layer with a shaped `SoCo` fill, pattern data).
- `original.psd` / `original.psb` / `original-uncompressed.psd` /
  `original-with-group.psd` — the clean base documents from which the
  `crates/psd/tests/malformed/` corpus was patched (RLE and raw PSD, PSB,
  and a grouped variant).
