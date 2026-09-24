//! Regenerates the synthetic documents under `fixtures/generated/`.
//!
//! ```text
//! cargo run -p psd --example generate_fixtures
//! ```
//!
//! The Photoshop-saved corpus in `fixtures/documents/` has no adjustment or
//! fill layers. These documents fill that gap: this port's writer produces
//! them, and their settings payloads use the layouts Photoshop writes (see
//! `fixtures/generated/README.md`). The output is deterministic, so
//! regenerating unchanged code leaves the files byte-identical.

mod adjustments;
mod builders;

use std::path::Path;

fn main() -> psd::core::Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/generated");
    adjustments::generate(&root.join("Adjustments"))?;
    Ok(())
}
