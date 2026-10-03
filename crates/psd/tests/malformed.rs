//! Malformed-input corpus vendored from psd-webtoon's `errors/` fixtures
//! (MIT, (c) NAVER WEBTOON — see `malformed/README.md`). Each file is a
//! deliberately damaged variant of `original.psd`/`original.psb` with one
//! field patched to an invalid value.
//!
//! The table asserts this port's deliberate stance per case:
//!
//! - `FAIL` files are rejected during the structural read.
//! - `OK` files are tolerated **and preserved**: the malformed scalar is a
//!   raw field we store verbatim (blend key, clipping byte, divider type,
//!   reserved bytes), or the damage sits in the lazily-decoded merged image
//!   which a layered document does not need. psd-webtoon's own suite rejects
//!   several of these; our policy is strict-where-unsafe, verbatim elsewhere.
//!
//! `image-depth-16/32` are not malformed at all — they are valid 16/32-bit
//! documents, and the last test reads them under the matching depth type.

use std::path::{Path, PathBuf};

use psd::LayeredFile;

fn malformed(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/malformed")
        .join(name)
}

#[test]
fn malformed_header_and_record_fields() {
    for (name, expect_ok) in [
        // --- rejected during the structural read ---
        ("file-channel-count-too-large.psd", false),
        ("file-color-mode-invalid.psd", false),
        ("file-empty.psd", false),
        ("file-image-depth-0.psd", false),
        ("file-image-depth-17.psd", false),
        ("file-image-depth-64.psd", false),
        ("file-image-height-300001.psb", false),
        ("file-image-height-30001.psd", false),
        ("file-image-width-300001.psb", false),
        ("file-image-width-30001.psd", false),
        ("file-signature-invalid.psd", false),
        ("file-version-invalid.psd", false),
        ("layer-blend-mode-signature-invalid.psd", false),
        ("layer-channel-compression-invalid.psd", false),
        // --- tolerated and preserved verbatim ---
        // Reserved bytes are ignored by design.
        ("file-reserved-nonzero.psd", true),
        // The merged image is decoded lazily; a layered document never needs
        // it, so a bogus merged-compression byte surfaces only if the merge is
        // actually read.
        ("image-compression-invalid.psd", true),
        // Unknown blend-mode 4cc keys are stored raw and written back.
        ("layer-blend-mode-key-invalid.psd", true),
        // Channel id 3 is a valid fourth color channel; psd-webtoon's "kind"
        // check rejects ids it does not model, ours keeps them.
        ("layer-channel-kind-invalid.psd", true),
        // The clipping byte is stored raw; Photoshop interprets nonzero as
        // clipped.
        ("layer-clipping-invalid.psd", true),
        // Unknown divider types become `SectionDivider::Unknown` — the block
        // is preserved verbatim rather than rejecting the record.
        ("layer-section-divider-invalid.psd", true),
    ] {
        let path = malformed(name);
        let result = LayeredFile::<u8>::read(&path);
        assert_eq!(
            result.is_ok(),
            expect_ok,
            "{name}: {}",
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );
    }
}

/// The "invalid depth" pair are complete, valid 16- and 32-bit documents —
/// only the u8-typed read rejects them. Read under the matching depth.
#[test]
fn deeper_variants_are_valid_documents() {
    assert!(LayeredFile::<u16>::read(malformed("image-depth-16.psd")).is_ok());
    assert!(LayeredFile::<f32>::read(malformed("image-depth-32.psd")).is_ok());
}
