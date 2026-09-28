//! The single error type for the whole workspace.
//!
//! Upstream C++ logs at Error severity and then throws `std::runtime_error`;
//! here every failure is a typed variant and recoverable anomalies are
//! `tracing::warn!` + continue instead.

use thiserror::Error;

/// Errors produced while parsing or writing PSD/PSB files.
#[derive(Debug, Error)]
pub enum PsdError {
    /// A hard-validated 4-byte signature did not match (`8BPS`/`8BIM`/`8B64`).
    #[error("invalid signature at offset {offset}: expected {expected}, found {found:?}")]
    InvalidSignature {
        expected: &'static str,
        found: [u8; 4],
        offset: u64,
    },

    /// The reader ran past the end of the input.
    #[error("unexpected end of input at offset {offset}")]
    UnexpectedEof { offset: u64 },

    /// File header declared a version other than 1 (PSD) or 2 (PSB).
    #[error("unsupported file version: {0}")]
    UnsupportedVersion(u16),

    /// The header declared a bit depth the port does not handle (8/16/32 only).
    /// One-bit documents are the common case: bitmap-mode files pack eight
    /// pixels per byte, a layout this port has no sample type for.
    #[error(
        "unsupported bit depth: {0}; this port reads 8-, 16- and 32-bit documents          (1-bit bitmap mode packs pixels per byte and is not supported)"
    )]
    UnsupportedBitDepth(u16),

    /// The header declared a color mode outside the known on-disk values.
    #[error("unsupported color mode: {0}")]
    UnsupportedColorMode(u16),

    /// A layer or image data section declared an unknown compression value.
    #[error("unsupported compression: {0}")]
    UnsupportedCompression(u16),

    /// Structurally invalid data at a known offset.
    #[error("invalid data at offset {offset}: {message}")]
    InvalidData { offset: u64, message: &'static str },

    /// A section did not fit its length marker (e.g. PSB-sized data in a PSD file).
    #[error("length overflow: {actual} bytes do not fit a {width}-byte length marker")]
    LengthOverflow { actual: u64, width: u8 },

    /// Codec (de)compression failure, e.g. corrupt PackBits or deflate stream.
    #[error("compression error: {0}")]
    Compression(String),

    /// Decoding another layer channel would exceed the configured cumulative
    /// bitmap memory budget.
    #[error(
        "decoded bitmap memory limit exceeded: requested {requested} bytes, {available} bytes remain"
    )]
    ExceededMemoryLimit { requested: usize, available: usize },

    /// A layer or mask rectangle is inverted, exceeds the PSD/PSB extent limit,
    /// or cannot be represented as an addressable bitmap.
    #[error("invalid {kind} bounds: {width}x{height}")]
    InvalidImageBounds {
        kind: &'static str,
        width: i64,
        height: i64,
    },

    /// A linked image source was unsupported or could not be decoded.
    #[error("smart object image decode failed: {0}")]
    ImageDecode(String),

    /// Underlying filesystem failure (mmap, open, read, write).
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Convenience alias used throughout the workspace.
pub type Result<T> = std::result::Result<T, PsdError>;
