//! 各 HTTP 路由按职责分组的子模块。

use astrcode_protocol::events::ClientNotification;

use super::HttpState;

pub(in crate::http) mod config;
pub(in crate::http) mod extensions;
pub(in crate::http) mod lifecycle;
pub(in crate::http) mod models;
pub(in crate::http) mod sessions;

async fn notify_extensions_config_changed(state: &HttpState) {
    for error in state
        .runtime
        .config_manager
        .notify_extensions_config_changed()
        .await
    {
        tracing::warn!("extension config notify error: {error}");
    }
}

async fn reload_extension_registry(state: &HttpState) -> Vec<String> {
    let errors = state.runtime.reload_extensions().await;
    state
        .event_bus
        .send_notification(ClientNotification::ExtensionRegistryChanged);
    for error in &errors {
        tracing::warn!("extension reload error: {error}");
    }
    errors
}
