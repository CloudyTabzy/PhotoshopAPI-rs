//! The document-level `Txt2` text cache (`invalidate_text_cache` upstream).
//!
//! Photoshop stores a document-wide `Txt2` block next to the per-layer `TySh`
//! data and treats it as a render-cache token: when it is present Photoshop
//! trusts the cached text internals, and when it is missing Photoshop asks to
//! "Update text layers?" on open and re-renders every text layer from `TySh`.
//! Editing `TySh` while keeping the stale cache can leave edited text
//! invisible until Photoshop refreshes it.
//!
//! Upstream only offers an explicit `invalidate_text_cache()`. Remembering to
//! call it is easy to miss, so this port also detects staleness itself:
//! reading a document fingerprints every layer's text
//! blocks, and writing drops the `Txt2` read from the file when any text layer
//! changed or a text layer was added. Unedited documents keep the cache byte
//! for byte, and a cache the caller replaced or inserted is never touched.

use std::borrow::Cow;
use std::hash::{DefaultHasher, Hash, Hasher};

use psd_core::{AdditionalLayerInfo, TaggedBlockKey};

use super::TYSH;
use crate::{BitDepth, Layer, LayeredFile};

/// Document- (and occasionally layer-) level text engine data.
pub(crate) const TXT2: TaggedBlockKey = TaggedBlockKey::new(*b"Txt2");

/// What the document's text looked like when it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextCacheBaseline {
    /// Fingerprint of the document-level `Txt2` block as read.
    cache: u64,
    /// Per layer (by id): fingerprint of its `TySh`/`Txt2` blocks as read.
    layers: Vec<Option<u64>>,
}

impl TextCacheBaseline {
    /// Capture the baseline of a freshly read document; `None` when it has no
    /// document-level `Txt2` cache to protect.
    pub(crate) fn capture<T: BitDepth>(document: &LayeredFile<T>) -> Option<Self> {
        let cache = document.document_blocks.as_ref()?.get(TXT2)?;
        Some(Self {
            cache: fingerprint(&cache.data),
            layers: document
                .slots()
                .iter()
                .map(|slot| slot.as_ref().and_then(text_fingerprint))
                .collect(),
        })
    }
}

fn fingerprint(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Fingerprint of a layer's text blocks, `None` for layers without any.
fn text_fingerprint<T: BitDepth>(layer: &Layer<T>) -> Option<u64> {
    let mut hasher = DefaultHasher::new();
    let mut any = false;
    for block in layer
        .blocks
        .blocks
        .iter()
        .filter(|block| block.key == TYSH || block.key == TXT2)
    {
        block.key.hash(&mut hasher);
        block.data.hash(&mut hasher);
        any = true;
    }
    any.then(|| hasher.finish())
}

impl<T: BitDepth> LayeredFile<T> {
    /// Remove the document-level `Txt2` text cache so Photoshop offers to
    /// update text layers on open and re-renders them from their `TySh`
    /// data. Returns whether a cache was removed.
    ///
    /// Writing already drops the cache read from the file when text layers
    /// changed (see [`text_cache_is_stale`](Self::text_cache_is_stale)); call
    /// this to force a refresh anyway, e.g. after editing `TySh` bytes by hand.
    pub fn invalidate_text_cache(&mut self) -> bool {
        self.text_cache = None;
        let Some(blocks) = self.document_blocks.as_mut() else {
            return false;
        };
        let before = blocks.blocks.len();
        blocks.blocks.retain(|block| block.key != TXT2);
        blocks.blocks.len() != before
    }

    /// Whether the document-level `Txt2` cache read from the file no longer
    /// matches the text layers: a text layer's `TySh`/`Txt2` bytes changed, a
    /// text layer was added, or one was removed. Writing a stale document
    /// omits the cache.
    pub fn text_cache_is_stale(&self) -> bool {
        let Some(baseline) = &self.text_cache else {
            return false;
        };
        let Some(cache) = self
            .document_blocks
            .as_ref()
            .and_then(|blocks| blocks.get(TXT2))
        else {
            return false;
        };
        if fingerprint(&cache.data) != baseline.cache {
            // The caller replaced the cache; it is theirs to keep consistent.
            return false;
        }
        self.slots().iter().enumerate().any(|(id, slot)| {
            slot.as_ref().and_then(text_fingerprint) != baseline.layers.get(id).copied().flatten()
        })
    }

    /// Keep writing the document-level `Txt2` cache verbatim even if text
    /// layers changed (opt out of the automatic invalidation on write).
    pub fn retain_text_cache(&mut self) {
        self.text_cache = None;
    }

    /// Document-level blocks as they should be written: without the stale
    /// `Txt2` cache when text layers changed since reading. Borrowed verbatim
    /// when fresh; cloned (and filtered) only when the cache must be dropped.
    pub(crate) fn document_blocks_for_output(&self) -> Option<Cow<'_, AdditionalLayerInfo>> {
        if !self.text_cache_is_stale() {
            return self.document_blocks.as_ref().map(Cow::Borrowed);
        }
        let mut blocks = self.document_blocks.clone()?;
        tracing::info!(
            "omitting the stale document-level Txt2 text cache so Photoshop re-renders the \
             edited text layers"
        );
        blocks.blocks.retain(|block| block.key != TXT2);
        Some(Cow::Owned(blocks))
    }
}
