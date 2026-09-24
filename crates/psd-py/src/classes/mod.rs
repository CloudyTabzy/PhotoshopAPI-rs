//! Per-depth Python classes.
//!
//! PyO3 classes cannot be generic, so each class is a macro instantiated in
//! `depth8`/`depth16`/`depth32` (see `depth.rs`) against a `Sample` alias.
//! The macro bodies stay thin: behavior lives in the depth-generic
//! `layer_ops`, `text_ops`, and `tree_ops` modules.

#[macro_use]
mod layer;
#[macro_use]
mod image;
#[macro_use]
mod group;
#[macro_use]
mod text;
#[macro_use]
mod smart;
#[macro_use]
mod document;
