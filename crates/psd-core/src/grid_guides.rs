//! The grid and guides resource (`1032`).
//!
//! Photoshop keeps the document grid and every guide in one image resource: a
//! 16-byte header (version, horizontal and vertical grid cycle, guide count)
//! followed by five bytes per guide (a position and a direction).
//!
//! Positions and the grid cycle are in **1/32 of a pixel** — the spec calls
//! them document coordinates and notes the default cycle is "every quarter
//! inch, i.e. 576 for both horizontal & vertical (at 72 dpi, that is
//! 18 * 32 = 576)", which is 18 px at 72 dpi. [`GridGuides::grid_px`] and
//! [`Guide::position_px`] convert for callers that think in pixels.
//!
//! The resource is read on demand from its raw block and written back only
//! when a caller changes it, so an untouched document keeps its own bytes.

use crate::error::{PsdError, Result};
use crate::io::{BeReader, BeWriter};

/// The unit the grid cycle and guide positions are stored in.
pub const UNITS_PER_PIXEL: f64 = 32.0;

/// A guide runs along one axis: a vertical guide sits at an x coordinate, a
/// horizontal one at a y coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuideDirection {
    Vertical,
    Horizontal,
}

impl GuideDirection {
    /// The on-disk byte (`VHSelect`: 0 = vertical, 1 = horizontal).
    pub const fn to_byte(self) -> u8 {
        match self {
            Self::Vertical => 0,
            Self::Horizontal => 1,
        }
    }

    /// The direction a raw byte means, if it is one of the two values.
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Vertical),
            1 => Some(Self::Horizontal),
            _ => None,
        }
    }
}

/// One guide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guide {
    /// Position in 1/32 pixel units, on the axis the direction names.
    pub location: i32,
    pub direction: GuideDirection,
}

impl Guide {
    /// The position in document pixels.
    pub fn position_px(&self) -> f64 {
        f64::from(self.location) / UNITS_PER_PIXEL
    }
}

/// The `1032` resource: the grid cycle and every guide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridGuides {
    /// Resource version; Photoshop writes 1.
    pub version: u32,
    /// Grid cycle in 1/32 pixel units.
    pub horizontal_grid: u32,
    pub vertical_grid: u32,
    pub guides: Vec<Guide>,
}

impl GridGuides {
    /// Parse the resource payload.
    ///
    /// Returns an error when the payload is too short for its own header or
    /// when the guide count does not fit the bytes that follow, so a malformed
    /// block is never read as a plausible-looking short list.
    pub fn read(reader: &mut BeReader) -> Result<Self> {
        let offset = reader.position() as u64;
        let version = reader.u32()?;
        let horizontal_grid = reader.u32()?;
        let vertical_grid = reader.u32()?;
        let count = reader.u32()? as usize;
        if count.saturating_mul(5) > reader.remaining() {
            return Err(PsdError::InvalidData {
                offset,
                message: "grid and guides count exceeds the resource",
            });
        }
        let mut guides = Vec::with_capacity(count);
        for _ in 0..count {
            let location = reader.i32()?;
            let byte = reader.u8()?;
            let direction = GuideDirection::from_byte(byte).ok_or(PsdError::InvalidData {
                offset: reader.position() as u64 - 1,
                message: "guide direction is neither vertical nor horizontal",
            })?;
            guides.push(Guide {
                location,
                direction,
            });
        }
        Ok(Self {
            version,
            horizontal_grid,
            vertical_grid,
            guides,
        })
    }

    /// The grid cycle in document pixels.
    pub fn grid_px(&self) -> (f64, f64) {
        (
            f64::from(self.horizontal_grid) / UNITS_PER_PIXEL,
            f64::from(self.vertical_grid) / UNITS_PER_PIXEL,
        )
    }

    /// Serialize to the resource payload.
    pub fn to_payload(&self) -> Result<Vec<u8>> {
        let count = u32::try_from(self.guides.len()).map_err(|_| PsdError::LengthOverflow {
            actual: self.guides.len() as u64,
            width: 4,
        })?;
        let mut writer = BeWriter::new();
        writer.u32(self.version);
        writer.u32(self.horizontal_grid);
        writer.u32(self.vertical_grid);
        writer.u32(count);
        for guide in &self.guides {
            writer.i32(guide.location);
            writer.u8(guide.direction.to_byte());
        }
        Ok(writer.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> Vec<u8> {
        GridGuides {
            version: 1,
            horizontal_grid: 576,
            vertical_grid: 576,
            guides: vec![
                Guide {
                    location: 32 * 100,
                    direction: GuideDirection::Vertical,
                },
                Guide {
                    location: -32 * 50,
                    direction: GuideDirection::Horizontal,
                },
            ],
        }
        .to_payload()
        .unwrap()
    }

    #[test]
    fn round_trips_and_converts_units() {
        let bytes = payload();
        assert_eq!(bytes.len(), 16 + 2 * 5);
        let parsed = GridGuides::read(&mut BeReader::new(&bytes)).unwrap();
        assert_eq!(parsed.guides.len(), 2);
        assert_eq!(parsed.guides[0].position_px(), 100.0);
        assert_eq!(parsed.guides[1].position_px(), -50.0);
        assert_eq!(parsed.guides[1].direction, GuideDirection::Horizontal);
        assert_eq!(parsed.grid_px(), (18.0, 18.0));
        assert_eq!(parsed.to_payload().unwrap(), bytes);
    }

    #[test]
    fn an_empty_guide_list_is_the_common_case() {
        let bytes = GridGuides {
            version: 1,
            horizontal_grid: 576,
            vertical_grid: 576,
            guides: Vec::new(),
        }
        .to_payload()
        .unwrap();
        assert_eq!(bytes.len(), 16);
        let parsed = GridGuides::read(&mut BeReader::new(&bytes)).unwrap();
        assert!(parsed.guides.is_empty());
    }

    #[test]
    fn a_count_past_the_payload_is_an_error_not_a_short_list() {
        let mut bytes = payload();
        bytes[12..16].copy_from_slice(&9u32.to_be_bytes());
        assert!(GridGuides::read(&mut BeReader::new(&bytes)).is_err());
        // A direction outside the two values is refused too.
        let mut bytes = payload();
        bytes[20] = 7;
        assert!(GridGuides::read(&mut BeReader::new(&bytes)).is_err());
    }
}
