//! zlib streams the Photoshop way: `0x78` + level byte + raw deflate + BE adler32.
//!
//! Mirrors upstream `Compress_ZIP.h` / `Decompress_ZIP.h` (`ZIP_Impl::Compress`
//! / `ZIP_Impl::Decompress`). Upstream hand-assembles the stream to control the
//! header byte; this port does the same via libdeflate's raw deflate plus its
//! adler32, at upstream's fixed compression level 4 (`ZIP_COMPRESSION_LVL`).

use libdeflater::{adler32, CompressionLvl, Compressor, DecompressionError, Decompressor};

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
    let level = CompressionLvl::new(COMPRESSION_LEVEL).expect("level is a valid constant");
    let mut compressor = Compressor::new(level);
    // Allocate the final frame up front and deflate straight into it: two
    // header bytes, the raw deflate stream, then the 4-byte adler trailer.
    // This avoids the scratch buffer + memcpy the naive assembly pays.
    let bound = compressor.deflate_compress_bound(uncompressed.len());
    let mut out = vec![0u8; 2 + bound + 4];
    let written = compressor
        .deflate_compress(uncompressed, &mut out[2..2 + bound])
        .map_err(|_| CodecError::Deflate)?;
    out.truncate(2 + written);
    out[0] = 0x78;
    out[1] = zlib_header_byte(COMPRESSION_LEVEL);
    out.extend_from_slice(&adler32(uncompressed).to_be_bytes());
    // The buffer was sized for incompressible input. Keeping that capacity
    // would hold every compressed channel at its raw size for as long as it
    // lives, so hand back the memory the stream did not use.
    out.shrink_to_fit();
    Ok(out)
}

/// Decompress a full zlib stream (header + deflate + adler32) into exactly
/// `out_len` bytes. Mirrors `ZIP_Impl::Decompress`: short output is an error.
pub fn decompress(compressed: &[u8], out_len: usize) -> Result<Vec<u8>> {
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
    if written != out_len {
        return Err(CodecError::OutputLength {
            expected: out_len,
            actual: written,
        });
    }
    Ok(out)
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
        assert_eq!(trailer, adler32(&data));
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
