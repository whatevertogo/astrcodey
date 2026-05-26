//! 扩展查看 / 重载 / 启停路由。

use std::collections::{BTreeMap, BTreeSet};

use astrcode_protocol::{
    events::ClientNotification,
    http::{
        ExtensionListResponseDto, ExtensionReloadResponseDto, ExtensionStateDto,
        SetExtensionEnabledRequest, SetExtensionEnabledResponseDto,
    },
};
use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};

use super::super::{HttpState, error_response};

pub(in crate::http) async fn list_extensions(State(state): State<HttpState>) -> Response {
    Json(ExtensionListResponseDto {
        extensions: collect_extensions(&state).await,
    })
    .into_response()
}

pub(in crate::http) async fn reload_extensions(State(state): State<HttpState>) -> Response {
    let reload_errors = state.runtime.reload_extensions().await;
    state
        .event_bus
        .send_notification(ClientNotification::ExtensionRegistryChanged);
    for error in &reload_errors {
        tracing::warn!("extension reload error: {error}");
    }
    Json(ExtensionReloadResponseDto { reload_errors }).into_response()
}

pub(in crate::http) async fn set_enabled(
    State(state): State<HttpState>,
    Json(request): Json<SetExtensionEnabledRequest>,
) -> Response {
    let mut candidate = state.runtime.config_manager().raw_config_snapshot();
    let extension_states = candidate
        .runtime
        .extension_states
        .get_or_insert_with(BTreeMap::new);
    extension_states.insert(request.extension_id.clone(), request.enabled);

    if let Err(error) = candidate.clone().into_effective() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_extension_state",
            error.to_string(),
        );
    }

    if let Err(error) = state
        .runtime
        .config_manager
        .config_store()
        .save(&candidate)
        .await
    {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "save_failed",
            error.to_string(),
        );
    }

    if let Err(error) = state
        .runtime
        .config_manager
        .apply_raw_config_and_rebuild(candidate)
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_extension_state",
            error.to_string(),
        );
    }

    // 通知扩展配置已变更
    let config_errors = state
        .runtime
        .config_manager
        .notify_extensions_config_changed()
        .await;
    for error in &config_errors {
        tracing::warn!("extension config notify error: {error}");
    }

    let reload_errors = state.runtime.reload_extensions().await;
    state
        .event_bus
        .send_notification(ClientNotification::ExtensionRegistryChanged);
    for error in &reload_errors {
        tracing::warn!("extension reload error: {error}");
    }

    Json(SetExtensionEnabledResponseDto {
        success: true,
        reload_errors,
    })
    .into_response()
}

async fn collect_extensions(state: &HttpState) -> Vec<ExtensionStateDto> {
    let effective = state.runtime.config_manager().read_effective();
    let loaded_ids = state
        .runtime
        .extension_runner()
        .registered_extension_ids()
        .await;
    let loaded_set: BTreeSet<_> = loaded_ids.iter().cloned().collect();
    let bundled_set: BTreeSet<_> = astrcode_bundled_extensions::bundled_extension_ids()
        .into_iter()
        .map(str::to_string)
        .collect();

    let mut ids: BTreeSet<String> = loaded_set.iter().cloned().collect();
    ids.extend(bundled_set.iter().cloned());
    ids.extend(effective.extensions.extension_states.keys().cloned());

    ids.into_iter()
        .map(|extension_id| {
            let source = if bundled_set.contains(&extension_id) {
                "builtin"
            } else if loaded_set.contains(&extension_id) {
                "disk"
            } else {
                "unknown"
            };
            ExtensionStateDto {
                enabled: effective
                    .extensions
                    .extension_states
                    .get(&extension_id)
                    .copied()
                    .unwrap_or(true),
                loaded: loaded_set.contains(&extension_id),
                extension_id,
                source: source.to_string(),
            }
        })
        .collect()
}
