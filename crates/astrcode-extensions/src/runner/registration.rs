use std::collections::HashSet;

use astrcode_extension_sdk::extension::{
    ExtensionCapability, ExtensionError, ExtensionHttpAccess, ExtensionHttpMethod,
    ExtensionHttpRouteRegistration, HookMode, ProviderEvent, Registrar,
    extension_http_route_patterns_conflict, lifecycle_event_allows_blocking,
};

use super::HostedExtension;

pub(super) fn validate_registrations(
    extension_id: &str,
    capabilities: &[ExtensionCapability],
    registrar: &Registrar,
    existing_extensions: &[HostedExtension],
) -> Result<(), ExtensionError> {
    validate_capability_registrations(extension_id, capabilities, registrar)?;
    validate_lifecycle_registrations(extension_id, registrar)?;
    validate_tool_registrations(extension_id, registrar, existing_extensions)?;
    validate_http_route_registrations(
        extension_id,
        capabilities,
        registrar.http_routes(),
        existing_extensions,
    )
}

fn validate_capability_registrations(
    extension_id: &str,
    capabilities: &[ExtensionCapability],
    registrar: &Registrar,
) -> Result<(), ExtensionError> {
    require_capability(
        extension_id,
        capabilities,
        !registrar.extension_event_decls().is_empty(),
        "event",
        ExtensionCapability::EmitEvents,
    )?;
    require_capability(
        extension_id,
        capabilities,
        !registrar.compact().is_empty(),
        "compact",
        ExtensionCapability::SessionHistory,
    )?;
    require_capability(
        extension_id,
        capabilities,
        !registrar.user_message_envelope().is_empty(),
        "user_message_envelope",
        ExtensionCapability::ProviderRequest,
    )?;
    require_capability(
        extension_id,
        capabilities,
        registrar
            .provider()
            .iter()
            .any(|(event, _, _, _)| *event == ProviderEvent::BeforeRequest),
        "before_provider_request",
        ExtensionCapability::ProviderRequest,
    )?;
    require_capability(
        extension_id,
        capabilities,
        registrar
            .provider()
            .iter()
            .any(|(event, _, _, _)| *event == ProviderEvent::AfterResponse),
        "after_provider_response",
        ExtensionCapability::ProviderRequest,
    )?;
    require_capability(
        extension_id,
        capabilities,
        registrar
            .pre_tool_use()
            .iter()
            .any(|registration| registration.mode == HookMode::Blocking),
        "pre_tool_use",
        ExtensionCapability::ToolIntercept,
    )?;
    require_capability(
        extension_id,
        capabilities,
        registrar
            .post_tool_use()
            .iter()
            .any(|registration| registration.mode == HookMode::Blocking),
        "post_tool_use",
        ExtensionCapability::ToolIntercept,
    )?;
    require_capability(
        extension_id,
        capabilities,
        !registrar.continue_after_stop().is_empty(),
        "continue_after_stop",
        ExtensionCapability::TurnContinuationControl,
    )
}

fn require_capability(
    extension_id: &str,
    capabilities: &[ExtensionCapability],
    registration_present: bool,
    hook: &'static str,
    capability: ExtensionCapability,
) -> Result<(), ExtensionError> {
    if registration_present && !capabilities.contains(&capability) {
        return Err(ExtensionError::MissingCapability {
            extension_id: extension_id.to_owned(),
            hook,
            capability,
        });
    }
    Ok(())
}

fn validate_lifecycle_registrations(
    extension_id: &str,
    registrar: &Registrar,
) -> Result<(), ExtensionError> {
    for (event, mode, _, _) in registrar.lifecycle() {
        if *mode == HookMode::Blocking && !lifecycle_event_allows_blocking(event) {
            return Err(ExtensionError::InvalidLifecycleMode {
                extension_id: extension_id.to_owned(),
                event: event.clone(),
            });
        }
    }
    Ok(())
}

fn validate_tool_registrations(
    extension_id: &str,
    registrar: &Registrar,
    existing_extensions: &[HostedExtension],
) -> Result<(), ExtensionError> {
    let mut names = HashSet::new();
    for (definition, _) in registrar.tools() {
        if !names.insert(definition.name.as_str()) {
            return Err(tool_conflict(extension_id, &definition.name, extension_id));
        }
        if let Some(existing) = existing_extensions.iter().find(|existing| {
            existing
                .manifest
                .registrations
                .tools()
                .iter()
                .any(|(existing, _)| existing.name == definition.name)
        }) {
            return Err(tool_conflict(
                extension_id,
                &definition.name,
                &existing.manifest.id,
            ));
        }
    }
    Ok(())
}

fn tool_conflict(
    extension_id: &str,
    tool_name: &str,
    conflicting_extension_id: &str,
) -> ExtensionError {
    ExtensionError::ToolConflict {
        extension_id: extension_id.to_owned(),
        tool_name: tool_name.to_owned(),
        conflicting_extension_id: conflicting_extension_id.to_owned(),
    }
}

fn validate_http_route_registrations(
    extension_id: &str,
    capabilities: &[ExtensionCapability],
    routes: &[ExtensionHttpRouteRegistration],
    existing_extensions: &[HostedExtension],
) -> Result<(), ExtensionError> {
    let invalid = |reason| ExtensionError::InvalidRegistration {
        extension_id: extension_id.to_owned(),
        reason,
    };
    for (index, registration) in routes.iter().enumerate() {
        let route = &registration.route;
        route.validate().map_err(&invalid)?;
        let required_capability = match route.access {
            ExtensionHttpAccess::Public => ExtensionCapability::PublicHttp,
            ExtensionHttpAccess::Authenticated => ExtensionCapability::AuthenticatedHttp,
        };
        if !capabilities.contains(&required_capability) {
            return Err(ExtensionError::MissingCapability {
                extension_id: extension_id.to_owned(),
                hook: match route.access {
                    ExtensionHttpAccess::Public => "http_route.public",
                    ExtensionHttpAccess::Authenticated => "http_route.authenticated",
                },
                capability: required_capability,
            });
        }
        if route.access == ExtensionHttpAccess::Public
            && (route.path == "/api" || route.path.starts_with("/api/"))
        {
            return Err(invalid(format!(
                "public route {} uses reserved /api namespace",
                route.path
            )));
        }
        if routes[..index].iter().any(|existing| {
            existing.route.access == route.access
                && existing.route.method == route.method
                && extension_http_route_patterns_conflict(&existing.route.path, &route.path)
        }) {
            return Err(invalid(format!(
                "conflicting {} routes for {}",
                http_method_name(route.method),
                route.path
            )));
        }
        if route.access == ExtensionHttpAccess::Public
            && existing_extensions.iter().any(|hosted| {
                hosted
                    .manifest
                    .registrations
                    .http_routes()
                    .iter()
                    .any(|existing| {
                        existing.route.access == ExtensionHttpAccess::Public
                            && existing.route.method == route.method
                            && extension_http_route_patterns_conflict(
                                &existing.route.path,
                                &route.path,
                            )
                    })
            })
        {
            return Err(invalid(format!(
                "public route conflicts with an existing {} route: {}",
                http_method_name(route.method),
                route.path
            )));
        }
    }
    Ok(())
}

fn http_method_name(method: ExtensionHttpMethod) -> &'static str {
    match method {
        ExtensionHttpMethod::Get => "GET",
        ExtensionHttpMethod::Post => "POST",
        ExtensionHttpMethod::Put => "PUT",
        ExtensionHttpMethod::Patch => "PATCH",
        ExtensionHttpMethod::Delete => "DELETE",
    }
}
