//! 配置查看 / 重载 / 激活选择路由。

use astrcode_core::{
    config::{
        Config, ConfigStoreError, ModelConfig, Profile, ProviderCapabilities, ProviderSpec,
        builtin_provider_catalog,
    },
    permission::ApprovalMode,
};
use astrcode_protocol::http::{
    ApplyProviderPresetRequest, ApplyProviderPresetResponseDto, ConfigReloadResponseDto,
    ConfigViewResponseDto, ModelDto, ModelOptionsDto, ProfileDto, ProviderCatalogResponseDto,
    ProviderEndpointPresetDto, ProviderSpecCapabilitiesDto, ProviderSpecDto,
    RemoveProviderPresetRequest, RemoveProviderPresetResponseDto, UpdateActiveSelectionRequest,
    UpdateActiveSelectionResponseDto,
};
use axum::{
    Json,
    extract::State,
    response::{IntoResponse, Response},
};

use super::{
    super::{HttpState, bad_request_response, internal_error_response},
    notify_extensions_config_changed, reload_extension_registry,
};
use crate::bootstrap::{self, BootstrapOptions};

pub(in crate::http) async fn get_config(State(state): State<HttpState>) -> Response {
    let raw = state.runtime.config_manager().raw_config_snapshot();
    let effective = state.runtime.config_manager().read_effective();
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
            wire_format: p.wire_format.into(),
            auth_scheme: p.auth_scheme.into(),
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
                        thinking_level: o.thinking_level.map(Into::into),
                    }),
                })
                .collect(),
        })
        .collect();
    Json(ConfigViewResponseDto {
        config_path,
        active_profile: raw.active_profile.clone(),
        active_model: raw.active_model.clone(),
        active_small_profile: raw.active_small_profile.clone(),
        active_small_model: raw.active_small_model,
        extension_states: effective.extensions.extension_states.clone(),
        approval_mode: effective.agent.approval_mode.into(),
        profiles,
        warning: None,
    })
    .into_response()
}

pub(in crate::http) async fn get_provider_catalog() -> Response {
    Json(ProviderCatalogResponseDto {
        providers: builtin_provider_catalog()
            .iter()
            .map(provider_spec_to_dto)
            .collect(),
    })
    .into_response()
}

pub(in crate::http) async fn apply_provider_preset(
    State(state): State<HttpState>,
    Json(request): Json<ApplyProviderPresetRequest>,
) -> Response {
    let Some(spec) = builtin_provider_catalog()
        .iter()
        .find(|spec| spec.id == request.provider_id)
    else {
        return bad_request_response(
            "unknown_provider_preset",
            format!("Unknown provider preset {:?}", request.provider_id),
        );
    };

    let profile_name = request
        .profile_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(spec.id)
        .to_string();
    let model_id = request
        .model_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(spec.default_model)
        .to_string();
    let Some(base_url) = provider_preset_base_url(
        spec,
        request.endpoint_id.as_deref(),
        request.base_url.as_deref(),
    ) else {
        return bad_request_response(
            "invalid_provider_endpoint",
            format!(
                "Provider preset {:?} requires a valid endpointId or baseUrl",
                spec.id
            ),
        );
    };

    let mut candidate = state.runtime.config_manager().raw_config_snapshot();
    let existing_api_key = candidate
        .profiles
        .iter()
        .find(|profile| profile.name == profile_name)
        .and_then(|profile| profile.api_key.clone());
    let api_key = provider_preset_api_key(spec, request.api_key.as_deref(), existing_api_key);
    let profile = profile_from_provider_spec(
        spec,
        profile_name.clone(),
        model_id.clone(),
        base_url,
        api_key,
    );
    upsert_profile(&mut candidate.profiles, profile);

    let mut activated = false;
    let mut warning = None;
    if request.activate {
        let mut activated_candidate = candidate.clone();
        activated_candidate.active_profile = profile_name.clone();
        activated_candidate.active_model = model_id.clone();
        match activated_candidate.clone().into_effective() {
            Ok(_) => {
                candidate = activated_candidate;
                activated = true;
            },
            Err(error) => {
                warning = Some(format!(
                    "Profile saved but not activated: {error}. Configure the API key first."
                ));
            },
        }
    }

    match persist_and_apply_config(&state, candidate).await {
        Ok(Some(apply_warning)) => append_warning(&mut warning, apply_warning),
        Ok(None) => {},
        Err(error) => return internal_error_response("save_failed", error),
    }

    Json(ApplyProviderPresetResponseDto {
        success: true,
        profile_name,
        model_id,
        activated,
        warning,
    })
    .into_response()
}

pub(in crate::http) async fn remove_provider_preset(
    State(state): State<HttpState>,
    Json(request): Json<RemoveProviderPresetRequest>,
) -> Response {
    let profile_name = request.profile_name.trim();
    if profile_name.is_empty() {
        return bad_request_response("invalid_profile_name", "Profile name cannot be empty");
    }

    let mut candidate = state.runtime.config_manager().raw_config_snapshot();
    let profile_count = candidate.profiles.len();
    candidate
        .profiles
        .retain(|profile| profile.name != profile_name);
    if candidate.profiles.len() == profile_count {
        return bad_request_response(
            "unknown_profile",
            format!("Profile {profile_name:?} is not configured"),
        );
    }
    if candidate.active_profile == profile_name {
        if let Some((next_profile, next_model)) = first_profile_model(&candidate.profiles) {
            candidate.active_profile = next_profile;
            candidate.active_model = next_model;
        } else {
            candidate.active_profile.clear();
            candidate.active_model.clear();
        }
    }
    if candidate.active_small_profile.as_deref() == Some(profile_name) {
        candidate.active_small_profile = None;
        candidate.active_small_model = None;
    }

    let active_profile = candidate.active_profile.clone();
    let active_model = candidate.active_model.clone();
    let warning = match persist_and_apply_config(&state, candidate).await {
        Ok(warning) => warning,
        Err(error) => return internal_error_response("save_failed", error),
    };

    Json(RemoveProviderPresetResponseDto {
        success: true,
        removed_profile_name: profile_name.to_string(),
        active_profile,
        active_model,
        warning,
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
    notify_extensions_config_changed(&state).await;
    // 重载扩展（处理启用/禁用状态变化）
    let _ = reload_extension_registry(&state).await;

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
    let Ok(approval_mode) = ApprovalMode::try_from(request.approval_mode) else {
        return bad_request_response(
            "invalid_approval_mode",
            "Invalid approvalMode; expected \"manual\" or \"yolo\"",
        );
    };

    let mut candidate = state.runtime.config_manager().raw_config_snapshot();
    candidate.active_profile = request.active_profile;
    candidate.active_model = request.active_model;

    if let (Some(p), Some(m)) = (request.active_small_profile, request.active_small_model) {
        candidate.active_small_profile = Some(p);
        candidate.active_small_model = Some(m);
    }

    candidate.runtime.approval_mode = Some(approval_mode.as_str().into());

    // Validate before persisting.
    if let Err(error) = candidate.clone().into_effective() {
        return bad_request_response("invalid_selection", error);
    };

    let warning = match persist_and_apply_config(&state, candidate).await {
        Ok(warning) => warning,
        Err(error) => return internal_error_response("save_failed", error),
    };

    // 通知扩展配置已变更（如果有扩展配置变化）
    notify_extensions_config_changed(&state).await;

    Json(UpdateActiveSelectionResponseDto {
        success: true,
        warning,
    })
    .into_response()
}

async fn persist_and_apply_config(
    state: &HttpState,
    candidate: Config,
) -> Result<Option<String>, ConfigStoreError> {
    state
        .runtime
        .config_manager
        .config_store()
        .save(&candidate)
        .await?;

    if let Err(error) = state
        .runtime
        .config_manager
        .apply_raw_config_and_rebuild(candidate)
    {
        tracing::warn!("apply_raw_config_and_rebuild failed after save: {error}");
        return Ok(Some(format!(
            "Saved to disk but runtime kept the previous effective configuration: {error}."
        )));
    }

    state.runtime.sync_session_model_bindings();
    Ok(None)
}

fn provider_spec_to_dto(spec: &ProviderSpec) -> ProviderSpecDto {
    ProviderSpecDto {
        id: spec.id.to_string(),
        display_name: spec.display_name.to_string(),
        provider_kind: spec.provider_kind.to_string(),
        wire_format: spec.wire_format.into(),
        auth_scheme: spec.auth_scheme.into(),
        default_model: spec.default_model.to_string(),
        api_key_env_vars: spec
            .api_key_env_vars
            .iter()
            .map(|env| (*env).to_string())
            .collect(),
        endpoints: spec
            .endpoints
            .iter()
            .map(|endpoint| ProviderEndpointPresetDto {
                id: endpoint.id.to_string(),
                label: endpoint.label.to_string(),
                base_url: endpoint.base_url.map(str::to_string),
                is_default: endpoint.is_default,
            })
            .collect(),
        capabilities: ProviderSpecCapabilitiesDto {
            prompt_cache_key: spec.capabilities.prompt_cache_key,
            stream_usage: spec.capabilities.stream_usage,
            reasoning_effort: spec.capabilities.reasoning_effort,
        },
    }
}

fn provider_preset_base_url(
    spec: &ProviderSpec,
    endpoint_id: Option<&str>,
    custom_base_url: Option<&str>,
) -> Option<String> {
    if let Some(base_url) = custom_base_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(base_url.trim_end_matches('/').to_string());
    }
    let endpoint = match endpoint_id {
        Some(id) => spec.endpoints.iter().find(|endpoint| endpoint.id == id)?,
        None => spec.endpoints.iter().find(|endpoint| endpoint.is_default)?,
    };
    endpoint
        .base_url
        .map(|base_url| base_url.trim_end_matches('/').to_string())
}

fn provider_preset_api_key(
    spec: &ProviderSpec,
    request_api_key: Option<&str>,
    existing_api_key: Option<String>,
) -> Option<String> {
    request_api_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or(existing_api_key)
        .or_else(|| {
            spec.api_key_env_vars
                .first()
                .map(|env| format!("env:{env}"))
        })
}

fn first_profile_model(profiles: &[Profile]) -> Option<(String, String)> {
    profiles.iter().find_map(|profile| {
        profile
            .models
            .first()
            .map(|model| (profile.name.clone(), model.id.clone()))
    })
}

fn profile_from_provider_spec(
    spec: &ProviderSpec,
    profile_name: String,
    model_id: String,
    base_url: String,
    api_key: Option<String>,
) -> Profile {
    Profile {
        name: profile_name,
        provider_kind: spec.provider_kind.to_string(),
        wire_format: spec.wire_format,
        auth_scheme: spec.auth_scheme,
        base_url,
        api_key,
        capabilities: ProviderCapabilities {
            supports_prompt_cache_key: spec.capabilities.prompt_cache_key.then_some(true),
            prompt_cache_retention: None,
            supports_stream_usage: spec.capabilities.stream_usage.then_some(true),
        },
        models: vec![ModelConfig {
            id: model_id,
            max_tokens: None,
            context_limit: None,
            model_options: None,
        }],
    }
}

fn upsert_profile(profiles: &mut Vec<Profile>, profile: Profile) {
    if let Some(existing) = profiles
        .iter_mut()
        .find(|existing| existing.name == profile.name)
    {
        *existing = profile;
    } else {
        profiles.push(profile);
    }
}

fn append_warning(warning: &mut Option<String>, next: String) {
    match warning {
        Some(existing) => {
            existing.push(' ');
            existing.push_str(&next);
        },
        None => *warning = Some(next),
    }
}
