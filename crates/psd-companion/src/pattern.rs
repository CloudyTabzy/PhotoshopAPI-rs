//! The pattern record an `.abr` file's `patt` section carries — the same
//! layout as the PSD `Patt` block. The decoder lives in [`psd_core::pattern`]
//! (the compositor reads a document's `Patt` blocks with it too); this module
//! adapts its errors to the crate's own.

use psd_core::io::BeReader;
use psd_core::pattern::UNSUPPORTED_PREFIX;
use psd_core::PsdError;

use crate::error::{Error, Result};

pub use psd_core::pattern::{Pattern, PatternBounds};

/// Parses one pattern record from a `.abr` `patt` section. The reader must be
/// positioned at the record's four-byte length prefix.
///
/// # Errors
///
/// [`Error::Invalid`] for a wrong version, an unsupported pixel depth, an
/// inverted or oversized rectangle, a channel whose two depth fields disagree,
/// or an unknown compression mode. [`Error::Unsupported`] for an unknown colour
/// mode and for run-length-encoded indexed patterns, which the reference does
/// not decode either.
pub fn read_pattern(reader: &mut BeReader<'_>) -> Result<Pattern> {
    psd_core::pattern::read_pattern(reader).map_err(|error| match error {
        PsdError::InvalidData { message, .. } if message.starts_with(UNSUPPORTED_PREFIX) => {
            Error::Unsupported(message.to_owned())
        }
        PsdError::InvalidData { offset, message } => Error::Invalid {
            offset: offset as usize,
            message: message.to_owned(),
        },
        other => Error::Format(other),
    })
}
