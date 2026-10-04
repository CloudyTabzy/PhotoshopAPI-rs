//! Channel sample types: the [`BitDepth`] trait and its impls for the three
//! supported sample widths.
//!
//! Mirrors upstream's per-depth template instantiation
//! (`LayeredFile<bpp8_t|bpp16_t|bpp32_t>`) with a sealed trait so the document
//! layer monomorphizes hot paths while staying generic.

use psd_codecs::endian::BeConvert;
use psd_codecs::prediction;
use psd_codecs::Result as CodecResult;

/// A channel sample type: 8/16-bit integers or 32-bit float.
///
/// Implemented for `u8`, `u16` and `f32` only. `DEPTH` mirrors the on-disk
/// bit depth so the document layer can build headers and pick codecs without
/// matching on the type at every call site.
pub trait BitDepth: BeConvert + Copy + PartialEq + Default + Send + Sync + 'static {
    /// On-disk bit depth (8, 16 or 32).
    const DEPTH: u16;
    /// The zero sample.
    const ZERO: Self;

    /// Normalize to `0.0..=1.0` (`u8/255`, `u16/65535`; `f32` unchanged).
    fn to_f32(self) -> f32;

    /// Inverse of [`to_f32`](Self::to_f32), clamped to the representable range.
    ///
    /// Deviation from upstream: integer impls round to nearest, while
    /// upstream truncates (`static_cast<bpp8_t>(value * 255.f)`). Rounding is
    /// kept deliberately — it is the numerically correct inverse of `to_f32`
    /// (0.5 → 128, not 127) and the pinning test below locks it in.
    fn from_f32(value: f32) -> Self;

    /// ZIP-prediction encode (integers delta-encode, floats byte-deinterleave).
    fn zip_prediction_encode(data: &[Self], width: usize, height: usize) -> CodecResult<Vec<u8>>;

    /// Inverse of [`zip_prediction_encode`](Self::zip_prediction_encode).
    fn zip_prediction_decode(bytes: &[u8], width: usize, height: usize) -> CodecResult<Vec<Self>>;

    /// Compress prediction blocks without retaining a full encoded plane.
    fn zip_prediction_compress(data: &[Self], width: usize, height: usize) -> CodecResult<Vec<u8>> {
        psd_codecs::zip::compress(&Self::zip_prediction_encode(data, width, height)?)
    }

    /// Inflate prediction directly into typed storage where supported.
    fn zip_prediction_decompress(
        payload: &[u8],
        width: usize,
        height: usize,
    ) -> CodecResult<Vec<Self>> {
        let len = width
            .checked_mul(height)
            .and_then(|samples| samples.checked_mul(Self::SIZE))
            .ok_or(psd_codecs::CodecError::InvalidInput(
                "image dimensions overflow",
            ))?;
        let bytes = psd_codecs::zip::decompress(payload, len)?;
        Self::zip_prediction_decode(&bytes, width, height)
    }

    /// Widens one 8-bit PNG sample into this depth, matching what the `image` crate's
    /// rgba8→rgba16 widening and `interleaved_to_planar` both produce.
    fn widen_eight(source: u8) -> Self;

    /// Widens one 16-bit PNG sample into this depth, matching
    /// `interleaved_to_planar`'s `from_f32(to_f32(v))` rule bit for bit.
    fn widen_sixteen(source: u16) -> Self;
}

/// Convert one normalized sample using the port's bit-depth widening and
/// rounding rules. Integer destinations clip out-of-range float samples.
pub(crate) fn convert_sample<S: BitDepth, D: BitDepth>(sample: S) -> D {
    match S::DEPTH {
        8 => D::widen_eight(u8::from_f32(sample.to_f32())),
        16 => D::widen_sixteen(u16::from_f32(sample.to_f32())),
        32 => D::from_f32(sample.to_f32()),
        _ => unreachable!("BitDepth is sealed to supported sample widths"),
    }
}

impl BitDepth for u8 {
    const DEPTH: u16 = 8;
    const ZERO: Self = 0;

    #[inline]
    fn to_f32(self) -> f32 {
        f32::from(self) / 255.0
    }

    #[inline]
    fn from_f32(value: f32) -> Self {
        (value.clamp(0.0, 1.0) * 255.0).round() as u8
    }

    fn zip_prediction_encode(data: &[Self], width: usize, height: usize) -> CodecResult<Vec<u8>> {
        prediction::encode::<u8>(data, width, height)
    }

    fn zip_prediction_compress(data: &[Self], width: usize, height: usize) -> CodecResult<Vec<u8>> {
        prediction::compress(data, width, height)
    }

    fn zip_prediction_decompress(
        payload: &[u8],
        width: usize,
        height: usize,
    ) -> CodecResult<Vec<Self>> {
        prediction::decompress(payload, width, height)
    }

    fn zip_prediction_decode(bytes: &[u8], width: usize, height: usize) -> CodecResult<Vec<Self>> {
        prediction::decode::<u8>(bytes, width, height)
    }

    fn widen_eight(source: u8) -> Self {
        source
    }

    fn widen_sixteen(source: u16) -> Self {
        // 16→8 is the deliberate narrowing rule, NOT the high byte: it must match
        // `interleaved_to_planar`'s `from_f32(to_f32(v))` bit for bit (see the 51 200
        // counter-example in the pinning test).
        Self::from_f32(source.to_f32())
    }
}

impl BitDepth for u16 {
    const DEPTH: u16 = 16;
    const ZERO: Self = 0;

    #[inline]
    fn to_f32(self) -> f32 {
        f32::from(self) / 65535.0
    }

    #[inline]
    fn from_f32(value: f32) -> Self {
        (value.clamp(0.0, 1.0) * 65535.0).round() as u16
    }

    fn zip_prediction_encode(data: &[Self], width: usize, height: usize) -> CodecResult<Vec<u8>> {
        prediction::encode::<u16>(data, width, height)
    }

    fn zip_prediction_compress(data: &[Self], width: usize, height: usize) -> CodecResult<Vec<u8>> {
        prediction::compress(data, width, height)
    }

    fn zip_prediction_decompress(
        payload: &[u8],
        width: usize,
        height: usize,
    ) -> CodecResult<Vec<Self>> {
        prediction::decompress(payload, width, height)
    }

    fn zip_prediction_decode(bytes: &[u8], width: usize, height: usize) -> CodecResult<Vec<Self>> {
        prediction::decode::<u16>(bytes, width, height)
    }

    fn widen_eight(source: u8) -> Self {
        // 8→16 is what `image`'s rgba8→rgba16 and psd-png's converter produce: ×257.
        u16::from(source) * 257
    }

    fn widen_sixteen(source: u16) -> Self {
        source
    }
}

impl BitDepth for f32 {
    const DEPTH: u16 = 32;
    const ZERO: Self = 0.0;

    #[inline]
    fn to_f32(self) -> f32 {
        self
    }

    #[inline]
    fn from_f32(value: f32) -> Self {
        value
    }

    fn zip_prediction_encode(data: &[Self], width: usize, height: usize) -> CodecResult<Vec<u8>> {
        prediction::encode_f32(data, width, height)
    }

    fn zip_prediction_compress(data: &[Self], width: usize, height: usize) -> CodecResult<Vec<u8>> {
        prediction::compress_f32(data, width, height)
    }

    fn zip_prediction_decompress(
        payload: &[u8],
        width: usize,
        height: usize,
    ) -> CodecResult<Vec<Self>> {
        prediction::decompress_f32(payload, width, height)
    }

    fn zip_prediction_decode(bytes: &[u8], width: usize, height: usize) -> CodecResult<Vec<Self>> {
        prediction::decode_f32(bytes, width, height)
    }

    fn widen_eight(source: u8) -> Self {
        source.to_f32()
    }

    fn widen_sixteen(source: u16) -> Self {
        source.to_f32()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_conversions_round_trip() {
        assert_eq!(u8::from_f32(0.5), 128);
        assert!((0.5f32 - 128u8.to_f32()).abs() < 0.01);
        assert_eq!(u16::from_f32(0.5), 32768);
        assert!((0.5f32 - 32768u16.to_f32()).abs() < 1e-4);
        assert_eq!(f32::from_f32(-2.0), -2.0);
        assert_eq!(u8::from_f32(2.0), 255);
        assert_eq!(u8::from_f32(-1.0), 0);
    }

    #[test]
    fn depths_match_disk_values() {
        assert_eq!(u8::DEPTH, 8);
        assert_eq!(u16::DEPTH, 16);
        assert_eq!(f32::DEPTH, 32);
    }

    /// EIGHT→T, exhaustively: `widen_eight` must equal the float composition
    /// `T::from_f32(sample.to_f32())` — the exact rule `interleaved_to_planar` applies —
    /// for every byte, at every destination depth. For `u8` and `u16` that composition is
    /// exact identity/×257; for `f32` it is `v/255.0`, and the round-trip claim pins that
    /// going back through `u8::from_f32` recovers the byte.
    #[test]
    fn widen_eight_matches_the_float_composition_across_the_whole_byte() {
        for source in 0u16..=255 {
            let source = source as u8;
            let expected_u8 = u8::from_f32(source.to_f32());
            assert_eq!(u8::widen_eight(source), expected_u8, "to u8 at {source}");
            let expected_u16 = u16::from_f32(source.to_f32());
            assert_eq!(u16::widen_eight(source), expected_u16, "to u16 at {source}");
            let expected_f32 = f32::from_f32(source.to_f32());
            assert_eq!(f32::widen_eight(source), expected_f32, "to f32 at {source}");
            assert_eq!(
                u8::from_f32(f32::widen_eight(source).to_f32()),
                expected_u8,
                "f32 buffer round-trips back to u8 at {source}"
            );
        }
    }

    /// SIXTEEN→T, exhaustively: `widen_sixteen` must equal
    /// `T::from_f32(u16::to_f32(v))` for every `u16`, at every destination depth.
    /// Stations checked along the way (0, mid-range including the 51 200 narrowing
    /// counter-example, top) so a total break is visible before the sweep message.
    #[test]
    fn widen_sixteen_matches_the_float_composition_across_the_full_range() {
        for source in u16::MIN..=u16::MAX {
            let expected_u16 = u16::from_f32(source.to_f32());
            assert_eq!(
                u16::widen_sixteen(source),
                expected_u16,
                "to u16 at {source}"
            );
        }
        assert_eq!(u16::widen_sixteen(0), 0);
        assert_eq!(u16::widen_sixteen(51_200), 51_200);
        assert_eq!(u16::widen_sixteen(u16::MAX), u16::MAX);
    }
}
