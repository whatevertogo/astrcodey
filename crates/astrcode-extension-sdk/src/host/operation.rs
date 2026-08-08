use crate::{extension::ExtensionCapability, s5r::CapabilityDescriptor};

/// Stable identity for every host operation currently accepted by the HostRouter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(usize)]
pub enum HostOperation {
    EventEmit,
    ExtensionHttpPublic,
    LlmMainChat,
    LlmSmallChat,
    NetworkClient,
    ProcessSpawn,
    SessionControlCancelTurn,
    SessionControlConfigureTools,
    SessionControlCreate,
    SessionControlDispose,
    SessionControlExecutionView,
    SessionControlInjectOrStart,
    SessionControlInterruptAndSubmit,
    SessionControlReactivate,
    SessionControlState,
    SessionControlSubmitTurn,
    SessionHistoryList,
    SessionHistoryProviderMessages,
    SessionHistorySnapshot,
    SessionHistoryTokenUsage,
    SessionHistoryTranscript,
    SessionInspectList,
    SessionInspectProviderMessages,
    SessionInspectReadModel,
    SessionInspectSnapshot,
    SessionReadEvents,
    SessionRootCreate,
    SessionRootState,
    SessionRootSubmitTurn,
    SessionStateRead,
    SessionStateWrite,
    WorkspaceEdit,
    WorkspaceGlob,
    WorkspaceGrep,
    WorkspaceList,
    WorkspaceRead,
    WorkspaceWrite,
}

impl HostOperation {
    pub const COUNT: usize = Self::WorkspaceWrite as usize + 1;

    pub fn from_wire_name(name: &str) -> Option<Self> {
        HOST_OPERATION_SPECS
            .binary_search_by(|spec| spec.name.cmp(name))
            .ok()
            .map(|index| HOST_OPERATION_SPECS[index].operation)
    }

    pub const fn spec(self) -> &'static HostOperationSpec {
        &HOST_OPERATION_SPECS[self as usize]
    }

    pub const fn wire_name(self) -> &'static str {
        self.spec().name
    }

    pub const fn required_capability(self) -> Option<ExtensionCapability> {
        self.spec().required
    }

    pub(super) const fn context_requirement(self) -> HostContextRequirement {
        match self {
            Self::SessionControlCancelTurn
            | Self::SessionControlConfigureTools
            | Self::SessionControlCreate
            | Self::SessionControlDispose
            | Self::SessionControlExecutionView
            | Self::SessionControlInjectOrStart
            | Self::SessionControlInterruptAndSubmit
            | Self::SessionControlReactivate
            | Self::SessionControlState
            | Self::SessionControlSubmitTurn
            | Self::SessionHistoryList
            | Self::SessionHistoryProviderMessages
            | Self::SessionHistorySnapshot
            | Self::SessionHistoryTokenUsage
            | Self::SessionHistoryTranscript
            | Self::SessionReadEvents
            | Self::SessionStateRead
            | Self::SessionStateWrite => HostContextRequirement::Session,
            Self::ProcessSpawn
            | Self::SessionRootCreate
            | Self::WorkspaceEdit
            | Self::WorkspaceGlob
            | Self::WorkspaceGrep
            | Self::WorkspaceList
            | Self::WorkspaceRead
            | Self::WorkspaceWrite => HostContextRequirement::Workspace,
            Self::EventEmit
            | Self::ExtensionHttpPublic
            | Self::LlmMainChat
            | Self::LlmSmallChat
            | Self::NetworkClient
            | Self::SessionInspectList
            | Self::SessionInspectProviderMessages
            | Self::SessionInspectReadModel
            | Self::SessionInspectSnapshot
            | Self::SessionRootState
            | Self::SessionRootSubmitTurn => HostContextRequirement::None,
        }
    }

    #[doc(hidden)]
    pub const fn requires_session_context(self) -> bool {
        matches!(self.context_requirement(), HostContextRequirement::Session)
    }

    #[doc(hidden)]
    pub const fn requires_workspace_context(self) -> bool {
        matches!(
            self.context_requirement(),
            HostContextRequirement::Workspace
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HostContextRequirement {
    None,
    Session,
    Workspace,
}

/// Metadata shared by authoring clients, authorization, and the S5R capability catalog.
#[derive(Debug, Clone, Copy)]
pub struct HostOperationSpec {
    pub operation: HostOperation,
    pub name: &'static str,
    pub required: Option<ExtensionCapability>,
    pub description: &'static str,
    pub supports_stream: bool,
    pub cancelable: bool,
    pub catalog: bool,
}

impl HostOperationSpec {
    pub fn descriptor(self) -> CapabilityDescriptor {
        CapabilityDescriptor {
            name: self.name.into(),
            description: self.description.into(),
            supports_stream: self.supports_stream,
            cancelable: self.cancelable,
        }
    }
}

macro_rules! spec {
    ($operation:ident, $name:literal, $required:expr, $description:literal $(,)?) => {
        HostOperationSpec {
            operation: HostOperation::$operation,
            name: $name,
            required: $required,
            description: $description,
            supports_stream: false,
            cancelable: false,
            catalog: true,
        }
    };
}

/// The canonical operation catalog, sorted by wire name.
pub const HOST_OPERATION_SPECS: [HostOperationSpec; HostOperation::COUNT] = [
    spec!(
        EventEmit,
        "astrcode.event.emit",
        Some(ExtensionCapability::EmitEvents),
        "Emit a declared extension event",
    ),
    spec!(
        ExtensionHttpPublic,
        "astrcode.extension.http.public",
        Some(ExtensionCapability::PublicHttpDispatch),
        "Dispatch a request to another extension's public HTTP route",
    ),
    HostOperationSpec {
        operation: HostOperation::LlmMainChat,
        name: "astrcode.llm.main_chat",
        required: Some(ExtensionCapability::MainModel),
        description: "Chat with the host-configured live main LLM provider",
        supports_stream: true,
        cancelable: true,
        catalog: true,
    },
    HostOperationSpec {
        operation: HostOperation::LlmSmallChat,
        name: "astrcode.llm.small_chat",
        required: Some(ExtensionCapability::SmallModel),
        description: "Chat with the host-configured small LLM",
        supports_stream: true,
        cancelable: true,
        catalog: true,
    },
    HostOperationSpec {
        operation: HostOperation::NetworkClient,
        name: "astrcode.network.client",
        required: Some(ExtensionCapability::NetworkClient),
        description: "Send a bounded outbound HTTP or HTTPS request with a binary body",
        supports_stream: false,
        cancelable: true,
        catalog: true,
    },
    HostOperationSpec {
        operation: HostOperation::ProcessSpawn,
        name: "astrcode.process.spawn",
        required: Some(ExtensionCapability::ProcessSpawn),
        description: "Run a bounded subprocess with an optional workspace-relative cwd",
        supports_stream: false,
        cancelable: true,
        catalog: true,
    },
    spec!(
        SessionControlCancelTurn,
        "astrcode.session.control.cancel_turn",
        Some(ExtensionCapability::SessionControl),
        "Cancel the active turn",
    ),
    spec!(
        SessionControlConfigureTools,
        "astrcode.session.control.configure_tools",
        Some(ExtensionCapability::SessionControl),
        "Configure the tool-name boundary used by subsequent session turns",
    ),
    spec!(
        SessionControlCreate,
        "astrcode.session.control.create",
        Some(ExtensionCapability::SessionControl),
        "Create a child session",
    ),
    spec!(
        SessionControlDispose,
        "astrcode.session.control.dispose",
        Some(ExtensionCapability::SessionControl),
        "Recycle a session while preserving its durable data",
    ),
    spec!(
        SessionControlExecutionView,
        "astrcode.session.control.execution_view",
        Some(ExtensionCapability::SessionControl),
        "Read active turn and queued-input state",
    ),
    spec!(
        SessionControlInjectOrStart,
        "astrcode.session.control.inject_or_start",
        Some(ExtensionCapability::SessionControl),
        "Inject input into a running turn or start when idle",
    ),
    spec!(
        SessionControlInterruptAndSubmit,
        "astrcode.session.control.interrupt_and_submit",
        Some(ExtensionCapability::SessionControl),
        "Interrupt the active turn and submit new input",
    ),
    spec!(
        SessionControlReactivate,
        "astrcode.session.control.reactivate",
        Some(ExtensionCapability::SessionControl),
        "Reactivate a recycled direct child session",
    ),
    spec!(
        SessionControlState,
        "astrcode.session.control.state",
        Some(ExtensionCapability::SessionControl),
        "Read active or recycled session lifecycle state",
    ),
    spec!(
        SessionControlSubmitTurn,
        "astrcode.session.control.submit_turn",
        Some(ExtensionCapability::SessionControl),
        "Submit a turn to a session",
    ),
    spec!(
        SessionHistoryList,
        "astrcode.session.history.list",
        Some(ExtensionCapability::SessionHistory),
        "List stable session summaries visible to session-history consumers",
    ),
    spec!(
        SessionHistoryProviderMessages,
        "astrcode.session.history.provider_messages",
        Some(ExtensionCapability::SessionHistory),
        "Read provider-visible messages from a session transcript",
    ),
    spec!(
        SessionHistorySnapshot,
        "astrcode.session.history.snapshot",
        Some(ExtensionCapability::SessionHistory),
        "Read an authorized active or recycled session snapshot",
    ),
    spec!(
        SessionHistoryTokenUsage,
        "astrcode.session.history.token_usage",
        Some(ExtensionCapability::SessionHistory),
        "Read accumulated non-cached token usage for a session",
    ),
    spec!(
        SessionHistoryTranscript,
        "astrcode.session.history.transcript",
        Some(ExtensionCapability::SessionHistory),
        "Read the extension-visible transcript for a session",
    ),
    spec!(
        SessionInspectList,
        "astrcode.session.inspect.list",
        Some(ExtensionCapability::SessionInspect),
        "List all sessions visible to the host (global privileged access)",
    ),
    spec!(
        SessionInspectProviderMessages,
        "astrcode.session.inspect.provider_messages",
        Some(ExtensionCapability::SessionInspect),
        "Read provider-visible messages for any host-visible session",
    ),
    spec!(
        SessionInspectReadModel,
        "astrcode.session.inspect.read_model",
        Some(ExtensionCapability::SessionInspect),
        "Read any host-visible projected session model through a stable wire DTO",
    ),
    spec!(
        SessionInspectSnapshot,
        "astrcode.session.inspect.snapshot",
        Some(ExtensionCapability::SessionInspect),
        "Read any host-visible session snapshot (global privileged access)",
    ),
    spec!(
        SessionReadEvents,
        "astrcode.session.read_events",
        Some(ExtensionCapability::SessionHistory),
        "Read a cursor page from the durable session event log",
    ),
    spec!(
        SessionRootCreate,
        "astrcode.session.root.create",
        Some(ExtensionCapability::InputDelivery),
        "Create a top-level session attributed to the calling extension",
    ),
    spec!(
        SessionRootState,
        "astrcode.session.root.state",
        Some(ExtensionCapability::InputDelivery),
        "Read an owned top-level session lifecycle state",
    ),
    spec!(
        SessionRootSubmitTurn,
        "astrcode.session.root.submit_turn",
        Some(ExtensionCapability::InputDelivery),
        "Submit a turn to an owned top-level session",
    ),
    spec!(
        SessionStateRead,
        "astrcode.session.state.read",
        None,
        "Read extension-namespaced session state",
    ),
    spec!(
        SessionStateWrite,
        "astrcode.session.state.write",
        None,
        "Write extension-namespaced session state",
    ),
    spec!(
        WorkspaceEdit,
        "astrcode.workspace.edit",
        Some(ExtensionCapability::WorkspaceWrite),
        "Replace an exact text fragment in a non-sensitive workspace file",
    ),
    spec!(
        WorkspaceGlob,
        "astrcode.workspace.glob",
        Some(ExtensionCapability::WorkspaceRead),
        "Match bounded workspace paths by glob",
    ),
    spec!(
        WorkspaceGrep,
        "astrcode.workspace.grep",
        Some(ExtensionCapability::WorkspaceRead),
        "Regex-search bounded UTF-8 workspace files",
    ),
    spec!(
        WorkspaceList,
        "astrcode.workspace.list",
        Some(ExtensionCapability::WorkspaceRead),
        "List a bounded workspace directory tree",
    ),
    spec!(
        WorkspaceRead,
        "astrcode.workspace.read",
        Some(ExtensionCapability::WorkspaceRead),
        "Read a bounded UTF-8 workspace file",
    ),
    spec!(
        WorkspaceWrite,
        "astrcode.workspace.write",
        Some(ExtensionCapability::WorkspaceWrite),
        "Create or replace a non-sensitive file under the working directory",
    ),
];

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn operation_catalog_is_exhaustive_sorted_and_round_trips() {
        assert_eq!(HOST_OPERATION_SPECS.len(), HostOperation::COUNT);
        assert!(
            HOST_OPERATION_SPECS
                .windows(2)
                .all(|pair| pair[0].name < pair[1].name),
            "operation catalog must remain sorted by wire name"
        );

        let mut names = HashSet::new();
        let mut operations = HashSet::new();
        for (index, spec) in HOST_OPERATION_SPECS.iter().enumerate() {
            assert_eq!(spec.operation as usize, index);
            assert!(names.insert(spec.name), "duplicate name: {}", spec.name);
            assert!(
                operations.insert(spec.operation),
                "duplicate operation: {:?}",
                spec.operation
            );
            assert_eq!(
                HostOperation::from_wire_name(spec.name),
                Some(spec.operation)
            );
            assert_eq!(spec.operation.wire_name(), spec.name);
        }
        assert_eq!(operations.len(), HostOperation::COUNT);
        assert_eq!(HostOperation::from_wire_name("astrcode.unknown"), None);
    }

    #[test]
    fn operation_policy_matrix_is_exhaustive() {
        macro_rules! policy {
            ($operation:ident, $required:expr, $context:ident) => {
                policy!($operation, $required, $context, false, false, true)
            };
            (
                $operation:ident,
                $required:expr,
                $context:ident,
                $supports_stream:expr,
                $cancelable:expr,
                $catalog:expr
            ) => {
                (
                    HostOperation::$operation,
                    $required,
                    HostContextRequirement::$context,
                    $supports_stream,
                    $cancelable,
                    $catalog,
                )
            };
        }

        let cases = [
            policy!(EventEmit, Some(ExtensionCapability::EmitEvents), None),
            policy!(
                ExtensionHttpPublic,
                Some(ExtensionCapability::PublicHttpDispatch),
                None
            ),
            policy!(
                LlmMainChat,
                Some(ExtensionCapability::MainModel),
                None,
                true,
                true,
                true
            ),
            policy!(
                LlmSmallChat,
                Some(ExtensionCapability::SmallModel),
                None,
                true,
                true,
                true
            ),
            policy!(
                NetworkClient,
                Some(ExtensionCapability::NetworkClient),
                None,
                false,
                true,
                true
            ),
            policy!(
                ProcessSpawn,
                Some(ExtensionCapability::ProcessSpawn),
                Workspace,
                false,
                true,
                true
            ),
            policy!(
                SessionControlCancelTurn,
                Some(ExtensionCapability::SessionControl),
                Session
            ),
            policy!(
                SessionControlConfigureTools,
                Some(ExtensionCapability::SessionControl),
                Session
            ),
            policy!(
                SessionControlCreate,
                Some(ExtensionCapability::SessionControl),
                Session
            ),
            policy!(
                SessionControlDispose,
                Some(ExtensionCapability::SessionControl),
                Session
            ),
            policy!(
                SessionControlExecutionView,
                Some(ExtensionCapability::SessionControl),
                Session
            ),
            policy!(
                SessionControlInjectOrStart,
                Some(ExtensionCapability::SessionControl),
                Session
            ),
            policy!(
                SessionControlInterruptAndSubmit,
                Some(ExtensionCapability::SessionControl),
                Session
            ),
            policy!(
                SessionControlReactivate,
                Some(ExtensionCapability::SessionControl),
                Session
            ),
            policy!(
                SessionControlState,
                Some(ExtensionCapability::SessionControl),
                Session
            ),
            policy!(
                SessionControlSubmitTurn,
                Some(ExtensionCapability::SessionControl),
                Session
            ),
            policy!(
                SessionHistoryList,
                Some(ExtensionCapability::SessionHistory),
                Session
            ),
            policy!(
                SessionHistoryProviderMessages,
                Some(ExtensionCapability::SessionHistory),
                Session
            ),
            policy!(
                SessionHistorySnapshot,
                Some(ExtensionCapability::SessionHistory),
                Session
            ),
            policy!(
                SessionHistoryTokenUsage,
                Some(ExtensionCapability::SessionHistory),
                Session
            ),
            policy!(
                SessionHistoryTranscript,
                Some(ExtensionCapability::SessionHistory),
                Session
            ),
            policy!(
                SessionInspectList,
                Some(ExtensionCapability::SessionInspect),
                None
            ),
            policy!(
                SessionInspectProviderMessages,
                Some(ExtensionCapability::SessionInspect),
                None
            ),
            policy!(
                SessionInspectReadModel,
                Some(ExtensionCapability::SessionInspect),
                None
            ),
            policy!(
                SessionInspectSnapshot,
                Some(ExtensionCapability::SessionInspect),
                None
            ),
            policy!(
                SessionReadEvents,
                Some(ExtensionCapability::SessionHistory),
                Session
            ),
            policy!(
                SessionRootCreate,
                Some(ExtensionCapability::InputDelivery),
                Workspace
            ),
            policy!(
                SessionRootState,
                Some(ExtensionCapability::InputDelivery),
                None
            ),
            policy!(
                SessionRootSubmitTurn,
                Some(ExtensionCapability::InputDelivery),
                None
            ),
            policy!(SessionStateRead, None, Session),
            policy!(SessionStateWrite, None, Session),
            policy!(
                WorkspaceEdit,
                Some(ExtensionCapability::WorkspaceWrite),
                Workspace
            ),
            policy!(
                WorkspaceGlob,
                Some(ExtensionCapability::WorkspaceRead),
                Workspace
            ),
            policy!(
                WorkspaceGrep,
                Some(ExtensionCapability::WorkspaceRead),
                Workspace
            ),
            policy!(
                WorkspaceList,
                Some(ExtensionCapability::WorkspaceRead),
                Workspace
            ),
            policy!(
                WorkspaceRead,
                Some(ExtensionCapability::WorkspaceRead),
                Workspace
            ),
            policy!(
                WorkspaceWrite,
                Some(ExtensionCapability::WorkspaceWrite),
                Workspace
            ),
        ];

        assert_eq!(cases.len(), HostOperation::COUNT);
        for (index, (operation, required, context, stream, cancelable, catalog)) in
            cases.into_iter().enumerate()
        {
            assert_eq!(operation as usize, index, "matrix order for {operation:?}");
            let spec = operation.spec();
            assert_eq!(spec.required, required, "{operation:?} capability");
            assert_eq!(
                operation.required_capability(),
                required,
                "{operation:?} capability accessor"
            );
            assert_eq!(
                operation.context_requirement(),
                context,
                "{operation:?} context"
            );
            assert_eq!(
                operation.requires_session_context(),
                context == HostContextRequirement::Session,
                "{operation:?} hidden session-context helper"
            );
            assert_eq!(
                operation.requires_workspace_context(),
                context == HostContextRequirement::Workspace,
                "{operation:?} hidden workspace-context helper"
            );
            assert_eq!(spec.supports_stream, stream, "{operation:?} streaming");
            assert_eq!(spec.cancelable, cancelable, "{operation:?} cancellation");
            assert_eq!(spec.catalog, catalog, "{operation:?} catalog visibility");
        }
    }
}
