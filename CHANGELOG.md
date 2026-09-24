# Changelog

## Unreleased

- Non-interlaced decoding reverses the scanline filters as inflation writes, lagging the output
  cursor by DEFLATE's 32 KiB match window, instead of walking the whole image a second time after
  inflation. Large decodes are 2–9% faster (measured on zlib-generated fixtures up to 36.9 MB,
  interleaved best-of-N against `main`), small ones take the unchanged two-pass path.
- `Decoder::decode_to` decodes a PNG one scanline at a time through a sink, in file order and the
  file's native layout, holding only the match window plus a segment of filtered rows — a few
  hundred kilobytes whatever the decompressed size. `max_decompressed_size` does not bound it:
  images `decode` must refuse stream row by row. Within a few percent of `decode()` in speed.

## 0.2.0

- `Encoder::encode_to` writes a PNG to any `io::Write`, filtering and compressing a band at a time. Peak working memory grows with the image's width but not its height, against the whole file plus a filtered copy of every row before.
- `WriteError` carries either an encoding fault or the sink's `io::Error`. `Error` is unchanged and still `Clone + PartialEq + Eq`.
- `Deflater::zlib_start` and `zlib_push` compress a zlib stream in pieces.
- `Encoder::encode` now goes through the same banded path. Output is a fraction of a percent larger, and a large image is split across several `IDAT` chunks instead of one.

## 0.1.0

First release.

- Decodes and encodes every PNG colour type and bit depth, interlaced or not
- Its own DEFLATE, CRC-32, Adler-32 and filter code, so there are no dependencies
- Ancillary chunks carried through on both read and write
- A 512 MiB default ceiling on decompressed size, for input you did not write
- Requires Rust 1.96, edition 2024
