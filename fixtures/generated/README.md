# Generated test documents

This repository's own generator produces these documents; Photoshop does not:

```text
cargo run -p psd --example generate_fixtures
```

They cover format features that the Photoshop-saved corpus in `../documents/`
does not contain. The generator is deterministic: running it again with unchanged
code reproduces every file byte for byte.

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
clipped to the layer below it.
