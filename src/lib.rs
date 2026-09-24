//! A streaming PNG decoder with no dependencies, built for the PhotoshopAPI-rs port.
//!
//! `psd-png` reads the whole PNG format — every colour type, every bit depth, interlaced
//! or not — through its own DEFLATE implementation, its own checksums, and its own filter
//! code. Nothing outside the standard library is involved, so it builds in a couple of
//! seconds and adds nothing to a dependency tree.
//!
//! Its reason to exist is [`Decoder::decode_to`]: a decode that hands out one reconstructed
//! scanline at a time while holding only DEFLATE's 32 KiB match window, a segment of
//! filtered rows, and the row being worked on. Memory is a few hundred kilobytes whatever
//! the image's size, which is what lets a raster the whole-image path must refuse — the
//! 512 MiB ceiling exists precisely so that a seventy-byte file cannot make a decoder
//! allocate for a petabyte — be decoded row by row into the caller's own storage.
//! [`decode`] remains for the ordinary case.
//!
//! # Provenance
//!
//! This crate began as [png-spark](https://github.com/stephenberry/png-spark) 0.2.0 by
//! Emil Dohne, which contributed the format coverage, the DEFLATE codec, the checksums,
//! the scanline filters, and the encoder still inherited below. The PhotoshopAPI-rs port
//! team added the fused reconstruction and the streaming decoder, and is removing the
//! encoder, which the port has no use for. Both licences (`MIT OR Apache-2.0`) and the
//! original copyright are retained unchanged.
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
//! into a buffer first and emitted from there — the memory bound is the non-interlaced
//! case, which is what every raster in the port's corpus is.
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
//! [`Decoder::max_decompressed_size`] where the images really are that large.
//!
//! [`read_info`] parses the header and colour chunks and stops at the image data, for a
//! caller that wants to decide something about a file before decoding it.
//!
//! # Inherited encoder
//!
//! The encoder, the DEFLATE compressor under it, and [`FilterStrategy`] arrived with the
//! fork and are on their way out: the port decodes PNG rasters and never writes one, so
//! carrying an encoder would mean maintaining a capability nothing calls. They are still
//! here, unchanged and still tested, so that this change is one of identity and nothing
//! else; the removal is the next step, and the README says so where a reader will see it.
//! The decode path does not touch any of it.
//!
//! # Carrying your own data in a PNG
//!
//! PNG stores everything in typed chunks, and a decoder must skip any *ancillary* chunk it
//! does not recognise. An ancillary chunk of your own is therefore a place to keep
//! application data inside the image file: arbitrary bytes, up to `i32::MAX` of them, with
//! no escaping and no encoding, which every other PNG reader ignores.
//!
//! Decoding keeps nothing by default, because retaining a chunk means copying it and
//! nothing the decoder returns depends on one. [`Decoder::keep`] asks for what a caller
//! actually wants, and what is kept travels on [`Info`]:
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let png = std::fs::read("asset.png")?;
//! let image = psd_png::Decoder::new()
//!     .keep(psd_png::Keep::Only(vec![*b"apPd"]))
//!     .decode(&png)?;
//! if let Some(asset_id) = image.info.chunk(b"apPd") {
//!     // use the carried bytes
//! }
//! # Ok(())
//! # }
//! ```
//!
//! [`Keep::All`] takes everything the file carries. A chunk that fails its CRC is dropped
//! rather than returned. What is checked is the type bytes and the placement relative to
//! `PLTE`, not what a *registered* type means: two `gAMA` chunks are the caller's mistake
//! to avoid, and private types have no such rules to break.
//!
//! Ignoring a chunk is not the same as preserving it. A tool that rewrites the file may
//! well drop it: libpng discards unknown chunks unless the application asks for them, and
//! optimisers such as oxipng and pngcrush strip ancillary chunks by default. Data that
//! must survive an arbitrary third-party tool does not belong here; data that must
//! survive your own pipeline does.
//!
//! # Layout
//!
//! The pieces are public in their own right, so the DEFLATE and checksum implementations can
//! be used on their own:
//!
//! - [`inflate`] — zlib streams, independent of PNG; the streaming decoder's engine
//! - [`crc32`] and [`adler32`] — the two checksums, with SIMD paths where they exist
//! - [`filter`] — the five PNG scanline filters, forward and reverse
//! - [`decoder`] and [`common`] — the PNG layer itself
//! - [`deflate`] and [`encoder`] — inherited, on their way out (see above)

#![warn(missing_docs, missing_debug_implementations)]

pub mod adler32;
pub mod common;
pub mod crc32;
pub mod decoder;
pub mod deflate;
pub mod encoder;
pub mod error;
pub mod filter;
pub mod huffman;
pub mod inflate;
pub mod tables;
pub mod transform;

pub use common::{BitDepth, Chunk, ColorType, Info, Interlacing};
pub use decoder::{
    Checks, DEFAULT_MAX_DECOMPRESSED_SIZE, Decoder, Image, Keep, Row, decode, read_info,
};
pub use encoder::{Encoder, FilterStrategy, encode, encode_rgb8, encode_rgba8};
pub use error::{Error, WriteError};
pub use filter::Filter;
