# Generated test documents

This repository's own generator produces these documents; Photoshop does not:

```text
cargo run -p psd --example generate_fixtures
```

They cover format features that the Photoshop-saved corpus in `../documents/`
does not contain: adjustment, fill, and shape layers, and artboards. The
generator is deterministic: running it again with unchanged code reproduces
every file byte for byte.

Payload layouts follow what Photoshop writes. The sources are the Adobe PSD/PSB
specification and Photoshop-saved samples. The documents were also checked with
[psd-tools](https://github.com/psd-tools/psd-tools) 1.19, a reader that shares
no code with this port. It reads every settings block with the values the tests
assert. Photoshop has not opened these files, so use them to test the format,
not as rendering references.

## Adjustments/

`adjustment_layers_8bit.psd`, `adjustment_layers_8bit.psb`, and
`adjustment_layers_16bit.psd` are 64×64 RGB documents with the same layers:

- `Background`, a pixel layer with a red/green gradient.
- One layer per adjustment block: Brightness/Contrast (`brit`), Levels
  (`levl` with the `Lvls` extension), Curves (`curv` with the `Crv `
  extension), Exposure (`expA`), Vibrance (`vibA`), Hue/Saturation (`hue2`),
  Color Balance (`blnc`), Black & White (`blwh`), Photo Filter (`phfl`),
  Channel Mixer (`mixr`), Color Lookup (`clrL` with a tiny embedded cube
  LUT), Invert (`nvrt`), Posterize (`post`), Threshold (`thrs`),
  Gradient Map (`grdm`), and Selective Color (`selc`).
- Two fill layers: Color Fill (`SoCo`) and Gradient Fill (`GdFl`). There is
  no pattern fill, because a valid one needs an embedded pattern.

Brightness/Contrast, Levels, and Curves also carry a `CgEd` block, as
Photoshop writes them. `Curves` has a pixel mask, and `Hue/Saturation` is
clipped to the layer below it. As in Photoshop, every adjustment and fill
layer marks its pixel data as irrelevant.

## Vectors/

`vector_shapes_8bit.psd` and `vector_shapes_8bit.psb` are 64×64 RGB
documents. Each shape layer carries pixels rasterized from its path, and, as
in Photoshop, marks them as derived data.

- `Background`, a flat gray pixel layer.
- `Legacy Rectangle`, a shape in the pre-CS6 form: a `SoCo` fill, a `vmsk`
  rectangle of unlinked corner knots, and a `vogk` rectangle entry.
- `Ellipse`, a shape in the CS6 form: `vscg` (solid fill), a `vsms` ellipse
  of linked knots, a dashed `vstk` stroke, and a `vogk` ellipse entry.
- `Frame`, a compound shape: an outer rounded rectangle (combine) minus an
  inner rectangle (subtract). Each subpath points at its own `vogk` entry.
- `Open Line`, an open two-knot subpath drawn by a stroke with fill disabled.
- `Masked Pixels`, a pixel layer with an inverted, unlinked `vmsk` triangle.
  This is not a shape layer.

The documents also store a work path (image resource 1025), a saved path
`Outline` (resource 2000), and a `pths` block that gives the saved path the
Unicode name `Outline ✓`. In the PSB, `pths` uses the `8B64` signature and an
8-byte length, as Photoshop writes it.

## Artboards/

`artboards_8bit.psd` and `artboards_8bit.psb` are 128×64 RGB documents. As in
Photoshop, each artboard is a layer group whose group record carries an `artb`
descriptor.

- `Plain Group`, an ordinary group with one pixel layer. It is not an
  artboard.
- `Artboard Left`, a white artboard spanning (0, 0)–(60, 50) with the preset
  name `Icon 60`. It holds one pixel layer.
- `Artboard Right`, an artboard spanning (68, 0)–(128, 50) with a custom
  background color and one guide index. It holds a nested plain group,
  `Inner Group`.

The documents also carry the artboard tool settings in the document-level
`artd` block. In the PSB, `artd` uses the `8B64` signature and an 8-byte
length.
