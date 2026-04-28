//! Configuration system for astrcode.
//!
//! # Architecture
//!
//! All config knowledge lives in this module:
//! - `raw.rs` — types from disk (all fields optional)
//! - `effective.rs` — resolved types (all fields concrete)
//! - `resolve.rs` — `Config::into_effective()` + `resolve_api_key()` + `merge_overlay()`
//! - `defaults.rs` — all default constants
//!
//! Adding a new config field touches 3 files in this one directory:
//! `raw.rs` (add Option field) → `effective.rs` (add concrete field)
//! → `resolve.rs` (add mapping line).
//!
//! # Usage
//!
//! ```ignore
//! let raw: Config = serde_json::from_str(&json)?;
//! let effective = raw.into_effective()?;
//! let provider = OpenAiProvider::new(
//!     LlmClientConfig { base_url: effective.llm.base_url, api_key: effective.llm.api_key, ... },
//!     effective.llm.api_mode,
//!     effective.llm.model_id,
//!     Some(effective.llm.max_tokens),
//!     Some(effective.llm.context_limit),
//! );
//! ```

pub mod defaults;
pub mod effective;
pub mod raw;
pub mod resolve;

// Re-export commonly used types at the config module level
pub use effective::*;
pub use raw::*;
pub use resolve::{ResolveError, merge_overlay, resolve_api_key};

// ─── ConfigStore trait (IO abstraction) ──────────────────────────────────

/// Trait for configuration persistence.
///
/// Implementations (e.g., FileConfigStore) handle the IO layer.
/// ConfigService in the server crate uses this to load raw config,
/// then calls `Config::into_effective()` for resolution.
#[async_trait::async_trait]
pub trait ConfigStore: Send + Sync {
    /// Load the full configuration.
    async fn load(&self) -> Result<Config, ConfigStoreError>;

    /// Save configuration (atomic write).
    async fn save(&self, config: &Config) -> Result<(), ConfigStoreError>;

    /// Return the config file path.
    fn path(&self) -> std::path::PathBuf;

    /// Load a project overlay config.
    async fn load_overlay(
        &self,
        working_dir: &str,
    ) -> Result<Option<ConfigOverlay>, ConfigStoreError>;

    /// Save a project overlay config.
    async fn save_overlay(
        &self,
        working_dir: &str,
        overlay: &ConfigOverlay,
    ) -> Result<(), ConfigStoreError>;
}

/// Error from configuration operations.
#[derive(Debug, thiserror::Error)]
pub enum ConfigStoreError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Invalid config: {0}")]
    Invalid(String),
    #[error("Missing required field: {0}")]
    MissingField(String),
}
