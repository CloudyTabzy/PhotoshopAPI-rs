//! A streaming PNG decoder whose only dependency is `fearless_simd`, built for the
//! PhotoshopAPI-rs port.
//!
//! `psd-png` reads the whole PNG format — every colour type, every bit depth, interlaced
//! or not — through its own DEFLATE implementation, its own checksums, and its own filter
//! code. Nothing outside the standard library is involved, so it builds in a couple of
//! seconds and adds nothing to a dependency tree.
//!
//! Its reason to exist is [`Decoder::decode_to`]: a decode that hands out one reconstructed
//! scanline at a time while holding only DEFLATE's 32 KiB match window, a segment of
//! filtered rows, and a few rows of headroom. Memory follows the row width and not the
//! height — a few hundred kilobytes for ordinary images, however tall — which is what lets
//! a raster the whole-image path must refuse (the 512 MiB ceiling exists precisely so that
//! a seventy-byte file cannot make a decoder allocate for a petabyte) be decoded row by row
//! into the caller's own storage. The ceiling still bounds that working memory, so a header
//! naming rows gigabytes wide is refused on either path. [`decode`] remains for the
//! ordinary case.
//!
//! # Provenance
//!
//! This crate began as [png-spark](https://github.com/stephenberry/png-spark) 0.2.0 by
//! Stephen Berry, which contributed the format coverage, the DEFLATE codec, the checksums,
//! the scanline filters and the encoder. The PhotoshopAPI-rs port team added the fused
//! reconstruction and the streaming decoder, and removed the encoder and the ancillary-
//! chunk retention, which the port has no use for. Both licences (`MIT OR Apache-2.0`) and
//! the original copyright are retained unchanged.
//!
//! # Decoding
//!
//! ```no_run
//! let bytes = std::fs::read("input.png")?;
//! let image = psd_png::decode(&bytes)?;
//!
//! println!("{}x{} {:?}", image.width(), image.height(), image.color_type());
//!
//! // `image.data` holds the pixels exactly as the file stores them. Convert when you need
//! // a uniform layout:
//! let rgba = image.to_rgba8()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Because the data is in the file's own format, the colour type is not the whole story on
//! transparency: a palette or greyscale image keeps its alpha in a `tRNS` chunk rather than
//! in its pixels. Ask [`Info::has_alpha`] rather than reading the colour type, or convert
//! with `to_rgba8`, which resolves `tRNS` for you.
//!
//! # Streaming decode
//!
//! [`Decoder::decode_to`] is the API the port uses. Rows arrive in file order, in the
//! file's native layout, each borrowed only for the duration of the call:
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let png = std::fs::read("asset.png")?;
//! psd_png::Decoder::new().decode_to(&png, |row: psd_png::Row<'_>| {
//!     // `row.index` counts from zero; `row.bytes` is one scanline, filters already reversed.
//!     consume(row.index, row.bytes);
//!     Ok::<(), psd_png::Error>(())
//! })?;
//! # fn consume(_index: usize, _bytes: &[u8]) {}
//! # Ok(())
//! # }
//! ```
//!
//! A sink error aborts the decode and is returned; rows already delivered stay delivered.
//! Interlaced images need a scatter target the callback does not have, so they are decoded
//! into a buffer first and emitted from there, under the same size ceiling as [`decode`] —
//! the memory bound is the non-interlaced case, which is what every raster in the port's
//! corpus is.
//!
//! # Converting while streaming
//!
//! [`Decoder::decode_to_rgba8`], [`Decoder::decode_to_rgb8`],
//! [`Decoder::decode_to_rgba16`] and [`Decoder::decode_to_rgb16`] deliver the same rows
//! already converted, so a caller never holds a converted image it did not ask for:
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let png = std::fs::read("asset.png")?;
//! psd_png::Decoder::new().decode_to_rgba16(&png, |row: psd_png::Row<'_>| {
//!     // Four big-endian u16 samples per pixel: palette resolved, tRNS turned into alpha,
//!     // a 16-bit source untouched and a narrower one scaled across the full range.
//!     consume(row.bytes);
//!     Ok::<(), psd_png::Error>(())
//! })?;
//! # fn consume(_bytes: &[u8]) {}
//! # Ok(())
//! # }
//! ```
//!
//! These are the same conversion as [`to_rgba8`](Image::to_rgba8) and
//! [`to_rgb8`](Image::to_rgb8), expressed per row: one image's palette and `tRNS` key are
//! resolved once, each reconstructed row is converted into a scratch buffer reused for the
//! whole image, and only that row is live. The 8-bit and 16-bit forms differ in what they do
//! to a narrow source, and nothing else: 16-bit input passes through untouched, 8-bit input
//! keeps its high byte going down and is scaled up going the other way, and sub-byte greys
//! reach the ends of the output range exactly.
//!
//! # Reading files you did not write
//!
//! `IHDR` states an image's dimensions in thirteen bytes, and a decoder needs the buffer
//! they imply before it can read any of the compressed data. A seventy-byte file can
//! therefore name a size in petabytes, so the decoder carries a ceiling on it, of
//! [`DEFAULT_MAX_DECOMPRESSED_SIZE`]; a header over
//! it is an error rather than an allocation. Raise it with
//! [`Decoder::max_decompressed_size`] where the images really are that large. The
//! streaming methods apply the same ceiling to the buffers they allocate, which depend on
//! the row width rather than the whole image.
//!
//! [`read_info`] parses the header and colour chunks and stops at the image data, for a
//! caller that wants to decide something about a file before decoding it.
//!
//! # What the fork removed
//!
//! The encoder, the DEFLATE compressor under it, `FilterStrategy` and `WriteError` arrived
//! with the fork and are gone: the port decodes PNG rasters and never writes one, so
//! carrying an encoder would mean maintaining a capability nothing calls. The ancillary-
//! chunk retention (`Keep`, `Info::metadata`) went with it — the decoder skips every chunk
//! it does not read, which is what a conforming reader does anyway. The decode path never
//! touched any of it, and the fork's decode suite is unchanged.
//!
//! # Layout
//!
//! The pieces are public in their own right, so the zlib and checksum implementations can
//! be used on their own:
//!
//! - [`inflate`] — zlib streams, independent of PNG; the streaming decoder's engine
//! - [`crc32`] and [`adler32`] — the two checksums, with SIMD paths where they exist
//! - [`filter`] — the five PNG scanline filters, reversed
//! - [`decoder`] and [`common`] — the PNG layer itself

#![warn(missing_docs, missing_debug_implementations)]

pub mod adler32;
pub mod common;
pub mod crc32;
pub mod decoder;
pub mod error;
pub mod filter;
pub mod huffman;
pub mod inflate;
mod simd;
pub mod tables;
pub mod transform;

pub use common::{BitDepth, ColorType, Info, Interlacing};
pub use decoder::{Checks, DEFAULT_MAX_DECOMPRESSED_SIZE, Decoder, Image, Row, decode, read_info};
pub use error::Error;
pub use filter::Filter;
