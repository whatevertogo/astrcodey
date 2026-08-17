//! Extension lifecycle contract.

use super::{
    ExtensionConfig, ExtensionError, ExtensionManifest, ExtensionStartContext,
    ExtensionStopContext, Registrar,
};

#[async_trait::async_trait]
pub trait Extension: Send + Sync {
    fn manifest(&self) -> ExtensionManifest;

    fn register(&self, _registrar: &mut Registrar) {}

    /// Validate extension-owned configuration without changing runtime state.
    ///
    /// The host calls this before persisting or publishing a candidate config.
    /// Configuration parsing and invariant checks belong here; resource access
    /// and state mutation belong in [`Extension::start`]. A configuration change
    /// creates a fresh extension generation instead of mutating a published one.
    fn validate_config(&self, _config: &ExtensionConfig) -> Result<(), ExtensionError> {
        Ok(())
    }

    async fn start(&self, _ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        Ok(())
    }

    async fn stop(&self, _ctx: ExtensionStopContext) -> Result<(), ExtensionError> {
        Ok(())
    }

    async fn health(&self) -> Result<(), ExtensionError> {
        Ok(())
    }
}
