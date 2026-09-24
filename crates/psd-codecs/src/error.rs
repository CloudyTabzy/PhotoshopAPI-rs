//! Error type for the pure codecs.
//!
//! Kept separate from `psd_core::PsdError` so `psd-codecs` stays dependency-free;
//! the document layer maps these into `PsdError` at the boundary.

use thiserror::Error;

/// Failures from codec operations (corrupt streams, size mismatches).
#[derive(Debug, Error)]
pub enum CodecError {
    /// Malformed PackBits stream (overrun, underrun, or trailing data).
    #[error("invalid packbits stream: {0}")]
    InvalidPackBits(&'static str),

    /// A byte buffer did not divide evenly into the target element size.
    #[error("byte buffer of {len} bytes is not a multiple of element size {elem_size}")]
    TrailingBytes { len: usize, elem_size: usize },

    /// Decompression produced a different byte count than declared.
    #[error("output length mismatch: expected {expected} bytes, produced {actual}")]
    OutputLength { expected: usize, actual: usize },

    /// Raw deflate compression failed.
    #[error("deflate compression failed")]
    Deflate,

    /// zlib decompression failed (bad stream, bad checksum, or short output).
    #[error("inflate decompression failed: {0}")]
    Inflate(&'static str),

    /// Caller passed buffers whose shapes don't line up.
    #[error("invalid codec input: {0}")]
    InvalidInput(&'static str),
}

/// Convenience alias for codec results.
pub type Result<T> = std::result::Result<T, CodecError>;
