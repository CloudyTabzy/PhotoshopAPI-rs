//! Portable-SIMD prototype of the f32 ZipPrediction decode (port TODO §F2b).
//!
//! Built only with the `fearless` feature so one binary can A/B it against
//! [`crate::prediction::decode_f32`]. It exists to answer one question: does
//! `fearless_simd` beat the scalar code on this crate's hottest measured
//! phase — the byte-wise prefix sum plus the four-plane transpose — and does
//! the portable API express it without an intrinsic escape hatch?
//!
//! The scalar original spends 2.4 ms on a 1024×1024 f32 channel, more than
//! libdeflate's inflate of the same data, because its prefix sum is a serial
//! byte chain and its transpose is a four-way gather. Here the scan becomes a
//! Hillis–Steele vector scan (log2(16) = 4 shuffle-and-add steps per 16 bytes)
//! and the transpose reads each plane contiguously, interleaving with
//! `store_four_interleaved`.

use fearless_simd::{dispatch, prelude::*, u8x16, Level, Simd};
use fearless_simd_macros::simd;

use crate::error::Result;
use crate::prediction::{check_sample_count, row_bytes};

/// Inclusive byte-wise prefix sum of one row.
///
/// A 16-byte block is scanned in four shuffle-and-add steps, then the running
/// carry from the previous block is splatted in; the tail stays scalar. The
/// carry chain is now one dependent add per *block* instead of one per byte.
#[simd]
fn prefix_sum_row<S: Simd>(simd: S, row: &mut [u8]) {
    const N: usize = 16;
    // Lane-shift index vectors, zero-filling: output byte i takes input byte
    // i - s, and the low s bytes are zeroed by the out-of-range index.
    const fn shift_indices(shift: usize) -> [u8; N] {
        let mut out = [0x80u8; N];
        let mut i = shift;
        while i < N {
            out[i] = (i - shift) as u8;
            i += 1;
        }
        out
    }
    const IDX1: [u8; N] = shift_indices(1);
    const IDX2: [u8; N] = shift_indices(2);
    const IDX4: [u8; N] = shift_indices(4);
    const IDX8: [u8; N] = shift_indices(8);
    let idx1 = u8x16::from_slice(simd, &IDX1);
    let idx2 = u8x16::from_slice(simd, &IDX2);
    let idx4 = u8x16::from_slice(simd, &IDX4);
    let idx8 = u8x16::from_slice(simd, &IDX8);

    let mut carry = 0u8;
    let mut chunks = row.chunks_exact_mut(N);
    for chunk in &mut chunks {
        let mut v = u8x16::from_slice(simd, chunk);
        for idx in [idx1, idx2, idx4, idx8] {
            v = v + simd.swizzle_dyn_precise_u8x16(v, idx);
        }
        v += u8x16::splat(simd, carry);
        v.store_slice(chunk);
        carry = chunk[N - 1];
    }
    for byte in chunks.into_remainder() {
        *byte = byte.wrapping_add(carry);
        carry = *byte;
    }
}

/// One row of the transpose: four contiguous plane reads per 16 pixels,
/// interleaved into pixel order, then read back as big-endian floats.
#[simd]
fn transpose_row<S: Simd>(simd: S, dst: &mut [f32], row: &[u8], width: usize) {
    const N: usize = 16;
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
        u8x16::store_four_interleaved([v0, v1, v2, v3], &mut scratch);
        for (i, pixel) in scratch.chunks_exact(4).enumerate() {
            dst[x + i] = f32::from_be_bytes([pixel[0], pixel[1], pixel[2], pixel[3]]);
        }
        x += N;
    }
    for i in x..width {
        dst[i] = f32::from_be_bytes([plane0[i], plane1[i], plane2[i], plane3[i]]);
    }
}

/// The vectorized scan over a whole channel buffer (in place).
///
/// Exposed for the A/B: with a warm buffer the measurement sees the loop, not
/// page faults on a fresh allocation.
pub fn scan_rows(planar: &mut [u8], stride: usize) {
    let level = Level::new();
    dispatch!(level, simd => {
        for row in planar.chunks_exact_mut(stride) {
            prefix_sum_row(simd, row);
        }
    });
}

/// The vectorized transpose over a whole channel buffer.
pub fn transpose_rows(out: &mut [f32], planar: &[u8], width: usize, stride: usize) {
    let level = Level::new();
    dispatch!(level, simd => {
        for (dst, row) in out.chunks_mut(width).zip(planar.chunks(stride)) {
            transpose_row(simd, dst, row, width);
        }
    });
}

/// Diagnostic split: the vectorized scan with the scalar transpose.
pub fn decode_f32_scan_only(bytes: &[u8], width: usize, height: usize) -> Result<Vec<f32>> {
    let stride = row_bytes(width, 4)?;
    check_sample_count(bytes.len(), stride, height)?;
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }
    let level = Level::new();
    let mut planar = bytes.to_vec();
    dispatch!(level, simd => {
        for row in planar.chunks_exact_mut(stride) {
            prefix_sum_row(simd, row);
        }
    });
    let mut out = vec![0.0f32; width * height];
    for (dst, row) in out.chunks_mut(width).zip(planar.chunks(stride)) {
        let (p0, rest) = row.split_at(width);
        let (p1, rest) = rest.split_at(width);
        let (p2, p3) = rest.split_at(width);
        for (i, value) in dst.iter_mut().enumerate() {
            *value = f32::from_be_bytes([p0[i], p1[i], p2[i], p3[i]]);
        }
    }
    Ok(out)
}

/// Diagnostic split: the scalar scan with the vectorized transpose.
pub fn decode_f32_transpose_only(bytes: &[u8], width: usize, height: usize) -> Result<Vec<f32>> {
    let stride = row_bytes(width, 4)?;
    check_sample_count(bytes.len(), stride, height)?;
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }
    let level = Level::new();
    let mut planar = bytes.to_vec();
    for row in planar.chunks_exact_mut(stride) {
        for i in 1..stride {
            row[i] = row[i].wrapping_add(row[i - 1]);
        }
    }
    let mut out = vec![0.0f32; width * height];
    dispatch!(level, simd => {
        for (dst, row) in out.chunks_mut(width).zip(planar.chunks(stride)) {
            transpose_row(simd, dst, row, width);
        }
    });
    Ok(out)
}

/// The prototype: same framing and validation as
/// [`crate::prediction::decode_f32`], both hot loops vectorized.
pub fn decode_f32(bytes: &[u8], width: usize, height: usize) -> Result<Vec<f32>> {
    let stride = row_bytes(width, 4)?;
    check_sample_count(bytes.len(), stride, height)?;
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }

    let level = Level::new();
    let mut planar = bytes.to_vec();
    let mut out = vec![0.0f32; width * height];
    dispatch!(level, simd => {
        for row in planar.chunks_exact_mut(stride) {
            prefix_sum_row(simd, row);
        }
        for (dst, row) in out.chunks_mut(width).zip(planar.chunks(stride)) {
            transpose_row(simd, dst, row, width);
        }
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_scalar_decode() {
        let (width, height) = (37usize, 5usize);
        let data: Vec<f32> = (0..width * height)
            .map(|i| ((i * 37) as f32) * 0.125 - 100.0)
            .collect();
        let encoded = crate::prediction::encode_f32(&data, width, height).unwrap();
        let scalar = crate::prediction::decode_f32(&encoded, width, height).unwrap();
        let vectorized = decode_f32(&encoded, width, height).unwrap();
        assert_eq!(scalar, vectorized);
    }

    #[test]
    fn matches_across_vector_boundaries() {
        for width in [1usize, 15, 16, 17, 31, 32, 33, 64, 100] {
            let height = 3usize;
            let data: Vec<f32> = (0..width * height).map(|i| (i as f32) * 0.5).collect();
            let encoded = crate::prediction::encode_f32(&data, width, height).unwrap();
            assert_eq!(
                crate::prediction::decode_f32(&encoded, width, height).unwrap(),
                decode_f32(&encoded, width, height).unwrap(),
                "width {width}"
            );
        }
    }
}
