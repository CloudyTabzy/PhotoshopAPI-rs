//! zlib streams the Photoshop way: `0x78` + level byte + raw deflate + BE adler32.
//!
//! Mirrors upstream `Compress_ZIP.h` / `Decompress_ZIP.h` (`ZIP_Impl::Compress`
//! / `ZIP_Impl::Decompress`). Upstream hand-assembles the stream to control the
//! header byte; this port does the same — raw deflate from the selected engine
//! plus its adler32 — at upstream's fixed compression level 4
//! (`ZIP_COMPRESSION_LVL`).
//!
//! The deflate engine is chosen by exactly one `zip-backend-*` feature:
//! `zlib-rs` (default, pure Rust — measured on `examples/zip_bench.rs` as
//! fast as C libdeflate on this workload), `libdeflater` (C libdeflate,
//! upstream's own engine, opt-in) or `miniz` (miniz_oxide, portable
//! fallback). Only the engine swaps; stream framing stays identical, so
//! files written by any backend read on all of them.

#[cfg(not(any(
    feature = "zip-backend-libdeflater",
    feature = "zip-backend-zlib-rs",
    feature = "zip-backend-miniz"
)))]
compile_error!("psd-codecs needs exactly one `zip-backend-*` feature enabled");

#[cfg(any(
    all(feature = "zip-backend-libdeflater", feature = "zip-backend-zlib-rs"),
    all(feature = "zip-backend-libdeflater", feature = "zip-backend-miniz"),
    all(feature = "zip-backend-zlib-rs", feature = "zip-backend-miniz")
))]
compile_error!("psd-codecs `zip-backend-*` features are mutually exclusive");

use crate::error::{CodecError, Result};

/// Upstream's `ZIP_COMPRESSION_LVL`. Fixed: the zlib header byte written on
/// compress is derived from this, so changing it changes the byte stream.
pub const COMPRESSION_LEVEL: i32 = 4;

/// zlib header's second byte for a compression level (upstream's mapping in
/// `ZIP_Impl::Compress`; the first byte is always `0x78`).
fn zlib_header_byte(level: i32) -> u8 {
    if level < 2 {
        0x01
    } else if level < 6 {
        0x5E
    } else if level < 8 {
        0x9C
    } else {
        0xDA
    }
}

/// Compress already-BE-encoded bytes into a Photoshop zlib stream.
///
/// Layout: `0x78` | level byte | raw deflate | big-endian adler32 of the
/// uncompressed input — byte-exact with upstream `ZIP_Impl::Compress`.
pub fn compress(uncompressed: &[u8]) -> Result<Vec<u8>> {
    let raw = engine::deflate(uncompressed)?;
    let mut out = Vec::with_capacity(2 + raw.len() + 4);
    out.push(0x78);
    out.push(zlib_header_byte(COMPRESSION_LEVEL));
    out.extend_from_slice(&raw);
    out.extend_from_slice(&engine::adler32(uncompressed).to_be_bytes());
    Ok(out)
}

/// Decompress a full zlib stream (header + deflate + adler32) into exactly
/// `out_len` bytes. Mirrors `ZIP_Impl::Decompress`: short output is an error.
pub fn decompress(compressed: &[u8], out_len: usize) -> Result<Vec<u8>> {
    let out = engine::zlib_inflate(compressed, out_len)?;
    if out.len() != out_len {
        return Err(CodecError::OutputLength {
            expected: out_len,
            actual: out.len(),
        });
    }
    Ok(out)
}

#[cfg(feature = "zip-backend-libdeflater")]
mod engine {
    use super::*;
    use libdeflater::{
        adler32 as libdeflate_adler32, CompressionLvl, Compressor, DecompressionError, Decompressor,
    };

    pub fn adler32(data: &[u8]) -> u32 {
        libdeflate_adler32(data)
    }

    /// Raw deflate at the fixed level, allocated once at the bound.
    pub fn deflate(uncompressed: &[u8]) -> Result<Vec<u8>> {
        let level = CompressionLvl::new(COMPRESSION_LEVEL).expect("level is a valid constant");
        let mut compressor = Compressor::new(level);
        let bound = compressor.deflate_compress_bound(uncompressed.len());
        let mut out = vec![0u8; bound];
        let written = compressor
            .deflate_compress(uncompressed, &mut out)
            .map_err(|_| CodecError::Deflate)?;
        out.truncate(written);
        // The buffer was sized for incompressible input. Keeping that capacity
        // would hold every compressed channel at its raw size for as long as it
        // lives, so hand back the memory the stream did not use.
        out.shrink_to_fit();
        Ok(out)
    }

    pub fn zlib_inflate(compressed: &[u8], out_len: usize) -> Result<Vec<u8>> {
        let mut decompressor = Decompressor::new();
        let mut out = vec![0u8; out_len];
        let written = decompressor
            .zlib_decompress(compressed, &mut out)
            .map_err(|e| match e {
                DecompressionError::BadData => CodecError::Inflate("invalid input data"),
                DecompressionError::InsufficientSpace => {
                    CodecError::Inflate("insufficient output space")
                }
            })?;
        out.truncate(written);
        Ok(out)
    }
}

#[cfg(feature = "zip-backend-zlib-rs")]
mod engine {
    use super::*;

    pub fn adler32(data: &[u8]) -> u32 {
        zlib_rs::adler32::adler32(1, data)
    }

    /// Raw deflate at the fixed level (negative window bits), allocated once
    /// at the bound.
    pub fn deflate(uncompressed: &[u8]) -> Result<Vec<u8>> {
        let mut out = vec![0u8; zlib_rs::compress_bound(uncompressed.len())];
        let config = zlib_rs::DeflateConfig {
            level: COMPRESSION_LEVEL,
            window_bits: -15,
            ..zlib_rs::DeflateConfig::default()
        };
        let (written, rc) = zlib_rs::compress_slice(&mut out, uncompressed, config);
        if rc != zlib_rs::ReturnCode::Ok {
            return Err(CodecError::Deflate);
        }
        let len = written.len();
        out.truncate(len);
        out.shrink_to_fit();
        Ok(out)
    }

    pub fn zlib_inflate(compressed: &[u8], out_len: usize) -> Result<Vec<u8>> {
        let mut out = vec![0u8; out_len];
        let (written, rc) =
            zlib_rs::decompress_slice(&mut out, compressed, zlib_rs::InflateConfig::default());
        let len = written.len();
        match rc {
            zlib_rs::ReturnCode::Ok | zlib_rs::ReturnCode::StreamEnd => {}
            zlib_rs::ReturnCode::BufError => {
                return Err(CodecError::Inflate("insufficient output space"))
            }
            _ => return Err(CodecError::Inflate("invalid input data")),
        }
        out.truncate(len);
        Ok(out)
    }
}

#[cfg(feature = "zip-backend-miniz")]
mod engine {
    use super::*;

    /// RFC 1950 adler32. Four adds per 16 bytes — for channel-sized inputs
    /// the checksum is never the bottleneck, so no extra dependency.
    pub fn adler32(data: &[u8]) -> u32 {
        const MOD: u32 = 65521;
        let (mut a, mut b) = (1u32, 0u32);
        // Deferred-modulo chunking: b <= n*(n+1)/2*255 fits u32 while n <= 5552.
        for chunk in data.chunks(5552) {
            for &byte in chunk {
                a += u32::from(byte);
                b += a;
            }
            a %= MOD;
            b %= MOD;
        }
        (b << 16) | a
    }

    /// Raw deflate at the fixed level (`compress_to_vec` emits no wrapper).
    pub fn deflate(uncompressed: &[u8]) -> Result<Vec<u8>> {
        Ok(miniz_oxide::deflate::compress_to_vec(
            uncompressed,
            COMPRESSION_LEVEL as u8,
        ))
    }

    pub fn zlib_inflate(compressed: &[u8], out_len: usize) -> Result<Vec<u8>> {
        miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(compressed, out_len)
            .map_err(|_| CodecError::Inflate("invalid input data"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_data() -> Vec<u8> {
        // Repetitive-ish data so deflate actually compresses.
        (0..4096u32).map(|i| ((i / 7) % 251) as u8).collect()
    }

    #[test]
    fn compress_keeps_no_capacity_beyond_the_stream() {
        // The output buffer is sized for incompressible input; a compressible
        // channel must not carry that capacity around.
        let data = vec![7u8; 1 << 20];
        let compressed = compress(&data).unwrap();
        assert!(compressed.len() < 4096);
        assert_eq!(compressed.capacity(), compressed.len());
    }

    #[test]
    fn compress_writes_photoshop_zlib_framing() {
        let data = sample_data();
        let compressed = compress(&data).unwrap();
        // Header: 0x78 + level-4 byte 0x5E (upstream: 0x78 0x5E).
        assert_eq!(compressed[0], 0x78);
        assert_eq!(compressed[1], 0x5E);
        // Trailer: big-endian adler32 of the uncompressed bytes.
        let trailer = u32::from_be_bytes(compressed[compressed.len() - 4..].try_into().unwrap());
        assert_eq!(trailer, engine::adler32(&data));
    }

    #[test]
    fn round_trip() {
        let data = sample_data();
        let compressed = compress(&data).unwrap();
        assert!(compressed.len() < data.len());
        assert_eq!(decompress(&compressed, data.len()).unwrap(), data);
    }

    #[test]
    fn empty_input_round_trip() {
        let compressed = compress(&[]).unwrap();
        assert_eq!(decompress(&compressed, 0).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn decompress_rejects_bad_data_and_short_output() {
        assert!(matches!(
            decompress(&[0x78, 0x5E, 0xFF, 0xFF], 16),
            Err(CodecError::Inflate(_))
        ));

        let data = sample_data();
        let compressed = compress(&data).unwrap();
        // Asking for more than the stream holds must error, not truncate.
        assert!(matches!(
            decompress(&compressed, data.len() + 1),
            Err(CodecError::Inflate(_)) | Err(CodecError::OutputLength { .. })
        ));
    }
}
