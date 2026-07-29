//! 配置查看 / 重载 / 激活选择路由。

use astrcode_core::{
    config::{
        Config, ModelConfig, ModelOptionsConfig, Profile, ProviderCapabilities, ProviderSpec,
        builtin_provider_catalog, model_thinking_config, resolve_thinking_capability,
    },
    llm::thinking::{ThinkingCapability, ThinkingConfig, ThinkingWireMapping, validate_thinking},
    permission::ApprovalMode,
};
use astrcode_protocol::http::{
    ApplyProviderPresetRequest, ApplyProviderPresetResponseDto, ConfigReloadResponseDto,
    ConfigViewResponseDto, ModelDto, ModelOptionsDto, ProfileDto, ProviderCatalogResponseDto,
    ProviderEndpointPresetDto, ProviderSpecCapabilitiesDto, ProviderSpecDto,
    RemoveProviderPresetRequest, RemoveProviderPresetResponseDto, UpdateActiveSelectionRequest,
    UpdateActiveSelectionResponseDto, UpdateModelOptionsRequest, UpdateModelOptionsResponseDto,
};
use axum::{
    Json,
    extract::State,
    response::{IntoResponse, Response},
};

use super::{
    super::{HttpState, bad_request_response, internal_error_response},
    ConfigRequestError, notify_extensions_config_changed, reload_extension_registry, update_config,
};
use crate::{
    bootstrap::{self, BootstrapOptions},
    config_manager::ConfigUpdateError,
};

pub(in crate::http) async fn get_config(State(state): State<HttpState>) -> Response {
    let (raw, effective) = state.app.runtime().config_manager().config_snapshot().await;
    let config_path = state
        .app
        .runtime()
        .config_manager()
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
                    model_options: m.model_options.as_ref().map(|o| ModelOptionsDto {
                        thinking: o.thinking.clone().map(Into::into),
                    }),
                    thinking: m
                        .model_options
                        .as_ref()
                        .and_then(model_thinking_config)
                        .map(Into::into),
                    thinking_capability: {
                        let cap = m.thinking_capability.clone().or_else(|| {
                            resolve_thinking_capability(&p.provider_kind, p.wire_format, &m.id)
                        });
                        cap.map(Into::into)
                    },
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

    let requested_api_key = request.api_key;
    let activate = request.activate;
    let update_result = update_config(&state, |candidate| {
        let existing_api_key = candidate
            .profiles
            .iter()
            .find(|profile| profile.name == profile_name)
            .and_then(|profile| profile.api_key.clone());
        let api_key = provider_preset_api_key(spec, requested_api_key.as_deref(), existing_api_key);
        let profile = profile_from_provider_spec(
            spec,
            profile_name.clone(),
            model_id.clone(),
            base_url,
            api_key,
        );
        upsert_profile(&mut candidate.profiles, profile);

        if !activate {
            return Ok((false, None));
        }
        let previous_profile = candidate.active_profile.clone();
        let previous_model = candidate.active_model.clone();
        candidate.active_profile = profile_name.clone();
        candidate.active_model = model_id.clone();
        match candidate.clone().into_effective() {
            Ok(_) => Ok((true, None)),
            Err(error) => {
                candidate.active_profile = previous_profile;
                candidate.active_model = previous_model;
                Ok((
                    false,
                    Some(format!(
                        "Profile saved but not activated: {error}. Configure the API key first."
                    )),
                ))
            },
        }
    })
    .await;
    let (activated, warning) = match update_result {
        Ok(result) => result,
        Err(error) => return error.into_response(),
    };

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

    let update_result = update_config(&state, |candidate| {
        let profile_count = candidate.profiles.len();
        candidate
            .profiles
            .retain(|profile| profile.name != profile_name);
        if candidate.profiles.len() == profile_count {
            return Err(ConfigRequestError::new(
                "unknown_profile",
                format!("Profile {profile_name:?} is not configured"),
            ));
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
        Ok((
            candidate.active_profile.clone(),
            candidate.active_model.clone(),
        ))
    })
    .await;
    let (active_profile, active_model) = match update_result {
        Ok(result) => result,
        Err(error) => return error.into_response(),
    };

    Json(RemoveProviderPresetResponseDto {
        success: true,
        removed_profile_name: profile_name.to_string(),
        active_profile,
        active_model,
        warning: None,
    })
    .into_response()
}

pub(in crate::http) async fn reload_config(State(state): State<HttpState>) -> Response {
    let reload_opts = BootstrapOptions {
        working_dir: Some(state.app.runtime().startup_working_dir().clone()),
        ..BootstrapOptions::default()
    };
    let config = match bootstrap::load_merged_config(
        state.app.runtime().config_manager().config_store().as_ref(),
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

    let apply_result: Result<(), ConfigUpdateError<ConfigRequestError>> = state
        .app
        .runtime()
        .config_manager()
        .apply_loaded_config(config)
        .await;
    if let Err(error) = apply_result {
        return match error {
            ConfigUpdateError::Mutation(error) => bad_request_response(error.code, error.message),
            ConfigUpdateError::Resolve(error) => bad_request_response(
                "invalid_config",
                format!("Reloaded config is invalid: {error}"),
            ),
            ConfigUpdateError::Provider(error) => bad_request_response("invalid_provider", error),
            ConfigUpdateError::Store(error) => internal_error_response("reload_failed", error),
        };
    }
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

    let update_result = update_config(&state, |candidate| {
        candidate.active_profile = request.active_profile;
        candidate.active_model = request.active_model;
        if let (Some(profile), Some(model)) =
            (request.active_small_profile, request.active_small_model)
        {
            candidate.active_small_profile = Some(profile);
            candidate.active_small_model = Some(model);
        }
        candidate.runtime.approval_mode = Some(approval_mode.as_str().into());
        candidate
            .clone()
            .into_effective()
            .map_err(|error| ConfigRequestError::new("invalid_selection", error))?;
        Ok(())
    })
    .await;
    if let Err(error) = update_result {
        return error.into_response();
    };

    Json(UpdateActiveSelectionResponseDto {
        success: true,
        warning: None,
    })
    .into_response()
}

pub(in crate::http) async fn update_model_options(
    State(state): State<HttpState>,
    Json(request): Json<UpdateModelOptionsRequest>,
) -> Response {
    match update_config(&state, |candidate| {
        apply_model_options_update(candidate, request)
    })
    .await
    {
        Ok(()) => Json(UpdateModelOptionsResponseDto {
            success: true,
            warning: None,
        })
        .into_response(),
        Err(error) => error.into_response(),
    }
}

fn apply_model_options_update(
    candidate: &mut Config,
    request: UpdateModelOptionsRequest,
) -> Result<(), ConfigRequestError> {
    let profile_idx = match candidate
        .profiles
        .iter()
        .position(|p| p.name == request.profile_name)
    {
        Some(idx) => idx,
        None => {
            return Err(ConfigRequestError::new(
                "unknown_profile",
                format!("Profile {:?} not found", request.profile_name),
            ));
        },
    };
    let model_idx = match candidate.profiles[profile_idx]
        .models
        .iter()
        .position(|m| m.id == request.model_id)
    {
        Some(idx) => idx,
        None => {
            return Err(ConfigRequestError::new(
                "unknown_model",
                format!(
                    "Model {:?} not found in profile {:?}",
                    request.model_id, request.profile_name
                ),
            ));
        },
    };

    let profile = &candidate.profiles[profile_idx];
    let model = &profile.models[model_idx];

    // 2. Convert incoming thinking DTO -> core ThinkingConfig (None/null = default/disabled)
    let thinking_submitted = request.thinking.is_some();
    let new_thinking: ThinkingConfig = request
        .thinking
        .map(ThinkingConfig::from)
        .unwrap_or_default()
        .normalized();

    // 3. Resolve capability: explicit override > built-in lookup
    let capability: Option<ThinkingCapability> = model.thinking_capability.clone().or_else(|| {
        resolve_thinking_capability(&profile.provider_kind, profile.wire_format, &model.id)
    });

    // 4. Validate every explicit override. An omitted value means "use model default".
    if thinking_submitted {
        match capability.as_ref() {
            None => {
                return Err(ConfigRequestError::new(
                    "no_thinking_capability",
                    "Thinking is not supported for this model (no matching capability found). Set \
                     a `thinkingCapability` override on the model in config.toml if this model \
                     should support thinking.",
                ));
            },
            Some(cap) => {
                let issues = validate_thinking(&new_thinking, cap);
                if !issues.is_empty() {
                    return Err(ConfigRequestError::new(
                        "invalid_thinking_config",
                        format!("Thinking config validation failed: {}", issues.join("; ")),
                    ));
                }

                if matches!(
                    cap.wire_mapping,
                    ThinkingWireMapping::AnthropicBudget | ThinkingWireMapping::AnthropicAdaptive
                ) {
                    if let Some(budget) = new_thinking.budget_tokens {
                        if let Some(max_tokens) = model.max_tokens {
                            if budget >= max_tokens {
                                return Err(ConfigRequestError::new(
                                    "invalid_budget_tokens",
                                    format!(
                                        "budget_tokens ({}) must be less than model max_tokens \
                                         ({})",
                                        budget, max_tokens
                                    ),
                                ));
                            }
                        }
                    }
                }
            },
        }
    }

    let model = &mut candidate.profiles[profile_idx].models[model_idx];
    if thinking_submitted || model.model_options.is_some() {
        let opts = model
            .model_options
            .get_or_insert_with(ModelOptionsConfig::default);
        opts.thinking = thinking_submitted.then_some(new_thinking);
        opts.reasoning = None;
        opts.thinking_level = None;
        if opts.thinking.is_none() {
            model.model_options = None;
        }
    }

    Ok(())
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
            strict_tool_use: spec.capabilities.strict_tool_use,
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
            supports_strict_tool_use: spec.capabilities.strict_tool_use.then_some(true),
        },
        models: vec![ModelConfig {
            id: model_id,
            max_tokens: None,
            context_limit: None,
            model_options: None,
            thinking_capability: None,
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
