//! Readers for the small Adobe formats that travel beside PSD files: brush
//! presets (`.abr`), custom shapes (`.csh`) and swatch palettes (`.ase`).
//!
//! These are a different product surface from PSD support — Photoshop ships
//! them as separate files and no PSD section references them — so they live in
//! their own crate rather than inside [`psd-core`] or [`psd`]. What they share
//! with the PSD crates is vocabulary, and that is exactly what is reused here:
//! `.csh` shapes carry the same 26-byte vector-path records a layer's vector
//! mask does ([`psd_core::vector::VectorPath`]), and `.abr` brush presets are
//! PSD action descriptors ([`psd_core::descriptor::Descriptor`]) read through
//! the same parser the port's adjustment and effect views use.
//!
//! All three readers are read-only and take a byte slice, returning owned data.
//! The reference for the on-disk layouts is the ag-psd port in this workspace
//! (`ag-psd-rs`, itself a port of the battle-tested TypeScript ag-psd); none of
//! the three formats has an official Adobe specification, so where sources
//! disagree the reference's reading of real Photoshop files wins.
//!
//! Deliberately unsupported, each with a named error rather than a guess:
//! `.abr` major versions 1 and 2 (the pre-CS entry-stream layout, for which no
//! fixture or reference exists in this workspace), 16-bit run-length-encoded
//! brush samples, and run-length-encoded indexed patterns.
//!
//! [`psd`]: ../psd/index.html
//! [`psd-core`]: ../psd_core/index.html

mod abr;
mod ase;
mod csh;
mod error;
mod packbits;
mod pattern;

pub use abr::{
    read_abr, Brush, BrushBlendMode, BrushDynamics, BrushPose, BrushShape, ColorDynamics,
    DualBrush, SampleBounds, SampleInfo, Scatter, ShapeDynamics, Texture, ToolOptions, ToolType,
    Transfer,
};
pub use ase::{read_ase, Ase, AseColor, AseColorType, AseColorValue, AseEntry, AseGroup};
pub use csh::{read_csh, Csh, CshShape};
pub use error::{Error, Result};
pub use pattern::{read_pattern, Pattern, PatternBounds};
