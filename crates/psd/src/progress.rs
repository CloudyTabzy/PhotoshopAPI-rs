//! Progress reporting for read/write operations.
//!
//! Upstream's mutex-guarded `ProgressCallback` becomes a plain `FnMut`
//! parameter: the document methods take
//! `&mut dyn FnMut(ProgressEvent)` and emit one event per layer.

/// An event emitted while reading or writing a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressEvent<'a> {
    /// A layer is about to be parsed or serialized.
    Layer {
        /// Layer name (the pascal name; the unicode name is not parsed yet).
        name: &'a str,
        /// Zero-based position in the on-disk record order.
        index: usize,
        /// Total records that will be emitted.
        total: usize,
    },
}

/// Convenience: a callback that ignores every event.
pub fn ignore_progress(_: ProgressEvent<'_>) {}
