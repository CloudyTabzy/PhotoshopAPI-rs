//! Bulk big-endian conversion over typed slices and byte buffers.
//!
//! Mirrors upstream `Core/Endian/` (`endianDecodeBEArray`,
//! `endianEncodeBEArray`, `endianDecodeBEBinaryArray`). The swap is symmetric,
//! so encode/decode share the implementation; the two entry points exist to
//! make call sites read correctly. Scalar loops — LLVM auto-vectorizes these
//! patterns in release; a runtime-dispatched AVX2 module comes later only if
//! benchmarks demand it.

use crate::error::{CodecError, Result};

/// Element types convertible to/from their big-endian representation.
///
/// `SIZE` is the on-disk width; `from_be_bytes`/`write_be_into` convert a
/// single element between a byte buffer and the native value; `swap_be` is the
/// in-place symmetric swap.
pub trait BeConvert: Copy {
    const SIZE: usize;
    fn from_be_bytes(bytes: &[u8]) -> Self;
    fn write_be_into(self, out: &mut [u8]);
    fn swap_be(self) -> Self;

    /// The slice itself as bytes, for the type whose big-endian form is its
    /// memory (`u8`); `None` for every other type. Lets [`be_bytes`] skip a
    /// copy without any unsafe reinterpretation.
    fn as_bytes(_data: &[Self]) -> Option<&[u8]> {
        None
    }
}

macro_rules! impl_be_convert_int {
    ($t:ty) => {
        impl BeConvert for $t {
            const SIZE: usize = std::mem::size_of::<$t>();

            #[inline]
            fn from_be_bytes(bytes: &[u8]) -> Self {
                <$t>::from_be_bytes(bytes.try_into().unwrap())
            }

            #[inline]
            fn write_be_into(self, out: &mut [u8]) {
                out.copy_from_slice(&self.to_be_bytes());
            }

            #[inline]
            fn swap_be(self) -> Self {
                self.swap_bytes()
            }
        }
    };
}

impl_be_convert_int!(u16);
impl_be_convert_int!(i16);
impl_be_convert_int!(u32);
impl_be_convert_int!(i32);
impl_be_convert_int!(u64);
impl_be_convert_int!(i64);

impl BeConvert for u8 {
    const SIZE: usize = 1;

    #[inline]
    fn from_be_bytes(bytes: &[u8]) -> Self {
        bytes[0]
    }

    #[inline]
    fn write_be_into(self, out: &mut [u8]) {
        out[0] = self;
    }

    #[inline]
    fn swap_be(self) -> Self {
        self
    }

    fn as_bytes(data: &[Self]) -> Option<&[u8]> {
        Some(data)
    }
}

impl BeConvert for i8 {
    const SIZE: usize = 1;

    #[inline]
    fn from_be_bytes(bytes: &[u8]) -> Self {
        bytes[0] as i8
    }

    #[inline]
    fn write_be_into(self, out: &mut [u8]) {
        out[0] = self as u8;
    }

    #[inline]
    fn swap_be(self) -> Self {
        self
    }
}

macro_rules! impl_be_convert_float {
    ($t:ty, $bits:ty) => {
        impl BeConvert for $t {
            const SIZE: usize = std::mem::size_of::<$t>();

            #[inline]
            fn from_be_bytes(bytes: &[u8]) -> Self {
                <$t>::from_be_bytes(bytes.try_into().unwrap())
            }

            #[inline]
            fn write_be_into(self, out: &mut [u8]) {
                out.copy_from_slice(&self.to_be_bytes());
            }

            #[inline]
            fn swap_be(self) -> Self {
                <$t>::from_bits(<$bits>::swap_bytes(self.to_bits()))
            }
        }
    };
}

impl_be_convert_float!(f32, u32);
impl_be_convert_float!(f64, u64);

/// In-place big-endian (on-disk) → native. Mirrors `endianDecodeBEArray`.
pub fn decode_be_slice<T: BeConvert>(data: &mut [T]) {
    for value in data.iter_mut() {
        *value = value.swap_be();
    }
}

/// In-place native → big-endian (on-disk). Mirrors `endianEncodeBEArray`;
/// identical to [`decode_be_slice`] because the swap is symmetric.
pub fn encode_be_slice<T: BeConvert>(data: &mut [T]) {
    decode_be_slice(data);
}

/// Big-endian bytes → typed values. Mirrors `endianDecodeBEBinaryArray`;
/// rejects trailing partial elements (upstream errors on this too).
pub fn decode_be_bytes<T: BeConvert>(bytes: &[u8]) -> Result<Vec<T>> {
    if !bytes.len().is_multiple_of(T::SIZE) {
        return Err(CodecError::TrailingBytes {
            len: bytes.len(),
            elem_size: T::SIZE,
        });
    }
    Ok(bytes.chunks_exact(T::SIZE).map(T::from_be_bytes).collect())
}

/// Typed values → big-endian bytes.
pub fn encode_be_bytes<T: BeConvert>(data: &[T]) -> Vec<u8> {
    let mut out = vec![0u8; data.len() * T::SIZE];
    for (value, chunk) in data.iter().zip(out.chunks_exact_mut(T::SIZE)) {
        value.write_be_into(chunk);
    }
    out
}

/// [`encode_be_bytes`] that borrows when the bytes already exist: `u8` samples
/// are their own big-endian form, so an 8-bit channel is not copied to be read.
pub fn be_bytes<T: BeConvert>(data: &[T]) -> std::borrow::Cow<'_, [u8]> {
    match T::as_bytes(data) {
        Some(bytes) => std::borrow::Cow::Borrowed(bytes),
        None => std::borrow::Cow::Owned(encode_be_bytes(data)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors upstream TestEndian.cpp roundtrips (encode → decode → equal)
    /// at 32×32, 2048×2048, and the deliberately uneven 3288×1671 sizes.
    #[test]
    fn round_trips_at_upstream_sizes() {
        for &(w, h) in &[(32usize, 32usize), (2048, 2048), (3288, 1671)] {
            let mut data16: Vec<u16> = (0..w * h).map(|i| (i % 1024) as u16).collect();
            let expected = data16.clone();
            encode_be_slice(&mut data16);
            decode_be_slice(&mut data16);
            assert_eq!(data16, expected, "u16 {w}x{h}");

            let mut data32: Vec<f32> = (0..w * h).map(|i| i as f32 * 0.5).collect();
            let expected = data32.clone();
            encode_be_slice(&mut data32);
            decode_be_slice(&mut data32);
            assert_eq!(data32, expected, "f32 {w}x{h}");
        }
    }

    /// Mirrors upstream TestEndian.cpp binary fixtures:
    /// `00 FF` → 255 (u16), `3F 80 00 00` → 1.0f.
    #[test]
    fn decodes_handcrafted_big_endian_bytes() {
        let be16 = [0x00u8, 0xFF];
        assert_eq!(decode_be_bytes::<u16>(&be16).unwrap(), vec![255u16]);

        let be32 = [0x3Fu8, 0x80, 0x00, 0x00];
        assert_eq!(decode_be_bytes::<f32>(&be32).unwrap(), vec![1.0f32]);
    }

    #[test]
    fn rejects_trailing_partial_elements() {
        // Upstream skips this case on ARM Mac but errors elsewhere.
        assert!(matches!(
            decode_be_bytes::<u16>(&[0x00]),
            Err(CodecError::TrailingBytes {
                len: 1,
                elem_size: 2
            })
        ));
        assert!(matches!(
            decode_be_bytes::<f32>(&[0x00, 0x01]),
            Err(CodecError::TrailingBytes {
                len: 2,
                elem_size: 4
            })
        ));
    }

    #[test]
    fn byte_encode_decode_round_trip() {
        let data = [0x0102u16, 0xFF00, 255];
        let bytes = encode_be_bytes(&data);
        assert_eq!(bytes, [0x01, 0x02, 0xFF, 0x00, 0x00, 0xFF]);
        assert_eq!(decode_be_bytes::<u16>(&bytes).unwrap(), data);

        let floats = [1.0f32, -2.5, f32::MAX];
        assert_eq!(
            decode_be_bytes::<f32>(&encode_be_bytes(&floats)).unwrap(),
            floats
        );
    }

    #[test]
    fn u8_is_noop() {
        let mut data = vec![0u8, 1, 128, 255];
        encode_be_slice(&mut data);
        assert_eq!(data, [0, 1, 128, 255]);
    }
}

#[cfg(test)]
mod be_bytes_tests {
    use super::*;
    use std::borrow::Cow;

    #[test]
    fn eight_bit_samples_are_borrowed_and_wider_ones_are_encoded() {
        let bytes = [1u8, 2, 3];
        assert!(matches!(be_bytes(&bytes), Cow::Borrowed(b) if b == bytes));
        let words = [0x0102u16, 0x0304];
        assert!(matches!(be_bytes(&words), Cow::Owned(ref b) if b == &[1, 2, 3, 4]));
        assert_eq!(be_bytes(&words), encode_be_bytes(&words));
        assert!(be_bytes::<i8>(&[-1]).len() == 1);
    }
}
