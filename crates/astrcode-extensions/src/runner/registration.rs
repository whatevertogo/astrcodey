use astrcode_extension_sdk::extension::{
    ExtensionError, ExtensionHttpAccess, ExtensionHttpMethod, ExtensionHttpRouteRegistration,
    ExtensionRegistrations, extension_http_route_patterns_conflict,
};

use super::HostedExtension;

pub(super) fn validate_registration_conflicts(
    extension_id: &str,
    registrations: &ExtensionRegistrations,
    existing_extensions: &[HostedExtension],
) -> Result<(), ExtensionError> {
    validate_tool_registrations(extension_id, registrations, existing_extensions)?;
    validate_http_route_registrations(
        extension_id,
        registrations.http_routes(),
        existing_extensions,
    )
}

fn validate_tool_registrations(
    extension_id: &str,
    registrations: &ExtensionRegistrations,
    existing_extensions: &[HostedExtension],
) -> Result<(), ExtensionError> {
    for registration in registrations.tools() {
        let definition = registration.definition();
        if let Some(existing) = existing_extensions.iter().find(|existing| {
            existing
                .manifest
                .registrations
                .tools()
                .iter()
                .any(|existing| existing.definition().name == definition.name)
        }) {
            return Err(tool_conflict(
                extension_id,
                &definition.name,
                existing.manifest.id(),
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
    routes: &[ExtensionHttpRouteRegistration],
    existing_extensions: &[HostedExtension],
) -> Result<(), ExtensionError> {
    let invalid = |reason| ExtensionError::InvalidRegistration {
        extension_id: extension_id.to_owned(),
        reason,
    };
    for registration in routes {
        let route = &registration.route;
        if route.access != ExtensionHttpAccess::Public {
            continue;
        }
        if route.path == "/api" || route.path.starts_with("/api/") {
            return Err(invalid(format!(
                "public route {} uses reserved /api namespace",
                route.path
            )));
        }
        if existing_extensions.iter().any(|hosted| {
            hosted
                .manifest
                .registrations
                .http_routes()
                .iter()
                .any(|existing| {
                    existing.route.access == ExtensionHttpAccess::Public
                        && existing.route.method == route.method
                        && extension_http_route_patterns_conflict(&existing.route.path, &route.path)
                })
        }) {
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
