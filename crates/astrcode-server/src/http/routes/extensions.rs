//! 扩展查看 / 重载 / 启停路由。

use std::collections::{BTreeMap, BTreeSet};

use astrcode_core::extension::{ExtensionHttpMethod, ExtensionHttpRequest};
use astrcode_extensions::runner::{
    ExtensionHttpDispatchResult, ExtensionStageDiagnostics, ExtensionStageStatus,
};
use astrcode_protocol::{
    http::{
        ExtensionDeclarationDto, ExtensionDiagnosticsDto, ExtensionHttpRouteDto,
        ExtensionListResponseDto, ExtensionReloadResponseDto, ExtensionStageDiagnosticsDto,
        ExtensionStateDto, SetExtensionEnabledRequest, SetExtensionEnabledResponseDto,
    },
    wire::{ExtensionSourceDto, ExtensionStageStatusDto},
};
use axum::{
    Json,
    body::Bytes,
    extract::{OriginalUri, State},
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
};

use super::{
    super::{
        HttpState, bad_request_response, error_response, internal_error_response,
        not_found_response,
    },
    notify_extensions_config_changed, reload_extension_registry,
};

pub(in crate::http) async fn list_extensions(State(state): State<HttpState>) -> Response {
    Json(ExtensionListResponseDto {
        extensions: collect_extensions(&state).await,
    })
    .into_response()
}

pub(in crate::http) async fn reload_extensions(State(state): State<HttpState>) -> Response {
    let reload_errors = reload_extension_registry(&state).await;
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
        return bad_request_response("invalid_extension_state", error);
    }

    if let Err(error) = state
        .runtime
        .config_manager
        .config_store()
        .save(&candidate)
        .await
    {
        return internal_error_response("save_failed", error);
    }

    if let Err(error) = state
        .runtime
        .config_manager
        .apply_raw_config_and_rebuild(candidate)
    {
        return bad_request_response("invalid_extension_state", error);
    }
    state.runtime.sync_session_model_bindings();

    notify_extensions_config_changed(&state).await;

    let reload_errors = reload_extension_registry(&state).await;

    Json(SetExtensionEnabledResponseDto {
        success: true,
        reload_errors,
    })
    .into_response()
}

pub(in crate::http) async fn dispatch_public_http(
    State(state): State<HttpState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    body: Bytes,
) -> Response {
    let Some(method) = extension_http_method(&method) else {
        return not_found_response("route_not_found", "route not found");
    };
    let request = ExtensionHttpRequest {
        method,
        path: uri.path().to_owned(),
        path_params: BTreeMap::new(),
        query: uri.query().map(str::to_owned),
        body: serde_json::Value::Null,
    };
    let result = state
        .runtime
        .extension_runner()
        .dispatch_public_http_route(request, &body)
        .await;
    extension_http_response(result)
}

fn extension_http_response(
    result: Result<ExtensionHttpDispatchResult, astrcode_core::extension::ExtensionError>,
) -> Response {
    match result {
        Ok(ExtensionHttpDispatchResult::Response(response)) => {
            match StatusCode::from_u16(response.status) {
                Ok(status) => (status, Json(response.body)).into_response(),
                Err(error) => internal_error_response("invalid_extension_status", error),
            }
        },
        Ok(ExtensionHttpDispatchResult::NotFound) => not_found_response(
            "extension_route_not_found",
            "extension HTTP route not found",
        ),
        Ok(ExtensionHttpDispatchResult::MethodNotAllowed) => error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "extension_http_method_not_allowed",
            "extension HTTP route does not support this method",
        ),
        Ok(ExtensionHttpDispatchResult::PayloadTooLarge { max_body_bytes }) => error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "extension_http_body_too_large",
            format!("extension HTTP body exceeds {max_body_bytes} bytes"),
        ),
        Ok(ExtensionHttpDispatchResult::InvalidJson { message }) => {
            bad_request_response("invalid_extension_http_json", message)
        },
        Err(error) => internal_error_response("extension_http_failed", error),
    }
}

fn extension_http_method(method: &Method) -> Option<ExtensionHttpMethod> {
    match *method {
        Method::GET => Some(ExtensionHttpMethod::Get),
        Method::POST => Some(ExtensionHttpMethod::Post),
        Method::PUT => Some(ExtensionHttpMethod::Put),
        Method::PATCH => Some(ExtensionHttpMethod::Patch),
        Method::DELETE => Some(ExtensionHttpMethod::Delete),
        _ => None,
    }
}

async fn collect_extensions(state: &HttpState) -> Vec<ExtensionStateDto> {
    let effective = state.runtime.config_manager().read_effective();
    let runner = state.runtime.extension_runner();
    let loaded_ids = runner.registered_extension_ids().await;
    let loaded_set: BTreeSet<_> = loaded_ids.iter().cloned().collect();
    let registry = runner.registry_snapshot().await;
    let declarations: BTreeMap<_, _> = registry
        .extensions
        .into_iter()
        .map(|declaration| (declaration.id.clone(), declaration))
        .collect();
    let diagnostics = runner.diagnostics_snapshot();
    let bundled_set: BTreeSet<_> = astrcode_bundled_extensions::bundled_extension_ids()
        .into_iter()
        .map(str::to_string)
        .collect();

    let mut ids: BTreeSet<String> = loaded_set.iter().cloned().collect();
    ids.extend(bundled_set.iter().cloned());
    ids.extend(effective.extensions.extension_states.keys().cloned());
    ids.extend(diagnostics.keys().cloned());

    ids.into_iter()
        .map(|extension_id| {
            let source = if bundled_set.contains(&extension_id) {
                ExtensionSourceDto::Builtin
            } else if loaded_set.contains(&extension_id) {
                ExtensionSourceDto::Disk
            } else {
                ExtensionSourceDto::Unknown
            };
            ExtensionStateDto {
                enabled: astrcode_bundled_extensions::extension_enabled(
                    &effective.extensions.extension_states,
                    &extension_id,
                ),
                loaded: loaded_set.contains(&extension_id),
                declaration: declarations
                    .get(&extension_id)
                    .cloned()
                    .map(extension_declaration_dto),
                diagnostics: diagnostics
                    .get(&extension_id)
                    .cloned()
                    .map(extension_diagnostics_dto),
                extension_id,
                source,
            }
        })
        .collect()
}

fn extension_declaration_dto(
    declaration: astrcode_extensions::runner::ExtensionDeclarationSnapshot,
) -> ExtensionDeclarationDto {
    ExtensionDeclarationDto {
        id: declaration.id,
        capabilities: declaration
            .capabilities
            .into_iter()
            .map(Into::into)
            .collect(),
        tools: declaration.tools.into_iter().map(Into::into).collect(),
        dynamic_tools: declaration.dynamic_tools,
        commands: declaration.commands.into_iter().map(Into::into).collect(),
        dynamic_commands: declaration.dynamic_commands,
        keybindings: declaration
            .keybindings
            .into_iter()
            .map(Into::into)
            .collect(),
        status_items: declaration
            .status_items
            .into_iter()
            .map(Into::into)
            .collect(),
        events: declaration.events.into_iter().map(Into::into).collect(),
        http_routes: declaration
            .http_routes
            .into_iter()
            .map(extension_http_route_dto)
            .collect(),
    }
}

fn extension_http_route_dto(
    route: astrcode_core::extension::ExtensionHttpRoute,
) -> ExtensionHttpRouteDto {
    ExtensionHttpRouteDto {
        method: route.method.into(),
        path: route.path,
        description: route.description,
        max_body_bytes: route.max_body_bytes,
    }
}

fn extension_diagnostics_dto(
    diagnostics: astrcode_extensions::runner::ExtensionDiagnostics,
) -> ExtensionDiagnosticsDto {
    ExtensionDiagnosticsDto {
        load: extension_stage_diagnostics_dto(diagnostics.load),
        register: extension_stage_diagnostics_dto(diagnostics.register),
        start: extension_stage_diagnostics_dto(diagnostics.start),
        hook_calls: diagnostics.hook_calls,
        hook_timeouts: diagnostics.hook_timeouts,
        last_hook: diagnostics.last_hook,
        last_duration_ms: diagnostics.last_duration_ms,
        last_error: diagnostics.last_error,
    }
}

fn extension_stage_diagnostics_dto(
    diagnostics: ExtensionStageDiagnostics,
) -> ExtensionStageDiagnosticsDto {
    ExtensionStageDiagnosticsDto {
        status: match diagnostics.status {
            ExtensionStageStatus::Unknown => ExtensionStageStatusDto::Unknown,
            ExtensionStageStatus::Running => ExtensionStageStatusDto::Running,
            ExtensionStageStatus::Succeeded => ExtensionStageStatusDto::Succeeded,
            ExtensionStageStatus::Failed => ExtensionStageStatusDto::Failed,
            ExtensionStageStatus::Skipped => ExtensionStageStatusDto::Skipped,
        },
        duration_ms: diagnostics.duration_ms,
        error: diagnostics.error,
    }
}
