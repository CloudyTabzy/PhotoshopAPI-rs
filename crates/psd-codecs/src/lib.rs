//! Pure byte/slice codecs for the PSD/PSB format — no file IO, no format knowledge.
//!
//! Mirrors `Core/Endian/`, `Core/Compression/`, and the interleave parts of
//! `Core/Render/`. Everything operates on bytes or typed slices; parallelism is
//! rayon over scanlines/channels.
//!
//! Module map (upstream reference in parentheses):
//! - [`endian`]: bulk big-endian swap + typed byte conversion (`Core/Endian/`)
//! - [`rle`]: PackBits compress/decompress with PSD(2-byte)/PSB(4-byte)
//!   scanline size tables (`Compress_RLE.h`, `Decompress_RLE*.h`)
//! - `zip`: hand-assembled zlib stream (`0x78 0x5E` + raw deflate + BE
//!   adler32) via libdeflate (`Compress_ZIP.h`, `Decompress_ZIP.h`)
//! - `prediction`: ZIP prediction delta encode/decode, incl. the f32
//!   byte-deinterleave (`Compress_ZIP.h`, `Decompress_ZIP.h`)
//! - `interleave`: planar <-> interleaved shuffling (`InterleavedToPlanar.h`,
//!   `Render/Interleave.h`, `Deinterleave.h`)
//!
//! Scanline-parallel paths ([`rle`], `prediction`) run on rayon once a
//! payload exceeds `rle::PARALLEL_MIN_BYTES`; smaller payloads stay
//! sequential to avoid task overhead. A runtime-dispatched AVX2 endian swap
//! stays a later option, only if benchmarks demand it.
//!
//! Test vectors come from upstream `PhotoshopTest/src/TestCompression/` and
//! `TestDecompression/` and must match byte-exactly.

pub mod endian;
mod error;
pub mod interleave;
pub mod prediction;
pub mod rle;
pub mod zip;

pub use error::{CodecError, Result};
