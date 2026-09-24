//! SIMD scanline-filter reversal, and the dispatch that chooses it.
//!
//! The Paeth predictor is a chain of compares and selects along a row: pixel *x* needs the
//! pixel to its left, so a single row leaves the machine waiting on that dependency rather
//! than working. The scalar decoder already fills the idle slots by reconstructing two rows
//! as a wavefront (`filter::unfilter_paeth_pair`). SIMD attacks the same problem from the
//! other side: the four channels of one RGB/RGBA pixel are independent of each other, so a
//! 128-bit register can carry a whole pixel and the four lanes issue at once.
//!
//! This follows libpng's SSE2 filters (`intel/filter_sse2_intrinsics.c`, from libpng PR #88)
//! rather than the multi-row and anti-diagonal arrangements discussed in Wuffs issue #157:
//! one row at a time, no structural change to the frontier, and no extra rows in flight for
//! the streaming stage to account for.
//!
//! Two things differ from libpng, both forced by reconstructing in place. libpng's rows live
//! in padded per-row buffers, so its kernels may write a whole pixel at the last position;
//! here the bytes after a row are the next row's still-filtered data, so this kernel touches
//! only bytes inside the row. And the stride is a runtime value here, so the kernel is
//! chosen per call rather than compiled in.

/// Reverses `Paeth` on one row with SIMD, or reports that it did not.
///
/// `row` and `prev` are one scanline each: `row` holds filtered bytes and is reconstructed in
/// place, `prev` the row above, already reconstructed. `bpp` is the pixel stride. `true`
/// means the whole row is reconstructed; `false` means it was left untouched and the caller
/// must use its scalar path. A row shorter than one pixel cannot be handled, since there is
/// no full pixel to put in a register.
pub(crate) fn paeth_row(row: &mut [u8], prev: &[u8], bpp: usize) -> bool {
    (routines().paeth_row)(row, prev, bpp)
}

/// Whether [`paeth_row`] claims this pixel stride on this machine.
///
/// The scalar path reconstructs two adjacent `Paeth` rows as a wavefront, which is the other
/// way to fill the dependency stall, so the caller has to choose between the two rather than
/// run both. It asks here instead of calling the kernel and looking at the answer, because
/// the choice is made per row and must be the same for every row of an image.
pub(crate) fn claims_stride(bpp: usize) -> bool {
    routines().strides.contains(&bpp)
}

/// What this machine will use, resolved once.
struct Routines {
    /// Reverses one `Paeth` row, or declines it.
    paeth_row: fn(&mut [u8], &[u8], usize) -> bool,
    /// The strides `paeth_row` claims. Empty when the scalar routines are in force.
    strides: &'static [usize],
}

static ROUTINES: std::sync::OnceLock<Routines> = std::sync::OnceLock::new();

fn routines() -> &'static Routines {
    ROUTINES.get_or_init(|| detect(force_scalar_from_env()))
}

/// Chooses the routines for this machine.
///
/// `force_scalar` exists so the choice can be tested and measured without depending on the
/// environment; [`routines`] reads the environment once and caches the result.
fn detect(force_scalar: bool) -> Routines {
    if force_scalar {
        return Routines { paeth_row: decline, strides: &[] };
    }
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("sse2") {
            return Routines { paeth_row: x86::dispatch, strides: &[3, 4] };
        }
    }
    Routines { paeth_row: decline, strides: &[] }
}

/// The scalar path's answer: this kernel declines every row.
fn decline(_row: &mut [u8], _prev: &[u8], _bpp: usize) -> bool {
    false
}

fn force_scalar_from_env() -> bool {
    use std::sync::OnceLock;
    static FORCE: OnceLock<bool> = OnceLock::new();
    *FORCE.get_or_init(|| std::env::var("PSD_PNG_FORCE_SCALAR").ok().as_deref() == Some("1"))
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use core::arch::x86_64::*;

    /// Picks the stride-specific kernel, or declines a stride it does not cover.
    pub(super) fn dispatch(row: &mut [u8], prev: &[u8], bpp: usize) -> bool {
        // SAFETY: SSE2 is verified at dispatch time by `is_x86_feature_detected!`, and each
        // kernel reads and writes only within `row` and `prev`, whose lengths are checked
        // here and re-checked inside for the bytes each iteration touches.
        unsafe {
            match bpp {
                3 => paeth3(row, prev),
                4 => paeth4(row, prev),
                _ => false,
            }
        }
    }

    /// The absolute value of each 16-bit lane.
    ///
    /// The distances are bounded by 510, so the negate-and-subtract form cannot overflow.
    ///
    /// # Safety
    /// Requires SSE2.
    #[target_feature(enable = "sse2")]
    unsafe fn abs_i16(value: __m128i) -> __m128i {
        // Lane-wise arithmetic on a value already in a register: no memory access, and no lane
        // can overflow, so SSE2 in force is the whole requirement.
        let negative = _mm_cmpgt_epi16(_mm_setzero_si128(), value);
        _mm_sub_epi16(_mm_xor_si128(value, negative), negative)
    }

    /// `mask ? yes : no`, lane by lane.
    ///
    /// # Safety
    /// Requires SSE2.
    #[target_feature(enable = "sse2")]
    unsafe fn if_then_else(mask: __m128i, yes: __m128i, no: __m128i) -> __m128i {
        _mm_or_si128(_mm_and_si128(mask, yes), _mm_andnot_si128(mask, no))
    }

    /// Narrows four 16-bit lanes, each already inside `0..=255`, to four consecutive bytes.
    ///
    /// `packus` interleaves its two operands lane by lane, so packing a vector against
    /// itself yields `s0, s0, s1, s1, ...`. The second operand is therefore the first shifted
    /// one lane along, which puts `s0, s1, s2, s3` in the low four bytes. Saturating is
    /// harmless here precisely because the wrapping eight-bit add already brought every lane
    /// into range.
    ///
    /// # Safety
    /// Requires SSE2.
    #[target_feature(enable = "sse2")]
    unsafe fn narrow(sum: __m128i) -> u32 {
        // As `abs_i16`, plus a byte shift within the same register.
        _mm_cvtsi128_si32(_mm_packus_epi16(sum, _mm_srli_si128(sum, 2))) as u32
    }

    /// Reverses `Paeth` on an RGB row, three bytes per pixel.
    ///
    /// The fourth lane carries a zero that is computed and thrown away, which is what lets
    /// one 128-bit register hold a whole pixel: lanes never read across pixels. Bytes past
    /// the last whole pixel are finished by [`tail`], which for a PNG never runs — a scanline
    /// is always a whole number of pixels — and is here so that no length can overrun.
    ///
    /// # Safety
    /// Requires SSE2. `row` and `prev` must be the same length.
    #[target_feature(enable = "sse2")]
    pub(super) unsafe fn paeth3(row: &mut [u8], prev: &[u8]) -> bool {
        if row.len() != prev.len() || row.len() < 3 {
            return false;
        }
        // SAFETY: SSE2 was verified at dispatch; every load is a 32-bit read of bytes the
        // length check above proved are inside `prev` and `row`, every store is the three
        // bytes just loaded, and the loop advances by a whole pixel each time.
        unsafe {
            let zero = _mm_setzero_si128();
            // `left` is the previous pixel's reconstructed bytes, `upper_left` its above
            // bytes.
            let mut left = zero;
            let mut upper_left = zero;
            let mut at = 0;
            while at + 3 <= row.len() {
                let above = widen(load3(&prev[at..]));
                let raw = widen(load3(&row[at..]));

                // `p - a` is `b - c` and `p - b` is `a - c`, so the three distances need only
                // two differences; `p - c` is their sum.
                let da = _mm_sub_epi16(above, upper_left);
                let db = _mm_sub_epi16(left, upper_left);
                let dc = _mm_add_epi16(da, db);
                let pa = abs_i16(da);
                let pb = abs_i16(db);
                let pc = abs_i16(dc);

                // Ties break a, then b, then c, as the specification requires.
                let smallest = _mm_min_epi16(pc, _mm_min_epi16(pa, pb));
                let nearest = if_then_else(
                    _mm_cmpeq_epi16(smallest, pa),
                    left,
                    if_then_else(_mm_cmpeq_epi16(smallest, pb), above, upper_left),
                );

                // The predictor is added in eight-bit lanes so the sum wraps modulo 256, and
                // the widened result is already inside 0..=255, so the narrow cannot
                // saturate.
                let sum = _mm_add_epi8(raw, nearest);
                let packed = narrow(sum);
                row[at] = packed as u8;
                row[at + 1] = (packed >> 8) as u8;
                row[at + 2] = (packed >> 16) as u8;

                upper_left = above;
                left = sum;
                at += 3;
            }
            tail(row, prev, at, 3);
        }
        true
    }

    /// Reverses `Paeth` on an RGBA row, four bytes per pixel.
    ///
    /// # Safety
    /// Requires SSE2. `row` and `prev` must be the same length.
    #[target_feature(enable = "sse2")]
    pub(super) unsafe fn paeth4(row: &mut [u8], prev: &[u8]) -> bool {
        if row.len() != prev.len() || row.len() < 4 {
            return false;
        }
        // SAFETY: as `paeth3`, with four bytes per pixel.
        unsafe {
            let zero = _mm_setzero_si128();
            let mut left = zero;
            let mut upper_left = zero;
            let mut at = 0;
            while at + 4 <= row.len() {
                let above = widen(load4(&prev[at..]));
                let raw = widen(load4(&row[at..]));

                let da = _mm_sub_epi16(above, upper_left);
                let db = _mm_sub_epi16(left, upper_left);
                let dc = _mm_add_epi16(da, db);
                let pa = abs_i16(da);
                let pb = abs_i16(db);
                let pc = abs_i16(dc);

                let smallest = _mm_min_epi16(pc, _mm_min_epi16(pa, pb));
                let nearest = if_then_else(
                    _mm_cmpeq_epi16(smallest, pa),
                    left,
                    if_then_else(_mm_cmpeq_epi16(smallest, pb), above, upper_left),
                );

                let sum = _mm_add_epi8(raw, nearest);
                row[at..at + 4].copy_from_slice(&narrow(sum).to_le_bytes());

                upper_left = above;
                left = sum;
                at += 4;
            }
            tail(row, prev, at, 4);
        }
        true
    }

    /// Finishes the bytes the register loop could not cover, reading the left neighbour back
    /// out of the row the loop has already reconstructed.
    fn tail(row: &mut [u8], prev: &[u8], at: usize, bpp: usize) {
        for x in at..row.len() {
            let left = row[x - bpp];
            let upper_left = prev[x - bpp];
            row[x] = row[x].wrapping_add(crate::filter::paeth_predictor(left, prev[x], upper_left));
        }
    }

    /// Three bytes into a zeroed word, so the fourth lane starts at zero.
    fn load3(bytes: &[u8]) -> [u8; 4] {
        [bytes[0], bytes[1], bytes[2], 0]
    }

    fn load4(bytes: &[u8]) -> [u8; 4] {
        [bytes[0], bytes[1], bytes[2], bytes[3]]
    }

    /// Spreads four adjacent bytes into four 16-bit lanes, which is the width the predictor's
    /// distances need: `a + b - c` leaves the byte range, and 16-bit lanes hold it without
    /// overflow. The load itself is a single 32-bit read, so nothing outside the row is
    /// touched.
    ///
    /// # Safety
    /// Requires SSE2.
    #[target_feature(enable = "sse2")]
    unsafe fn widen(bytes: [u8; 4]) -> __m128i {
        // A 32-bit move from a value already in a register.
        _mm_unpacklo_epi8(_mm_cvtsi32_si128(i32::from_le_bytes(bytes)), _mm_setzero_si128())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::paeth_predictor;

    /// The scalar definition, written out here rather than borrowed from the filter loops,
    /// so a change to those loops cannot make this test agree with a wrong kernel.
    fn reference(row: &mut [u8], prev: &[u8], bpp: usize) {
        for x in 0..row.len() {
            let left = if x >= bpp { row[x - bpp] } else { 0 };
            let upper_left = if x >= bpp { prev[x - bpp] } else { 0 };
            row[x] = row[x].wrapping_add(paeth_predictor(left, prev[x], upper_left));
        }
    }

    /// Deterministic bytes, so a failure is reproducible from the test name alone.
    fn corpus(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    /// The kernel for a stride, called directly.
    ///
    /// The parity tests go through this rather than [`paeth_row`] so that what they check does
    /// not depend on what the dispatch decided: an interpreter that declines to report SSE2
    /// would otherwise turn a kernel test into a test of the scalar fallback.
    #[cfg(target_arch = "x86_64")]
    fn kernel(bpp: usize) -> fn(&mut [u8], &[u8]) -> bool {
        // SAFETY: every call site below passes equal-length slices of at least one pixel,
        // which is what the kernels require; SSE2 is baseline on this target.
        match bpp {
            3 => |row, prev| unsafe { super::x86::paeth3(row, prev) },
            4 => |row, prev| unsafe { super::x86::paeth4(row, prev) },
            _ => unreachable!("no kernel for stride {bpp}"),
        }
    }

    /// The kernel must reproduce the scalar predictor for every stride it claims, at every
    /// length — including lengths that are not a whole number of pixels, which the register
    /// loop leaves to the tail and a PNG scanline never actually has.
    #[test]
    #[cfg_attr(not(target_arch = "x86_64"), ignore = "the kernel is x86-only")]
    fn kernel_matches_the_scalar_definition() {
        for bpp in [3usize, 4] {
            let kernel = kernel(bpp);
            for pixels in [1usize, 2, 3, 4, 5, 7, 8, 15, 16, 17, 33, 64, 129] {
                for extra in 0..bpp {
                    let len = pixels * bpp + extra;
                    let filtered = corpus(len, 0x9E37_79B9_7F4A_7C15 ^ len as u64);
                    let prev = corpus(len, 0xD1B5_4A32_D192_ED03 ^ (len as u64) << 7);

                    let mut want = filtered.clone();
                    reference(&mut want, &prev, bpp);

                    let mut got = filtered.clone();
                    assert!(kernel(&mut got, &prev), "bpp {bpp}, len {len}: declined");
                    assert_eq!(got, want, "bpp {bpp}, len {len}");
                }
            }
        }
    }

    /// The first row of an image has no row above, and the specification defines every
    /// neighbour as zero there — which reduces `Paeth` to `Sub`.
    #[test]
    #[cfg_attr(not(target_arch = "x86_64"), ignore = "the kernel is x86-only")]
    fn kernel_handles_a_first_row_over_zeros() {
        for bpp in [3usize, 4] {
            let kernel = kernel(bpp);
            let len = 37 * bpp;
            let filtered = corpus(len, 0x0123456789ABCDEF);
            let mut want = filtered.clone();
            reference(&mut want, &vec![0u8; len], bpp);

            let mut got = filtered.clone();
            assert!(kernel(&mut got, &vec![0u8; len]), "bpp {bpp}");
            assert_eq!(got, want, "bpp {bpp}");
        }
    }

    /// Extreme byte values exercise the wrap in the final add and the extremes of the
    /// predictor's distances, which are where a lane-wise port most easily diverges.
    #[test]
    #[cfg_attr(not(target_arch = "x86_64"), ignore = "the kernel is x86-only")]
    fn kernel_handles_extreme_values() {
        for bpp in [3usize, 4] {
            let kernel = kernel(bpp);
            for seed in 0..4u64 {
                let len = 8 * bpp;
                let mut filtered = corpus(len, 0xFEED_FACE_CAFE_BEEF ^ seed);
                let mut prev = corpus(len, 0x0123_4567_89AB_CDEF ^ seed);
                for (i, byte) in filtered.iter_mut().enumerate() {
                    *byte = [0x00, 0xFF, 0x80, 0x7F][(i + seed as usize) % 4];
                }
                for (i, byte) in prev.iter_mut().enumerate() {
                    *byte = [0xFF, 0x00, 0x01, 0xFE][(i + seed as usize) % 4];
                }
                let mut want = filtered.clone();
                reference(&mut want, &prev, bpp);
                let mut got = filtered.clone();
                assert!(kernel(&mut got, &prev), "bpp {bpp} seed {seed}");
                assert_eq!(got, want, "bpp {bpp} seed {seed}");
            }
        }
    }

    /// A stride the kernels do not cover must be declined, not approximated: the caller's
    /// scalar path is correct for every stride, and a wrong answer here would be silent.
    #[test]
    fn unsupported_strides_decline() {
        let len = 64;
        let filtered = corpus(len, 0xABCD);
        let prev = corpus(len, 0x1234);
        for bpp in [1usize, 2, 6, 8] {
            let mut row = filtered.clone();
            assert!(!paeth_row(&mut row, &prev, bpp), "bpp {bpp} must decline");
            assert_eq!(row, filtered, "bpp {bpp}: a declined row must be untouched");
        }
    }

    /// Rows too short to fill a register, and rows whose two halves disagree in length, are
    /// the cases where a hand-written kernel would read past the end.
    ///
    /// These go through the dispatch so that the forced-scalar build is held to the same
    /// promise: a declined row is never partly reconstructed, whichever routines are in force.
    #[test]
    fn short_and_mismatched_rows_decline() {
        let filtered = corpus(64, 0x5555);
        let prev = corpus(64, 0xAAAA);
        for bpp in [1usize, 2, 3, 4, 6, 8] {
            for len in 0..bpp.min(4) {
                let mut row = filtered[..len].to_vec();
                assert!(!paeth_row(&mut row, &prev[..len], bpp), "bpp {bpp} len {len}");
                assert_eq!(row, filtered[..len], "a declined row must be untouched");
            }
        }
        let mut row = filtered[..16].to_vec();
        assert!(!paeth_row(&mut row, &prev[..15], 4), "mismatched lengths must decline");
        assert_eq!(row, filtered[..16], "a declined row must be untouched");
    }

    /// The scalar override has to win over the machine's capabilities, or it cannot be used
    /// to measure the kernel against the path it replaces.
    #[test]
    fn the_scalar_override_declines_everything() {
        let len = 64;
        let filtered = corpus(len, 0x7777);
        let prev = corpus(len, 0x8888);
        let scalar = detect(true);
        assert!(scalar.strides.is_empty(), "the scalar routines claim no stride");
        for bpp in [3usize, 4] {
            let mut row = filtered.clone();
            assert!(!(scalar.paeth_row)(&mut row, &prev, bpp), "bpp {bpp}");
            assert_eq!(row, filtered, "bpp {bpp}: a declined row must be untouched");
        }
    }

    /// The stride claim and the kernel have to agree, because the caller picks between this
    /// kernel and the scalar two-row wavefront using the claim alone.
    #[test]
    fn the_stride_claim_matches_the_kernel() {
        let len = 64;
        let filtered = corpus(len, 0x2468);
        let prev = corpus(len, 0x1357);
        let routines = detect(false);
        for bpp in [1usize, 2, 3, 4, 6, 8] {
            let mut row = filtered.clone();
            let handled = (routines.paeth_row)(&mut row, &prev, bpp);
            assert_eq!(
                handled,
                routines.strides.contains(&bpp),
                "bpp {bpp}: claim and kernel disagree"
            );
        }
        // One full pixel is the shortest row a claimed stride accepts, and a claim that
        // quietly stopped covering it would leave the caller reconstructing nothing.
        for bpp in routines.strides {
            let mut row = filtered[..*bpp].to_vec();
            assert!(
                (routines.paeth_row)(&mut row, &prev[..*bpp], *bpp),
                "bpp {bpp}: a one-pixel row must be claimed"
            );
        }
    }
}
