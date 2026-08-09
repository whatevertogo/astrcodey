//! 各 HTTP 路由按职责分组的子模块。

use astrcode_core::config::Config;
use astrcode_protocol::events::ClientNotification;
use axum::response::Response;

use super::{HttpState, bad_request_response, internal_error_response};
use crate::config_manager::ConfigUpdateError;

pub(in crate::http) mod config;
pub(in crate::http) mod event_consumers;
pub(in crate::http) mod extensions;
pub(in crate::http) mod lifecycle;
pub(in crate::http) mod models;
pub(in crate::http) mod sessions;

pub(in crate::http) struct ConfigRequestError {
    pub(in crate::http) code: &'static str,
    pub(in crate::http) message: String,
}

impl ConfigRequestError {
    pub(in crate::http) fn new(code: &'static str, message: impl ToString) -> Self {
        Self {
            code,
            message: message.to_string(),
        }
    }
}

pub(in crate::http) struct ConfigUpdateHttpError(Box<Response>);

impl ConfigUpdateHttpError {
    pub(in crate::http) fn into_response(self) -> Response {
        *self.0
    }
}

pub(in crate::http) async fn update_config<T>(
    state: &HttpState,
    update: impl FnOnce(&mut Config) -> Result<T, ConfigRequestError>,
) -> Result<T, ConfigUpdateHttpError> {
    let result = state
        .app
        .runtime()
        .config_manager()
        .update_and_save(update)
        .await
        .map_err(|error| match error {
            ConfigUpdateError::Mutation(error) => {
                ConfigUpdateHttpError(Box::new(bad_request_response(error.code, error.message)))
            },
            ConfigUpdateError::Resolve(error) => {
                ConfigUpdateHttpError(Box::new(bad_request_response("invalid_config", error)))
            },
            ConfigUpdateError::Provider(error) => {
                ConfigUpdateHttpError(Box::new(bad_request_response("invalid_provider", error)))
            },
            ConfigUpdateError::Store(error) => {
                ConfigUpdateHttpError(Box::new(internal_error_response("save_failed", error)))
            },
        })?;
    notify_extensions_config_changed(state).await;
    Ok(result)
}

async fn notify_extensions_config_changed(state: &HttpState) {
    for error in state
        .app
        .runtime()
        .config_manager()
        .notify_extensions_config_changed()
        .await
    {
        tracing::warn!("extension config notify error: {error}");
    }
}

async fn reload_extension_registry(state: &HttpState) -> Vec<String> {
    let errors = state.app.runtime().reload_extensions().await;
    state
        .app
        .event_bus()
        .send_notification(ClientNotification::ExtensionRegistryChanged);
    for error in &errors {
        tracing::warn!("extension reload error: {error}");
    }
    errors
}
