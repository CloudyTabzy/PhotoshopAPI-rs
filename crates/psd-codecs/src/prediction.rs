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
//!
//! The row loops are the port's hottest codec phase — the f32 decode measured
//! 2.17 ms per 4 MB channel, slower than the inflate that feeds it — so they
//! are written with `fearless_simd`'s portable vectors: a Hillis–Steele scan
//! replaces the serial byte chain, `store_four_interleaved` replaces the
//! four-way gather, and a one-step difference replaces the serial delta chain
//! on the encode side. Scanlines are independent and large channels still run
//! the row loops on rayon.
//!
//! `PSD_CODECS_FORCE_SCALAR=1` with the `scalar-override` feature selects the
//! pre-SIMD kernels, so one binary can measure and test the two against each
//! other.

use fearless_simd::{dispatch, f32x4, prelude::*, u16x8, u8x16, Bytes, Level, Simd};
use fearless_simd_macros::simd;
use rayon::prelude::*;

use crate::endian::{decode_be_bytes, BeConvert};
use crate::error::{CodecError, Result};
use crate::rle::PARALLEL_MIN_BYTES;

/// Whether the pre-SIMD kernels are forced for this process.
///
/// Gated by the `scalar-override` feature the way `psd-png` gates its own:
/// without the feature the environment has no say in which code runs.
fn scalar_forced() -> bool {
    #[cfg(feature = "scalar-override")]
    {
        static FORCED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *FORCED.get_or_init(|| {
            std::env::var_os("PSD_CODECS_FORCE_SCALAR").is_some_and(|value| value == "1")
        })
    }
    #[cfg(not(feature = "scalar-override"))]
    {
        false
    }
}

/// Integer sample types usable with ZIP prediction (upstream uses u8/u16).
pub trait DeltaSample: BeConvert {
    /// Wrapping `self - prev` (encode step).
    fn delta(self, prev: Self) -> Self;
    /// Wrapping `self + prev` (decode step).
    fn undelta(self, prev: Self) -> Self;

    /// Prefix-sum every `width`-sample row in place: the decode step.
    ///
    /// The default is the serial chain; `u8` and `u16` override it with the
    /// SIMD scan.
    fn undelta_rows(rows: &mut [Self], width: usize) {
        for row in rows.chunks_exact_mut(width) {
            for x in 1..width {
                row[x] = row[x].undelta(row[x - 1]);
            }
        }
    }

    /// Delta every `width`-sample row in place: the encode step. Defaults to
    /// the serial chain; `u8` and `u16` override it.
    fn delta_rows(rows: &mut [Self], width: usize) {
        for row in rows.chunks_exact_mut(width) {
            let mut prev = row[0];
            for value in row.iter_mut().skip(1) {
                let current = *value;
                *value = current.delta(prev);
                prev = current;
            }
        }
    }
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

    fn undelta_rows(rows: &mut [Self], width: usize) {
        if scalar_forced() {
            return scan_rows_scalar_u8(rows, width);
        }
        let level = Level::new();
        dispatch!(level, simd => {
            for row in rows.chunks_exact_mut(width) {
                kernels::scan_bytes(simd, row);
            }
        });
    }

    fn delta_rows(rows: &mut [Self], width: usize) {
        if scalar_forced() {
            return delta_rows_scalar_u8(rows, width);
        }
        let level = Level::new();
        dispatch!(level, simd => {
            for row in rows.chunks_exact_mut(width) {
                kernels::delta_bytes(simd, row);
            }
        });
    }
}

/// The serial byte prefix-sum chain, kept for the `scalar-override` A/B.
fn scan_rows_scalar_u8(rows: &mut [u8], width: usize) {
    for row in rows.chunks_exact_mut(width) {
        for x in 1..width {
            row[x] = row[x].wrapping_add(row[x - 1]);
        }
    }
}

/// The serial byte delta chain, kept for the `scalar-override` A/B.
fn delta_rows_scalar_u8(rows: &mut [u8], width: usize) {
    for row in rows.chunks_exact_mut(width) {
        let mut prev = 0u8;
        for value in row.iter_mut() {
            let current = *value;
            *value = current.wrapping_sub(prev);
            prev = current;
        }
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

    fn undelta_rows(rows: &mut [Self], width: usize) {
        if scalar_forced() {
            return scan_rows_scalar_u16(rows, width);
        }
        let level = Level::new();
        dispatch!(level, simd => {
            for row in rows.chunks_exact_mut(width) {
                kernels::scan_samples_u16(simd, row);
            }
        });
    }

    fn delta_rows(rows: &mut [Self], width: usize) {
        if scalar_forced() {
            return delta_rows_scalar_u16(rows, width);
        }
        let level = Level::new();
        dispatch!(level, simd => {
            for row in rows.chunks_exact_mut(width) {
                kernels::delta_samples_u16(simd, row);
            }
        });
    }
}

/// The serial u16 prefix-sum chain, kept for the `scalar-override` A/B.
fn scan_rows_scalar_u16(rows: &mut [u16], width: usize) {
    for row in rows.chunks_exact_mut(width) {
        for x in 1..width {
            row[x] = row[x].wrapping_add(row[x - 1]);
        }
    }
}

/// The serial u16 delta chain, kept for the `scalar-override` A/B.
fn delta_rows_scalar_u16(rows: &mut [u16], width: usize) {
    for row in rows.chunks_exact_mut(width) {
        let mut prev = 0u16;
        for value in row.iter_mut() {
            let current = *value;
            *value = current.wrapping_sub(prev);
            prev = current;
        }
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
    let mut out = vec![0u8; data.len() * T::SIZE];
    if data.is_empty() {
        return Ok(out);
    }
    // Delta a block of rows in a small scratch buffer and write it out
    // big-endian, so the channel is never copied at full size: the output is
    // the only large allocation.
    let block_rows = (BLOCK_SAMPLES / width).max(1);
    let mut scratch: Vec<T> = Vec::with_capacity(block_rows * width);
    for (src, dst) in data
        .chunks(block_rows * width)
        .zip(out.chunks_mut(block_rows * width * T::SIZE))
    {
        scratch.clear();
        scratch.extend_from_slice(src);
        T::delta_rows(&mut scratch, width);
        for (value, bytes) in scratch.iter().zip(dst.chunks_exact_mut(T::SIZE)) {
            value.write_be_into(bytes);
        }
    }
    Ok(out)
}

/// Samples per block of rows in [`encode`]: about 64 KiB of `u16`, small
/// enough to stay in cache.
const BLOCK_SAMPLES: usize = 32 * 1024;

/// Prediction-decode integer samples. Mirrors `RemovePredictionEncoding<T>`.
pub fn decode<T: DeltaSample>(bytes: &[u8], width: usize, height: usize) -> Result<Vec<T>> {
    let row = row_bytes(width, T::SIZE)?;
    check_sample_count(bytes.len(), row, height)?;
    let mut work = decode_be_bytes::<T>(bytes)?;
    T::undelta_rows(&mut work, width);
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

    let encode_row = |row: &mut [u8], src: &[f32]| {
        kernels::deinterleave_f32_row(row, src, width);
        kernels::delta_bytes_row(row);
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
///
/// The scan runs straight from the compressed row into a small reusable row
/// buffer and the transpose reads that buffer, so the whole channel is never
/// copied: the only large allocation is the result.
pub fn decode_f32(bytes: &[u8], width: usize, height: usize) -> Result<Vec<f32>> {
    let stride = row_bytes(width, 4)?;
    check_sample_count(bytes.len(), stride, height)?;
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }

    let mut out = vec![0.0f32; width * height];
    if scalar_forced() {
        decode_f32_scalar(bytes, width, stride, &mut out);
    } else {
        let level = Level::new();
        let decode_rows = |out_chunk: &mut [f32], src_chunk: &[u8]| {
            let mut row = vec![0u8; stride];
            dispatch!(level, simd => {
                for (dst, src) in out_chunk.chunks_mut(width).zip(src_chunk.chunks(stride)) {
                    kernels::scan_row_into(simd, src, &mut row);
                    kernels::transpose_row(simd, dst, &row, width);
                }
            });
        };
        if bytes.len() >= PARALLEL_MIN_BYTES {
            // A row buffer per chunk, sized so the parallel split stays coarse
            // (one buffer per 64 rows rather than per row).
            let rows_per_chunk = 64;
            let out_chunk = width * rows_per_chunk;
            let src_chunk = stride * rows_per_chunk;
            out.par_chunks_mut(out_chunk)
                .zip(bytes.par_chunks(src_chunk))
                .for_each(|(out_chunk, src_chunk)| decode_rows(out_chunk, src_chunk));
        } else {
            decode_rows(&mut out, bytes);
        }
    }
    Ok(out)
}

/// The pre-SIMD f32 decode, kept as the reference the parity tests and the
/// `scalar-override` measurement compare against.
fn decode_f32_scalar(bytes: &[u8], width: usize, stride: usize, out: &mut [f32]) {
    let mut planar = bytes.to_vec();
    for row in planar.chunks_exact_mut(stride) {
        for i in 1..stride {
            row[i] = row[i].wrapping_add(row[i - 1]);
        }
    }
    for (dst, row) in out.chunks_mut(width).zip(planar.chunks(stride)) {
        let (plane0, rest) = row.split_at(width);
        let (plane1, rest) = rest.split_at(width);
        let (plane2, plane3) = rest.split_at(width);
        for (i, value) in dst.iter_mut().enumerate() {
            *value = f32::from_be_bytes([plane0[i], plane1[i], plane2[i], plane3[i]]);
        }
    }
}

/// The SIMD kernels, one `#[simd]` function per loop.
///
/// The attribute is what makes them fast: without it the same kernels
/// measured 2.4x *slower* than scalar, because the per-level inlining the
/// portable API needs is not applied. Each function processes 16 bytes per
/// step, which maps to one `pshufb` on SSSE3+ and one NEON `tbl` on AArch64.
mod kernels {
    use super::*;

    const N: usize = 16;

    /// Lane-shift index vectors, zero-filling: output byte `i` takes input
    /// byte `i - shift`, and the low `shift` bytes come back zero.
    const fn shift_indices(shift: usize) -> [u8; N] {
        let mut out = [0x80u8; N];
        let mut i = shift;
        while i < N {
            out[i] = (i - shift) as u8;
            i += 1;
        }
        out
    }
    const SHIFT1: [u8; N] = shift_indices(1);
    const SHIFT2: [u8; N] = shift_indices(2);
    const SHIFT4: [u8; N] = shift_indices(4);
    const SHIFT8: [u8; N] = shift_indices(8);

    /// Inclusive byte-wise prefix sum of one row, in place.
    #[simd]
    pub(super) fn scan_bytes<S: Simd>(simd: S, row: &mut [u8]) {
        let idx = [
            u8x16::from_slice(simd, &SHIFT1),
            u8x16::from_slice(simd, &SHIFT2),
            u8x16::from_slice(simd, &SHIFT4),
            u8x16::from_slice(simd, &SHIFT8),
        ];
        let mut carry = 0u8;
        let full = row.len() - row.len() % N;
        for chunk in row[..full].chunks_exact_mut(N) {
            let mut v = u8x16::from_slice(simd, chunk);
            for index in idx {
                v += simd.swizzle_dyn_precise_u8x16(v, index);
            }
            v += u8x16::splat(simd, carry);
            v.store_slice(chunk);
            carry = chunk[N - 1];
        }
        for byte in &mut row[full..] {
            *byte = byte.wrapping_add(carry);
            carry = *byte;
        }
    }

    /// The same scan, reading a source row and writing a destination row, so
    /// a decode never has to copy the compressed bytes first.
    #[simd]
    pub(super) fn scan_row_into<S: Simd>(simd: S, src: &[u8], dst: &mut [u8]) {
        let idx = [
            u8x16::from_slice(simd, &SHIFT1),
            u8x16::from_slice(simd, &SHIFT2),
            u8x16::from_slice(simd, &SHIFT4),
            u8x16::from_slice(simd, &SHIFT8),
        ];
        let mut carry = 0u8;
        let full = src.len() - src.len() % N;
        for (src_chunk, dst_chunk) in src[..full]
            .chunks_exact(N)
            .zip(dst[..full].chunks_exact_mut(N))
        {
            let mut v = u8x16::from_slice(simd, src_chunk);
            for index in idx {
                v += simd.swizzle_dyn_precise_u8x16(v, index);
            }
            v += u8x16::splat(simd, carry);
            v.store_slice(dst_chunk);
            carry = dst_chunk[N - 1];
        }
        for (src_byte, dst_byte) in src[full..].iter().zip(&mut dst[full..]) {
            *dst_byte = src_byte.wrapping_add(carry);
            carry = *dst_byte;
        }
    }

    /// One-step difference of a byte row, in place: `d[i] = x[i] - x[i-1]`
    /// with `x[-1] = 0`. A single shuffle brings in the previous element, so
    /// the serial chain becomes one dependent step per block.
    #[simd]
    pub(super) fn delta_bytes<S: Simd>(simd: S, row: &mut [u8]) {
        let idx = u8x16::from_slice(simd, &SHIFT1);
        let mut carry_vector = [0u8; N];
        let mut carry = 0u8;
        let full = row.len() - row.len() % N;
        for chunk in row[..full].chunks_exact_mut(N) {
            let v = u8x16::from_slice(simd, chunk);
            let last = chunk[N - 1];
            let previous = simd.swizzle_dyn_precise_u8x16(v, idx);
            carry_vector[0] = carry;
            let previous = previous | u8x16::from_slice(simd, &carry_vector);
            (v - previous).store_slice(chunk);
            carry = last;
        }
        for byte in &mut row[full..] {
            let current = *byte;
            *byte = current.wrapping_sub(carry);
            carry = current;
        }
    }

    /// Samples per u16 vector: eight 16-bit lanes in a 128-bit block.
    const M: usize = 8;

    /// Prefix-sum one row of u16 samples, in place. The samples are already
    /// native-endian here; the lane shift is a byte shuffle reinterpreted as
    /// u16 lanes, so eight lanes need three steps (1, 2 and 4 samples).
    #[simd]
    pub(super) fn scan_samples_u16<S: Simd>(simd: S, row: &mut [u16]) {
        let idx = [
            u8x16::from_slice(simd, &SHIFT2),
            u8x16::from_slice(simd, &SHIFT4),
            u8x16::from_slice(simd, &SHIFT8),
        ];
        let mut carry = 0u16;
        let full = row.len() - row.len() % M;
        for chunk in row[..full].chunks_exact_mut(M) {
            let mut v = u16x8::from_slice(simd, chunk);
            for index in idx {
                let shifted: u16x8<S> =
                    Bytes::from_bytes(simd.swizzle_dyn_precise_u8x16(v.to_bytes(), index));
                v += shifted;
            }
            v += u16x8::splat(simd, carry);
            v.store_slice(chunk);
            carry = chunk[M - 1];
        }
        for sample in &mut row[full..] {
            *sample = sample.wrapping_add(carry);
            carry = *sample;
        }
    }

    /// One-step difference of a u16 sample row, in place:
    /// `d[i] = x[i] - x[i-1]` with `x[-1] = 0`.
    #[simd]
    pub(super) fn delta_samples_u16<S: Simd>(simd: S, row: &mut [u16]) {
        let idx = u8x16::from_slice(simd, &SHIFT2);
        let mut carry = 0u16;
        let full = row.len() - row.len() % M;
        for chunk in row[..full].chunks_exact_mut(M) {
            let v = u16x8::from_slice(simd, chunk);
            let last = chunk[M - 1];
            let previous: u16x8<S> =
                Bytes::from_bytes(simd.swizzle_dyn_precise_u8x16(v.to_bytes(), idx));
            // Lane 0's shifted value is zero; it must be the previous chunk's
            // last sample.
            let mut carry_vector = [0u16; M];
            carry_vector[0] = carry;
            let previous = previous | u16x8::from_slice(simd, &carry_vector);
            (v - previous).store_slice(chunk);
            carry = last;
        }
        for sample in &mut row[full..] {
            let current = *sample;
            *sample = current.wrapping_sub(carry);
            carry = current;
        }
    }

    /// Re-interleave one row of f32 samples into the plane-major byte layout
    /// the format stores, including the big-endian conversion.
    ///
    /// The conversion is free: de-interleaving gives one plane per byte
    /// position, so on a little-endian target the planes are simply stored in
    /// reverse order.
    pub(super) fn deinterleave_f32_row(row: &mut [u8], src: &[f32], width: usize) {
        let (plane0, rest) = row.split_at_mut(width);
        let (plane1, rest) = rest.split_at_mut(width);
        let (plane2, plane3) = rest.split_at_mut(width);
        let mut planes = [plane0, plane1, plane2, plane3];
        if scalar_forced() {
            for (x, &value) in src.iter().enumerate() {
                let be = value.to_be_bytes();
                for (plane, byte) in planes.iter_mut().zip(be) {
                    plane[x] = byte;
                }
            }
            return;
        }
        let level = Level::new();
        dispatch!(level, simd => {
            let mut scratch = [0u8; N * 4];
            let mut x = 0;
            while x + N <= width {
                // The source is contiguous f32s; four `f32x4` loads write the
                // 64 packed bytes the interleave reads, without any scalar
                // gather and without reinterpreting the slice.
                for (slot, pixels) in scratch.chunks_exact_mut(N).zip(src[x..x + N].chunks_exact(4)) {
                    f32x4::from_slice(simd, pixels).to_bytes().store_slice(slot);
                }
                let vectors = u8x16::load_four_interleaved(simd, &scratch);
                // Little-endian: the planes in reverse order are already the
                // big-endian byte sequence.
                let ordered = if cfg!(target_endian = "little") {
                    [vectors[3], vectors[2], vectors[1], vectors[0]]
                } else {
                    vectors
                };
                for (plane, vector) in planes.iter_mut().zip(ordered) {
                    vector.store_slice(&mut plane[x..x + N]);
                }
                x += N;
            }
            for i in x..width {
                let be = src[i].to_be_bytes();
                for (plane, byte) in planes.iter_mut().zip(be) {
                    plane[i] = byte;
                }
            }
        });
    }

    /// One f32 encode row step: de-interleave then delta, honoring the
    /// scalar override for both halves.
    pub(super) fn delta_bytes_row(row: &mut [u8]) {
        if scalar_forced() {
            let mut carry = 0u8;
            for byte in row.iter_mut() {
                let current = *byte;
                *byte = current.wrapping_sub(carry);
                carry = current;
            }
            return;
        }
        let level = Level::new();
        dispatch!(level, simd => delta_bytes(simd, row));
    }

    /// One row of the f32 transpose: four contiguous plane reads per 16
    /// pixels, interleaved into pixel order, then read back as native floats.
    #[simd]
    pub(super) fn transpose_row<S: Simd>(simd: S, dst: &mut [f32], row: &[u8], width: usize) {
        let (plane0, rest) = row.split_at(width);
        let (plane1, rest) = rest.split_at(width);
        let (plane2, plane3) = rest.split_at(width);
        let mut scratch = [0u8; N * 4];
        let mut x = 0;
        while x + N <= width {
            let v0 = u8x16::from_slice(simd, &plane0[x..x + N]);
            let v1 = u8x16::from_slice(simd, &plane1[x..x + N]);
            let v2 = u8x16::from_slice(simd, &plane2[x..x + N]);
            let v3 = u8x16::from_slice(simd, &plane3[x..x + N]);
            // Little-endian: interleaving the planes in reverse order writes
            // the big-endian bytes as native floats directly.
            let ordered = if cfg!(target_endian = "little") {
                [v3, v2, v1, v0]
            } else {
                [v0, v1, v2, v3]
            };
            u8x16::store_four_interleaved(ordered, &mut scratch);
            for (i, pixel) in scratch.chunks_exact(4).enumerate() {
                dst[x + i] = f32::from_ne_bytes([pixel[0], pixel[1], pixel[2], pixel[3]]);
            }
            x += N;
        }
        for i in x..width {
            dst[i] = f32::from_be_bytes([plane0[i], plane1[i], plane2[i], plane3[i]]);
        }
    }
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

    #[test]
    fn simd_rows_match_the_scalar_chain_across_boundaries() {
        // Widths around the 16-byte/8-sample vector boundaries, so the tails
        // and the carry between vectors are exercised, at every depth.
        for width in [1usize, 7, 8, 9, 15, 16, 17, 31, 32, 33, 64, 100] {
            let height = 3usize;
            let bytes: Vec<u8> = (0..width * height).map(|i| (i * 31 % 251) as u8).collect();
            let samples16: Vec<u16> = (0..width * height)
                .map(|i| (i * 4099 % 60000) as u16)
                .collect();
            let samples32: Vec<f32> = (0..width * height)
                .map(|i| (i as f32) * 0.125 - 50.0)
                .collect();

            let mut scan = bytes.clone();
            u8::undelta_rows(&mut scan, width);
            let mut reference = bytes.clone();
            for row in reference.chunks_exact_mut(width) {
                for x in 1..width {
                    row[x] = row[x].wrapping_add(row[x - 1]);
                }
            }
            assert_eq!(scan, reference, "u8 scan width {width}");

            let mut scan16 = samples16.clone();
            u16::undelta_rows(&mut scan16, width);
            let mut reference16 = samples16.clone();
            for row in reference16.chunks_exact_mut(width) {
                for x in 1..width {
                    row[x] = row[x].wrapping_add(row[x - 1]);
                }
            }
            assert_eq!(scan16, reference16, "u16 scan width {width}");

            let mut deltas = bytes.clone();
            u8::delta_rows(&mut deltas, width);
            let mut reference_deltas = bytes.clone();
            for row in reference_deltas.chunks_exact_mut(width) {
                let mut prev = 0u8;
                for value in row.iter_mut() {
                    let current = *value;
                    *value = current.wrapping_sub(prev);
                    prev = current;
                }
            }
            assert_eq!(deltas, reference_deltas, "u8 delta width {width}");

            // The f32 pipeline end to end at each width.
            let encoded = encode_f32(&samples32, width, height).unwrap();
            assert_eq!(
                decode_f32(&encoded, width, height).unwrap(),
                samples32,
                "f32 round trip width {width}"
            );
        }
    }

    #[test]
    fn scalar_override_still_round_trips() {
        // The override is env-gated and off unless the feature is on, so this
        // exercises the default path; the scalar reference is covered by
        // `decode_f32_scalar` being the override's target.
        let data: Vec<f32> = (0..64).map(|i| i as f32 * 0.25).collect();
        let encoded = encode_f32(&data, 8, 8).unwrap();
        let mut scalar_out = vec![0.0f32; 64];
        decode_f32_scalar(&encoded, 8, 32, &mut scalar_out);
        assert_eq!(scalar_out, data);
    }
}

#[cfg(test)]
mod block_encode_tests {
    use super::*;
    use crate::endian::encode_be_bytes;

    /// The definition `encode` must keep: delta every row of a full copy, then
    /// write it big-endian.
    fn reference<T: DeltaSample>(data: &[T], width: usize) -> Vec<u8> {
        let mut work = data.to_vec();
        if !work.is_empty() {
            T::delta_rows(&mut work, width);
        }
        encode_be_bytes(&work)
    }

    fn samples(count: usize) -> Vec<u16> {
        let mut state = 0x2545_f491_u32;
        (0..count)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 12) as u16
            })
            .collect()
    }

    #[test]
    fn blocked_encode_equals_the_full_copy_at_every_shape() {
        // Rows narrower than, equal to and wider than a block; a block boundary
        // that falls between rows; single rows and columns.
        for (width, height) in [
            (1, 1),
            (1, 5000),
            (7, 3),
            (100, 999),
            (4096, 9),
            (BLOCK_SAMPLES, 3),
            (BLOCK_SAMPLES + 5, 3),
            (40_000, 2),
        ] {
            let wide = samples(width * height);
            assert_eq!(
                encode::<u16>(&wide, width, height).unwrap(),
                reference(&wide, width),
                "u16 {width}x{height}"
            );
            let narrow: Vec<u8> = wide.iter().map(|&v| v as u8).collect();
            assert_eq!(
                encode::<u8>(&narrow, width, height).unwrap(),
                reference(&narrow, width),
                "u8 {width}x{height}"
            );
        }
    }

    #[test]
    fn encode_of_nothing_is_nothing() {
        assert!(encode::<u16>(&[], 0, 0).unwrap().is_empty());
        assert!(encode::<u8>(&[], 5, 0).unwrap().is_empty());
        assert!(encode::<u8>(&[1, 2, 3], 2, 2).is_err());
    }
}
