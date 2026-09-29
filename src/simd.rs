//! SIMD scanline-filter reversal, and the choice of when to use it.
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
//!
//! The kernels are written once against `fearless_simd`'s portable vectors and compiled for
//! whatever the CPU supports at run time - SSE2, SSE4.2, AVX2, AVX-512, NEON or wasm SIMD -
//! so the acceleration is no longer x86-64-only. Two backends are deliberately left to the
//! scalar path: the scalar fallback, where the generic code runs a lane at a time and loses
//! to this crate's own wavefront, and - for the shuffle-built conversion kernels - the bare
//! SSE2 level, where a dynamic byte shuffle is emulated per lane. The choice between the
//! kernels and the scalar path is made once per call, except under the `scalar-override`
//! feature, which lets `PSD_PNG_FORCE_SCALAR=1` turn the kernels off for a whole process so
//! the two can be measured and tested against each other.

use fearless_simd::Level;

/// The pixel strides the SIMD filter kernel reconstructs.
const KERNEL_STRIDES: &[usize] = &[3, 4];

/// The backend this process runs on. `Level::new` detects it once and caches the answer
/// itself (on x86; every other target knows it at compile time), so this is a load, not a
/// detection. The facade decides from this value and the kernels dispatch on it, so the two
/// cannot disagree about the backend.
fn level() -> Level {
    Level::new()
}

/// Whether the dispatched backend is a real vector unit rather than the scalar fallback.
/// The filter kernel works on every vector backend; on the fallback level its generic code
/// runs one lane at a time and loses to the scalar wavefront.
///
/// Big-endian targets are treated as having no vector unit. The kernels bitcast between
/// lane widths and read a big-endian PNG sample as a native lane, both of which assume the
/// little-endian lane layout every supported vector backend has in practice; a target that
/// breaks that assumption takes the scalar path, which is correct everywhere.
fn vectors_available() -> bool {
    cfg!(target_endian = "little") && !level().is_fallback()
}

/// Whether the dispatched backend has a hardware dynamic byte shuffle (`pshufb` and
/// friends). The conversion kernels are built from shuffles; on the bare SSE2 backend
/// `swizzle_dyn_precise` falls back to a per-lane scalar emulation, which measures slower
/// than the autovectorised scalar loop, so those kernels decline there. Every other vector
/// backend - SSE4.2 and up, NEON, wasm SIMD - has the real instruction.
fn shuffles_available() -> bool {
    if !vectors_available() {
        return false;
    }
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        level().as_sse4_2().is_some()
    }
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    {
        true
    }
}

/// Reverses `Paeth` on one row with SIMD, or reports that it did not.
///
/// `row` and `prev` are one scanline each: `row` holds filtered bytes and is reconstructed in
/// place, `prev` the row above, already reconstructed. `bpp` is the pixel stride. `true`
/// means the whole row is reconstructed; `false` means it was left untouched and the caller
/// must use its scalar path. A row shorter than one pixel cannot be handled, since there is
/// no full pixel to put in a register.
pub(crate) fn paeth_row(row: &mut [u8], prev: &[u8], bpp: usize) -> bool {
    if !claims_stride(bpp) {
        return false;
    }
    kernels::paeth_row(row, prev, bpp)
}

/// Whether [`paeth_row`] claims this pixel stride.
///
/// The scalar path reconstructs two adjacent `Paeth` rows as a wavefront, which is the other
/// way to fill the dependency stall, so the caller has to choose between the two rather than
/// run both. It asks here instead of calling the kernel and looking at the answer, because
/// the choice is made per row and must be the same for every row of an image.
#[inline(always)]
pub(crate) fn claims_stride(bpp: usize) -> bool {
    KERNEL_STRIDES.contains(&bpp) && vectors_available() && !scalar_forced()
}

/// Whether `PSD_PNG_FORCE_SCALAR=1` has turned the kernel off, read once per process.
#[cfg(feature = "scalar-override")]
fn scalar_forced() -> bool {
    use std::sync::OnceLock;
    static FORCE: OnceLock<bool> = OnceLock::new();
    *FORCE.get_or_init(|| std::env::var("PSD_PNG_FORCE_SCALAR").ok().as_deref() == Some("1"))
}

/// Without the `scalar-override` feature nothing can turn the kernel off, and the process
/// environment has no say in which code runs.
#[cfg(not(feature = "scalar-override"))]
#[inline(always)]
fn scalar_forced() -> bool {
    false
}

// ---------------------------------------------------------------------------
// Row-conversion kernels
//
// `transform.rs` converts a reconstructed row into the layout a caller asked for. Most of
// that work is per-pixel shuffling — widen, narrow, replicate, resolve a palette — which
// is the shape SIMD eats. The same rules as the filter kernel apply: the kernels are
// portable, every load and store goes through bounds-checked indexing, the
// `scalar-override` switch turns the whole layer off for measurement, and anything not
// claimed falls back to `transform.rs`'s scalar helpers untouched.

use crate::common::{BitDepth, ColorType};
use crate::error::Error;
use crate::transform::Palette;

/// What one converting row needs, resolved once per image by `RowConverter`.
pub(crate) struct RowConversion<'a> {
    pub(crate) color_type: ColorType,
    pub(crate) bit_depth: BitDepth,
    /// Samples per output pixel: 3 or 4.
    pub(crate) channels: usize,
    /// Whether output samples are 16-bit big-endian, or 8-bit.
    pub(crate) wide: bool,
    /// The `tRNS` grey level of a greyscale image, at the file's own depth.
    pub(crate) grey_key: Option<u16>,
    /// The `tRNS` colour of an RGB image, at the file's own depth.
    pub(crate) rgb_key: Option<[u16; 3]>,
    /// The resolved palette of an indexed image.
    pub(crate) palette: Option<&'a Palette>,
}

/// Converts `row` into `target` on the fastest path the shape has, or reports that the
/// caller's scalar path must do it.
///
/// `Ok(false)` leaves `target` untouched; `Err` means a malformed row was caught
/// mid-conversion, exactly as the scalar path would report it.
pub(crate) fn convert_row(
    conv: &RowConversion<'_>,
    row: &[u8],
    target: &mut [u8],
) -> Result<bool, Error> {
    if scalar_forced() {
        return Ok(false);
    }
    // The indexed path is a resolved-table copy — portable scalar, worth having on every
    // target. The palette load per pixel is a gather SSE2 cannot do, so no kernel exists
    // for it; the win over the generic path is one entry copy instead of four
    // per-channel writes.
    if conv.color_type == ColorType::Indexed {
        let Some(palette) = conv.palette else { return Ok(false) };
        return indexed(conv, row, target, palette);
    }
    // `Rgb` → `Rgba` is one copy plus a fixed alpha lane per pixel — portable scalar,
    // and only for the keyless shape, which the byte-level kernels express exactly. A
    // `tRNS`-keyed row needs a whole-pixel compare per pixel and stays scalar.
    if conv.color_type == ColorType::Rgb {
        return rgb_to_rgba(conv, row, target);
    }
    // The conversion kernels are shuffle-built, so a backend without a hardware shuffle
    // leaves the row to the scalar helpers.
    if !shuffles_available() {
        return Ok(false);
    }
    Ok(kernels::convert_row(conv, row, target))
}

/// Pixels the sub-byte depths are unpacked into before the byte-level paths run. Any
/// width is fine as long as the per-row cost stays small; the scratch is stack memory.
const SUBBYTE_CHUNK: usize = 512;

/// Splits a sub-byte-depth row into ≤ [`SUBBYTE_CHUNK`]-pixel spans, unpacks each span
/// MSB-first into a scratch row of whole bytes, and runs `emit` on it.
fn chunked_subbyte(
    bits: usize,
    row: &[u8],
    target: &mut [u8],
    out_stride: usize,
    emit: &mut impl FnMut(&[u8], &mut [u8]) -> Result<(), Error>,
) -> Result<bool, Error> {
    let width = target.len() / out_stride;
    if row.len() != (width * bits).div_ceil(8) {
        return Ok(false);
    }
    let mut scratch = [0u8; SUBBYTE_CHUNK];
    let mut x = 0;
    while x < width {
        let n = (width - x).min(SUBBYTE_CHUNK);
        let mask = (1u8 << bits) - 1;
        for (i, slot) in scratch[..n].iter_mut().enumerate() {
            let bit = (x + i) * bits;
            *slot = (row[bit / 8] >> (8 - bits - bit % 8)) & mask;
        }
        emit(&scratch[..n], &mut target[x * out_stride..(x + n) * out_stride])?;
        x += n;
    }
    Ok(true)
}

/// One resolved-table lookup and one fixed-size entry copy per pixel — the indexed
/// conversion's inner loop.
///
/// `OUT` is the bytes an output pixel occupies (3, 4, 6 or 8) and fixes the table stride
/// with it: entries are 4 bytes wide for 8-bit outputs and 8 for 16-bit ones. The copy is
/// a constant size, so it compiles to a single load and store — a runtime stride here
/// would turn it into a `memcpy` call per pixel, which measured slower than the scalar
/// per-channel write it replaces.
fn indexed_emit<const OUT: usize>(
    palette: &Palette,
    indices: &[u8],
    target: &mut [u8],
) -> Result<(), Error> {
    if OUT == 4 {
        for (pixel, &index) in target.chunks_exact_mut(OUT).zip(indices) {
            if usize::from(index) >= palette.len {
                return Err(Error::PaletteIndexOutOfRange);
            }
            pixel.copy_from_slice(&palette.rgba[usize::from(index)]);
        }
    } else if OUT == 8 {
        for (pixel, &index) in target.chunks_exact_mut(OUT).zip(indices) {
            if usize::from(index) >= palette.len {
                return Err(Error::PaletteIndexOutOfRange);
            }
            pixel.copy_from_slice(&palette.rgba16[usize::from(index)]);
        }
    } else if OUT == 3 {
        // Three-channel targets take the RGB prefix of the same entries, alpha dropped.
        for (pixel, &index) in target.chunks_exact_mut(OUT).zip(indices) {
            if usize::from(index) >= palette.len {
                return Err(Error::PaletteIndexOutOfRange);
            }
            pixel.copy_from_slice(&palette.rgba[usize::from(index)][..OUT]);
        }
    } else {
        for (pixel, &index) in target.chunks_exact_mut(OUT).zip(indices) {
            if usize::from(index) >= palette.len {
                return Err(Error::PaletteIndexOutOfRange);
            }
            pixel.copy_from_slice(&palette.rgba16[usize::from(index)][..OUT]);
        }
    }
    Ok(())
}

/// `Rgb` → `Rgba` for the keyless shape: the three channels copy straight over and the
/// alpha lane takes the constant. 16-bit copies carry the big-endian byte order through
/// untouched.
///
/// Only the widening shapes are claimed: measured against the scalar helpers, the
/// equal-width ones — 8→8 and 16→16 — lose to the autovectorised scalar loop.
fn rgb_to_rgba(conv: &RowConversion<'_>, row: &[u8], target: &mut [u8]) -> Result<bool, Error> {
    if conv.channels != 4 || conv.rgb_key.is_some() {
        return Ok(false);
    }
    // A target that is not a whole number of pixels is declined: the paired chunk walk
    // below would leave its tail unwritten while reporting the row converted.
    match (conv.bit_depth, conv.wide) {
        (BitDepth::Eight, true) => {
            if !target.len().is_multiple_of(8) || row.len() != target.len() / 8 * 3 {
                return Ok(false);
            }
            for (px, out) in row.chunks_exact(3).zip(target.chunks_exact_mut(8)) {
                let (r, g, b) = (px[0], px[1], px[2]);
                out.copy_from_slice(&[r, r, g, g, b, b, 0xFF, 0xFF]);
            }
        }
        (BitDepth::Sixteen, false) => {
            if !target.len().is_multiple_of(4) || row.len() != target.len() / 4 * 6 {
                return Ok(false);
            }
            for (px, out) in row.chunks_exact(6).zip(target.chunks_exact_mut(4)) {
                out.copy_from_slice(&[px[0], px[2], px[4], 255]);
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// The indexed conversion: a table copy per pixel, portable to every target.
///
/// At depth 8 every output width is claimed. The sub-byte depths are claimed only for
/// 16-bit output, where the widened table copy beats the scalar per-channel write —
/// measured against the scalar helpers, the 8-bit sub-byte path loses to the
/// autovectorised scalar loop.
fn indexed(
    conv: &RowConversion<'_>,
    row: &[u8],
    target: &mut [u8],
    palette: &Palette,
) -> Result<bool, Error> {
    let out = conv.channels * usize::from(conv.wide) + conv.channels;
    // Anything but the four shapes `indexed_emit` is instantiated for is declined rather
    // than reported as converted: a target that goes unwritten would surface as pixels
    // nobody set. So is a target that is not a whole number of pixels, whose tail
    // `chunks_exact_mut` would leave unwritten.
    if !matches!(out, 3 | 4 | 6 | 8) || !target.len().is_multiple_of(out) {
        return Ok(false);
    }
    let mut emit = |indices: &[u8], target: &mut [u8]| match out {
        3 => indexed_emit::<3>(palette, indices, target),
        4 => indexed_emit::<4>(palette, indices, target),
        6 => indexed_emit::<6>(palette, indices, target),
        8 => indexed_emit::<8>(palette, indices, target),
        _ => unreachable!("`out` was checked against the four instantiated widths above"),
    };
    match conv.bit_depth {
        BitDepth::Eight => {
            if row.len() != target.len() / out {
                return Ok(false);
            }
            emit(row, target)?;
            Ok(true)
        }
        depth @ (BitDepth::One | BitDepth::Two | BitDepth::Four) if conv.wide => {
            chunked_subbyte(depth.bits(), row, target, out, &mut emit)
        }
        BitDepth::One | BitDepth::Two | BitDepth::Four | BitDepth::Sixteen => Ok(false),
    }
}

// ---------------------------------------------------------------------------
// Portable SIMD kernels.
//
// One source, compiled per backend: `fearless_simd` picks SSE2, SSE4.2, AVX2, AVX-512,
// NEON or wasm SIMD at run time, with a scalar fallback for everything else. Two rules
// from the crate's F2b evaluation carry over. Every kernel carries `#[simd]`: without it
// the same source measured 2.4x *slower* than scalar. And a kernel that would lean on a
// slow path for its backend declines instead, leaving the caller's scalar code to run.
//
// The filter kernel needs only lane arithmetic, so it claims every vector backend. The
// conversion kernels are built from dynamic byte shuffles (`swizzle_dyn_precise`), which
// are a single `pshufb` on SSE4.2+, NEON and wasm SIMD but a per-lane scalar emulation on
// the bare SSE2 backend - so those decline on SSE2, where the autovectorised scalar loop
// is faster than the emulation. The contract is the SSE2 kernels' contract: check the
// slice lengths, touch memory only through indexing, and leave a declined row untouched.

mod kernels {
    use fearless_simd::{Bytes, Simd, dispatch, i16x8, prelude::*, u8x16, u16x8, u32x4};
    use fearless_simd_macros::simd;

    use super::{RowConversion, level};
    use crate::common::{BitDepth, ColorType};

    /// Reverses `Paeth` on one row at the best level this machine has, or declines a stride
    /// the kernels do not cover.
    ///
    /// Each stride gets its own `dispatch!` rather than one around the stride match: with
    /// both kernels inside a single dispatched closure the 4-byte kernel measured about
    /// 1.6% slower on 2048x512 rows (3.13 against 3.18 ms, every alternating run), the
    /// price of how the shared closure was laid out and nothing the source shows.
    pub(super) fn paeth_row(row: &mut [u8], prev: &[u8], bpp: usize) -> bool {
        match bpp {
            3 => dispatch!(level(), simd => paeth3(simd, row, prev)),
            4 => dispatch!(level(), simd => paeth4(simd, row, prev)),
            _ => false,
        }
    }

    /// [`paeth_row`] at a chosen level. The parity tests call this once per backend the
    /// machine supports: `dispatch!` only ever reaches the best one, which would leave
    /// every lower level's code unexercised.
    #[cfg(test)]
    pub(super) fn paeth_row_with<V: Simd>(
        simd: V,
        row: &mut [u8],
        prev: &[u8],
        bpp: usize,
    ) -> bool {
        match bpp {
            3 => paeth3(simd, row, prev),
            4 => paeth4(simd, row, prev),
            _ => false,
        }
    }

    /// The absolute value of each 16-bit lane.
    ///
    /// The distances are bounded by 510, so the negate-and-maximum form cannot overflow.
    /// It is also the shortest form on the serial chain: one subtract and one maximum,
    /// where a compare-and-select costs a blend the old kernel did not pay.
    #[simd]
    fn abs_i16<V: Simd>(simd: V, value: i16x8<V>) -> i16x8<V> {
        simd.max_i16x8(value, i16x8::splat(simd, 0) - value)
    }

    /// One pixel's Paeth prediction added to its residual, as the four reconstructed bytes
    /// in the low lanes.
    ///
    /// `p - a` is `b - c` and `p - b` is `a - c`, so the three distances need only two
    /// differences; `p - c` is their sum. Ties break a, then b, then c, as the
    /// specification requires. The predictor is added in eight-bit lanes so the sum wraps
    /// modulo 256, and the narrowed result is already inside `0..=255`.
    #[simd]
    fn predict<V: Simd>(
        simd: V,
        raw: i16x8<V>,
        left: i16x8<V>,
        above: i16x8<V>,
        upper_left: i16x8<V>,
    ) -> u8x16<V> {
        let da = above - upper_left;
        let db = left - upper_left;
        let dc = da + db;
        let pa = abs_i16(simd, da);
        let pb = abs_i16(simd, db);
        let pc = abs_i16(simd, dc);

        let smallest = simd.min_i16x8(pc, simd.min_i16x8(pa, pb));
        let nearest = simd.select_i16x8(
            simd.simd_eq_i16x8(smallest, pa),
            left,
            simd.select_i16x8(simd.simd_eq_i16x8(smallest, pb), above, upper_left),
        );
        // Both operands hold their bytes in the low half of each 16-bit lane (the widening
        // zero-extended them), so the eight-bit add leaves each sum in its lane's low byte
        // and the narrowing packs those four bytes together.
        let sum = raw.bitcast::<u8x16<V>>() + nearest.bitcast::<u8x16<V>>();
        simd.narrow_u16x8(sum.bitcast(), u16x8::splat(simd, 0))
    }

    /// Four adjacent bytes as four 16-bit lanes, the width the predictor's distances need:
    /// `a + b - c` leaves the byte range, and 16-bit lanes hold it without overflow.
    ///
    /// The load is a single 32-bit read spread over the register by a splat, which is what
    /// the SSE2 kernel did: it touches nothing outside the pixel, needs no branch for the
    /// row end, and keeps the loop's L1 traffic to a pixel per iteration. The callers
    /// guarantee four bytes are in reach.
    #[simd]
    fn widen_pixel<V: Simd>(simd: V, bytes: &[u8]) -> i16x8<V> {
        let word = u32::from_le_bytes(bytes[..4].try_into().unwrap());
        let v: u8x16<V> = u32x4::splat(simd, word).bitcast();
        let (low, _) = v.widen();
        low.bitcast()
    }

    /// Reverses `Paeth` on an RGB row, three bytes per pixel.
    ///
    /// The fourth lane carries the next pixel's first residual byte, which is computed and
    /// thrown away: it is what lets one register hold a whole pixel without lanes reading
    /// across pixels. That fourth byte has to exist, so the register loop stops one pixel
    /// short of the row end and [`tail`] finishes the last pixel - on every row, not only on
    /// a ragged one. A PNG scanline is always a whole number of pixels, so the tail is
    /// exactly one pixel there; it also keeps an odd length from overrunning.
    ///
    /// Declines, leaving `row` untouched, unless `row` and `prev` are the same length and
    /// hold at least one pixel.
    #[simd]
    fn paeth3<V: Simd>(simd: V, row: &mut [u8], prev: &[u8]) -> bool {
        if row.len() != prev.len() || row.len() < 3 {
            return false;
        }
        let mut left = i16x8::splat(simd, 0);
        let mut upper_left = i16x8::splat(simd, 0);
        let mut at = 0;
        // Each load reads four bytes but the pixel is three, so the loop needs a full four
        // in reach: the last pixel of the row, whatever its length, goes to `tail`.
        while at + 4 <= row.len() {
            let above = widen_pixel(simd, &prev[at..]);
            let packed = predict(simd, widen_pixel(simd, &row[at..]), left, above, upper_left);
            // Three single-byte stores rather than four: the fourth byte belongs to the
            // next pixel, and a store that reaches into it makes the next iteration's load
            // of the row forward from a partly-overlapping store, which measured slower
            // than the stores themselves.
            let lanes: [u32; 4] = packed.bitcast::<u32x4<V>>().into();
            let word = lanes[0];
            row[at] = word as u8;
            row[at + 1] = (word >> 8) as u8;
            row[at + 2] = (word >> 16) as u8;

            upper_left = above;
            let (low, _) = packed.widen();
            left = low.bitcast();
            at += 3;
        }
        tail(row, prev, at, 3);
        true
    }

    /// Reverses `Paeth` on an RGBA row, four bytes per pixel.
    ///
    /// Declines, leaving `row` untouched, unless `row` and `prev` are the same length and
    /// hold at least one pixel.
    #[simd]
    fn paeth4<V: Simd>(simd: V, row: &mut [u8], prev: &[u8]) -> bool {
        if row.len() != prev.len() || row.len() < 4 {
            return false;
        }
        let mut left = i16x8::splat(simd, 0);
        let mut upper_left = i16x8::splat(simd, 0);
        let mut at = 0;
        while at + 4 <= row.len() {
            let above = widen_pixel(simd, &prev[at..]);
            let packed = predict(simd, widen_pixel(simd, &row[at..]), left, above, upper_left);
            let lanes: [u32; 4] = packed.bitcast::<u32x4<V>>().into();
            row[at..at + 4].copy_from_slice(&lanes[0].to_le_bytes());

            upper_left = above;
            let (low, _) = packed.widen();
            left = low.bitcast();
            at += 4;
        }
        tail(row, prev, at, 4);
        true
    }

    /// Finishes the bytes the register loop could not cover, reading the left neighbour
    /// back out of the row the loop has already reconstructed. A byte with no left
    /// neighbour - the first pixel of a row too short for the register loop - takes the
    /// specification's zero, exactly as the scalar decoder's own reference does.
    fn tail(row: &mut [u8], prev: &[u8], at: usize, bpp: usize) {
        for x in at..row.len() {
            let left = if x >= bpp { row[x - bpp] } else { 0 };
            let upper_left = if x >= bpp { prev[x - bpp] } else { 0 };
            row[x] = row[x].wrapping_add(crate::filter::paeth_predictor(left, prev[x], upper_left));
        }
    }

    // ------------------------------------------------------------------
    // Conversion kernels: a native-layout row in, interleaved pixels out.
    //
    // Each kernel is a small table of compile-time swizzle patterns applied per 16-byte
    // block, plus a lane compare when a `tRNS` key is in play. A pattern byte of 16 or
    // more zeroes its output lane (`swizzle_dyn_precise`), which is how the alpha lane of
    // a replicated sample is left for the alpha vector to fill.
    //
    // The 16-bit cases never byteswap: a big-endian sample read as a little-endian lane is
    // the swapped value, and storing that lane little-endian writes the same bytes back -
    // the swap is invisible end to end. Only the `tRNS` compares see it, and they compare
    // against a swapped key instead.

    /// `[v, v, v, -]` per pixel for the four pixel positions of a 16-byte block.
    const REPLICATE3: [[u8; 16]; 4] = [
        [0, 0, 0, 16, 1, 1, 1, 16, 2, 2, 2, 16, 3, 3, 3, 16],
        [4, 4, 4, 16, 5, 5, 5, 16, 6, 6, 6, 16, 7, 7, 7, 16],
        [8, 8, 8, 16, 9, 9, 9, 16, 10, 10, 10, 16, 11, 11, 11, 16],
        [12, 12, 12, 16, 13, 13, 13, 16, 14, 14, 14, 16, 15, 15, 15, 16],
    ];

    /// The matching `[-, -, -, alpha]` lane gather, reading the compare byte of each pixel.
    const ALPHA3: [[u8; 16]; 4] = [
        [16, 16, 16, 0, 16, 16, 16, 1, 16, 16, 16, 2, 16, 16, 16, 3],
        [16, 16, 16, 4, 16, 16, 16, 5, 16, 16, 16, 6, 16, 16, 16, 7],
        [16, 16, 16, 8, 16, 16, 16, 9, 16, 16, 16, 10, 16, 16, 16, 11],
        [16, 16, 16, 12, 16, 16, 16, 13, 16, 16, 16, 14, 16, 16, 16, 15],
    ];

    /// `ALPHA3` for a compare mask that is one 16-bit word per pixel: pixel *n*'s mask sits
    /// in bytes `2n` and `2n + 1`, so the gather reads every other byte.
    const ALPHA3_FROM_WORDS: [[u8; 16]; 2] = [
        [16, 16, 16, 0, 16, 16, 16, 2, 16, 16, 16, 4, 16, 16, 16, 6],
        [16, 16, 16, 8, 16, 16, 16, 10, 16, 16, 16, 12, 16, 16, 16, 14],
    ];

    /// The low half moved to the high half, for joining two gathered halves.
    const UPPER_HALF: [u8; 16] = [16, 16, 16, 16, 16, 16, 16, 16, 0, 1, 2, 3, 4, 5, 6, 7];

    /// `[v, v, v, v]` per sample pair for the two sample positions of a 16-byte block:
    /// 16-bit samples replicated three times with the alpha word left for another vector.
    const REPLICATE2: [[u8; 16]; 4] = [
        [0, 1, 0, 1, 0, 1, 16, 16, 2, 3, 2, 3, 2, 3, 16, 16],
        [4, 5, 4, 5, 4, 5, 16, 16, 6, 7, 6, 7, 6, 7, 16, 16],
        [8, 9, 8, 9, 8, 9, 16, 16, 10, 11, 10, 11, 10, 11, 16, 16],
        [12, 13, 12, 13, 12, 13, 16, 16, 14, 15, 14, 15, 14, 15, 16, 16],
    ];

    /// The matching `[-, -, alpha]` word gather, reading the compare word of each sample.
    const ALPHA2: [[u8; 16]; 4] = [
        [16, 16, 16, 16, 16, 16, 0, 1, 16, 16, 16, 16, 16, 16, 2, 3],
        [16, 16, 16, 16, 16, 16, 4, 5, 16, 16, 16, 16, 16, 16, 6, 7],
        [16, 16, 16, 16, 16, 16, 8, 9, 16, 16, 16, 16, 16, 16, 10, 11],
        [16, 16, 16, 16, 16, 16, 12, 13, 16, 16, 16, 16, 16, 16, 14, 15],
    ];

    /// `[g, g, g, a]` per pixel, from `[g, a]` pairs.
    const GREY_ALPHA_PAIRS: [[u8; 16]; 2] = [
        [0, 0, 0, 1, 2, 2, 2, 3, 4, 4, 4, 5, 6, 6, 6, 7],
        [8, 8, 8, 9, 10, 10, 10, 11, 12, 12, 12, 13, 14, 14, 14, 15],
    ];

    /// `[g, g, g, g, g, g, a, a]` per pixel, from `[g, a]` pairs.
    const GREY_ALPHA_PAIRS_WIDE: [[u8; 16]; 4] = [
        [0, 0, 0, 0, 0, 0, 1, 1, 2, 2, 2, 2, 2, 2, 3, 3],
        [4, 4, 4, 4, 4, 4, 5, 5, 6, 6, 6, 6, 6, 6, 7, 7],
        [8, 8, 8, 8, 8, 8, 9, 9, 10, 10, 10, 10, 10, 10, 11, 11],
        [12, 12, 12, 12, 12, 12, 13, 13, 14, 14, 14, 14, 14, 14, 15, 15],
    ];

    /// `[G, G, G, A]` from `[G, A]` samples, one 16-byte block per two samples.
    const GREY_ALPHA_WIDE_SAMPLES: [[u8; 16]; 2] = [
        [0, 1, 0, 1, 0, 1, 2, 3, 4, 5, 4, 5, 4, 5, 6, 7],
        [8, 9, 8, 9, 8, 9, 10, 11, 12, 13, 12, 13, 12, 13, 14, 15],
    ];

    /// Every other byte of a 16-byte block, gathered into the low half: the file's high
    /// bytes of the eight big-endian samples.
    const SAMPLE_HIGH: [u8; 16] = [0, 2, 4, 6, 8, 10, 12, 14, 16, 16, 16, 16, 16, 16, 16, 16];

    /// `[g, g, g, a]` from `[G_hi, G_lo, A_hi, A_lo]` pixels, one block per four pixels.
    const GREY_ALPHA_16_TO_8: [u8; 16] = [0, 0, 0, 2, 4, 4, 4, 6, 8, 8, 8, 10, 12, 12, 12, 14];

    /// `[v, v]` per byte for the two eight-byte pixels of a 16-byte block.
    const WIDEN_BYTES: [[u8; 16]; 2] = [
        [0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7],
        [8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13, 14, 14, 15, 15],
    ];

    /// `Greyscale` at depth 8 → RGBA8: every byte becomes `v, v, v, alpha`.
    #[simd]
    fn grey8_rgba8<V: Simd>(simd: V, row: &[u8], target: &mut [u8], key: Option<u16>) -> bool {
        let width = target.len() / 4;
        if row.len() != width {
            return false;
        }
        // A key past one byte can never equal a byte sample - every pixel stays opaque.
        let key = key.and_then(|k| u8::try_from(k).ok());
        let keyv = u8x16::splat(simd, key.unwrap_or(0));
        let mut x = 0;
        let mut o = 0;
        while x + 16 <= width {
            let v = u8x16::from_slice(simd, &row[x..x + 16]);
            let alpha = if key.is_some() {
                let matched = simd.simd_eq_u8x16(v, keyv);
                simd.select_u8x16(matched, u8x16::splat(simd, 0), u8x16::splat(simd, 0xFF))
            } else {
                u8x16::splat(simd, 0xFF)
            };
            for (k, pattern) in REPLICATE3.iter().enumerate() {
                let replicate = u8x16::from_slice(simd, pattern);
                let alpha_pattern = u8x16::from_slice(simd, &ALPHA3[k]);
                let out = simd.swizzle_dyn_precise_u8x16(v, replicate)
                    | simd.swizzle_dyn_precise_u8x16(alpha, alpha_pattern);
                target[o + 16 * k..o + 16 * k + 16].copy_from_slice(&<[u8; 16]>::from(out));
            }
            x += 16;
            o += 64;
        }
        for &g in &row[x..width] {
            let a = if key.is_some_and(|k| g == k) { 0 } else { 255 };
            target[o..o + 4].copy_from_slice(&[g, g, g, a]);
            o += 4;
        }
        true
    }

    /// `Greyscale` at depth 16 → RGBA8: the file's high byte is the output sample - the
    /// even byte of the big-endian pair - replicated, with the `tRNS` compare still taken
    /// at the full 16 bits against a swapped key.
    #[simd]
    fn grey16_rgba8<V: Simd>(simd: V, row: &[u8], target: &mut [u8], key: Option<u16>) -> bool {
        let width = target.len() / 4;
        if row.len() != width * 2 {
            return false;
        }
        let keyv = u16x8::splat(simd, key.unwrap_or(0).swap_bytes());
        let mut x = 0;
        let mut o = 0;
        while x + 8 <= width {
            let v = u8x16::from_slice(simd, &row[2 * x..2 * x + 16]);
            let samples = simd.swizzle_dyn_precise_u8x16(v, u8x16::from_slice(simd, &SAMPLE_HIGH));
            let alpha = if key.is_some() {
                let matched = simd.simd_eq_u16x8(v.bitcast(), keyv);
                let words =
                    simd.select_u16x8(matched, u16x8::splat(simd, 0), u16x8::splat(simd, 0xFFFF));
                words.bitcast::<u8x16<V>>()
            } else {
                u8x16::splat(simd, 0xFF)
            };
            for (k, pattern) in REPLICATE3[..2].iter().enumerate() {
                let replicate = u8x16::from_slice(simd, pattern);
                // `alpha` holds a 16-bit mask word per pixel here, not a byte per pixel as
                // in `grey8_rgba8`, so it takes the word-indexed gather.
                let alpha_pattern = u8x16::from_slice(simd, &ALPHA3_FROM_WORDS[k]);
                let out = simd.swizzle_dyn_precise_u8x16(samples, replicate)
                    | simd.swizzle_dyn_precise_u8x16(alpha, alpha_pattern);
                target[o + 16 * k..o + 16 * k + 16].copy_from_slice(&<[u8; 16]>::from(out));
            }
            x += 8;
            o += 32;
        }
        for px in row[2 * x..2 * width].chunks_exact(2) {
            let raw = u16::from_be_bytes([px[0], px[1]]);
            let a = if key == Some(raw) { 0 } else { 255 };
            target[o..o + 4].copy_from_slice(&[px[0], px[0], px[0], a]);
            o += 4;
        }
        true
    }

    /// `Greyscale` at depth 16 → RGBA16: the `u16` sample is already the output sample.
    #[simd]
    fn grey16_rgba16<V: Simd>(simd: V, row: &[u8], target: &mut [u8], key: Option<u16>) -> bool {
        let width = target.len() / 8;
        if row.len() != width * 2 {
            return false;
        }
        let keyv = u16x8::splat(simd, key.unwrap_or(0).swap_bytes());
        let mut x = 0;
        let mut o = 0;
        while x + 8 <= width {
            let v = u8x16::from_slice(simd, &row[2 * x..2 * x + 16]);
            let alpha = if key.is_some() {
                let matched = simd.simd_eq_u16x8(v.bitcast(), keyv);
                let words =
                    simd.select_u16x8(matched, u16x8::splat(simd, 0), u16x8::splat(simd, 0xFFFF));
                words.bitcast::<u8x16<V>>()
            } else {
                u8x16::splat(simd, 0xFF)
            };
            for (k, pattern) in REPLICATE2.iter().enumerate() {
                let replicate = u8x16::from_slice(simd, pattern);
                let alpha_pattern = u8x16::from_slice(simd, &ALPHA2[k]);
                let out = simd.swizzle_dyn_precise_u8x16(v, replicate)
                    | simd.swizzle_dyn_precise_u8x16(alpha, alpha_pattern);
                target[o + 16 * k..o + 16 * k + 16].copy_from_slice(&<[u8; 16]>::from(out));
            }
            x += 8;
            o += 64;
        }
        for px in row[2 * x..2 * width].chunks_exact(2) {
            let raw = u16::from_be_bytes([px[0], px[1]]);
            let a = if key == Some(raw) { [0, 0] } else { [0xFF, 0xFF] };
            target[o..o + 8]
                .copy_from_slice(&[px[0], px[1], px[0], px[1], px[0], px[1], a[0], a[1]]);
            o += 8;
        }
        true
    }

    /// `GreyscaleAlpha` at depth 8 → RGBA8: `[g, a]` pairs become `g, g, g, a`.
    #[simd]
    fn graya8_rgba8<V: Simd>(simd: V, row: &[u8], target: &mut [u8]) -> bool {
        let width = target.len() / 4;
        if row.len() != width * 2 {
            return false;
        }
        let mut x = 0;
        let mut o = 0;
        while x + 8 <= width {
            let v = u8x16::from_slice(simd, &row[2 * x..2 * x + 16]);
            for (k, pattern) in GREY_ALPHA_PAIRS.iter().enumerate() {
                let out = simd.swizzle_dyn_precise_u8x16(v, u8x16::from_slice(simd, pattern));
                target[o + 16 * k..o + 16 * k + 16].copy_from_slice(&<[u8; 16]>::from(out));
            }
            x += 8;
            o += 32;
        }
        for px in row[2 * x..2 * width].chunks_exact(2) {
            target[o..o + 4].copy_from_slice(&[px[0], px[0], px[0], px[1]]);
            o += 4;
        }
        true
    }

    /// `GreyscaleAlpha` at depth 8 → RGBA16: the `[g, g, g, a]` bytes widen to `u16`
    /// samples by replicating each byte once more.
    #[simd]
    fn graya8_rgba16<V: Simd>(simd: V, row: &[u8], target: &mut [u8]) -> bool {
        let width = target.len() / 8;
        if row.len() != width * 2 {
            return false;
        }
        let mut x = 0;
        let mut o = 0;
        while x + 8 <= width {
            let v = u8x16::from_slice(simd, &row[2 * x..2 * x + 16]);
            for (k, pattern) in GREY_ALPHA_PAIRS_WIDE.iter().enumerate() {
                let out = simd.swizzle_dyn_precise_u8x16(v, u8x16::from_slice(simd, pattern));
                target[o + 16 * k..o + 16 * k + 16].copy_from_slice(&<[u8; 16]>::from(out));
            }
            x += 8;
            o += 64;
        }
        for px in row[2 * x..2 * width].chunks_exact(2) {
            let (g, a) = (px[0], px[1]);
            target[o..o + 8].copy_from_slice(&[g, g, g, g, g, g, a, a]);
            o += 8;
        }
        true
    }

    /// `GreyscaleAlpha` at depth 16 → RGBA8: the file's high bytes of `[G, A]` become
    /// `g, g, g, a`.
    #[simd]
    fn graya16_rgba8<V: Simd>(simd: V, row: &[u8], target: &mut [u8]) -> bool {
        let width = target.len() / 4;
        if row.len() != width * 4 {
            return false;
        }
        let mut x = 0;
        let mut o = 0;
        while x + 4 <= width {
            let v = u8x16::from_slice(simd, &row[4 * x..4 * x + 16]);
            let out =
                simd.swizzle_dyn_precise_u8x16(v, u8x16::from_slice(simd, &GREY_ALPHA_16_TO_8));
            target[o..o + 16].copy_from_slice(&<[u8; 16]>::from(out));
            x += 4;
            o += 16;
        }
        for px in row[4 * x..4 * width].chunks_exact(4) {
            target[o..o + 4].copy_from_slice(&[px[0], px[0], px[0], px[2]]);
            o += 4;
        }
        true
    }

    /// `GreyscaleAlpha` at depth 16 → RGBA16: `[G, A]` samples become `G, G, G, A`, the
    /// byte order riding through untouched.
    #[simd]
    fn graya16_rgba16<V: Simd>(simd: V, row: &[u8], target: &mut [u8]) -> bool {
        let width = target.len() / 8;
        if row.len() != width * 4 {
            return false;
        }
        let mut x = 0;
        let mut o = 0;
        while x + 4 <= width {
            let v = u8x16::from_slice(simd, &row[4 * x..4 * x + 16]);
            for (k, pattern) in GREY_ALPHA_WIDE_SAMPLES.iter().enumerate() {
                let out = simd.swizzle_dyn_precise_u8x16(v, u8x16::from_slice(simd, pattern));
                target[o + 16 * k..o + 16 * k + 16].copy_from_slice(&<[u8; 16]>::from(out));
            }
            x += 4;
            o += 32;
        }
        for px in row[4 * x..4 * width].chunks_exact(4) {
            let (g, a) = ([px[0], px[1]], [px[2], px[3]]);
            target[o..o + 8].copy_from_slice(&[g[0], g[1], g[0], g[1], g[0], g[1], a[0], a[1]]);
            o += 8;
        }
        true
    }

    /// `Rgba` at depth 16 → RGBA8: each `u16` sample narrows to its high byte - the even
    /// byte of the big-endian pair.
    #[simd]
    fn rgba16_rgba8<V: Simd>(simd: V, row: &[u8], target: &mut [u8]) -> bool {
        let width = target.len() / 4;
        if row.len() != width * 8 {
            return false;
        }
        let mut x = 0;
        let mut o = 0;
        while x + 4 <= width {
            let a = u8x16::from_slice(simd, &row[8 * x..8 * x + 16]);
            let b = u8x16::from_slice(simd, &row[8 * x + 16..8 * x + 32]);
            let low = simd.swizzle_dyn_precise_u8x16(a, u8x16::from_slice(simd, &SAMPLE_HIGH));
            let high = simd.swizzle_dyn_precise_u8x16(b, u8x16::from_slice(simd, &SAMPLE_HIGH));
            let joined =
                low | simd.swizzle_dyn_precise_u8x16(high, u8x16::from_slice(simd, &UPPER_HALF));
            target[o..o + 16].copy_from_slice(&<[u8; 16]>::from(joined));
            x += 4;
            o += 16;
        }
        for px in row[8 * x..8 * width].chunks_exact(8) {
            target[o..o + 4].copy_from_slice(&[px[0], px[2], px[4], px[6]]);
            o += 4;
        }
        true
    }

    /// `Rgba` at depth 8 → RGBA16: every byte widens to `v * 257` by repeating it.
    #[simd]
    fn rgba8_rgba16<V: Simd>(simd: V, row: &[u8], target: &mut [u8]) -> bool {
        let width = target.len() / 8;
        if row.len() != width * 4 {
            return false;
        }
        let mut x = 0;
        let mut o = 0;
        while x + 4 <= width {
            let v = u8x16::from_slice(simd, &row[4 * x..4 * x + 16]);
            for (k, pattern) in WIDEN_BYTES.iter().enumerate() {
                let out = simd.swizzle_dyn_precise_u8x16(v, u8x16::from_slice(simd, pattern));
                target[o + 16 * k..o + 16 * k + 16].copy_from_slice(&<[u8; 16]>::from(out));
            }
            x += 4;
            o += 32;
        }
        for px in row[4 * x..4 * width].chunks_exact(4) {
            let (r, g, b, a) = (px[0], px[1], px[2], px[3]);
            target[o..o + 8].copy_from_slice(&[r, r, g, g, b, b, a, a]);
            o += 8;
        }
        true
    }

    /// The conversion dispatch: four-channel targets only - the three-channel layouts
    /// have no kernel and stay scalar.
    pub(super) fn convert_row(conv: &RowConversion<'_>, row: &[u8], target: &mut [u8]) -> bool {
        dispatch!(level(), simd => convert_row_with(simd, conv, row, target))
    }

    /// [`convert_row`] at a chosen level, for the same reason as [`paeth_row_with`]: the
    /// parity tests run it once per backend the machine supports. `false` leaves `target`
    /// untouched.
    #[inline(always)]
    pub(super) fn convert_row_with<V: Simd>(
        simd: V,
        conv: &RowConversion<'_>,
        row: &[u8],
        target: &mut [u8],
    ) -> bool {
        if conv.channels != 4 {
            return false;
        }
        match (conv.color_type, conv.bit_depth) {
            // Measured against the scalar helpers, the depth-8 → 16-bit greyscale kernel
            // loses to the autovectorised scalar loop, so only the 8-bit target is
            // claimed here.
            (ColorType::Grayscale, BitDepth::Eight) if !conv.wide => {
                grey8_rgba8(simd, row, target, conv.grey_key)
            }
            (ColorType::Grayscale, BitDepth::Sixteen) if conv.wide => {
                grey16_rgba16(simd, row, target, conv.grey_key)
            }
            (ColorType::Grayscale, BitDepth::Sixteen) => {
                grey16_rgba8(simd, row, target, conv.grey_key)
            }
            (ColorType::GrayscaleAlpha, BitDepth::Eight) if conv.wide => {
                graya8_rgba16(simd, row, target)
            }
            (ColorType::GrayscaleAlpha, BitDepth::Eight) => graya8_rgba8(simd, row, target),
            (ColorType::GrayscaleAlpha, BitDepth::Sixteen) if conv.wide => {
                graya16_rgba16(simd, row, target)
            }
            (ColorType::GrayscaleAlpha, BitDepth::Sixteen) => graya16_rgba8(simd, row, target),
            (ColorType::Rgba, BitDepth::Eight) if conv.wide => rgba8_rgba16(simd, row, target),
            (ColorType::Rgba, BitDepth::Sixteen) if !conv.wide => rgba16_rgba8(simd, row, target),
            _ => false,
        }
    }
}

/// Runs `$body` once for each vector backend this machine can run, with `$simd` bound to
/// that backend's token, and collects `(backend name, result)` pairs.
///
/// `dispatch!` only ever reaches the best backend, so on any one machine every lower
/// level's code - SSE2 and SSE4.2 on an AVX2 CPU, say - would go unexercised, although each
/// is a separate lowering of the same source. A token for a lower level can be had from the
/// detected one (`as_sse2` is `Some` on every x86 CPU that has anything better). The list is
/// empty exactly where the facade sees no vector unit: the scalar fallback and big-endian
/// targets. Annotate the result's type at the call site, so that it is known even on a
/// target where no backend arm is compiled in.
#[cfg(test)]
macro_rules! on_each_backend {
    (|$simd:ident| $body:expr) => {{
        #[allow(unused_mut, reason = "no backend arm is compiled in on some targets")]
        let mut results = Vec::new();
        if vectors_available() {
            let detected = level();
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            {
                if let Some($simd) = detected.as_sse2() {
                    results.push(("sse2", $body));
                }
                if let Some($simd) = detected.as_sse4_2() {
                    results.push(("sse4.2", $body));
                }
                if let Some($simd) = detected.as_avx2() {
                    results.push(("avx2", $body));
                }
                if let Some($simd) = detected.as_avx512() {
                    results.push(("avx512", $body));
                }
            }
            #[cfg(target_arch = "aarch64")]
            if let Some($simd) = detected.as_neon() {
                results.push(("neon", $body));
            }
            #[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
            if let Some($simd) = detected.as_wasm_simd128() {
                results.push(("wasm-simd128", $body));
            }
        }
        results
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::paeth_predictor;

    /// Whether the facade hands the conversion kernels any row on this machine: the backend
    /// needs a hardware byte shuffle. The parity tests drive the kernels below the facade,
    /// past its scalar-override gate, so `PSD_PNG_FORCE_SCALAR=1` cannot turn a kernel test
    /// into a test of the fallback; what they expect of the facade's own route is this
    /// machine's answer, not the override's.
    fn kernels_expected() -> bool {
        shuffles_available()
    }

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

    /// The Paeth kernel for `bpp` run on `filtered` once per backend this machine can run,
    /// as `(backend, claimed, row)`.
    ///
    /// The parity tests go through this rather than [`paeth_row`] so that what they check
    /// depends neither on the stride claim - with the scalar override in force the facade
    /// declines everything, and a kernel test would quietly become a test of the scalar
    /// fallback - nor on which single backend `dispatch!` would have picked.
    fn paeth_runs(bpp: usize, filtered: &[u8], prev: &[u8]) -> Vec<(&'static str, bool, Vec<u8>)> {
        let runs: Vec<(&'static str, (bool, Vec<u8>))> = on_each_backend!(|simd| {
            let mut row = filtered.to_vec();
            let claimed = kernels::paeth_row_with(simd, &mut row, prev, bpp);
            (claimed, row)
        });
        runs.into_iter().map(|(backend, (claimed, row))| (backend, claimed, row)).collect()
    }

    /// Every backend the machine supports gets exercised - the tests above and below would
    /// otherwise pass vacuously on a machine where the list came back empty - and the list
    /// is empty exactly when the facade sees no vector unit.
    #[test]
    fn the_backend_list_follows_the_facade() {
        let runs = paeth_runs(4, &[0u8; 8], &[0u8; 8]);
        assert_eq!(runs.is_empty(), !vectors_available(), "backends run: {runs:?}");
        // Cross-checked against the standard library's own detection, so that a backend
        // silently dropping out of the list cannot make every parity test vacuous for it.
        #[cfg(target_arch = "x86_64")]
        {
            let ran = |name: &str| runs.iter().any(|(backend, ..)| *backend == name);
            // SSE2 is x86-64's baseline.
            assert!(ran("sse2"), "backends run: {runs:?}");
            // The SSE4.2 level is the x86-64-v2 feature set.
            if is_x86_feature_detected!("sse4.2")
                && is_x86_feature_detected!("popcnt")
                && is_x86_feature_detected!("cmpxchg16b")
            {
                assert!(ran("sse4.2"), "backends run: {runs:?}");
            }
        }
    }

    /// The kernel must reproduce the scalar predictor for every stride it claims, at every
    /// length and on every backend — including lengths that are not a whole number of
    /// pixels, which the register loop leaves to the tail and a PNG scanline never
    /// actually has.
    #[test]
    fn kernel_matches_the_scalar_definition() {
        for bpp in [3usize, 4] {
            for pixels in [1usize, 2, 3, 4, 5, 7, 8, 15, 16, 17, 33, 64, 129] {
                for extra in 0..bpp {
                    let len = pixels * bpp + extra;
                    let filtered = corpus(len, 0x9E37_79B9_7F4A_7C15 ^ len as u64);
                    let prev = corpus(len, 0xD1B5_4A32_D192_ED03 ^ (len as u64) << 7);

                    let mut want = filtered.clone();
                    reference(&mut want, &prev, bpp);

                    for (backend, claimed, got) in paeth_runs(bpp, &filtered, &prev) {
                        assert!(claimed, "{backend}, bpp {bpp}, len {len}: declined");
                        assert_eq!(got, want, "{backend}, bpp {bpp}, len {len}");
                    }
                }
            }
        }
    }

    /// The first row of an image has no row above, and the specification defines every
    /// neighbour as zero there — which reduces `Paeth` to `Sub`.
    #[test]
    fn kernel_handles_a_first_row_over_zeros() {
        for bpp in [3usize, 4] {
            let len = 37 * bpp;
            let filtered = corpus(len, 0x0123456789ABCDEF);
            let zeros = vec![0u8; len];
            let mut want = filtered.clone();
            reference(&mut want, &zeros, bpp);

            for (backend, claimed, got) in paeth_runs(bpp, &filtered, &zeros) {
                assert!(claimed, "{backend}, bpp {bpp}: declined");
                assert_eq!(got, want, "{backend}, bpp {bpp}");
            }
        }
    }

    /// Extreme byte values exercise the wrap in the final add and the extremes of the
    /// predictor's distances, which are where a lane-wise port most easily diverges.
    #[test]
    fn kernel_handles_extreme_values() {
        for bpp in [3usize, 4] {
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
                for (backend, claimed, got) in paeth_runs(bpp, &filtered, &prev) {
                    assert!(claimed, "{backend}, bpp {bpp} seed {seed}: declined");
                    assert_eq!(got, want, "{backend}, bpp {bpp} seed {seed}");
                }
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

    /// The kernel claims exactly the RGB and RGBA strides whenever a vector backend is in
    /// force, and nothing otherwise — which is also the promise the caller relies on when
    /// it picks between the kernel and the scalar two-row wavefront.
    #[test]
    fn the_claimed_strides_follow_the_backend() {
        let expected: &[usize] =
            if vectors_available() && !scalar_forced() { &[3, 4] } else { &[] };
        let claimed: Vec<usize> =
            [1usize, 2, 3, 4, 6, 8].into_iter().filter(|&bpp| claims_stride(bpp)).collect();
        assert_eq!(claimed, expected);
    }

    /// The stride claim and the kernel have to agree, because the caller picks between this
    /// kernel and the scalar two-row wavefront using the claim alone.
    #[test]
    fn the_stride_claim_matches_the_kernel() {
        let len = 64;
        let filtered = corpus(len, 0x2468);
        let prev = corpus(len, 0x1357);
        for bpp in [1usize, 2, 3, 4, 6, 8] {
            let mut row = filtered.clone();
            let handled = paeth_row(&mut row, &prev, bpp);
            assert_eq!(handled, claims_stride(bpp), "bpp {bpp}: claim and kernel disagree");
            if !handled {
                assert_eq!(row, filtered, "bpp {bpp}: a declined row must be untouched");
            }
        }
        // One full pixel is the shortest row a claimed stride accepts, and a claim that
        // quietly stopped covering it would leave the caller reconstructing nothing.
        for bpp in [3usize, 4].into_iter().filter(|&bpp| claims_stride(bpp)) {
            let mut row = filtered[..bpp].to_vec();
            assert!(paeth_row(&mut row, &prev[..bpp], bpp), "bpp {bpp}: one pixel must be claimed");
        }
    }

    // ------------------------------------------------------------------
    // Conversion parity: every row the accelerated paths claim must equal the scalar
    // helpers in transform.rs byte for byte — they are the reference the kernels are
    // pinned to. The tests drive the kernels (once per backend the machine supports) and
    // the portable paths directly rather than the facade, so a compiled-in scalar
    // override cannot turn a kernel test into a test of the fallback.

    use crate::transform::{self, Palette, RowSample};

    /// A four-channel `RowConversion`, the shape the kernels cover.
    fn conv<'a>(
        color: ColorType,
        depth: BitDepth,
        wide: bool,
        palette: Option<&'a Palette>,
    ) -> RowConversion<'a> {
        RowConversion {
            color_type: color,
            bit_depth: depth,
            channels: 4,
            wide,
            grey_key: None,
            rgb_key: None,
            palette,
        }
    }

    /// The scalar helpers from transform.rs dispatch the same way `RowConverter::convert`
    /// does — so what this computes is what the caller gets when a kernel declines.
    fn scalar_convert<S: RowSample>(
        conv: &RowConversion<'_>,
        row: &[u8],
        target: &mut [u8],
    ) -> Result<(), Error> {
        match (conv.color_type, conv.bit_depth) {
            (ColorType::Grayscale, BitDepth::One) => {
                transform::grey_row::<4, 1, S>(row, target, conv.grey_key)
            }
            (ColorType::Grayscale, BitDepth::Two) => {
                transform::grey_row::<4, 2, S>(row, target, conv.grey_key)
            }
            (ColorType::Grayscale, BitDepth::Four) => {
                transform::grey_row::<4, 4, S>(row, target, conv.grey_key)
            }
            (ColorType::Grayscale, BitDepth::Eight) => {
                transform::grey_row::<4, 8, S>(row, target, conv.grey_key)
            }
            (ColorType::Grayscale, BitDepth::Sixteen) => {
                transform::grey_row::<4, 16, S>(row, target, conv.grey_key)
            }
            (ColorType::GrayscaleAlpha, BitDepth::Eight) => {
                transform::grey_alpha_row::<4, false, S>(row, target)
            }
            (ColorType::GrayscaleAlpha, BitDepth::Sixteen) => {
                transform::grey_alpha_row::<4, true, S>(row, target)
            }
            (ColorType::Rgb, BitDepth::Eight) => {
                transform::rgb_row::<4, false, S>(row, target, conv.rgb_key)
            }
            (ColorType::Rgb, BitDepth::Sixteen) => {
                transform::rgb_row::<4, true, S>(row, target, conv.rgb_key)
            }
            (ColorType::Rgba, BitDepth::Eight) => transform::rgba_row::<4, false, S>(row, target),
            (ColorType::Rgba, BitDepth::Sixteen) => transform::rgba_row::<4, true, S>(row, target),
            (ColorType::Indexed, depth) => {
                let palette = conv.palette.expect("an indexed row needs a palette");
                match depth {
                    BitDepth::One => transform::indexed_row::<4, 1, S>(row, target, palette)?,
                    BitDepth::Two => transform::indexed_row::<4, 2, S>(row, target, palette)?,
                    BitDepth::Four => transform::indexed_row::<4, 4, S>(row, target, palette)?,
                    BitDepth::Eight => transform::indexed_row::<4, 8, S>(row, target, palette)?,
                    BitDepth::Sixteen => unreachable!(),
                }
            }
            _ => unreachable!("a shape the tests never build"),
        }
        Ok(())
    }

    /// The scalar reference for a row, picking the sample width from `conv`.
    fn scalar(conv: &RowConversion<'_>, row: &[u8], target: &mut [u8]) -> Result<(), Error> {
        if conv.wide {
            scalar_convert::<u16>(conv, row, target)
        } else {
            scalar_convert::<u8>(conv, row, target)
        }
    }

    /// One native-layout row of `width` pixels — deterministic garbage, seeded per shape.
    /// Sub-byte depths pack MSB-first, so random bytes are already a valid row.
    fn native_row(color: ColorType, depth: BitDepth, width: usize, seed: u64) -> Vec<u8> {
        let pixels_per_byte = match depth {
            BitDepth::One => 8,
            BitDepth::Two => 4,
            BitDepth::Four => 2,
            _ => 1,
        };
        let channels = match color {
            ColorType::Grayscale | ColorType::Indexed => 1,
            ColorType::GrayscaleAlpha => 2,
            ColorType::Rgb => 3,
            ColorType::Rgba => 4,
        };
        let bytes = match depth {
            BitDepth::Sixteen => width * channels * 2,
            _ => (width * channels).div_ceil(pixels_per_byte),
        };
        corpus(bytes, seed)
    }

    /// The facade's routes in the facade's order - the portable paths, then the conversion
    /// kernels on the best backend behind the shuffle gate - minus only the scalar override,
    /// so that `PSD_PNG_FORCE_SCALAR=1` cannot turn a kernel test into a test of the fallback.
    fn accelerated(conv: &RowConversion<'_>, row: &[u8], target: &mut [u8]) -> Result<bool, Error> {
        if conv.color_type == ColorType::Indexed {
            let Some(palette) = conv.palette else { return Ok(false) };
            return indexed(conv, row, target, palette);
        }
        if conv.color_type == ColorType::Rgb {
            return rgb_to_rgba(conv, row, target);
        }
        if !shuffles_available() {
            return Ok(false);
        }
        Ok(kernels::convert_row(conv, row, target))
    }

    /// The byte-level conversion kernels on `row`, once per backend this machine can run, as
    /// `(backend, claimed, target)` over a `0x55`-filled target of `target_len` bytes.
    ///
    /// Whether the facade would hand a backend the row is a policy (`shuffles_available`);
    /// what a kernel produces once it has the row is its own contract, and is checked on
    /// every backend, the bare-SSE2 one the facade declines included.
    fn kernel_runs(
        conv: &RowConversion<'_>,
        row: &[u8],
        target_len: usize,
    ) -> Vec<(&'static str, bool, Vec<u8>)> {
        let runs: Vec<(&'static str, (bool, Vec<u8>))> = on_each_backend!(|simd| {
            let mut target = vec![0x55u8; target_len];
            let claimed = kernels::convert_row_with(simd, conv, row, &mut target);
            (claimed, target)
        });
        runs.into_iter().map(|(backend, (claimed, target))| (backend, claimed, target)).collect()
    }

    /// Asserts a portable route (`Rgb` → `Rgba`, indexed) claims the row and matches the
    /// scalar helpers.
    fn assert_parity(conv: &RowConversion<'_>, row: &[u8], width: usize, ctx: &str) {
        let out = 4 * usize::from(conv.wide) + 4;
        let mut want = vec![0xCCu8; width * out];
        scalar(conv, row, &mut want).unwrap();

        let mut got = vec![0x55u8; width * out];
        let claimed = accelerated(conv, row, &mut got).unwrap();
        assert!(claimed, "{ctx}: the accelerated path declined a covered shape");
        assert_eq!(got, want, "{ctx}: accelerated and scalar disagree");
    }

    /// Asserts the conversion kernels match the scalar helpers byte for byte on every
    /// backend, and that the facade's own route claims the row exactly when this machine's
    /// backend has the shuffle the kernels need - declining, on one that does not, with the
    /// target untouched.
    fn assert_kernel_parity(conv: &RowConversion<'_>, row: &[u8], width: usize, ctx: &str) {
        let out = 4 * usize::from(conv.wide) + 4;
        let mut want = vec![0xCCu8; width * out];
        scalar(conv, row, &mut want).unwrap();

        for (backend, claimed, got) in kernel_runs(conv, row, width * out) {
            assert!(claimed, "{ctx}: {backend} declined a covered shape");
            assert_eq!(got, want, "{ctx}: {backend} and scalar disagree");
        }

        let mut got = vec![0x55u8; width * out];
        let claimed = accelerated(conv, row, &mut got).unwrap();
        assert_eq!(claimed, kernels_expected(), "{ctx}: the route's claim must follow the backend");
        if claimed {
            assert_eq!(got, want, "{ctx}: accelerated and scalar disagree");
        } else {
            assert_eq!(got, vec![0x55u8; width * out], "{ctx}: a declined row must be untouched");
        }
    }

    /// Asserts the facade's route and every backend's kernel decline, leaving the target
    /// untouched.
    fn assert_declined(conv: &RowConversion<'_>, row: &[u8], width: usize, ctx: &str) {
        let out = 4 * usize::from(conv.wide) + 4;
        let before = vec![0x55u8; width * out];

        let mut got = before.clone();
        let claimed = accelerated(conv, row, &mut got).unwrap();
        assert!(!claimed, "{ctx}: an uncovered shape was claimed");
        assert_eq!(got, before, "{ctx}: a declined row must be untouched");

        for (backend, claimed, got) in kernel_runs(conv, row, width * out) {
            assert!(!claimed, "{ctx}: {backend} claimed an uncovered shape");
            assert_eq!(got, before, "{ctx}: {backend}: a declined row must be untouched");
        }
    }

    /// The widths a parity check should cover: tails of every length below the block
    /// size, a boundary or two, and enough to span the chunked sub-byte path twice.
    fn widths() -> [usize; 11] {
        [1, 2, 3, 7, 8, 9, 15, 16, 17, 63, 1100]
    }

    /// Greyscale rows at every covered depth and output width, with no `tRNS` key and
    /// with keys that do and do not appear in the row — including one above the byte
    /// range, which can never match a depth-8 sample. The depth-8 → 16-bit target is
    /// not covered: measured to lose to the autovectorised scalar loop, it declines.
    #[test]
    fn greyscale_conversion_matches_scalar() {
        for depth in [BitDepth::Eight, BitDepth::Sixteen] {
            for wide in [false, true] {
                if depth == BitDepth::Eight && wide {
                    continue;
                }
                for key in [None, Some(0x5A5A), Some(0x1234), Some(0xFFFF)] {
                    let mut spec = conv(ColorType::Grayscale, depth, wide, None);
                    spec.grey_key = key;
                    for width in widths() {
                        let row =
                            native_row(ColorType::Grayscale, depth, width, 0xA511 ^ width as u64);
                        assert_kernel_parity(
                            &spec,
                            &row,
                            width,
                            &format!("{depth:?} wide={wide} key={key:?} w={width}"),
                        );
                    }
                }
            }
        }
        let spec = conv(ColorType::Grayscale, BitDepth::Eight, true, None);
        let row = native_row(ColorType::Grayscale, BitDepth::Eight, 64, 0xA511);
        assert_declined(&spec, &row, 64, "gray8 → rgba16 declines");
    }

    /// A greyscale row that matches `key` at every fifth pixel, so every lane of every
    /// register block sees both a match and a miss.
    ///
    /// Random rows almost never contain a given 16-bit key, so parity over them alone leaves
    /// the keyed compare - and the alpha gather behind it - untested. (It did: the 16-bit
    /// to 8-bit kernel once gathered the wrong mask byte and made the wrong pixels
    /// transparent, and every random-row parity test passed.)
    fn keyed_grey_row(depth: BitDepth, width: usize, key: u16, seed: u64) -> Vec<u8> {
        let mut row = native_row(ColorType::Grayscale, depth, width, seed);
        for x in (2..width).step_by(5) {
            match depth {
                BitDepth::Eight => row[x] = key as u8,
                BitDepth::Sixteen => row[2 * x..2 * x + 2].copy_from_slice(&key.to_be_bytes()),
                _ => unreachable!("only the byte-level depths have kernels"),
            }
        }
        row
    }

    /// Greyscale rows in which the `tRNS` key really occurs, at every width and both output
    /// widths. The keys include ones whose byte swap differs from themselves, since the
    /// 16-bit compare runs on native lanes against a swapped key.
    #[test]
    fn greyscale_keyed_conversion_matches_scalar() {
        let keys8 = [0x00u16, 0x5A, 0xFF];
        let keys16 = [0x0000u16, 0x0100, 0x1234, 0x5A5A, 0xFFFF];
        for (depth, wide, keys) in [
            (BitDepth::Eight, false, &keys8[..]),
            (BitDepth::Sixteen, false, &keys16[..]),
            (BitDepth::Sixteen, true, &keys16[..]),
        ] {
            for &key in keys {
                let mut spec = conv(ColorType::Grayscale, depth, wide, None);
                spec.grey_key = Some(key);
                for width in widths() {
                    let row = keyed_grey_row(depth, width, key, 0x7E57 ^ width as u64);
                    assert_kernel_parity(
                        &spec,
                        &row,
                        width,
                        &format!("{depth:?} wide={wide} key={key:#06x} w={width}"),
                    );
                }
            }
        }
    }

    /// Sub-byte greyscale rows decline at every depth and output width: measured against
    /// the scalar helpers, the chunked unpack plus byte kernels loses to the
    /// autovectorised scalar loop, keyed or not.
    #[test]
    fn subbyte_greyscale_declines() {
        for depth in [BitDepth::One, BitDepth::Two, BitDepth::Four] {
            for wide in [false, true] {
                for key in [None, Some(1)] {
                    let mut spec = conv(ColorType::Grayscale, depth, wide, None);
                    spec.grey_key = key;
                    let row = native_row(ColorType::Grayscale, depth, 64, 0xBADC_0FFE);
                    assert_declined(&spec, &row, 64, &format!("{depth:?} wide={wide} key={key:?}"));
                }
            }
        }
    }

    /// Greyscale-alpha rows at both depths and both output widths.
    #[test]
    fn greyscale_alpha_conversion_matches_scalar() {
        for depth in [BitDepth::Eight, BitDepth::Sixteen] {
            for wide in [false, true] {
                let spec = conv(ColorType::GrayscaleAlpha, depth, wide, None);
                for width in widths() {
                    let row = native_row(
                        ColorType::GrayscaleAlpha,
                        depth,
                        width,
                        0xC0DE ^ (width as u64) << 3,
                    );
                    assert_kernel_parity(
                        &spec,
                        &row,
                        width,
                        &format!("{depth:?} wide={wide} w={width}"),
                    );
                }
            }
        }
    }

    /// The RGBA width conversions the kernels claim — 8→16 and 16→8. The equal-width
    /// cases never reach conversion at all (`passes_through` in transform.rs).
    #[test]
    fn rgba_width_conversion_matches_scalar() {
        for (depth, wide) in [(BitDepth::Eight, true), (BitDepth::Sixteen, false)] {
            let spec = conv(ColorType::Rgba, depth, wide, None);
            for width in widths() {
                let row = native_row(ColorType::Rgba, depth, width, 0xD00D ^ width as u64);
                assert_kernel_parity(
                    &spec,
                    &row,
                    width,
                    &format!("{depth:?} wide={wide} w={width}"),
                );
            }
        }
    }

    /// The same-width RGBA cases decline — they are pass-throughs, not conversions —
    /// and so does every three-channel target, which has no kernel.
    #[test]
    fn uncovered_shapes_decline() {
        let width = 32;
        for (color, depth, wide) in [
            (ColorType::Rgba, BitDepth::Eight, false),
            (ColorType::Rgba, BitDepth::Sixteen, true),
            (ColorType::Grayscale, BitDepth::Eight, false),
        ] {
            let mut spec = conv(color, depth, wide, None);
            spec.channels = 3;
            let row = native_row(color, depth, width, 0xABCD);
            let out = 3 * usize::from(wide) + 3;
            let mut got = vec![0x55u8; width * out];
            let before = got.clone();
            let claimed = accelerated(&spec, &row, &mut got).unwrap();
            assert!(!claimed, "{color:?}/{depth:?}: a 3-channel shape must decline");
            assert_eq!(got, before, "a declined row must be untouched");
        }
        // A keyed RGB row declines: the per-pixel colour compare is scalar work.
        let mut spec = conv(ColorType::Rgb, BitDepth::Eight, false, None);
        spec.rgb_key = Some([0x11, 0x22, 0x33]);
        let row = native_row(ColorType::Rgb, BitDepth::Eight, width, 0xFACE);
        assert_declined(&spec, &row, width, "keyed RGB must decline");
    }

    /// A route that walks pixel-sized chunks would leave a ragged tail unwritten yet report
    /// the row converted, so a target that is not a whole number of pixels, and a channel
    /// count the indexed emitters are not instantiated for, decline instead.
    #[test]
    fn ragged_and_unknown_shapes_decline_untouched() {
        let plte = vec![10u8, 20, 30, 40, 50, 60];
        let palette = Palette::new(&plte, None);
        let indexed_spec = conv(ColorType::Indexed, BitDepth::Eight, false, Some(&palette));
        let rgb_8_to_16 = conv(ColorType::Rgb, BitDepth::Eight, true, None);
        let rgb_16_to_8 = conv(ColorType::Rgb, BitDepth::Sixteen, false, None);
        let mut five_channels = conv(ColorType::Indexed, BitDepth::Eight, false, Some(&palette));
        five_channels.channels = 5;

        // Three pixels' worth of row, and a target one byte longer than three pixels.
        for (spec, row_len, target_len, what) in [
            (&indexed_spec, 3, 13, "indexed, ragged target"),
            (&rgb_8_to_16, 9, 25, "rgb8 -> rgba16, ragged target"),
            (&rgb_16_to_8, 18, 13, "rgb16 -> rgba8, ragged target"),
            (&five_channels, 3, 15, "indexed, five channels"),
        ] {
            let row = vec![0u8; row_len];
            let mut target = vec![0x55u8; target_len];
            let claimed = accelerated(spec, &row, &mut target).unwrap();
            assert!(!claimed, "{what}: must decline");
            assert_eq!(target, vec![0x55u8; target_len], "{what}: a declined row is untouched");
        }
    }

    /// `Rgb` → `Rgba`, keyless: the widening shapes are claimed and pinned to the scalar
    /// helpers; the equal-width ones decline — measured to lose to the autovectorised
    /// scalar loop.
    #[test]
    fn rgb_to_rgba_matches_scalar() {
        for (depth, wide) in [(BitDepth::Eight, true), (BitDepth::Sixteen, false)] {
            let spec = conv(ColorType::Rgb, depth, wide, None);
            for width in widths() {
                let row = native_row(ColorType::Rgb, depth, width, 0xBEEF ^ width as u64);
                assert_parity(&spec, &row, width, &format!("{depth:?} wide={wide} w={width}"));
            }
        }
        for (depth, wide) in [(BitDepth::Eight, false), (BitDepth::Sixteen, true)] {
            let spec = conv(ColorType::Rgb, depth, wide, None);
            let row = native_row(ColorType::Rgb, depth, 64, 0xFACE);
            assert_declined(&spec, &row, 64, &format!("{depth:?} wide={wide}"));
        }
    }

    /// The indexed path: a full palette and a short one, with and without `tRNS`, at
    /// every legal depth — and, since the palette copy is portable scalar, on every
    /// architecture.
    #[test]
    fn indexed_conversion_matches_scalar() {
        // 256 entries of deterministic colour, and a 4-entry palette for the sub-byte
        // depths.
        let mut plte = Vec::new();
        for i in 0..256u32 {
            plte.extend_from_slice(&[(i as u8).wrapping_mul(3), (i as u8) >> 1, 255 - i as u8]);
        }
        let trns: Vec<u8> = (0..256u32).map(|i| (i as u8).wrapping_mul(37)).collect();
        let short_plte = plte[..12].to_vec();
        for (plte, trns, name) in [
            (plte.clone(), None, "full"),
            (plte.clone(), Some(trns.as_slice()), "full+trns"),
            (short_plte.clone(), None, "short"),
            (short_plte, Some(&[64, 128][..]), "short+trns"),
        ] {
            let palette = Palette::new(&plte, trns);
            for depth in [BitDepth::One, BitDepth::Two, BitDepth::Four, BitDepth::Eight] {
                for wide in [false, true] {
                    let spec = conv(ColorType::Indexed, depth, wide, Some(&palette));
                    // The sub-byte depths are claimed only for 16-bit output; the 8-bit
                    // sub-byte path measured slower than the scalar loop and declined.
                    let claimed = depth == BitDepth::Eight || wide;
                    for width in [1usize, 5, 16, 17, 40, 600] {
                        let mut row =
                            native_row(ColorType::Indexed, depth, width, 0xC0FF ^ width as u64);
                        if palette.len < 256 && depth == BitDepth::Eight {
                            for b in &mut row {
                                *b &= (palette.len - 1) as u8;
                            }
                        }
                        let out = 4 * usize::from(wide) + 4;
                        let mut want = vec![0u8; width * out];
                        match scalar(&spec, &row, &mut want) {
                            Ok(()) => {
                                let mut got = vec![0u8; width * out];
                                if claimed {
                                    assert!(
                                        accelerated(&spec, &row, &mut got).unwrap(),
                                        "{name} {depth:?} wide={wide} w={width}: declined"
                                    );
                                    assert_eq!(got, want, "{name} {depth:?} w={width}: disagree");
                                } else {
                                    assert!(
                                        !accelerated(&spec, &row, &mut got).unwrap(),
                                        "{name} {depth:?} wide={wide} w={width}: claimed"
                                    );
                                }
                            }
                            Err(_) => {
                                let mut got = vec![0u8; width * out];
                                let outcome = accelerated(&spec, &row, &mut got);
                                if claimed {
                                    assert!(
                                        outcome.is_err(),
                                        "{name} {depth:?}: out-of-range must error too"
                                    );
                                } else {
                                    assert!(
                                        !outcome.unwrap(),
                                        "{name} {depth:?}: a declined row must decline"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// An out-of-range index must produce the same error the scalar path does — and a
    /// short palette with a tall index row is exactly that case.
    #[test]
    fn indexed_out_of_range_errors() {
        let plte = vec![1u8, 2, 3, 4, 5, 6]; // two entries
        let palette = Palette::new(&plte, None);
        let spec = conv(ColorType::Indexed, BitDepth::Eight, false, Some(&palette));
        let row = [0u8, 1, 200, 0];
        let mut got = vec![0u8; 16];
        assert_eq!(accelerated(&spec, &row, &mut got).unwrap_err(), Error::PaletteIndexOutOfRange);
    }

    /// When the scalar override is in force the facade declines everything — kernels and
    /// portable paths alike — and leaves the row to the scalar helpers.
    #[test]
    fn the_override_declines_every_shape() {
        if !scalar_forced() {
            return;
        }
        let plte = vec![1u8, 2, 3];
        let palette = Palette::new(&plte, None);
        for color in [ColorType::Grayscale, ColorType::Rgb, ColorType::Indexed] {
            let palette_ref = if color == ColorType::Indexed { Some(&palette) } else { None };
            let spec = conv(color, BitDepth::Eight, false, palette_ref);
            let row = native_row(color, BitDepth::Eight, 8, 0x1234);
            let mut got = vec![0u8; 32];
            assert!(!convert_row(&spec, &row, &mut got).unwrap());
        }
    }
}
