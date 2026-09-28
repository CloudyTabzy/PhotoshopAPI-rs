//! ZIP prediction: per-scanline delta coding around the deflate step.
//!
//! Mirrors the `PredictionEncode` / `RemovePredictionEncoding` templates in
//! upstream `Compress_ZIP.h` / `Decompress_ZIP.h`:
//!
//! - **8/16-bit (integer)**: wrapping delta on native values per scanline,
//!   *then* big-endian encode. Decode reverses: BE decode, then prefix-sum.
//! - **32-bit (f32)**: per scanline, byte de-interleave *including* BE
//!   conversion (`1234 1234 … -> 1111 2222 3333 4444`), then bytewise delta.
//!   Decode reverses: bytewise prefix-sum, re-interleave, BE decode.
//!
//! Encode functions take typed input and return the BE byte stream handed to
//! deflate; decode functions take the inflated bytes and return typed values
//! (explicit in/out buffers instead of upstream's in-place mutation).
//! Scanline loops are independent and ready for rayon once benchmarks call
//! for it.

use rayon::prelude::*;

use crate::endian::{decode_be_bytes, encode_be_bytes, BeConvert};
use crate::error::{CodecError, Result};
use crate::rle::PARALLEL_MIN_BYTES;

/// Integer sample types usable with ZIP prediction (upstream uses u8/u16).
pub trait DeltaSample: BeConvert {
    /// Wrapping `self - prev` (encode step).
    fn delta(self, prev: Self) -> Self;
    /// Wrapping `self + prev` (decode step).
    fn undelta(self, prev: Self) -> Self;
}

impl DeltaSample for u8 {
    #[inline]
    fn delta(self, prev: Self) -> Self {
        self.wrapping_sub(prev)
    }

    #[inline]
    fn undelta(self, prev: Self) -> Self {
        self.wrapping_add(prev)
    }
}

impl DeltaSample for u16 {
    #[inline]
    fn delta(self, prev: Self) -> Self {
        self.wrapping_sub(prev)
    }

    #[inline]
    fn undelta(self, prev: Self) -> Self {
        self.wrapping_add(prev)
    }
}

pub(crate) fn check_sample_count(samples: usize, width: usize, height: usize) -> Result<()> {
    let expected = width
        .checked_mul(height)
        .ok_or(CodecError::InvalidInput("image dimensions overflow"))?;
    if samples != expected {
        return Err(CodecError::InvalidInput(
            "sample count does not equal width * height",
        ));
    }
    Ok(())
}

/// `width * bytes_per_sample` with an overflow check: the callers pass
/// file-derived geometry where a hostile `width` could otherwise wrap and
/// slip past the sample-count check.
pub(crate) fn row_bytes(width: usize, bytes_per_sample: usize) -> Result<usize> {
    width
        .checked_mul(bytes_per_sample)
        .ok_or(CodecError::InvalidInput("image dimensions overflow"))
}

/// Prediction-encode integer samples. Mirrors `PredictionEncode<T>`.
pub fn encode<T: DeltaSample>(data: &[T], width: usize, height: usize) -> Result<Vec<u8>> {
    check_sample_count(data.len(), width, height)?;
    let mut work = data.to_vec();
    for row in work.chunks_exact_mut(width) {
        let mut prev = row[0];
        for value in row.iter_mut().skip(1) {
            let current = *value;
            *value = current.delta(prev);
            prev = current;
        }
    }
    Ok(encode_be_bytes(&work))
}

/// Prediction-decode integer samples. Mirrors `RemovePredictionEncoding<T>`.
pub fn decode<T: DeltaSample>(bytes: &[u8], width: usize, height: usize) -> Result<Vec<T>> {
    let row = row_bytes(width, T::SIZE)?;
    check_sample_count(bytes.len(), row, height)?;
    let mut work = decode_be_bytes::<T>(bytes)?;
    for row in work.chunks_exact_mut(width) {
        for x in 1..width {
            row[x] = row[x].undelta(row[x - 1]);
        }
    }
    Ok(work)
}

/// Prediction-encode f32 samples: per-scanline byte de-interleave including
/// BE conversion, then bytewise delta. Mirrors `PredictionEncode<float32_t>`.
pub fn encode_f32(data: &[f32], width: usize, height: usize) -> Result<Vec<u8>> {
    check_sample_count(data.len(), width, height)?;
    let stride = row_bytes(width, 4)?;
    let mut out = vec![0u8; data.len() * 4];
    if width == 0 || height == 0 {
        return Ok(out);
    }

    // Per scanline: scatter the BE float bytes into 4 plane-major slices,
    // then bytewise delta across the whole row (planes included). Rows are
    // independent, so large images run in parallel.
    let encode_row = |row: &mut [u8], src: &[f32]| {
        let (plane0, rest) = row.split_at_mut(width);
        let (plane1, rest) = rest.split_at_mut(width);
        let (plane2, plane3) = rest.split_at_mut(width);
        for (x, &value) in src.iter().enumerate() {
            let be = value.to_be_bytes();
            plane0[x] = be[0];
            plane1[x] = be[1];
            plane2[x] = be[2];
            plane3[x] = be[3];
        }
        let mut prev = row[0];
        for value in row.iter_mut().skip(1) {
            let current = *value;
            *value = current.wrapping_sub(prev);
            prev = current;
        }
    };

    if data.len() >= PARALLEL_MIN_BYTES {
        out.par_chunks_mut(stride)
            .zip(data.par_chunks(width))
            .for_each(|(row, src)| encode_row(row, src));
    } else {
        for (row, src) in out.chunks_mut(stride).zip(data.chunks(width)) {
            encode_row(row, src);
        }
    }
    Ok(out)
}

/// Prediction-decode f32 samples: per-scanline bytewise prefix-sum, then
/// re-interleave and BE decode. Mirrors `RemovePredictionEncoding<float32_t>`.
pub fn decode_f32(bytes: &[u8], width: usize, height: usize) -> Result<Vec<f32>> {
    let stride = row_bytes(width, 4)?;
    check_sample_count(bytes.len(), stride, height)?;
    let mut planar = bytes.to_vec();
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }
    // Prefix-sum in place on the single working buffer (the transpose below
    // reads each plane element exactly once, so no second copy is needed).
    for row in planar.chunks_exact_mut(stride) {
        for i in 1..stride {
            row[i] = row[i].wrapping_add(row[i - 1]);
        }
    }

    let mut out = vec![0.0f32; width * height];
    let decode_row = |dst: &mut [f32], row: &[u8]| {
        let (plane0, rest) = row.split_at(width);
        let (plane1, rest) = rest.split_at(width);
        let (plane2, plane3) = rest.split_at(width);
        for ((((x, &b0), &b1), &b2), &b3) in dst
            .iter_mut()
            .zip(plane0)
            .zip(plane1)
            .zip(plane2)
            .zip(plane3)
        {
            *x = f32::from_be_bytes([b0, b1, b2, b3]);
        }
    };
    if bytes.len() >= PARALLEL_MIN_BYTES {
        out.par_chunks_mut(width)
            .zip(planar.par_chunks(stride))
            .for_each(|(dst, row)| decode_row(dst, row));
    } else {
        for (dst, row) in out.chunks_mut(width).zip(planar.chunks(stride)) {
            decode_row(dst, row);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_prediction_round_trip_u8() {
        let data: Vec<u8> = (0..12).map(|i| ((i * 37 + 14) % 200) as u8).collect();
        let encoded = encode(&data, 4, 3).unwrap();
        // First row by hand: deltas of [14, 51, 88, 125] -> [14, 37, 37, 37].
        assert_eq!(&encoded[..4], &[14, 37, 37, 37]);
        assert_eq!(decode::<u8>(&encoded, 4, 3).unwrap(), data);
    }

    #[test]
    fn int_prediction_round_trip_u16_be_framing() {
        let data: Vec<u16> = (0..8).map(|i| (i * 1000 + 255) as u16).collect();
        let encoded = encode(&data, 4, 2).unwrap();
        // Delta of [255, 1255, 2255, 3255] -> [255, 1000, 1000, 1000], BE bytes.
        let expected: Vec<u8> = [255u16, 1000, 1000, 1000]
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect();
        assert_eq!(&encoded[..8], &expected[..]);
        assert_eq!(decode::<u16>(&encoded, 4, 2).unwrap(), data);
    }

    #[test]
    fn f32_deinterleave_and_delta_pinned() {
        // 1.0f32 BE = 3F 80 00 00, 2.0f32 BE = 40 00 00 00, one 2-px scanline.
        // De-interleaved planes: [3F 40 | 80 00 | 00 00 | 00 00],
        // then bytewise delta: 3F, 40-3F=01, 80-40=40, 00-80=80 (wrapping), 00…
        let encoded = encode_f32(&[1.0, 2.0], 2, 1).unwrap();
        assert_eq!(encoded, [0x3F, 0x01, 0x40, 0x80, 0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn f32_prediction_round_trip() {
        let data: Vec<f32> = (0..24).map(|i| (i as f32 - 12.0) * 0.75).collect();
        let encoded = encode_f32(&data, 6, 4).unwrap();
        let decoded = decode_f32(&encoded, 6, 4).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn rejects_mismatched_geometry() {
        assert!(encode(&[1u8, 2, 3], 2, 2).is_err());
        assert!(decode::<u8>(&[0u8; 3], 2, 2).is_err());
        assert!(encode_f32(&[1.0f32; 3], 2, 2).is_err());
        assert!(decode_f32(&[0u8; 12], 2, 2).is_err());
    }

    #[test]
    fn full_zip_prediction_pipeline() {
        // The composition the format layer performs: prediction -> deflate ->
        // inflate -> un-prediction, for both integer and f32 paths.
        let data16: Vec<u16> = (0..4096u32).map(|i| (i % 1024) as u16).collect();
        let encoded = encode(&data16, 64, 64).unwrap();
        let compressed = crate::zip::compress(&encoded).unwrap();
        let inflated = crate::zip::decompress(&compressed, encoded.len()).unwrap();
        assert_eq!(decode::<u16>(&inflated, 64, 64).unwrap(), data16);

        let data32: Vec<f32> = (0..4096u32).map(|i| i as f32 * 0.5).collect();
        let encoded = encode_f32(&data32, 64, 64).unwrap();
        let compressed = crate::zip::compress(&encoded).unwrap();
        let inflated = crate::zip::decompress(&compressed, encoded.len()).unwrap();
        assert_eq!(decode_f32(&inflated, 64, 64).unwrap(), data32);
    }

    #[test]
    fn large_f32_prediction_round_trip_matches_parallel_and_sequential() {
        // 128×512 f32 samples — over PARALLEL_MIN_BYTES, so the f32 paths
        // take the rayon row loops.
        const { assert!(128 * 512 * 4 > crate::rle::PARALLEL_MIN_BYTES) };
        let data: Vec<f32> = (0..128 * 512u32)
            .map(|i| ((i % 1024) as f32 - 512.0) * 0.001)
            .collect();
        let encoded = encode_f32(&data, 512, 128).unwrap();
        assert_eq!(decode_f32(&encoded, 512, 128).unwrap(), data);
    }
}
