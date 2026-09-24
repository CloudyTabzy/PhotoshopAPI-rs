//! Small shared numeric types (`Core/Struct/PhotoshopTypes.h`).
//!
//! Grows as more of upstream's struct zoo is ported: the fixed-point
//! resolution type used by the DPI image-resource block and the 10-byte color
//! structure shared by legacy effects and adjustment settings.

use crate::error::Result;
use crate::io::BeReader;

/// The PSD specification's 10-byte color structure: a `u16` color-space id
/// followed by four `u16` components whose meaning depends on the space
/// (RGB, HSB, CMYK, Lab, grayscale, ...). Components beyond the space's
/// channel count are stored but unused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RawColor {
    pub color_space: u16,
    pub components: [u16; 4],
}

impl RawColor {
    pub fn read(reader: &mut BeReader) -> Result<Self> {
        Ok(Self {
            color_space: reader.u16()?,
            components: [reader.u16()?, reader.u16()?, reader.u16()?, reader.u16()?],
        })
    }
}

/// A 4-byte fixed-point number: `u16` integer part plus `u16` fraction.
///
/// Both conversions scale the fraction by **65535**, not 65536 — an upstream
/// quirk (`getFloat` / `operator*=` in `PhotoshopTypes.h`) the port keeps so
/// the written bytes are identical to upstream for the same input. The
/// round-trip that matters for the file format (read `(number, fraction)`,
/// write `(number, fraction)`) never leaves that exact representation; only
/// [`from_f32`](Self::from_f32) / [`to_f32`](Self::to_f32) are lossy, and they
/// are lossy upstream too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FixedFloat4 {
    number: u16,
    fraction: u16,
}

impl FixedFloat4 {
    /// Build from the two on-disk halves exactly as stored.
    pub const fn from_parts(number: u16, fraction: u16) -> Self {
        Self { number, fraction }
    }

    /// The two on-disk halves.
    pub const fn parts(self) -> (u16, u16) {
        (self.number, self.fraction)
    }

    /// Build from a float, truncating like upstream (never rounds).
    ///
    /// Out-of-range and non-finite inputs are clamped to the representable
    /// `0.0..=65535.0` range with a warning; upstream casts out of range
    /// (implementation-defined) and logs at error level.
    pub fn from_f32(value: f32) -> Self {
        if !value.is_finite() || !(0.0..=f32::from(u16::MAX)).contains(&value) {
            tracing::warn!("FixedFloat4 input {value} is outside 0..=65535, clamping");
        }
        let clamped = if value.is_nan() {
            0.0
        } else {
            value.clamp(0.0, f32::from(u16::MAX))
        };
        let number = clamped as u16; // truncating, not rounding (upstream parity)
        let fraction = ((clamped - f32::from(number)) * f32::from(u16::MAX)) as u16;
        Self { number, fraction }
    }

    /// Recover the floating-point value (`number + fraction / 65535`).
    pub fn to_f32(self) -> f32 {
        f32::from(self.number) + f32::from(self.fraction) / f32::from(u16::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_round_trip_exactly() {
        let value = FixedFloat4::from_parts(300, 32768);
        assert_eq!(value.parts(), (300, 32768));
        assert_eq!(value.to_f32(), 300.5); // 300 + 32768/65535 rounds to 300.5f32
    }

    #[test]
    fn from_f32_truncates_fraction_like_upstream() {
        // 0.5 * 65535 = 32767.5 -> truncated to 32767.
        assert_eq!(FixedFloat4::from_f32(300.5).parts(), (300, 32767));
        assert_eq!(FixedFloat4::from_f32(72.0).parts(), (72, 0));
        assert_eq!(FixedFloat4::from_f32(0.0).parts(), (0, 0));
    }

    #[test]
    fn out_of_range_inputs_clamp() {
        assert_eq!(FixedFloat4::from_f32(-1.0).parts(), (0, 0));
        assert_eq!(FixedFloat4::from_f32(70_000.0).parts(), (u16::MAX, 0));
        assert_eq!(FixedFloat4::from_f32(f32::NAN).parts(), (0, 0));
    }
}
