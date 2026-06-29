//! 配置查看 / 重载 / 激活选择路由。

use astrcode_core::permission::ApprovalMode;
use astrcode_protocol::http::{
    ConfigReloadResponseDto, ConfigViewResponseDto, ModelDto, ModelOptionsDto, ProfileDto,
    UpdateActiveSelectionRequest, UpdateActiveSelectionResponseDto,
};
use axum::{
    Json,
    extract::State,
    response::{IntoResponse, Response},
};

use super::super::{HttpState, bad_request_response, internal_error_response};
use crate::bootstrap::{self, BootstrapOptions};

pub(in crate::http) async fn get_config(State(state): State<HttpState>) -> Response {
    let raw = state.runtime.config_manager().raw_config_snapshot();
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
            has_api_key: astrcode_core::config::profile_has_resolvable_api_key(p),
            models: p
                .models
                .iter()
                .map(|m| ModelDto {
                    id: m.id.clone(),
                    max_tokens: m.max_tokens,
                    context_limit: m.context_limit,
                    model_options: m.model_options.as_ref().map(|o| ModelOptionsDto {
                        reasoning: o.reasoning,
                        thinking_level: o.thinking_level,
                    }),
                })
                .collect(),
        })
        .collect();
    let approval_mode = state
        .runtime
        .config_manager
        .read_effective()
        .agent
        .approval_mode;
    Json(ConfigViewResponseDto {
        config_path,
        active_profile: raw.active_profile.clone(),
        active_model: raw.active_model.clone(),
        active_small_profile: raw.active_small_profile.clone(),
        active_small_model: raw.active_small_model,
        extension_states: state
            .runtime
            .config_manager
            .read_effective()
            .extensions
            .extension_states
            .clone(),
        approval_mode: approval_mode_to_wire(approval_mode),
        profiles,
        warning: None,
    })
    .into_response()
}

pub(in crate::http) async fn reload_config(State(state): State<HttpState>) -> Response {
    let reload_opts = BootstrapOptions {
        working_dir: Some(state.runtime.startup_working_dir().clone()),
        ..BootstrapOptions::default()
    };
    let config = match bootstrap::load_merged_config(
        state.runtime.config_manager().config_store().as_ref(),
        &reload_opts,
    )
    .await
    {
        Ok(c) => c,
        Err(error) => {
            return internal_error_response("reload_failed", error);
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
        return bad_request_response(
            "invalid_config",
            format!("Reloaded config is invalid: {error}"),
        );
    }
    state.runtime.sync_session_model_bindings();
    // 通知扩展配置已变更（针对已运行扩展的配置热更新）
    let config_errors = state
        .runtime
        .config_manager
        .notify_extensions_config_changed()
        .await;
    for error in &config_errors {
        tracing::warn!("extension config notify error: {error}");
    }
    // 重载扩展（处理启用/禁用状态变化）
    let reload_errors = state.runtime.reload_extensions().await;
    state
        .event_bus
        .send_notification(astrcode_protocol::events::ClientNotification::ExtensionRegistryChanged);
    for error in reload_errors {
        tracing::warn!("extension reload error: {error}");
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
    let approval_mode = match ApprovalMode::parse(&request.approval_mode) {
        Some(mode) => mode,
        None => {
            return bad_request_response(
                "invalid_approval_mode",
                format!(
                    "Invalid approvalMode {:?}; expected \"manual\" or \"yolo\"",
                    request.approval_mode
                ),
            );
        },
    };

    let mut candidate = state.runtime.config_manager().raw_config_snapshot();
    candidate.active_profile = request.active_profile;
    candidate.active_model = request.active_model;

    if let (Some(p), Some(m)) = (request.active_small_profile, request.active_small_model) {
        candidate.active_small_profile = Some(p);
        candidate.active_small_model = Some(m);
    }

    candidate.runtime.approval_mode = Some(approval_mode_to_wire(approval_mode));

    // Validate before persisting.
    if let Err(error) = candidate.clone().into_effective() {
        return bad_request_response("invalid_selection", error);
    };

    // Persist the validated candidate.
    if let Err(error) = state
        .runtime
        .config_manager
        .config_store()
        .save(&candidate)
        .await
    {
        return internal_error_response("save_failed", error);
    }

    // apply_raw_config_and_rebuild re-validates internally; failure here after
    // the explicit check above indicates a race or I/O issue.
    if let Err(error) = state
        .runtime
        .config_manager
        .apply_raw_config_and_rebuild(candidate)
    {
        tracing::warn!("apply_raw_config_and_rebuild failed after save: {error}");
    } else {
        state.runtime.sync_session_model_bindings();
    }

    // 通知扩展配置已变更（如果有扩展配置变化）
    let config_errors = state
        .runtime
        .config_manager
        .notify_extensions_config_changed()
        .await;
    for error in &config_errors {
        tracing::warn!("extension config notify error: {error}");
    }

    Json(UpdateActiveSelectionResponseDto {
        success: true,
        warning: None,
    })
    .into_response()
}

fn approval_mode_to_wire(mode: ApprovalMode) -> String {
    match mode {
        ApprovalMode::Manual => "manual".to_string(),
        ApprovalMode::Yolo => "yolo".to_string(),
    }
}
