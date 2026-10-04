//! Controls for lossless serialization.

pub use psd_codecs::zip::CompressionPolicy;

/// Lossless encoding policy and the estimated concurrent codec workspace.
/// Canonical document pixels and the returned `to_bytes` buffer are separate.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct WriteOptions {
    pub compression_policy: CompressionPolicy,
    /// Zero requests sequential work. An individual oversized job runs alone.
    pub working_memory_limit: usize,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            compression_policy: CompressionPolicy::Balanced,
            working_memory_limit: 256 * 1024 * 1024,
        }
    }
}

impl WriteOptions {
    pub fn with_compression_policy(mut self, policy: CompressionPolicy) -> Self {
        self.compression_policy = policy;
        self
    }
    pub fn with_working_memory_limit(mut self, bytes: usize) -> Self {
        self.working_memory_limit = bytes;
        self
    }
}
