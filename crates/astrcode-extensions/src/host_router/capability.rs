//! HostRouter lookup, authorization, and backend availability over the SDK-owned operation
//! catalog. Dispatch grouping and backend requirements live on `HostOperationSpec`; this module
//! only interprets them against the router's configured backends and the call context.

use astrcode_extension_sdk::{
    extension::ExtensionCapability,
    host::{
        HostOperation,
        internal::{HOST_OPERATION_SPECS, HostBackendRequirement, HostOperationSpec},
    },
    s5r::ErrorPayload,
    wire::WireErrorCode,
};

pub(super) fn lookup(name: &str) -> Result<&'static HostOperationSpec, ErrorPayload> {
    let operation = HostOperation::from_wire_name(name).ok_or_else(|| {
        ErrorPayload::new(
            WireErrorCode::UnknownCapability,
            format!("unknown astrcode capability: {name}"),
        )
    })?;
    Ok(operation.spec())
}

pub(super) fn available_operations(
    router: &super::HostRouter,
    ctx: &super::InvokeContext,
) -> Vec<HostOperation> {
    HOST_OPERATION_SPECS
        .iter()
        .filter_map(|spec| backend_available(router, spec.backend, ctx).then_some(spec.operation))
        .collect()
}

fn backend_available(
    router: &super::HostRouter,
    backend: HostBackendRequirement,
    ctx: &super::InvokeContext,
) -> bool {
    match backend {
        HostBackendRequirement::MainLlm => router.llm.has_main(ctx.llm_providers.as_ref()),
        HostBackendRequirement::SmallLlm => router.llm.has_small(ctx.llm_providers.as_ref()),
        HostBackendRequirement::SessionEventReader => router.session.has_event_reader(),
        HostBackendRequirement::SessionReader => router.session.has_session_reader(),
        HostBackendRequirement::SessionOperations => ctx.session_ops.is_some(),
        HostBackendRequirement::SessionOperationsAndReader => {
            ctx.session_ops.is_some() && router.session.has_session_reader()
        },
        HostBackendRequirement::SessionStoreDir => ctx.session_store_dir.is_some(),
        HostBackendRequirement::SessionStoreDirAndTasks => {
            ctx.session_store_dir.is_some() && ctx.tasks.is_some()
        },
        HostBackendRequirement::EventSender => ctx.event_tx.is_some(),
        HostBackendRequirement::WorkspaceDir => {
            router.workspace.has_root(ctx.working_dir.as_deref())
        },
        HostBackendRequirement::WorkspaceDirAndTasks => {
            router.workspace.has_root(ctx.working_dir.as_deref()) && ctx.tasks.is_some()
        },
        HostBackendRequirement::ToolResultReader => ctx.tool_result_reader.is_some(),
        HostBackendRequirement::ProcessWorkingDir => {
            router.process.is_available(ctx.working_dir.as_deref())
        },
        HostBackendRequirement::NetworkService => router.network.is_available(),
        HostBackendRequirement::PublicHttpDispatcher => router
            .extension_http
            .is_available(ctx.public_http_dispatcher.as_ref()),
    }
}

pub(super) fn authorize(
    spec: &HostOperationSpec,
    declared: &[ExtensionCapability],
) -> Result<(), ErrorPayload> {
    let Some(required) = spec.required else {
        return Ok(());
    };
    if declared.contains(&required) {
        return Ok(());
    }
    Err(ErrorPayload::new(
        WireErrorCode::PermissionDenied,
        format!(
            "{} requires declared capability {}",
            spec.name,
            required.as_str()
        ),
    ))
}

pub(crate) fn supported_operation_catalog() -> Vec<String> {
    HOST_OPERATION_SPECS
        .iter()
        .map(|spec| spec.name.to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_authorize_and_supported_catalog_follow_the_sdk_catalog() {
        let supported = supported_operation_catalog();
        assert_eq!(supported.len(), HOST_OPERATION_SPECS.len());
        for (index, spec) in HOST_OPERATION_SPECS.iter().enumerate() {
            assert_eq!(lookup(spec.name).unwrap().operation, spec.operation);
            assert_eq!(supported[index], spec.name);

            if let Some(required) = spec.required {
                assert!(authorize(spec, &[required]).is_ok());
                assert_eq!(
                    authorize(spec, &[]).expect_err("missing grant").code_enum(),
                    Some(WireErrorCode::PermissionDenied)
                );
            } else {
                assert!(authorize(spec, &[]).is_ok());
            }
        }

        assert_eq!(
            lookup("astrcode.unknown")
                .expect_err("unknown operation")
                .code_enum(),
            Some(WireErrorCode::UnknownCapability)
        );
    }
}
