//! 扩展查看 / 重载 / 启停路由。

use std::collections::{BTreeMap, BTreeSet};

use astrcode_extension_sdk::extension::{
    ExtensionHttpAccess, ExtensionHttpMethod, ExtensionHttpRequest,
};
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
    extract::{OriginalUri, Path, State},
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
};

use super::{
    super::{
        HttpState, bad_request_response, error_response, internal_error_response,
        not_found_response,
    },
    ConfigRequestError, reload_extension_registry, update_config,
};
use crate::{
    config_manager::ConfigResolve,
    protocol_mapping::{
        custom_event_declaration_to_dto, custom_event_subscription_to_dto,
        extension_capability_to_dto, extension_http_method_to_dto, keybinding_to_dto,
        slash_command_to_dto, status_item_to_dto, transport_feature_to_dto,
    },
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
    let update_result = update_config(&state, |candidate| {
        candidate
            .runtime
            .extension_states
            .get_or_insert_with(BTreeMap::new)
            .insert(request.extension_id, request.enabled);
        candidate
            .clone()
            .into_effective()
            .map_err(|error| ConfigRequestError::new("invalid_extension_state", error))?;
        Ok(())
    })
    .await;
    if let Err(error) = update_result {
        return error.into_response();
    }

    state
        .app
        .event_bus()
        .send_notification(astrcode_protocol::events::ClientNotification::ExtensionRegistryChanged);

    Json(SetExtensionEnabledResponseDto {
        success: true,
        reload_errors: Vec::new(),
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
        .app
        .runtime()
        .extension_runner()
        .dispatch_public_http_route(request, &body)
        .await;
    extension_http_response(result)
}

pub(in crate::http) async fn dispatch_authenticated_http(
    State(state): State<HttpState>,
    Path((extension_id, path)): Path<(String, String)>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    body: Bytes,
) -> Response {
    let Some(method) = extension_http_method(&method) else {
        return not_found_response("route_not_found", "route not found");
    };
    let request = ExtensionHttpRequest {
        method,
        path: format!("/{path}"),
        path_params: BTreeMap::new(),
        query: uri.query().map(str::to_owned),
        body: serde_json::Value::Null,
    };
    let result = state
        .app
        .runtime()
        .extension_runner()
        .dispatch_authenticated_http_route(&extension_id, request, &body)
        .await;
    extension_http_response(result)
}

fn extension_http_response(
    result: Result<ExtensionHttpDispatchResult, astrcode_extension_sdk::extension::ExtensionError>,
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
    let effective = state.app.runtime().config_manager().read_effective();
    let runner = state.app.runtime().extension_runner();
    let registry = runner.registry_snapshot().await;
    let declarations: BTreeMap<_, _> = registry
        .extensions
        .into_iter()
        .map(|declaration| (declaration.id.clone(), declaration))
        .collect();
    let loaded_set: BTreeSet<_> = declarations.keys().cloned().collect();
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
    let id = declaration.id.clone();
    ExtensionDeclarationDto {
        id: declaration.id,
        capabilities: declaration
            .capabilities
            .into_iter()
            .map(extension_capability_to_dto)
            .collect(),
        required_transport_features: declaration
            .required_transport_features
            .into_iter()
            .map(transport_feature_to_dto)
            .collect(),
        tools: declaration.tools.into_iter().map(Into::into).collect(),
        dynamic_tools: declaration.dynamic_tools,
        commands: declaration
            .commands
            .into_iter()
            .map(|command| slash_command_to_dto(&id, command))
            .collect(),
        dynamic_commands: declaration.dynamic_commands,
        keybindings: declaration
            .keybindings
            .into_iter()
            .map(keybinding_to_dto)
            .collect(),
        status_items: declaration
            .status_items
            .into_iter()
            .map(status_item_to_dto)
            .collect(),
        custom_events: declaration
            .custom_events
            .into_iter()
            .map(custom_event_declaration_to_dto)
            .collect(),
        custom_event_subscriptions: declaration
            .custom_event_subscriptions
            .into_iter()
            .map(custom_event_subscription_to_dto)
            .collect(),
        http_routes: declaration
            .http_routes
            .into_iter()
            .map(extension_http_route_dto)
            .collect(),
    }
}

fn extension_http_route_dto(
    route: astrcode_extension_sdk::extension::ExtensionHttpRoute,
) -> ExtensionHttpRouteDto {
    ExtensionHttpRouteDto {
        method: extension_http_method_to_dto(route.method),
        path: route.path,
        authenticated: route.access == ExtensionHttpAccess::Authenticated,
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
