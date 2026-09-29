//! Adler-32, as used by the zlib wrapper around the DEFLATE stream inside `IDAT`.
//!
//! Every byte of a PNG passes through this checksum twice over an encode/decode round trip,
//! so it is worth real attention: a naive implementation is slower than the DEFLATE decoder
//! it accompanies. (On the default decode path it does not run at all: the chunk CRC already
//! covers the same bytes, and the zlib checksum is only verified under `Checks::Full`.)
//!
//! All the implementations here share one reformulation. Over a run of `n` bytes,
//!
//! ```text
//! a' = a + sum(x[j])
//! b' = b + n * a + sum((n - j) * x[j])
//! ```
//!
//! which replaces the textbook `a += x; b += a;` per-byte recurrence with two independent
//! reductions plus one multiply. The modulo is deferred until the accumulators are close to
//! overflowing.
//!
//! The vector implementation is written once against `fearless_simd`'s portable vectors, like
//! the filter and conversion kernels, and so runs on every target that has a vector unit
//! without any `unsafe` of its own.

use fearless_simd::{Simd, dispatch, prelude::*, u8x32, u16x16, u32x8};
use fearless_simd_macros::simd;

use crate::simd::{level, vectors_available};

/// Largest prime below 65536; the modulus for both halves of the sum.
const BASE: u32 = 65521;

/// Number of bytes that can be accumulated before the `b` half risks overflowing a `u32`.
///
/// This is the standard zlib constant: the largest `n` for which
/// `255 * n * (n + 1) / 2 + (n + 1) * (BASE - 1)` still fits in 32 bits.
const NMAX: usize = 5552;

/// Bytes one vector step consumes.
const STEP: usize = 32;

/// The weight of each byte within a step, `STEP - j` for the byte at index `j`.
const WEIGHTS: [u16; STEP] = {
    let mut weights = [0u16; STEP];
    let mut j = 0;
    while j < STEP {
        weights[j] = (STEP - j) as u16;
        j += 1;
    }
    weights
};

/// Inputs shorter than this take the scalar path: below two steps the vector setup and the
/// dispatch cost more than the bytes they save.
const VECTOR_MIN: usize = 2 * STEP;

/// Incremental Adler-32 hasher.
#[derive(Clone, Copy, Debug)]
pub struct Adler32 {
    a: u32,
    b: u32,
}

impl Default for Adler32 {
    fn default() -> Self {
        Self::new()
    }
}

impl Adler32 {
    /// A checksum over no bytes, which Adler-32 defines as 1 rather than 0.
    #[inline]
    pub const fn new() -> Self {
        Self { a: 1, b: 0 }
    }

    /// The checksum of everything fed in so far.
    #[inline]
    pub const fn finish(&self) -> u32 {
        (self.b << 16) | self.a
    }

    /// Folds `data` into the running checksum. Any split into calls gives the same result.
    #[inline]
    pub fn update(&mut self, data: &[u8]) {
        if data.len() >= VECTOR_MIN && vectors_available() {
            dispatch!(level(), simd => update_with(simd, &mut self.a, &mut self.b, data));
        } else {
            update_portable(&mut self.a, &mut self.b, data);
        }
    }
}

/// Scalar implementation, folding sixteen bytes at a time.
fn update_portable(a_out: &mut u32, b_out: &mut u32, data: &[u8]) {
    let (mut a, mut b) = (*a_out, *b_out);

    for block in data.chunks(NMAX) {
        let (chunks, remainder) = block.as_chunks::<16>();
        for chunk in chunks {
            b += a * 16;

            let mut sum = 0u32;
            let mut weighted = 0u32;
            for (j, &byte) in chunk.iter().enumerate() {
                sum += byte as u32;
                weighted += (16 - j) as u32 * byte as u32;
            }

            a += sum;
            b += weighted;
        }

        for &byte in remainder {
            a += byte as u32;
            b += a;
        }

        a %= BASE;
        b %= BASE;
    }

    *a_out = a;
    *b_out = b;
}

/// The vector implementation at a chosen level: whole steps through [`fold_steps`], the
/// bytes that do not fill a step through the scalar recurrence.
///
/// Each `NMAX`-byte block is folded and reduced modulo `BASE` on its own, exactly as the scalar
/// path does, so the bound that keeps `b` inside a `u32` is the same one.
#[inline(always)]
fn update_with<V: Simd>(simd: V, a_out: &mut u32, b_out: &mut u32, data: &[u8]) {
    let (mut a, mut b) = (*a_out, *b_out);

    for block in data.chunks(NMAX) {
        let (steps, tail) = block.split_at(block.len() / STEP * STEP);
        (a, b) = fold_steps(simd, a, b, steps);

        for &byte in tail {
            a += u32::from(byte);
            b += a;
        }

        a %= BASE;
        b %= BASE;
    }

    *a_out = a;
    *b_out = b;
}

/// Folds `steps`, a whole number of [`STEP`]-byte steps, into the running pair, without
/// reducing it modulo `BASE`.
///
/// The recurrence over one step of 32 bytes is `b += 32 * a + sum((32 - j) * x[j])`, and the
/// vector form never needs `a` mid-block. Three vectors of `u32` lanes stand in for it:
///
/// - `sums` holds, per lane, the bytes seen so far. Its lanes total the `a` that the current
///   step starts from, less the caller's own `a`.
/// - `carry` adds `sums` in before each step, so its lanes total, over all steps, how many
///   bytes each step had behind it. That times 32, plus the caller's `a` times the byte count,
///   is the whole `32 * a` term.
/// - `weighted` holds the `(32 - j) * x[j]` sums. A byte times its weight is at most
///   `255 * 32`, so one step's products fit a `u16` lane and are widened once per step.
///
/// The lanes are `u32`, wide enough because the block is at most [`NMAX`] bytes: the sum they
/// finally make is `b`, which that bound keeps under `2^32`, and no lane exceeds the total.
///
/// Requires `steps.len()` to be a multiple of [`STEP`] and at most `NMAX`.
#[simd]
fn fold_steps<V: Simd>(simd: V, a: u32, b: u32, steps: &[u8]) -> (u32, u32) {
    debug_assert!(steps.len().is_multiple_of(STEP) && steps.len() <= NMAX);

    let weight_low = u16x16::from_slice(simd, &WEIGHTS[..16]);
    let weight_high = u16x16::from_slice(simd, &WEIGHTS[16..]);

    let mut sums = u32x8::splat(simd, 0);
    let mut carry = u32x8::splat(simd, 0);
    let mut weighted = u32x8::splat(simd, 0);

    for step in steps.chunks_exact(STEP) {
        let (low, high) = u8x32::from_slice(simd, step).widen();

        carry += sums;

        let (weighted_low, weighted_high) = (low * weight_low + high * weight_high).widen();
        weighted = weighted + weighted_low + weighted_high;

        let (sum_low, sum_high) = (low + high).widen();
        sums = sums + sum_low + sum_high;
    }

    // At most `NMAX`, so this cannot truncate.
    let bytes = steps.len() as u32;
    let b = b + a * bytes + STEP as u32 * carry.reduce_sum() + weighted.reduce_sum();
    let a = a + sums.reduce_sum();
    (a, b)
}

/// Computes the Adler-32 of `data` in one shot.
#[inline]
pub fn adler32(data: &[u8]) -> u32 {
    let mut hasher = Adler32::new();
    hasher.update(data);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simd::on_each_backend;

    fn reference(data: &[u8]) -> u32 {
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in data {
            a = (a + byte as u32) % BASE;
            b = (b + a) % BASE;
        }
        (b << 16) | a
    }

    /// Lengths around every boundary the implementations have: the sixteen-byte scalar
    /// chunk, the thirty-two-byte vector step, the vector cut-over, and `NMAX` blocks.
    const LENGTHS: [usize; 34] = [
        0, 1, 5, 15, 16, 17, 31, 32, 33, 63, 64, 65, 95, 96, 97, 100, 1000, 5503, 5504, 5505, 5535,
        5536, 5537, 5551, 5552, 5553, 5568, 11_007, 11_008, 11_009, 11_071, 11_104, 11_105, 20_000,
    ];

    #[test]
    fn known_vectors() {
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"a"), 0x0062_0062);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    fn varied_data(len: usize) -> Vec<u8> {
        (0..len as u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect()
    }

    #[test]
    fn matches_reference() {
        let data = varied_data(20_000);
        for len in LENGTHS {
            assert_eq!(adler32(&data[..len]), reference(&data[..len]), "len {len}");
        }
    }

    /// The `b` half is what can overflow; saturating the input exercises that bound.
    #[test]
    fn saturated_input_does_not_overflow() {
        let data = vec![0xffu8; 40_000];
        assert_eq!(adler32(&data), reference(&data));
    }

    /// The worst case for the vector lanes: the largest block, every byte saturated, and a
    /// running pair already at its largest, so the terms sum to the closest a block comes to
    /// the `u32` limit. Run on every backend the machine has, with the scalar path beside it.
    #[test]
    fn a_full_block_at_its_limit_does_not_overflow() {
        let data = vec![0xffu8; NMAX];
        let start = (BASE - 1, BASE - 1);

        let (mut a, mut b) = start;
        update_portable(&mut a, &mut b, &data);
        let want = (a, b);

        let runs: Vec<(&'static str, (u32, u32))> = on_each_backend!(|simd| {
            let (mut a, mut b) = start;
            update_with(simd, &mut a, &mut b, &data);
            (a, b)
        });
        for (backend, got) in runs {
            assert_eq!(got, want, "{backend}");
        }
    }

    /// Every implementation must agree, not just whichever one this CPU selects: the scalar
    /// path, and the vector path on each backend the machine can run.
    #[test]
    fn all_implementations_agree() {
        let data = varied_data(20_000);
        for len in LENGTHS {
            let slice = &data[..len];
            let expected = reference(slice);

            let (mut a, mut b) = (1u32, 0u32);
            update_portable(&mut a, &mut b, slice);
            assert_eq!((b << 16) | a, expected, "portable, len {len}");

            let runs: Vec<(&'static str, u32)> = on_each_backend!(|simd| {
                let (mut a, mut b) = (1u32, 0u32);
                update_with(simd, &mut a, &mut b, slice);
                (b << 16) | a
            });
            for (backend, got) in runs {
                assert_eq!(got, expected, "{backend}, len {len}");
            }
        }
    }

    #[test]
    fn incremental_matches_one_shot() {
        let data: Vec<u8> = (0..9000u32).map(|i| (i % 251) as u8).collect();
        for split in [0, 1, 17, 64, 65, 5504, 6000, 9000] {
            let mut hasher = Adler32::new();
            hasher.update(&data[..split]);
            hasher.update(&data[split..]);
            assert_eq!(hasher.finish(), reference(&data), "split {split}");
        }
    }
}
