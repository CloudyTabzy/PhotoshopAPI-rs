# Python binding test data

Vendored unchanged from upstream `PhotoshopAPI/python/psapi-test/`
(BSD-3-Clause, Emil Dohne) for the ported binding tests in
`crates/psd-py/tests/` and the Rust smart-object tests:

- `documents/BaseFile.psb`, `documents/MismatchedMaskChannel.psb` — 16-bit
  documents used by the image-layer tests.
- `bin_data/monza_npy.bin` — a NumPy `.npy` array of shape `(4, 188, 400)`
  (`uint16`, RGBA) used to build layers.
- `image_data/ImageStackerImage_lowres.png` (200×108) and
  `image_data/uv_grid.jpg` — smart-object sources.

Do not edit these files; regenerate them from upstream if needed.
