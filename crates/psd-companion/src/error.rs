//! The crate's error type.

use psd_core::PsdError;
use thiserror::Error;

/// What went wrong while reading a companion file.
#[derive(Debug, Error)]
pub enum Error {
    /// The bytes do not match the format's structure. The offset is where the
    /// reader stopped, relative to the start of the buffer.
    #[error("{message} at offset {offset}")]
    Invalid { offset: usize, message: String },
    /// A well-formed file this reader deliberately does not support, with the
    /// reason. Versions rejected this way are named so a caller can tell a
    /// corrupt file from an old one.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// A failure surfaced by the shared PSD primitives the readers reuse
    /// (bounds checks, string decoding, descriptor parsing).
    #[error(transparent)]
    Format(#[from] PsdError),
}

/// The result type every reader returns.
pub type Result<T> = std::result::Result<T, Error>;

/// An invalid-structure error at the reader's current position.
pub(crate) fn invalid(reader: &psd_core::io::BeReader<'_>, message: impl Into<String>) -> Error {
    Error::Invalid {
        offset: reader.position(),
        message: message.into(),
    }
}
