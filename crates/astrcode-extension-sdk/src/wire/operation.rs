use crate::wire::ExtensionCapability;

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
    label = "use a marker from astrcode_extension_sdk::wire::operation::operations"
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
        request: crate::wire::host::HostEventEmitRequest,
        response: crate::wire::host::HostEventEmitOutput,
        description: "Emit a declared extension event",
    }
    ExtensionHttpPublic {
        name: "astrcode.extension.http.public",
        required: Some(ExtensionCapability::PublicHttpDispatch),
        context: None,
        group: ExtensionHttp,
        backend: PublicHttpDispatcher,
        request: crate::wire::extension_http::ExtensionHttpDispatchRequest,
        response: crate::wire::extension_http::ExtensionHttpResponse,
        description: "Dispatch a request to another extension's public HTTP route",
    }
    LlmMainChat {
        name: "astrcode.llm.main_chat",
        required: Some(ExtensionCapability::MainModel),
        context: None,
        group: Llm,
        backend: MainLlm,
        request: crate::wire::host::HostLlmChatRequest,
        response: crate::wire::host::HostLlmChatOutput,
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
        request: crate::wire::host::HostLlmChatRequest,
        response: crate::wire::host::HostLlmChatOutput,
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
        request: crate::wire::host::HostNetworkRequest,
        response: crate::wire::host::HostNetworkResponse,
        description: "Send a bounded outbound HTTP or HTTPS request with a binary body",
        cancelable: true,
    }
    ProcessSpawn {
        name: "astrcode.process.spawn",
        required: Some(ExtensionCapability::ProcessSpawn),
        context: Workspace,
        group: Process,
        backend: ProcessWorkingDir,
        request: crate::wire::host::HostProcessRequest,
        response: crate::wire::host::HostProcessOutput,
        description: "Run a bounded subprocess with an optional workspace-relative cwd",
        cancelable: true,
    }
    ProcessStart {
        name: "astrcode.process.start",
        required: Some(ExtensionCapability::ProcessSpawn),
        context: Workspace,
        group: Process,
        backend: ProcessWorkingDir,
        request: crate::wire::host::HostProcessStartRequest,
        response: crate::wire::host::HostProcessHandleOutput,
        description: "Start a session-owned process and return an opaque handle",
    }
    ProcessRead {
        name: "astrcode.process.read",
        required: Some(ExtensionCapability::ProcessSpawn),
        context: Session,
        group: Process,
        backend: ProcessWorkingDir,
        request: crate::wire::host::HostProcessReadRequest,
        response: crate::wire::host::HostProcessReadOutput,
        description: "Read incremental output from a session-owned process",
        cancelable: true,
    }
    ProcessInput {
        name: "astrcode.process.input",
        required: Some(ExtensionCapability::ProcessSpawn),
        context: Session,
        group: Process,
        backend: ProcessWorkingDir,
        request: crate::wire::host::HostProcessInputRequest,
        response: crate::wire::host::Acknowledgement,
        description: "Write or close stdin for a session-owned process",
    }
    ProcessStatus {
        name: "astrcode.process.status",
        required: Some(ExtensionCapability::ProcessSpawn),
        context: Session,
        group: Process,
        backend: ProcessWorkingDir,
        request: crate::wire::host::HostProcessTargetRequest,
        response: crate::wire::host::HostProcessStatusOutput,
        description: "Read process liveness and exit status",
    }
    ProcessPromote {
        name: "astrcode.process.promote",
        required: Some(ExtensionCapability::ProcessSpawn),
        context: Session,
        group: Process,
        backend: ProcessWorkingDir,
        request: crate::wire::host::HostProcessTargetRequest,
        response: crate::wire::host::Acknowledgement,
        description: "Promote a call-owned process to session ownership",
    }
    ProcessKill {
        name: "astrcode.process.kill",
        required: Some(ExtensionCapability::ProcessSpawn),
        context: Session,
        group: Process,
        backend: ProcessWorkingDir,
        request: crate::wire::host::HostProcessTargetRequest,
        response: crate::wire::host::Acknowledgement,
        description: "Terminate and reclaim a session-owned process",
    }
    ProcessList {
        name: "astrcode.process.list",
        required: Some(ExtensionCapability::ProcessSpawn),
        context: Session,
        group: Process,
        backend: ProcessWorkingDir,
        request: crate::wire::host::EmptyRequest,
        response: crate::wire::host::HostProcessListOutput,
        description: "List process handles owned by the current session and extension",
    }
    SessionControlCancelTurn {
        name: "astrcode.session.control.cancel_turn",
        required: Some(ExtensionCapability::SessionControl),
        context: Session,
        group: Session,
        backend: SessionOperations,
        request: crate::wire::session::HostSessionTargetRequest,
        response: crate::wire::host::HostSessionCancelOutput,
        description: "Cancel the active turn",
    }
    SessionControlConfigureTools {
        name: "astrcode.session.control.configure_tools",
        required: Some(ExtensionCapability::SessionControl),
        context: Session,
        group: Session,
        backend: SessionOperations,
        request: crate::wire::host::HostConfigureSessionToolsRequest,
        response: crate::wire::host::HostConfigureSessionToolsOutput,
        description: "Configure the tool-name boundary used by subsequent session turns",
    }
    SessionControlCreate {
        name: "astrcode.session.control.create",
        required: Some(ExtensionCapability::SessionControl),
        context: Session,
        group: Session,
        backend: SessionOperations,
        request: crate::wire::session::HostCreateSessionRequest,
        response: crate::wire::session::HostCreateSessionOutput,
        description: "Create a child session",
    }
    SessionControlDeferContext {
        name: "astrcode.session.control.defer_context",
        required: Some(ExtensionCapability::SessionControl),
        context: Session,
        group: Session,
        backend: SessionOperations,
        request: crate::wire::host::HostSessionInputRequest,
        response: crate::wire::host::HostSessionDeliveryOutput,
        description: "Append input to the active turn without starting or queueing a new one",
    }
    SessionControlDispose {
        name: "astrcode.session.control.dispose",
        required: Some(ExtensionCapability::SessionControl),
        context: Session,
        group: Session,
        backend: SessionOperations,
        request: crate::wire::session::HostRecycleSessionRequest,
        response: crate::wire::host::Acknowledgement,
        description: "Recycle a session while preserving its durable data",
    }
    SessionControlExecutionView {
        name: "astrcode.session.control.execution_view",
        required: Some(ExtensionCapability::SessionControl),
        context: Session,
        group: Session,
        backend: SessionOperations,
        request: crate::wire::session::HostSessionTargetRequest,
        response: crate::wire::host::HostSessionExecutionView,
        description: "Read active turn and queued-input state",
    }
    SessionControlInjectOrStart {
        name: "astrcode.session.control.inject_or_start",
        required: Some(ExtensionCapability::SessionControl),
        context: Session,
        group: Session,
        backend: SessionOperations,
        request: crate::wire::host::HostSessionInputRequest,
        response: crate::wire::host::HostSessionDeliveryOutput,
        description: "Inject input into a running turn or start when idle",
    }
    SessionControlInterruptAndSubmit {
        name: "astrcode.session.control.interrupt_and_submit",
        required: Some(ExtensionCapability::SessionControl),
        context: Session,
        group: Session,
        backend: SessionOperations,
        request: crate::wire::host::HostSessionInputRequest,
        response: crate::wire::host::HostSessionDeliveryOutput,
        description: "Interrupt the active turn and submit new input",
    }
    SessionControlQueueOrStart {
        name: "astrcode.session.control.queue_or_start",
        required: Some(ExtensionCapability::SessionControl),
        context: Session,
        group: Session,
        backend: SessionOperations,
        request: crate::wire::host::HostSessionInputRequest,
        response: crate::wire::host::HostSessionDeliveryOutput,
        description: "Queue input behind the active turn or start a turn when idle",
    }
    SessionControlReactivate {
        name: "astrcode.session.control.reactivate",
        required: Some(ExtensionCapability::SessionControl),
        context: Session,
        group: Session,
        backend: SessionOperations,
        request: crate::wire::session::HostSessionTargetRequest,
        response: crate::wire::session::HostSessionReactivateOutput,
        description: "Reactivate a recycled direct child session",
    }
    SessionControlState {
        name: "astrcode.session.control.state",
        required: Some(ExtensionCapability::SessionControl),
        context: Session,
        group: Session,
        backend: SessionOperations,
        request: crate::wire::session::HostSessionTargetRequest,
        response: crate::wire::session::HostSessionStateOutput,
        description: "Read active or recycled session lifecycle state",
    }
    SessionControlSubmitTurn {
        name: "astrcode.session.control.submit_turn",
        required: Some(ExtensionCapability::SessionControl),
        context: Session,
        group: Session,
        backend: SessionOperations,
        request: crate::wire::session::HostSubmitTurnRequest,
        response: crate::wire::session::HostSubmitTurnOutput,
        description: "Submit a turn to a session",
    }
    SessionHistoryList {
        name: "astrcode.session.history.list",
        required: Some(ExtensionCapability::SessionHistory),
        context: Session,
        group: Session,
        backend: SessionReader,
        request: crate::wire::host::EmptyRequest,
        response: crate::wire::host::HostSessionSummariesOutput,
        description: "List stable session summaries visible to session-history consumers",
    }
    SessionHistoryProviderMessages {
        name: "astrcode.session.history.provider_messages",
        required: Some(ExtensionCapability::SessionHistory),
        context: Session,
        group: Session,
        backend: SessionReader,
        request: crate::wire::session::HostSessionTargetRequest,
        response: crate::wire::host::HostSessionProviderMessagesOutput,
        description: "Read provider-visible messages from a session transcript",
    }
    SessionHistorySnapshot {
        name: "astrcode.session.history.snapshot",
        required: Some(ExtensionCapability::SessionHistory),
        context: Session,
        group: Session,
        backend: SessionReader,
        request: crate::wire::session::HostSessionTargetRequest,
        response: crate::wire::session_inspect::SessionHistorySnapshotOutput,
        description: "Read an authorized active or recycled session snapshot",
    }
    SessionHistoryTokenUsage {
        name: "astrcode.session.history.token_usage",
        required: Some(ExtensionCapability::SessionHistory),
        context: Session,
        group: Session,
        backend: SessionEventReader,
        request: crate::wire::session::HostSessionTargetRequest,
        response: crate::wire::host::HostSessionTokenUsageOutput,
        description: "Read accumulated non-cached token usage for a session",
    }
    SessionHistoryTranscript {
        name: "astrcode.session.history.transcript",
        required: Some(ExtensionCapability::SessionHistory),
        context: Session,
        group: Session,
        backend: SessionReader,
        request: crate::wire::session::HostSessionTargetRequest,
        response: crate::wire::host::HostSessionTranscript,
        description: "Read the extension-visible transcript for a session",
    }
    SessionInspectList {
        name: "astrcode.session.inspect.list",
        required: Some(ExtensionCapability::SessionInspect),
        context: None,
        group: Session,
        backend: SessionReader,
        request: crate::wire::host::EmptyRequest,
        response: crate::wire::session_inspect::SessionInspectListOutput,
        description: "List all sessions visible to the host (global privileged access)",
    }
    SessionInspectProviderMessages {
        name: "astrcode.session.inspect.provider_messages",
        required: Some(ExtensionCapability::SessionInspect),
        context: None,
        group: Session,
        backend: SessionReader,
        request: crate::wire::session_inspect::HostSessionInspectRequest,
        response: crate::wire::session_inspect::SessionInspectProviderMessagesOutput,
        description: "Read provider-visible messages for any host-visible session",
    }
    SessionInspectReadModel {
        name: "astrcode.session.inspect.read_model",
        required: Some(ExtensionCapability::SessionInspect),
        context: None,
        group: Session,
        backend: SessionReader,
        request: crate::wire::session_inspect::HostSessionInspectRequest,
        response: crate::wire::session_inspect::SessionInspectReadModelOutput,
        description: "Read any host-visible projected session model through a stable wire DTO",
    }
    SessionInspectSnapshot {
        name: "astrcode.session.inspect.snapshot",
        required: Some(ExtensionCapability::SessionInspect),
        context: None,
        group: Session,
        backend: SessionReader,
        request: crate::wire::session_inspect::HostSessionInspectRequest,
        response: crate::wire::session_inspect::SessionInspectSnapshotOutput,
        description: "Read any host-visible session snapshot (global privileged access)",
    }
    SessionReadEvents {
        name: "astrcode.session.read_events",
        required: Some(ExtensionCapability::SessionHistory),
        context: Session,
        group: Session,
        backend: SessionEventReader,
        request: crate::wire::session::HostSessionEventsPageRequest,
        response: crate::wire::session::HostSessionEventsPageOutput,
        description: "Read a cursor page from the durable session event log",
    }
    SessionRootCreate {
        name: "astrcode.session.root.create",
        required: Some(ExtensionCapability::InputDelivery),
        context: None,
        group: Session,
        backend: SessionOperations,
        request: crate::wire::session::HostCreateRootSessionRequest,
        response: crate::wire::session::HostCreateSessionOutput,
        description: "Create a top-level session attributed to the calling extension",
    }
    SessionRootState {
        name: "astrcode.session.root.state",
        required: Some(ExtensionCapability::InputDelivery),
        context: None,
        group: Session,
        backend: SessionOperationsAndReader,
        request: crate::wire::session::HostSessionTargetRequest,
        response: crate::wire::session::HostSessionStateOutput,
        description: "Read an owned top-level session lifecycle state",
    }
    SessionRootSubmitTurn {
        name: "astrcode.session.root.submit_turn",
        required: Some(ExtensionCapability::InputDelivery),
        context: None,
        group: Session,
        backend: SessionOperationsAndReader,
        request: crate::wire::session::HostRootSubmitTurnRequest,
        response: crate::wire::session::HostSubmitTurnOutput,
        description: "Submit a turn to an owned top-level session",
    }
    SessionStateRead {
        name: "astrcode.session.state.read",
        required: None,
        context: Session,
        group: Context,
        backend: SessionStoreDir,
        request: crate::wire::host::HostSessionStateReadRequest,
        response: crate::wire::host::HostSessionStateReadOutput,
        description: "Read extension-namespaced session state",
    }
    SessionStateWrite {
        name: "astrcode.session.state.write",
        required: None,
        context: Session,
        group: Context,
        backend: SessionStoreDirAndTasks,
        request: crate::wire::host::HostSessionStateWriteRequest,
        response: crate::wire::host::Acknowledgement,
        description: "Write extension-namespaced session state",
    }
    WorkspaceEdit {
        name: "astrcode.workspace.edit",
        required: Some(ExtensionCapability::WorkspaceWrite),
        context: Workspace,
        group: Workspace,
        backend: WorkspaceDirAndTasks,
        request: crate::wire::host::HostWorkspaceEditRequest,
        response: crate::wire::host::HostWorkspaceEditOutput,
        description: "Replace an exact text fragment in a non-sensitive workspace file",
    }
    WorkspaceApplyPatch {
        name: "astrcode.workspace.apply_patch",
        required: Some(ExtensionCapability::WorkspaceWrite),
        context: Workspace,
        group: Workspace,
        backend: WorkspaceDirAndTasks,
        request: crate::wire::host::HostWorkspaceApplyPatchRequest,
        response: crate::wire::host::HostWorkspaceApplyPatchOutput,
        description: "Apply a bounded unified diff to non-sensitive workspace files",
    }
    WorkspaceGlob {
        name: "astrcode.workspace.glob",
        required: Some(ExtensionCapability::WorkspaceRead),
        context: Workspace,
        group: Workspace,
        backend: WorkspaceDir,
        request: crate::wire::host::HostWorkspaceGlobRequest,
        response: crate::wire::host::HostWorkspaceGlobOutput,
        description: "Match bounded workspace paths by glob",
    }
    WorkspaceGrep {
        name: "astrcode.workspace.grep",
        required: Some(ExtensionCapability::WorkspaceRead),
        context: Workspace,
        group: Workspace,
        backend: WorkspaceDir,
        request: crate::wire::host::HostWorkspaceGrepRequest,
        response: crate::wire::host::HostWorkspaceGrepOutput,
        description: "Regex-search bounded UTF-8 workspace files",
    }
    WorkspaceList {
        name: "astrcode.workspace.list",
        required: Some(ExtensionCapability::WorkspaceRead),
        context: Workspace,
        group: Workspace,
        backend: WorkspaceDir,
        request: crate::wire::host::HostWorkspaceListRequest,
        response: crate::wire::host::HostWorkspaceListOutput,
        description: "List a bounded workspace directory tree",
    }
    WorkspaceRead {
        name: "astrcode.workspace.read",
        required: Some(ExtensionCapability::WorkspaceRead),
        context: Workspace,
        group: Workspace,
        backend: WorkspaceDir,
        request: crate::wire::host::HostWorkspaceReadRequest,
        response: crate::wire::host::HostWorkspaceReadOutput,
        description: "Read a bounded UTF-8 workspace file",
    }
    ToolResultRead {
        name: "astrcode.tool_result.read",
        required: Some(ExtensionCapability::ToolResultRead),
        context: Session,
        group: ToolResult,
        backend: ToolResultReader,
        request: crate::wire::host::HostToolResultReadRequest,
        response: crate::wire::host::HostToolResultReadOutput,
        description: "Read a bounded slice from a session-owned tool-result artifact",
    }
    WorkspaceWrite {
        name: "astrcode.workspace.write",
        required: Some(ExtensionCapability::WorkspaceWrite),
        context: Workspace,
        group: Workspace,
        backend: WorkspaceDirAndTasks,
        request: crate::wire::host::HostWorkspaceWriteRequest,
        response: crate::wire::host::HostWorkspaceWriteOutput,
        description: "Create or replace a non-sensitive file under the working directory",
    }
    SessionRootDispose {
        name: "astrcode.session.root.dispose",
        required: Some(ExtensionCapability::InputDelivery),
        context: None,
        group: Session,
        backend: SessionOperationsAndReader,
        request: crate::wire::session::HostSessionTargetRequest,
        response: crate::wire::host::Acknowledgement,
        description: "Recycle an owned top-level session",
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
    ToolResult,
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
    ToolResultReader,
    ProcessWorkingDir,
    NetworkService,
    PublicHttpDispatcher,
}

/// Canonical metadata shared by authorization, host dispatch, and the S5R operation catalog.
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
    fn root_session_domain_specs_match_the_extension_owned_model() {
        let create = HostOperation::SessionRootCreate.spec();
        assert_eq!(create.name, "astrcode.session.root.create");
        assert_eq!(create.required, Some(ExtensionCapability::InputDelivery));
        assert_eq!(create.context, HostContextRequirement::None);
        assert_eq!(create.backend, HostBackendRequirement::SessionOperations);

        let dispose = HostOperation::SessionRootDispose.spec();
        assert_eq!(dispose.name, "astrcode.session.root.dispose");
        assert_eq!(dispose.required, Some(ExtensionCapability::InputDelivery));
        assert_eq!(dispose.context, HostContextRequirement::None);
        assert_eq!(dispose.group, HostOperationGroup::Session);
        assert_eq!(
            dispose.backend,
            HostBackendRequirement::SessionOperationsAndReader
        );
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
