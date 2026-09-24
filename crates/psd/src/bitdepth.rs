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

    fn zip_prediction_decode(bytes: &[u8], width: usize, height: usize) -> CodecResult<Vec<Self>> {
        prediction::decode::<u8>(bytes, width, height)
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

    fn zip_prediction_decode(bytes: &[u8], width: usize, height: usize) -> CodecResult<Vec<Self>> {
        prediction::decode::<u16>(bytes, width, height)
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

    fn zip_prediction_decode(bytes: &[u8], width: usize, height: usize) -> CodecResult<Vec<Self>> {
        prediction::decode_f32(bytes, width, height)
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
}
