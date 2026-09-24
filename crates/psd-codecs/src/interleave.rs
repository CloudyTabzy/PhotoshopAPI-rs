//! Planar <-> interleaved channel shuffling.
//!
//! Mirrors upstream `Render/Interleave.h` and `Render/Deinterleave.h`. The
//! variadic-span C++ API becomes slices of slices — the Rust-native shape.
//! Used by the render pipeline (Phase 2); the f32 ZIP-prediction byte
//! de-interleave lives in [`crate::prediction`] and is intentionally separate.

use crate::error::{CodecError, Result};

/// Interleave equally-sized planar channels into one packed buffer
/// (`ch0[0], ch1[0], …, chN[0], ch0[1], …`). Mirrors `Render::interleave`.
pub fn interleave<T: Copy>(channels: &[&[T]]) -> Result<Vec<T>> {
    if channels.is_empty() {
        return Err(CodecError::InvalidInput("no channels to interleave"));
    }
    let len = channels[0].len();
    if channels.iter().any(|c| c.len() != len) {
        return Err(CodecError::InvalidInput(
            "all channels must have the same length to interleave",
        ));
    }
    let width = channels.len();
    // Every slot is overwritten below; the initial value is just a placeholder.
    let mut out = vec![channels[0][0]; len * width];
    for (i, chunk) in out.chunks_exact_mut(width).enumerate() {
        for (dst, channel) in chunk.iter_mut().zip(channels) {
            *dst = channel[i];
        }
    }
    Ok(out)
}

/// Split a packed buffer into `num_channels` planar channels.
/// Mirrors `Render::deinterleave`.
pub fn deinterleave<T: Copy>(packed: &[T], num_channels: usize) -> Result<Vec<Vec<T>>> {
    if num_channels == 0 {
        return Err(CodecError::InvalidInput("channel count must be at least 1"));
    }
    if !packed.len().is_multiple_of(num_channels) {
        return Err(CodecError::InvalidInput(
            "packed length is not a multiple of the channel count",
        ));
    }
    let len = packed.len() / num_channels;
    let mut channels: Vec<Vec<T>> = (0..num_channels).map(|_| Vec::with_capacity(len)).collect();
    for chunk in packed.chunks_exact(num_channels) {
        for (channel, &value) in channels.iter_mut().zip(chunk) {
            channel.push(value);
        }
    }
    Ok(channels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_three_channels() {
        let r = [255u8, 0, 0, 255];
        let g = [0u8, 255, 0, 0];
        let b = [0u8, 0, 255, 0];
        let packed = interleave(&[&r, &g, &b]).unwrap();
        assert_eq!(packed, [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 0, 0]);
        let planar = deinterleave(&packed, 3).unwrap();
        assert_eq!(planar, vec![r.to_vec(), g.to_vec(), b.to_vec()]);
    }

    #[test]
    fn single_channel_is_identity() {
        let data = [1u16, 2, 3];
        let packed = interleave(&[&data]).unwrap();
        assert_eq!(packed, data);
        assert_eq!(deinterleave(&packed, 1).unwrap(), vec![data.to_vec()]);
    }

    #[test]
    fn rejects_bad_shapes() {
        let a = [1u8, 2];
        let b = [1u8, 2, 3];
        assert!(matches!(
            interleave(&[&a, &b]),
            Err(CodecError::InvalidInput(_))
        ));
        assert!(matches!(
            interleave(&[] as &[&[u8]]),
            Err(CodecError::InvalidInput(_))
        ));
        assert!(matches!(
            deinterleave(&a, 0),
            Err(CodecError::InvalidInput(_))
        ));
        assert!(matches!(
            deinterleave(&b, 2),
            Err(CodecError::InvalidInput(_))
        ));
    }
}
