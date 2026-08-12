use crate::{ExtensionCapability, protocol::CapabilityDescriptor};

macro_rules! host_operations {
    (@count $operation:ident) => {
        ()
    };
    (@flag) => {
        false
    };
    (@flag $value:expr) => {
        $value
    };
    (
        $(
            $operation:ident {
                name: $name:literal,
                required: $required:expr,
                context: $context:ident,
                group: $group:ident,
                backend: $backend:ident,
                request: $request:path,
                response: $response:path,
                description: $description:literal
                $(, supports_stream: $stream:expr)?
                $(, cancelable: $cancelable:expr)?
                $(,)?
            }
        )*
    ) => {
        /// Stable identity for every host operation currently accepted by the HostRouter.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr(usize)]
        pub enum HostOperation {
            $($operation),*
        }

        pub mod operations {
            $(
                #[derive(Debug, Clone, Copy)]
                pub struct $operation;
            )*
        }

        $(
            impl HostOp for operations::$operation {
                type Request = $request;
                type Response = $response;

                const OPERATION: HostOperation = HostOperation::$operation;
            }
        )*

        impl HostOperation {
            pub const COUNT: usize = [$(host_operations!(@count $operation)),*].len();

            pub fn from_wire_name(name: &str) -> Option<Self> {
                match name {
                    $($name => Some(Self::$operation),)*
                    _ => None,
                }
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

        }

        /// The canonical operation catalog, in declaration order.
        pub const HOST_OPERATION_SPECS: [HostOperationSpec; HostOperation::COUNT] = [
            $(
                HostOperationSpec {
                    operation: HostOperation::$operation,
                    name: $name,
                    required: $required,
                    context: HostContextRequirement::$context,
                    group: HostOperationGroup::$group,
                    backend: HostBackendRequirement::$backend,
                    description: $description,
                    supports_stream: host_operations!(@flag $($stream)?),
                    cancelable: host_operations!(@flag $($cancelable)?),
                }
            ),*
        ];
    };
}

#[diagnostic::on_unimplemented(
    message = "{Self} is not a declared S5R host operation",
    label = "use a marker from astrcode_extension_contract::operation::operations"
)]
pub trait HostOp: Send + Sync + 'static {
    type Request: serde::Serialize + serde::de::DeserializeOwned + Send + 'static;
    type Response: serde::Serialize + serde::de::DeserializeOwned + Send + 'static;

    const OPERATION: HostOperation;
}

host_operations! {
    EventEmit {
        name: "astrcode.event.emit",
        required: Some(ExtensionCapability::EmitCustomEvents),
        context: None,
        group: Context,
        backend: EventSender,
        request: crate::host::HostEventEmitRequest,
        response: crate::host::HostEventEmitOutput,
        description: "Emit a declared extension event",
    }
    ExtensionHttpPublic {
        name: "astrcode.extension.http.public",
        required: Some(ExtensionCapability::PublicHttpDispatch),
        context: None,
        group: ExtensionHttp,
        backend: PublicHttpDispatcher,
        request: crate::extension_http::ExtensionHttpDispatchRequest,
        response: crate::extension_http::ExtensionHttpResponse,
        description: "Dispatch a request to another extension's public HTTP route",
    }
    LlmMainChat {
        name: "astrcode.llm.main_chat",
        required: Some(ExtensionCapability::MainModel),
        context: None,
        group: Llm,
        backend: MainLlm,
        request: crate::host::HostLlmChatRequest,
        response: crate::host::HostLlmChatOutput,
        description: "Chat with the host-configured live main LLM provider",
        supports_stream: true,
        cancelable: true,
    }
    LlmSmallChat {
        name: "astrcode.llm.small_chat",
        required: Some(ExtensionCapability::SmallModel),
        context: None,
        group: Llm,
        backend: SmallLlm,
        request: crate::host::HostLlmChatRequest,
        response: crate::host::HostLlmChatOutput,
        description: "Chat with the host-configured small LLM",
        supports_stream: true,
        cancelable: true,
    }
    NetworkClient {
        name: "astrcode.network.client",
        required: Some(ExtensionCapability::NetworkClient),
        context: None,
        group: Network,
        backend: NetworkService,
        request: crate::host::HostNetworkRequest,
        response: crate::host::HostNetworkResponse,
        description: "Send a bounded outbound HTTP or HTTPS request with a binary body",
        cancelable: true,
    }
    ProcessSpawn {
        name: "astrcode.process.spawn",
        required: Some(ExtensionCapability::ProcessSpawn),
        context: Workspace,
        group: Process,
        backend: ProcessWorkingDir,
        request: crate::host::HostProcessRequest,
        response: crate::host::HostProcessOutput,
        description: "Run a bounded subprocess with an optional workspace-relative cwd",
        cancelable: true,
    }
    SessionControlCancelTurn {
        name: "astrcode.session.control.cancel_turn",
        required: Some(ExtensionCapability::SessionControl),
        context: Session,
        group: Session,
        backend: SessionOperations,
        request: crate::session::HostSessionTargetRequest,
        response: crate::host::HostSessionCancelOutput,
        description: "Cancel the active turn",
    }
    SessionControlConfigureTools {
        name: "astrcode.session.control.configure_tools",
        required: Some(ExtensionCapability::SessionControl),
        context: Session,
        group: Session,
        backend: SessionOperations,
        request: crate::host::HostConfigureSessionToolsRequest,
        response: crate::host::HostConfigureSessionToolsOutput,
        description: "Configure the tool-name boundary used by subsequent session turns",
    }
    SessionControlCreate {
        name: "astrcode.session.control.create",
        required: Some(ExtensionCapability::SessionControl),
        context: Session,
        group: Session,
        backend: SessionOperations,
        request: crate::session::HostCreateSessionRequest,
        response: crate::session::HostCreateSessionOutput,
        description: "Create a child session",
    }
    SessionControlDispose {
        name: "astrcode.session.control.dispose",
        required: Some(ExtensionCapability::SessionControl),
        context: Session,
        group: Session,
        backend: SessionOperations,
        request: crate::session::HostRecycleSessionRequest,
        response: crate::host::Acknowledgement,
        description: "Recycle a session while preserving its durable data",
    }
    SessionControlExecutionView {
        name: "astrcode.session.control.execution_view",
        required: Some(ExtensionCapability::SessionControl),
        context: Session,
        group: Session,
        backend: SessionOperations,
        request: crate::session::HostSessionTargetRequest,
        response: crate::host::HostSessionExecutionView,
        description: "Read active turn and queued-input state",
    }
    SessionControlInjectOrStart {
        name: "astrcode.session.control.inject_or_start",
        required: Some(ExtensionCapability::SessionControl),
        context: Session,
        group: Session,
        backend: SessionOperations,
        request: crate::host::HostSessionInputRequest,
        response: crate::host::HostSessionDeliveryOutput,
        description: "Inject input into a running turn or start when idle",
    }
    SessionControlInterruptAndSubmit {
        name: "astrcode.session.control.interrupt_and_submit",
        required: Some(ExtensionCapability::SessionControl),
        context: Session,
        group: Session,
        backend: SessionOperations,
        request: crate::host::HostSessionInputRequest,
        response: crate::host::HostSessionDeliveryOutput,
        description: "Interrupt the active turn and submit new input",
    }
    SessionControlReactivate {
        name: "astrcode.session.control.reactivate",
        required: Some(ExtensionCapability::SessionControl),
        context: Session,
        group: Session,
        backend: SessionOperations,
        request: crate::session::HostSessionTargetRequest,
        response: crate::session::HostSessionReactivateOutput,
        description: "Reactivate a recycled direct child session",
    }
    SessionControlState {
        name: "astrcode.session.control.state",
        required: Some(ExtensionCapability::SessionControl),
        context: Session,
        group: Session,
        backend: SessionOperations,
        request: crate::session::HostSessionTargetRequest,
        response: crate::session::HostSessionStateOutput,
        description: "Read active or recycled session lifecycle state",
    }
    SessionControlSubmitTurn {
        name: "astrcode.session.control.submit_turn",
        required: Some(ExtensionCapability::SessionControl),
        context: Session,
        group: Session,
        backend: SessionOperations,
        request: crate::session::HostSubmitTurnRequest,
        response: crate::session::HostSubmitTurnOutput,
        description: "Submit a turn to a session",
    }
    SessionHistoryList {
        name: "astrcode.session.history.list",
        required: Some(ExtensionCapability::SessionHistory),
        context: Session,
        group: Session,
        backend: SessionReader,
        request: crate::host::EmptyRequest,
        response: crate::host::HostSessionSummariesOutput,
        description: "List stable session summaries visible to session-history consumers",
    }
    SessionHistoryProviderMessages {
        name: "astrcode.session.history.provider_messages",
        required: Some(ExtensionCapability::SessionHistory),
        context: Session,
        group: Session,
        backend: SessionReader,
        request: crate::session::HostSessionTargetRequest,
        response: crate::host::HostSessionProviderMessagesOutput,
        description: "Read provider-visible messages from a session transcript",
    }
    SessionHistorySnapshot {
        name: "astrcode.session.history.snapshot",
        required: Some(ExtensionCapability::SessionHistory),
        context: Session,
        group: Session,
        backend: SessionReader,
        request: crate::session::HostSessionTargetRequest,
        response: crate::session_inspect::SessionHistorySnapshotOutput,
        description: "Read an authorized active or recycled session snapshot",
    }
    SessionHistoryTokenUsage {
        name: "astrcode.session.history.token_usage",
        required: Some(ExtensionCapability::SessionHistory),
        context: Session,
        group: Session,
        backend: SessionEventReader,
        request: crate::session::HostSessionTargetRequest,
        response: crate::host::HostSessionTokenUsageOutput,
        description: "Read accumulated non-cached token usage for a session",
    }
    SessionHistoryTranscript {
        name: "astrcode.session.history.transcript",
        required: Some(ExtensionCapability::SessionHistory),
        context: Session,
        group: Session,
        backend: SessionReader,
        request: crate::session::HostSessionTargetRequest,
        response: crate::host::HostSessionTranscript,
        description: "Read the extension-visible transcript for a session",
    }
    SessionInspectList {
        name: "astrcode.session.inspect.list",
        required: Some(ExtensionCapability::SessionInspect),
        context: None,
        group: Session,
        backend: SessionReader,
        request: crate::host::EmptyRequest,
        response: crate::session_inspect::SessionInspectListOutput,
        description: "List all sessions visible to the host (global privileged access)",
    }
    SessionInspectProviderMessages {
        name: "astrcode.session.inspect.provider_messages",
        required: Some(ExtensionCapability::SessionInspect),
        context: None,
        group: Session,
        backend: SessionReader,
        request: crate::session_inspect::HostSessionInspectRequest,
        response: crate::session_inspect::SessionInspectProviderMessagesOutput,
        description: "Read provider-visible messages for any host-visible session",
    }
    SessionInspectReadModel {
        name: "astrcode.session.inspect.read_model",
        required: Some(ExtensionCapability::SessionInspect),
        context: None,
        group: Session,
        backend: SessionReader,
        request: crate::session_inspect::HostSessionInspectRequest,
        response: crate::session_inspect::SessionInspectReadModelOutput,
        description: "Read any host-visible projected session model through a stable wire DTO",
    }
    SessionInspectSnapshot {
        name: "astrcode.session.inspect.snapshot",
        required: Some(ExtensionCapability::SessionInspect),
        context: None,
        group: Session,
        backend: SessionReader,
        request: crate::session_inspect::HostSessionInspectRequest,
        response: crate::session_inspect::SessionInspectSnapshotOutput,
        description: "Read any host-visible session snapshot (global privileged access)",
    }
    SessionReadEvents {
        name: "astrcode.session.read_events",
        required: Some(ExtensionCapability::SessionHistory),
        context: Session,
        group: Session,
        backend: SessionEventReader,
        request: crate::session::HostSessionEventsPageRequest,
        response: crate::session::HostSessionEventsPageOutput,
        description: "Read a cursor page from the durable session event log",
    }
    SessionRootCreate {
        name: "astrcode.session.root.create",
        required: Some(ExtensionCapability::InputDelivery),
        context: Workspace,
        group: Session,
        backend: SessionOperations,
        request: crate::host::EmptyRequest,
        response: crate::session::HostCreateSessionOutput,
        description: "Create a top-level session attributed to the calling extension",
    }
    SessionRootState {
        name: "astrcode.session.root.state",
        required: Some(ExtensionCapability::InputDelivery),
        context: None,
        group: Session,
        backend: SessionOperationsAndReader,
        request: crate::session::HostSessionTargetRequest,
        response: crate::session::HostSessionStateOutput,
        description: "Read an owned top-level session lifecycle state",
    }
    SessionRootSubmitTurn {
        name: "astrcode.session.root.submit_turn",
        required: Some(ExtensionCapability::InputDelivery),
        context: None,
        group: Session,
        backend: SessionOperationsAndReader,
        request: crate::session::HostRootSubmitTurnRequest,
        response: crate::session::HostSubmitTurnOutput,
        description: "Submit a turn to an owned top-level session",
    }
    SessionStateRead {
        name: "astrcode.session.state.read",
        required: None,
        context: Session,
        group: Context,
        backend: SessionStoreDir,
        request: crate::host::HostSessionStateReadRequest,
        response: crate::host::HostSessionStateReadOutput,
        description: "Read extension-namespaced session state",
    }
    SessionStateWrite {
        name: "astrcode.session.state.write",
        required: None,
        context: Session,
        group: Context,
        backend: SessionStoreDirAndTasks,
        request: crate::host::HostSessionStateWriteRequest,
        response: crate::host::Acknowledgement,
        description: "Write extension-namespaced session state",
    }
    WorkspaceEdit {
        name: "astrcode.workspace.edit",
        required: Some(ExtensionCapability::WorkspaceWrite),
        context: Workspace,
        group: Workspace,
        backend: WorkspaceDirAndTasks,
        request: crate::host::HostWorkspaceEditRequest,
        response: crate::host::HostWorkspaceEditOutput,
        description: "Replace an exact text fragment in a non-sensitive workspace file",
    }
    WorkspaceGlob {
        name: "astrcode.workspace.glob",
        required: Some(ExtensionCapability::WorkspaceRead),
        context: Workspace,
        group: Workspace,
        backend: WorkspaceDir,
        request: crate::host::HostWorkspaceGlobRequest,
        response: crate::host::HostWorkspaceGlobOutput,
        description: "Match bounded workspace paths by glob",
    }
    WorkspaceGrep {
        name: "astrcode.workspace.grep",
        required: Some(ExtensionCapability::WorkspaceRead),
        context: Workspace,
        group: Workspace,
        backend: WorkspaceDir,
        request: crate::host::HostWorkspaceGrepRequest,
        response: crate::host::HostWorkspaceGrepOutput,
        description: "Regex-search bounded UTF-8 workspace files",
    }
    WorkspaceList {
        name: "astrcode.workspace.list",
        required: Some(ExtensionCapability::WorkspaceRead),
        context: Workspace,
        group: Workspace,
        backend: WorkspaceDir,
        request: crate::host::HostWorkspaceListRequest,
        response: crate::host::HostWorkspaceListOutput,
        description: "List a bounded workspace directory tree",
    }
    WorkspaceRead {
        name: "astrcode.workspace.read",
        required: Some(ExtensionCapability::WorkspaceRead),
        context: Workspace,
        group: Workspace,
        backend: WorkspaceDir,
        request: crate::host::HostWorkspaceReadRequest,
        response: crate::host::HostWorkspaceReadOutput,
        description: "Read a bounded UTF-8 workspace file",
    }
    WorkspaceWrite {
        name: "astrcode.workspace.write",
        required: Some(ExtensionCapability::WorkspaceWrite),
        context: Workspace,
        group: Workspace,
        backend: WorkspaceDirAndTasks,
        request: crate::host::HostWorkspaceWriteRequest,
        response: crate::host::HostWorkspaceWriteOutput,
        description: "Create or replace a non-sensitive file under the working directory",
    }
}

/// Session/workspace context an operation requires the host to resolve before dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostContextRequirement {
    None,
    Session,
    Workspace,
}

/// Dispatch group the HostRouter routes an operation to; one arm per backend state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostOperationGroup {
    Llm,
    Session,
    Context,
    Workspace,
    Process,
    Network,
    ExtensionHttp,
}

/// Backend availability predicate for an operation: the concrete host-side dependency the
/// HostRouter checks before dispatching, one variant per injectable backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostBackendRequirement {
    MainLlm,
    SmallLlm,
    SessionEventReader,
    SessionReader,
    SessionOperations,
    SessionOperationsAndReader,
    SessionStoreDir,
    SessionStoreDirAndTasks,
    EventSender,
    WorkspaceDir,
    WorkspaceDirAndTasks,
    ProcessWorkingDir,
    NetworkService,
    PublicHttpDispatcher,
}

/// Metadata shared by authoring clients, authorization, host dispatch, and the S5R capability
/// catalog.
#[derive(Debug, Clone, Copy)]
pub struct HostOperationSpec {
    pub operation: HostOperation,
    pub name: &'static str,
    pub required: Option<ExtensionCapability>,
    pub context: HostContextRequirement,
    pub group: HostOperationGroup,
    pub backend: HostBackendRequirement,
    pub description: &'static str,
    pub supports_stream: bool,
    pub cancelable: bool,
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn operation_catalog_is_exhaustive_and_round_trips() {
        assert_eq!(HOST_OPERATION_SPECS.len(), HostOperation::COUNT);

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
    fn operation_spec_policy_fields_are_consistent() {
        for spec in HOST_OPERATION_SPECS {
            let operation = spec.operation;
            assert!(!spec.name.is_empty(), "{operation:?} wire name");
            assert!(!spec.description.is_empty(), "{operation:?} description");
            assert_eq!(
                operation.required_capability(),
                spec.required,
                "{operation:?} capability accessor"
            );
            assert!(
                !spec.supports_stream || spec.cancelable,
                "{operation:?} streaming operations must be cancelable"
            );
            assert_eq!(
                spec.supports_stream,
                spec.group == HostOperationGroup::Llm,
                "{operation:?} only the llm group has a stream handler"
            );
        }
    }
}
