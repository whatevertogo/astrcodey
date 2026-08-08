//! Extension lifecycle contract.

use super::{
    ExtensionConfig, ExtensionError, ExtensionManifest, ExtensionStartContext, Registrar,
    StopReason,
};

#[async_trait::async_trait]
pub trait Extension: Send + Sync {
    fn manifest(&self) -> ExtensionManifest;

    fn register(&self, _registrar: &mut Registrar) {}

    async fn start(&self, _ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        Ok(())
    }

    async fn stop(&self, _reason: StopReason) -> Result<(), ExtensionError> {
        Ok(())
    }

    async fn health(&self) -> Result<(), ExtensionError> {
        Ok(())
    }

    async fn on_config_changed(&self, _config: ExtensionConfig) -> Result<(), ExtensionError> {
        Ok(())
    }
}
