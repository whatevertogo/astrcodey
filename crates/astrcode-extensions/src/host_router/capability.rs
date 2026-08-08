//! HostRouter dispatch mapping over the SDK-owned operation catalog.

use std::ops::Deref;

use astrcode_core::wire::WireErrorCode;
use astrcode_extension_sdk::{
    extension::ExtensionCapability,
    host::{HOST_OPERATION_SPECS, HostOperation, HostOperationSpec},
    s5r::{CapabilityDescriptor, ErrorPayload},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum HostCapability {
    Llm(LlmCapability),
    Session(SessionCapability),
    Context(ContextCapability),
    Workspace(WorkspaceCapability),
    Process(ProcessCapability),
    Network(NetworkCapability),
    ExtensionHttp(ExtensionHttpCapability),
}

macro_rules! capability_enum {
    ($name:ident { $($variant:ident),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub(super) enum $name {
            $($variant),+
        }

        impl $name {
            #[cfg(test)]
            const ALL: &'static [Self] = &[$(Self::$variant),+];
        }
    };
}

capability_enum!(LlmCapability {
    MainChat,
    SmallChat,
});

capability_enum!(SessionCapability {
    ReadEvents,
    Create,
    ConfigureTools,
    SubmitTurn,
    InterruptAndSubmit,
    Inject,
    CancelTurn,
    ExecutionView,
    Dispose,
    Reactivate,
    State,
    HistoryList,
    HistoryProviderMessages,
    HistorySnapshot,
    HistoryTokenUsage,
    HistoryTranscript,
    InspectList,
    InspectSnapshot,
    InspectReadModel,
    InspectProviderMessages,
    RootCreate,
    RootState,
    RootSubmitTurn,
});

capability_enum!(ContextCapability {
    StateRead,
    StateWrite,
    EmitEvent,
});

capability_enum!(WorkspaceCapability {
    Read,
    List,
    Grep,
    Glob,
    Write,
    Edit,
});

capability_enum!(ProcessCapability { Spawn });
capability_enum!(NetworkCapability { Client });
capability_enum!(ExtensionHttpCapability { PublicDispatch });

#[derive(Debug, Clone, Copy)]
pub(super) struct HostCapabilitySpec {
    pub(super) capability: HostCapability,
    operation: HostOperation,
}

impl Deref for HostCapabilitySpec {
    type Target = HostOperationSpec;

    fn deref(&self) -> &Self::Target {
        self.operation.spec()
    }
}

const HOST_CAPABILITY_SPECS: [HostCapabilitySpec; HostOperation::COUNT] = [
    HostCapabilitySpec {
        capability: HostCapability::Context(ContextCapability::EmitEvent),
        operation: HostOperation::EventEmit,
    },
    HostCapabilitySpec {
        capability: HostCapability::ExtensionHttp(ExtensionHttpCapability::PublicDispatch),
        operation: HostOperation::ExtensionHttpPublic,
    },
    HostCapabilitySpec {
        capability: HostCapability::Llm(LlmCapability::MainChat),
        operation: HostOperation::LlmMainChat,
    },
    HostCapabilitySpec {
        capability: HostCapability::Llm(LlmCapability::SmallChat),
        operation: HostOperation::LlmSmallChat,
    },
    HostCapabilitySpec {
        capability: HostCapability::Network(NetworkCapability::Client),
        operation: HostOperation::NetworkClient,
    },
    HostCapabilitySpec {
        capability: HostCapability::Process(ProcessCapability::Spawn),
        operation: HostOperation::ProcessSpawn,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::CancelTurn),
        operation: HostOperation::SessionControlCancelTurn,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::ConfigureTools),
        operation: HostOperation::SessionControlConfigureTools,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::Create),
        operation: HostOperation::SessionControlCreate,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::Dispose),
        operation: HostOperation::SessionControlDispose,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::ExecutionView),
        operation: HostOperation::SessionControlExecutionView,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::Inject),
        operation: HostOperation::SessionControlInjectOrStart,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::InterruptAndSubmit),
        operation: HostOperation::SessionControlInterruptAndSubmit,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::Reactivate),
        operation: HostOperation::SessionControlReactivate,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::State),
        operation: HostOperation::SessionControlState,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::SubmitTurn),
        operation: HostOperation::SessionControlSubmitTurn,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::HistoryList),
        operation: HostOperation::SessionHistoryList,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::HistoryProviderMessages),
        operation: HostOperation::SessionHistoryProviderMessages,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::HistorySnapshot),
        operation: HostOperation::SessionHistorySnapshot,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::HistoryTokenUsage),
        operation: HostOperation::SessionHistoryTokenUsage,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::HistoryTranscript),
        operation: HostOperation::SessionHistoryTranscript,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::InspectList),
        operation: HostOperation::SessionInspectList,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::InspectProviderMessages),
        operation: HostOperation::SessionInspectProviderMessages,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::InspectReadModel),
        operation: HostOperation::SessionInspectReadModel,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::InspectSnapshot),
        operation: HostOperation::SessionInspectSnapshot,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::ReadEvents),
        operation: HostOperation::SessionReadEvents,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::RootCreate),
        operation: HostOperation::SessionRootCreate,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::RootState),
        operation: HostOperation::SessionRootState,
    },
    HostCapabilitySpec {
        capability: HostCapability::Session(SessionCapability::RootSubmitTurn),
        operation: HostOperation::SessionRootSubmitTurn,
    },
    HostCapabilitySpec {
        capability: HostCapability::Context(ContextCapability::StateRead),
        operation: HostOperation::SessionStateRead,
    },
    HostCapabilitySpec {
        capability: HostCapability::Context(ContextCapability::StateWrite),
        operation: HostOperation::SessionStateWrite,
    },
    HostCapabilitySpec {
        capability: HostCapability::Workspace(WorkspaceCapability::Edit),
        operation: HostOperation::WorkspaceEdit,
    },
    HostCapabilitySpec {
        capability: HostCapability::Workspace(WorkspaceCapability::Glob),
        operation: HostOperation::WorkspaceGlob,
    },
    HostCapabilitySpec {
        capability: HostCapability::Workspace(WorkspaceCapability::Grep),
        operation: HostOperation::WorkspaceGrep,
    },
    HostCapabilitySpec {
        capability: HostCapability::Workspace(WorkspaceCapability::List),
        operation: HostOperation::WorkspaceList,
    },
    HostCapabilitySpec {
        capability: HostCapability::Workspace(WorkspaceCapability::Read),
        operation: HostOperation::WorkspaceRead,
    },
    HostCapabilitySpec {
        capability: HostCapability::Workspace(WorkspaceCapability::Write),
        operation: HostOperation::WorkspaceWrite,
    },
];

pub(super) fn lookup(name: &str) -> Result<&'static HostCapabilitySpec, ErrorPayload> {
    let operation = HostOperation::from_wire_name(name).ok_or_else(|| {
        ErrorPayload::new(
            WireErrorCode::UnknownCapability,
            format!("unknown astrcode capability: {name}"),
        )
    })?;
    Ok(&HOST_CAPABILITY_SPECS[operation as usize])
}

pub(super) fn available_operations(
    router: &super::HostRouter,
    ctx: &super::InvokeContext,
) -> Vec<HostOperation> {
    HOST_CAPABILITY_SPECS
        .iter()
        .filter_map(|spec| {
            backend_available(router, spec.capability, ctx).then_some(spec.operation)
        })
        .collect()
}

fn backend_available(
    router: &super::HostRouter,
    capability: HostCapability,
    ctx: &super::InvokeContext,
) -> bool {
    match capability {
        HostCapability::Llm(capability) => router.llm.is_available(capability),
        HostCapability::Session(capability) => match capability {
            SessionCapability::ReadEvents | SessionCapability::HistoryTokenUsage => {
                router.session.has_event_reader()
            },
            SessionCapability::HistoryList
            | SessionCapability::HistoryProviderMessages
            | SessionCapability::HistorySnapshot
            | SessionCapability::HistoryTranscript
            | SessionCapability::InspectList
            | SessionCapability::InspectSnapshot
            | SessionCapability::InspectReadModel
            | SessionCapability::InspectProviderMessages => router.session.has_session_reader(),
            SessionCapability::RootState | SessionCapability::RootSubmitTurn => {
                ctx.session_ops.is_some() && router.session.has_session_reader()
            },
            SessionCapability::RootCreate
            | SessionCapability::Create
            | SessionCapability::ConfigureTools
            | SessionCapability::SubmitTurn
            | SessionCapability::InterruptAndSubmit
            | SessionCapability::Inject
            | SessionCapability::CancelTurn
            | SessionCapability::ExecutionView
            | SessionCapability::Dispose
            | SessionCapability::Reactivate
            | SessionCapability::State => ctx.session_ops.is_some(),
        },
        HostCapability::Context(capability) => router.context.is_available(capability, ctx),
        HostCapability::Workspace(capability) => router.workspace.is_available(
            capability,
            ctx.working_dir.as_deref(),
            ctx.tasks.as_ref(),
        ),
        HostCapability::Process(_) => router.process.is_available(ctx.working_dir.as_deref()),
        HostCapability::Network(_) => router.network.is_available(),
        HostCapability::ExtensionHttp(_) => router.extension_http.is_available(),
    }
}

pub(super) fn authorize(
    spec: &HostCapabilitySpec,
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
            astrcode_extension_sdk::s5r::capability_to_wire(required)
        ),
    ))
}

pub(super) fn catalog_for_grants(
    capabilities: &[ExtensionCapability],
) -> Vec<CapabilityDescriptor> {
    HOST_OPERATION_SPECS
        .iter()
        .filter(|spec| spec.catalog)
        .filter(|spec| match spec.required {
            Some(required) => capabilities.contains(&required),
            None => true,
        })
        .copied()
        .map(HostOperationSpec::descriptor)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn dispatch_mapping_covers_the_sdk_catalog_and_preserves_authorization() {
        let mut capabilities = HashSet::new();

        for (index, operation_spec) in HOST_OPERATION_SPECS.iter().enumerate() {
            let spec = &HOST_CAPABILITY_SPECS[index];
            assert_eq!(spec.operation, operation_spec.operation);
            capabilities.insert(spec.capability);
            assert_eq!(lookup(spec.name).unwrap().capability, spec.capability);

            let granted_catalog = match spec.required {
                Some(required) => catalog_for_grants(&[required]),
                None => catalog_for_grants(&[]),
            };
            assert_eq!(
                granted_catalog
                    .iter()
                    .any(|descriptor| descriptor.name == spec.name),
                spec.catalog,
                "catalog visibility mismatch: {}",
                spec.name
            );

            if let Some(required) = spec.required {
                assert!(authorize(spec, &[required]).is_ok());
                assert_eq!(
                    authorize(spec, &[]).expect_err("missing grant").code_enum(),
                    Some(WireErrorCode::PermissionDenied)
                );
            } else {
                assert!(authorize(spec, &[]).is_ok());
            }

            assert_eq!(
                spec.supports_stream,
                matches!(spec.capability, HostCapability::Llm(_)),
                "stream handler mismatch: {}",
                spec.name
            );
        }

        let expected_capabilities = LlmCapability::ALL
            .iter()
            .copied()
            .map(HostCapability::Llm)
            .chain(
                SessionCapability::ALL
                    .iter()
                    .copied()
                    .map(HostCapability::Session),
            )
            .chain(
                ContextCapability::ALL
                    .iter()
                    .copied()
                    .map(HostCapability::Context),
            )
            .chain(
                WorkspaceCapability::ALL
                    .iter()
                    .copied()
                    .map(HostCapability::Workspace),
            )
            .chain(
                ProcessCapability::ALL
                    .iter()
                    .copied()
                    .map(HostCapability::Process),
            )
            .chain(
                NetworkCapability::ALL
                    .iter()
                    .copied()
                    .map(HostCapability::Network),
            )
            .chain(
                ExtensionHttpCapability::ALL
                    .iter()
                    .copied()
                    .map(HostCapability::ExtensionHttp),
            )
            .collect::<HashSet<_>>();
        assert_eq!(capabilities, expected_capabilities);
        assert_eq!(
            lookup("astrcode.unknown")
                .expect_err("unknown operation")
                .code_enum(),
            Some(WireErrorCode::UnknownCapability)
        );
    }
}
