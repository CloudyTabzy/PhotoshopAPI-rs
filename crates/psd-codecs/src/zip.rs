//! zlib streams the Photoshop way: `0x78` + level byte + raw deflate + BE adler32.
//!
//! Mirrors upstream `Compress_ZIP.h` / `Decompress_ZIP.h` (`ZIP_Impl::Compress`
//! / `ZIP_Impl::Decompress`). Upstream hand-assembles the stream to control the
//! header byte; this port preserves that framing at upstream's compression level 4
//! (`ZIP_COMPRESSION_LVL`).
//!
//! The deflate engine is chosen by exactly one `zip-backend-*` feature:
//! `linflate` (default — the znippy decoder, fastest measured inflate at
//! ~15–25% over zlib-rs on compressible channel planes, with zlib-rs handling
//! large literal-heavy streams and deflate; both verify the Adler trailer), `zlib-rs`
//! (pure Rust zlib-ng port), or `miniz` (miniz_oxide, portable fallback).
//! Large noisy inputs may use Huffman-only deflate when a sampled size comparison
//! favors it. Framing and decoded pixels are identical across all backends.

#[cfg(not(any(
    feature = "zip-backend-linflate",
    feature = "zip-backend-zlib-rs",
    feature = "zip-backend-miniz"
)))]
compile_error!("psd-codecs needs exactly one `zip-backend-*` feature enabled");

#[cfg(any(
    all(feature = "zip-backend-linflate", feature = "zip-backend-zlib-rs"),
    all(feature = "zip-backend-linflate", feature = "zip-backend-miniz"),
    all(feature = "zip-backend-zlib-rs", feature = "zip-backend-miniz")
))]
compile_error!("psd-codecs `zip-backend-*` features are mutually exclusive");

use crate::error::{CodecError, Result};

/// Upstream's `ZIP_COMPRESSION_LVL`. Fixed: the zlib header byte written on
/// compress is derived from this, so changing it changes the byte stream.
pub const COMPRESSION_LEVEL: i32 = 4;

/// zlib header's second byte for a compression level (upstream's mapping in
/// `ZIP_Impl::Compress`; the first byte is always `0x78`).
#[cfg(feature = "zip-backend-miniz")]
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
/// uncompressed input. The framing matches upstream `ZIP_Impl::Compress`;
/// entropy coding can differ between engines and compression strategies.
pub fn compress(uncompressed: &[u8]) -> Result<Vec<u8>> {
    engine::compress(uncompressed)
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

/// Decode into the final aligned allocation. The byte view is checked by
/// bytemuck; every supported sample type accepts every possible bit pattern.
pub(crate) fn decompress_native<T: bytemuck::Pod + Default>(
    compressed: &[u8],
    samples: usize,
) -> Result<Vec<T>> {
    let expected = samples
        .checked_mul(std::mem::size_of::<T>())
        .filter(|&bytes| bytes <= isize::MAX as usize)
        .ok_or(CodecError::InvalidInput("sample byte count overflows"))?;
    if compressed.len() < 6 {
        return Err(CodecError::Inflate("invalid input data"));
    }
    #[cfg(feature = "zip-backend-linflate")]
    if expected > linflate::max_inflated_size(compressed.len() - 6) {
        return Err(CodecError::Inflate("implausible output size"));
    }
    let mut output = vec![T::default(); samples];
    let bytes = bytemuck::cast_slice_mut(&mut output);
    #[cfg(any(feature = "zip-backend-zlib-rs", feature = "zip-backend-linflate"))]
    {
        let (written, rc) =
            zlib_rs::decompress_slice(bytes, compressed, zlib_rs::InflateConfig::default());
        if rc != zlib_rs::ReturnCode::Ok {
            return Err(CodecError::Inflate("invalid input data"));
        }
        if written.len() != expected {
            return Err(CodecError::OutputLength {
                expected,
                actual: written.len(),
            });
        }
    }
    #[cfg(feature = "zip-backend-miniz")]
    {
        bytes.copy_from_slice(&decompress(compressed, expected)?);
    }
    Ok(output)
}

#[cfg(any(feature = "zip-backend-zlib-rs", feature = "zip-backend-linflate"))]
mod zlib_compress {
    use super::*;

    fn config(strategy: zlib_rs::Strategy) -> zlib_rs::DeflateConfig {
        zlib_rs::DeflateConfig {
            level: COMPRESSION_LEVEL,
            strategy,
            ..Default::default()
        }
    }

    // Noisy prediction data has few useful matches. Compare three small,
    // separated windows before paying for a full LZ77 search. Huffman-only
    // is selected only when its estimated size is within 2% of the default
    // and the default cannot halve the input. Smooth channels retain LZ77.
    fn strategy(data: &[u8], total_len: usize) -> Result<(zlib_rs::Strategy, usize)> {
        use zlib_rs::Strategy;
        const SAMPLE: usize = 16 * 1024;
        if total_len < 1024 * 1024 {
            return Ok((Strategy::Default, zlib_rs::compress_bound(total_len)));
        }
        let mut output = vec![0; zlib_rs::compress_bound(SAMPLE)];
        let starts = [0, (data.len() - SAMPLE) / 2, data.len() - SAMPLE];
        let capacity = |size: usize| {
            size.saturating_mul(total_len)
                .div_ceil(3 * SAMPLE)
                .saturating_mul(102)
                .div_ceil(100)
                .saturating_add(32 * 1024)
                .min(zlib_rs::compress_bound(total_len))
        };
        let mut sizes = [0usize; 2];
        for (index, strategy) in [Strategy::Default, Strategy::HuffmanOnly]
            .into_iter()
            .enumerate()
        {
            for start in starts {
                let (written, rc) = zlib_rs::compress_slice(
                    &mut output,
                    &data[start..start + SAMPLE],
                    config(strategy),
                );
                if rc != zlib_rs::ReturnCode::Ok {
                    return Err(CodecError::Deflate);
                }
                sizes[index] += written.len();
            }
            if index == 0 && sizes[0] < 3 * SAMPLE / 2 {
                return Ok((Strategy::Default, capacity(sizes[0])));
            }
        }
        Ok(if sizes[1] * 100 <= sizes[0] * 102 {
            (Strategy::HuffmanOnly, capacity(sizes[1]))
        } else {
            (Strategy::Default, capacity(sizes[0]))
        })
    }

    pub(super) fn compress(data: &[u8]) -> Result<Vec<u8>> {
        let (strategy, _) = strategy(data, data.len())?;
        // Write the complete stream once. The engine computes Adler during
        // compression, eliminating the raw-stream copy and a separate pass.
        let mut output = vec![0; zlib_rs::compress_bound(data.len())];
        let (written, rc) = zlib_rs::compress_slice(&mut output, data, config(strategy));
        if rc != zlib_rs::ReturnCode::Ok {
            return Err(CodecError::Deflate);
        }
        let len = written.len();
        output.truncate(len);
        // The engine's Huffman strategy advertises a different level hint.
        // Photoshop's fixed level-4 framing stays 0x78 0x5E for every strategy.
        output[0] = 0x78;
        output[1] = 0x5E;
        output.shrink_to_fit();
        Ok(output)
    }

    /// Feed prediction blocks without retaining an encoded channel-sized buffer.
    pub(crate) struct StreamingEncoder {
        deflate: zlib_rs::Deflate,
        output: Vec<u8>,
        scratch: Vec<u8>,
    }

    impl StreamingEncoder {
        pub(crate) fn new(sample: &[u8], total_len: usize) -> Result<Self> {
            let (strategy, capacity) = strategy(sample, total_len)?;
            Ok(Self {
                deflate: zlib_rs::Deflate::new_with_config(config(strategy)),
                // Sampled capacity avoids reserving the whole raw input for a
                // tiny stream. Vec can still grow if the estimate was low.
                output: Vec::with_capacity(capacity),
                scratch: vec![0; 32 * 1024],
            })
        }

        pub(crate) fn push(&mut self, mut input: &[u8]) -> Result<()> {
            while !input.is_empty() {
                let before_in = self.deflate.total_in();
                let before_out = self.deflate.total_out();
                self.deflate
                    .compress(input, &mut self.scratch, zlib_rs::DeflateFlush::NoFlush)
                    .map_err(|_| CodecError::Deflate)?;
                let consumed = (self.deflate.total_in() - before_in) as usize;
                let written = (self.deflate.total_out() - before_out) as usize;
                if consumed == 0 && written == 0 {
                    return Err(CodecError::Deflate);
                }
                self.output.extend_from_slice(&self.scratch[..written]);
                input = &input[consumed..];
            }
            Ok(())
        }

        pub(crate) fn finish(mut self) -> Result<Vec<u8>> {
            loop {
                let before = self.deflate.total_out();
                let status = self
                    .deflate
                    .compress(&[], &mut self.scratch, zlib_rs::DeflateFlush::Finish)
                    .map_err(|_| CodecError::Deflate)?;
                let written = (self.deflate.total_out() - before) as usize;
                self.output.extend_from_slice(&self.scratch[..written]);
                if status == zlib_rs::Status::StreamEnd {
                    break;
                }
                if written == 0 {
                    return Err(CodecError::Deflate);
                }
            }
            self.output[..2].copy_from_slice(&[0x78, 0x5E]);
            self.output.shrink_to_fit();
            Ok(self.output)
        }
    }
}

#[cfg(any(feature = "zip-backend-zlib-rs", feature = "zip-backend-linflate"))]
pub(crate) use zlib_compress::StreamingEncoder;

#[cfg(feature = "zip-backend-zlib-rs")]
mod engine {
    use super::*;

    #[cfg(test)]
    pub fn adler32(data: &[u8]) -> u32 {
        zlib_rs::adler32::adler32(1, data)
    }

    /// Framed deflate at the fixed level, allocated once at the bound.
    pub fn compress(uncompressed: &[u8]) -> Result<Vec<u8>> {
        zlib_compress::compress(uncompressed)
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

/// `zip-backend-linflate`: the znippy decoder (fastest measured inflate)
/// paired with zlib-rs's deflate — ldeflate itself is zlib-rs inside, so
/// the compress side comes straight from the same engine.
#[cfg(feature = "zip-backend-linflate")]
mod engine {
    use super::*;

    pub fn adler32(data: &[u8]) -> u32 {
        zlib_rs::adler32::adler32(1, data)
    }

    pub fn compress(uncompressed: &[u8]) -> Result<Vec<u8>> {
        zlib_compress::compress(uncompressed)
    }

    /// linflate speaks raw DEFLATE — strip our own zlib header and adler32
    /// trailer, then verify the trailer against the decoded output so this
    /// backend keeps the zlib engines' checksum guarantee.
    pub fn zlib_inflate(compressed: &[u8], out_len: usize) -> Result<Vec<u8>> {
        if compressed.len() < 6 {
            return Err(CodecError::Inflate("invalid input data"));
        }
        let header = u16::from_be_bytes([compressed[0], compressed[1]]);
        if compressed[0] & 0x0F != 8
            || compressed[0] >> 4 > 7
            || !header.is_multiple_of(31)
            || compressed[1] & 0x20 != 0
        {
            return Err(CodecError::Inflate("invalid zlib header"));
        }
        // Large literal-heavy streams favor zlib-rs; linflate leads on the
        // smaller, highly compressible planes. Both verify the Adler trailer.
        if compressed.len() >= 1024 * 1024 && compressed.len() >= out_len / 2 {
            let mut out = vec![0; out_len];
            let (written, rc) =
                zlib_rs::decompress_slice(&mut out, compressed, zlib_rs::InflateConfig::default());
            let len = written.len();
            if rc != zlib_rs::ReturnCode::Ok {
                return Err(CodecError::Inflate("invalid input data"));
            }
            out.truncate(len);
            return Ok(out);
        }
        let trailer = u32::from_be_bytes(
            compressed[compressed.len() - 4..]
                .try_into()
                .map_err(|_| CodecError::Inflate("invalid input data"))?,
        );
        let out = linflate::inflate_to_vec(&compressed[2..compressed.len() - 4], out_len).map_err(
            |e| match e {
                linflate::InflateError::ImplausibleSize { .. } => {
                    CodecError::Inflate("insufficient output space")
                }
                _ => CodecError::Inflate("invalid input data"),
            },
        )?;
        if adler32(&out) != trailer {
            return Err(CodecError::Inflate("invalid input data"));
        }
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
    pub fn compress(uncompressed: &[u8]) -> Result<Vec<u8>> {
        let raw = miniz_oxide::deflate::compress_to_vec(uncompressed, COMPRESSION_LEVEL as u8);
        let mut output = Vec::with_capacity(raw.len() + 6);
        output.extend_from_slice(&[0x78, zlib_header_byte(COMPRESSION_LEVEL)]);
        output.extend_from_slice(&raw);
        output.extend_from_slice(&adler32(uncompressed).to_be_bytes());
        Ok(output)
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

    #[test]
    fn noisy_and_smooth_large_streams_keep_framing_checksums_and_pixels() {
        let mut state = 0x1234_5678u32;
        let noise: Vec<_> = (0..1024 * 1024)
            .map(|_| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                (state >> 24) as u8
            })
            .collect();
        for data in [noise, vec![7u8; 1024 * 1024]] {
            let mut encoded = compress(&data).unwrap();
            assert_eq!(&encoded[..2], &[0x78, 0x5E]);
            assert_eq!(decompress(&encoded, data.len()).unwrap(), data);
            let end = encoded.len() - 1;
            encoded[end] ^= 1;
            assert!(decompress(&encoded, data.len()).is_err());
        }
    }

    #[test]
    fn zlib_header_validation_rejects_invalid_methods_and_dictionary_requests() {
        let encoded = compress(&sample_data()).unwrap();
        for header in [[0x78, 0x00], [0x79, 0x18], [0x88, 0x1C], [0x78, 0x20]] {
            let mut invalid = encoded.clone();
            invalid[..2].copy_from_slice(&header);
            assert!(decompress(&invalid, sample_data().len()).is_err());
        }
    }

    #[test]
    fn direct_typed_decode_checks_empty_streams_lengths_and_overflow() {
        let empty = compress(&[]).unwrap();
        assert!(decompress_native::<u16>(&empty, 0).unwrap().is_empty());
        assert!(decompress_native::<f32>(&empty, 0).unwrap().is_empty());
        assert!(matches!(
            decompress_native::<u16>(&empty, usize::MAX),
            Err(CodecError::InvalidInput(_))
        ));
        let packed = compress(&[1, 2, 3, 4]).unwrap();
        assert!(decompress_native::<u16>(&packed, 3).is_err());
        assert!(decompress_native::<u16>(&packed, 1).is_err());
    }
}
