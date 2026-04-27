//! astrcode-core: Shared types, traits, and data models for the astrcode platform.
//!
//! This crate is the foundation layer — it defines all public interfaces
//! that other crates implement or consume. No business logic lives here.

pub mod config;
pub mod event;
pub mod extension;
pub mod llm;
pub mod prompt;
pub mod storage;
pub mod tool;
pub mod types;

// Re-export commonly used types
pub use config::*;
pub use event::*;
pub use extension::*;
pub use llm::*;
pub use prompt::*;
pub use storage::*;
pub use tool::*;
pub use types::*;
