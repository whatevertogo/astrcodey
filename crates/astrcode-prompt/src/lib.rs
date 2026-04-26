//! astrcode-prompt: Prompt composition pipeline.
//!
//! Contributor pattern with 4-layer caching, template engine,
//! topological sort, and diagnostics.

pub mod composer;
pub mod contributors;
pub mod layer;
pub mod template;
