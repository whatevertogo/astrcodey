//! 配置查看 / 重载 / 激活选择路由。

use astrcode_protocol::http::{
    ConfigReloadResponseDto, ConfigViewResponseDto, ModelDto, ProfileDto,
    UpdateActiveSelectionRequest, UpdateActiveSelectionResponseDto,
};
use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};

use super::super::{HttpState, error_response};

pub(in crate::http) async fn get_config(State(state): State<HttpState>) -> Response {
    let raw = state.runtime.config_manager.read_raw_config();
    let config_path = state
        .runtime
        .config_manager
        .config_store()
        .path()
        .display()
        .to_string();
    let profiles: Vec<ProfileDto> = raw
        .profiles
        .iter()
        .map(|p| ProfileDto {
            name: p.name.clone(),
            provider_kind: p.provider_kind.clone(),
            base_url: p.base_url.clone(),
            has_api_key: p.api_key.as_ref().is_some_and(|k| !k.is_empty()),
            models: p
                .models
                .iter()
                .map(|m| ModelDto {
                    id: m.id.clone(),
                    max_tokens: m.max_tokens,
                    context_limit: m.context_limit,
                })
                .collect(),
        })
        .collect();
    Json(ConfigViewResponseDto {
        config_path,
        active_profile: raw.active_profile.clone(),
        active_model: raw.active_model.clone(),
        active_small_profile: raw.active_small_profile.clone(),
        active_small_model: raw.active_small_model.clone(),
        profiles,
        warning: None,
    })
    .into_response()
}

pub(in crate::http) async fn reload_config(State(state): State<HttpState>) -> Response {
    let config = match state.runtime.config_manager.config_store().load().await {
        Ok(c) => c,
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "reload_failed",
                error.to_string(),
            );
        },
    };
    let active_profile = config.active_profile.clone();
    let active_model = config.active_model.clone();
    let active_small_profile = config.active_small_profile.clone();
    let active_small_model = config.active_small_model.clone();

    if let Err(error) = state
        .runtime
        .config_manager
        .apply_raw_config_and_rebuild(config)
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_config",
            format!("Reloaded config is invalid: {error}"),
        );
    }

    Json(ConfigReloadResponseDto {
        active_profile,
        active_model,
        active_small_profile,
        active_small_model,
    })
    .into_response()
}

pub(in crate::http) async fn update_active_selection(
    State(state): State<HttpState>,
    Json(request): Json<UpdateActiveSelectionRequest>,
) -> Response {
    let mut candidate = state.runtime.config_manager.read_raw_config().clone();
    candidate.active_profile = request.active_profile;
    candidate.active_model = request.active_model;

    if let (Some(p), Some(m)) = (request.active_small_profile, request.active_small_model) {
        candidate.active_small_profile = Some(p);
        candidate.active_small_model = Some(m);
    }

    // Validate before persisting.
    if let Err(error) = candidate.clone().into_effective() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_selection",
            error.to_string(),
        );
    };

    // Persist the validated candidate.
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

    // apply_raw_config_and_rebuild re-validates internally; failure here after
    // the explicit check above indicates a race or I/O issue.
    if let Err(error) = state
        .runtime
        .config_manager
        .apply_raw_config_and_rebuild(candidate)
    {
        tracing::warn!("apply_raw_config_and_rebuild failed after save: {error}");
    }

    Json(UpdateActiveSelectionResponseDto {
        success: true,
        warning: None,
    })
    .into_response()
}
