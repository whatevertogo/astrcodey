use serde_json::{Value, json};

use super::contracts::HostAcknowledgement;
use crate::{
    extension::{ExtensionCapability, ExtensionHttpRequest, ExtensionHttpResponse},
    host::{
        HostConfigureSessionToolsOutput, HostConfigureSessionToolsRequest, HostLlmChatRequest,
        HostNetworkRequest, HostNetworkResponse, HostProcessOutput, HostProcessRequest,
        HostSessionCancelOutput, HostSessionDeliveryOutput, HostSessionExecutionView,
        HostSessionInputRequest, HostSessionProviderMessagesOutput, HostSessionSummariesOutput,
        HostSessionTokenUsageOutput, HostSessionTranscript, HostWorkspaceEditOutput,
        HostWorkspaceEditRequest, HostWorkspaceGlobOutput, HostWorkspaceGlobRequest,
        HostWorkspaceGrepOutput, HostWorkspaceGrepRequest, HostWorkspaceListOutput,
        HostWorkspaceListRequest, HostWorkspaceReadOutput, HostWorkspaceReadRequest,
        HostWorkspaceWriteOutput, HostWorkspaceWriteRequest, host_llm_chat_response_schema,
    },
    s5r::CapabilityDescriptor,
    session::{
        HostCreateSessionOutput, HostCreateSessionRequest, HostRecycleSessionRequest,
        HostRootSubmitTurnRequest, HostSessionEventsPageOutput, HostSessionEventsPageRequest,
        HostSessionReactivateOutput, HostSessionStateOutput, HostSessionTargetRequest,
        HostSubmitTurnOutput, HostSubmitTurnRequest,
    },
    session_inspect::{
        SessionHistorySnapshotOutput, SessionInspectListOutput,
        SessionInspectProviderMessagesOutput, SessionInspectReadModelOutput,
        SessionInspectSnapshotOutput,
    },
};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapabilitySchema {
    Object,
    EmptyObject,
    Acknowledgement,
    EventEmitRequest,
    ExtensionHttpRequest,
    ExtensionHttpResponse,
    LlmChatRequest,
    LlmChatOutput,
    NetworkRequest,
    NetworkResponse,
    ProcessSpawn,
    ProcessSpawnOutput,
    SessionId,
    SessionEventsPage,
    SessionEventsPageOutput,
    SessionCancelOutput,
    SessionDeliveryOutput,
    SessionExecutionView,
    SessionHistorySnapshotOutput,
    SessionInput,
    SessionInspectListOutput,
    SessionInspectProviderMessagesOutput,
    SessionInspectReadModelOutput,
    SessionInspectSnapshotOutput,
    SessionSummariesOutput,
    SessionTranscriptOutput,
    SessionProviderMessagesOutput,
    SessionTokenUsageOutput,
    SessionCreate,
    SessionCreateOutput,
    SessionTarget,
    SessionStateOutput,
    SessionReactivateOutput,
    SessionSubmitTurn,
    SessionSubmitTurnOutput,
    SessionRootSubmitTurn,
    SessionRecycle,
    SessionToolSelection,
    SessionToolSelectionOutput,
    WorkspaceEdit,
    WorkspaceEditOutput,
    WorkspaceGlob,
    WorkspaceGlobOutput,
    WorkspaceGrep,
    WorkspaceGrepOutput,
    WorkspaceList,
    WorkspaceListOutput,
    WorkspaceRead,
    WorkspaceReadOutput,
    WorkspaceWrite,
    WorkspaceWriteOutput,
}

/// Metadata shared by authoring clients, authorization, and the S5R capability catalog.
#[derive(Debug, Clone, Copy)]
pub struct HostOperationSpec {
    pub operation: HostOperation,
    pub name: &'static str,
    pub required: Option<ExtensionCapability>,
    pub description: &'static str,
    input_schema: CapabilitySchema,
    output_schema: CapabilitySchema,
    pub supports_stream: bool,
    pub cancelable: bool,
    pub catalog: bool,
}

impl HostOperationSpec {
    pub fn input_schema(self) -> Value {
        capability_schema(self.input_schema)
    }

    pub fn output_schema(self) -> Value {
        capability_schema(self.output_schema)
    }

    pub fn descriptor(self) -> CapabilityDescriptor {
        CapabilityDescriptor {
            name: self.name.into(),
            description: self.description.into(),
            input_schema: self.input_schema(),
            output_schema: self.output_schema(),
            supports_stream: self.supports_stream,
            cancelable: self.cancelable,
        }
    }
}

macro_rules! spec {
    (
        $operation:ident,
        $name:literal,
        $required:expr,
        $description:literal,
        $input_schema:ident,
        $output_schema:ident
    ) => {
        HostOperationSpec {
            operation: HostOperation::$operation,
            name: $name,
            required: $required,
            description: $description,
            input_schema: CapabilitySchema::$input_schema,
            output_schema: CapabilitySchema::$output_schema,
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
        EventEmitRequest,
        Acknowledgement
    ),
    spec!(
        ExtensionHttpPublic,
        "astrcode.extension.http.public",
        Some(ExtensionCapability::PublicHttpDispatch),
        "Dispatch a request to another extension's public HTTP route",
        ExtensionHttpRequest,
        ExtensionHttpResponse
    ),
    HostOperationSpec {
        operation: HostOperation::LlmMainChat,
        name: "astrcode.llm.main_chat",
        required: Some(ExtensionCapability::MainModel),
        description: "Chat with the host-configured live main LLM provider",
        input_schema: CapabilitySchema::LlmChatRequest,
        output_schema: CapabilitySchema::LlmChatOutput,
        supports_stream: true,
        cancelable: true,
        catalog: true,
    },
    HostOperationSpec {
        operation: HostOperation::LlmSmallChat,
        name: "astrcode.llm.small_chat",
        required: Some(ExtensionCapability::SmallModel),
        description: "Chat with the host-configured small LLM",
        input_schema: CapabilitySchema::LlmChatRequest,
        output_schema: CapabilitySchema::LlmChatOutput,
        supports_stream: true,
        cancelable: true,
        catalog: true,
    },
    HostOperationSpec {
        operation: HostOperation::NetworkClient,
        name: "astrcode.network.client",
        required: Some(ExtensionCapability::NetworkClient),
        description: "Send a bounded outbound HTTP or HTTPS request with a binary body",
        input_schema: CapabilitySchema::NetworkRequest,
        output_schema: CapabilitySchema::NetworkResponse,
        supports_stream: false,
        cancelable: true,
        catalog: true,
    },
    HostOperationSpec {
        operation: HostOperation::ProcessSpawn,
        name: "astrcode.process.spawn",
        required: Some(ExtensionCapability::ProcessSpawn),
        description: "Run a bounded subprocess with an optional workspace-relative cwd",
        input_schema: CapabilitySchema::ProcessSpawn,
        output_schema: CapabilitySchema::ProcessSpawnOutput,
        supports_stream: false,
        cancelable: true,
        catalog: true,
    },
    spec!(
        SessionControlCancelTurn,
        "astrcode.session.control.cancel_turn",
        Some(ExtensionCapability::SessionControl),
        "Cancel the active turn",
        SessionTarget,
        SessionCancelOutput
    ),
    spec!(
        SessionControlConfigureTools,
        "astrcode.session.control.configure_tools",
        Some(ExtensionCapability::SessionControl),
        "Configure the tool-name boundary used by subsequent session turns",
        SessionToolSelection,
        SessionToolSelectionOutput
    ),
    spec!(
        SessionControlCreate,
        "astrcode.session.control.create",
        Some(ExtensionCapability::SessionControl),
        "Create a child session",
        SessionCreate,
        SessionCreateOutput
    ),
    spec!(
        SessionControlDispose,
        "astrcode.session.control.dispose",
        Some(ExtensionCapability::SessionControl),
        "Recycle a session while preserving its durable data",
        SessionRecycle,
        Acknowledgement
    ),
    spec!(
        SessionControlExecutionView,
        "astrcode.session.control.execution_view",
        Some(ExtensionCapability::SessionControl),
        "Read active turn and queued-input state",
        SessionTarget,
        SessionExecutionView
    ),
    spec!(
        SessionControlInjectOrStart,
        "astrcode.session.control.inject_or_start",
        Some(ExtensionCapability::SessionControl),
        "Inject input into a running turn or start when idle",
        SessionInput,
        SessionDeliveryOutput
    ),
    spec!(
        SessionControlInterruptAndSubmit,
        "astrcode.session.control.interrupt_and_submit",
        Some(ExtensionCapability::SessionControl),
        "Interrupt the active turn and submit new input",
        SessionInput,
        SessionDeliveryOutput
    ),
    spec!(
        SessionControlReactivate,
        "astrcode.session.control.reactivate",
        Some(ExtensionCapability::SessionControl),
        "Reactivate a recycled direct child session",
        SessionTarget,
        SessionReactivateOutput
    ),
    spec!(
        SessionControlState,
        "astrcode.session.control.state",
        Some(ExtensionCapability::SessionControl),
        "Read active or recycled session lifecycle state",
        SessionTarget,
        SessionStateOutput
    ),
    spec!(
        SessionControlSubmitTurn,
        "astrcode.session.control.submit_turn",
        Some(ExtensionCapability::SessionControl),
        "Submit a turn to a session",
        SessionSubmitTurn,
        SessionSubmitTurnOutput
    ),
    spec!(
        SessionHistoryList,
        "astrcode.session.history.list",
        Some(ExtensionCapability::SessionHistory),
        "List stable session summaries visible to session-history consumers",
        EmptyObject,
        SessionSummariesOutput
    ),
    spec!(
        SessionHistoryProviderMessages,
        "astrcode.session.history.provider_messages",
        Some(ExtensionCapability::SessionHistory),
        "Read provider-visible messages from a session transcript",
        SessionTarget,
        SessionProviderMessagesOutput
    ),
    spec!(
        SessionHistorySnapshot,
        "astrcode.session.history.snapshot",
        Some(ExtensionCapability::SessionHistory),
        "Read an authorized active or recycled session snapshot",
        SessionTarget,
        SessionHistorySnapshotOutput
    ),
    spec!(
        SessionHistoryTokenUsage,
        "astrcode.session.history.token_usage",
        Some(ExtensionCapability::SessionHistory),
        "Read accumulated non-cached token usage for a session",
        SessionTarget,
        SessionTokenUsageOutput
    ),
    spec!(
        SessionHistoryTranscript,
        "astrcode.session.history.transcript",
        Some(ExtensionCapability::SessionHistory),
        "Read the extension-visible transcript for a session",
        SessionTarget,
        SessionTranscriptOutput
    ),
    spec!(
        SessionInspectList,
        "astrcode.session.inspect.list",
        Some(ExtensionCapability::SessionInspect),
        "List all sessions visible to the host (global privileged access)",
        EmptyObject,
        SessionInspectListOutput
    ),
    spec!(
        SessionInspectProviderMessages,
        "astrcode.session.inspect.provider_messages",
        Some(ExtensionCapability::SessionInspect),
        "Read provider-visible messages for any host-visible session",
        SessionId,
        SessionInspectProviderMessagesOutput
    ),
    spec!(
        SessionInspectReadModel,
        "astrcode.session.inspect.read_model",
        Some(ExtensionCapability::SessionInspect),
        "Read any host-visible projected session model through a stable wire DTO",
        SessionId,
        SessionInspectReadModelOutput
    ),
    spec!(
        SessionInspectSnapshot,
        "astrcode.session.inspect.snapshot",
        Some(ExtensionCapability::SessionInspect),
        "Read any host-visible session snapshot (global privileged access)",
        SessionId,
        SessionInspectSnapshotOutput
    ),
    spec!(
        SessionReadEvents,
        "astrcode.session.read_events",
        Some(ExtensionCapability::SessionHistory),
        "Read a cursor page from the durable session event log",
        SessionEventsPage,
        SessionEventsPageOutput
    ),
    spec!(
        SessionRootCreate,
        "astrcode.session.root.create",
        Some(ExtensionCapability::InputDelivery),
        "Create a top-level session attributed to the calling extension",
        EmptyObject,
        SessionCreateOutput
    ),
    spec!(
        SessionRootState,
        "astrcode.session.root.state",
        Some(ExtensionCapability::InputDelivery),
        "Read an owned top-level session lifecycle state",
        SessionTarget,
        SessionStateOutput
    ),
    spec!(
        SessionRootSubmitTurn,
        "astrcode.session.root.submit_turn",
        Some(ExtensionCapability::InputDelivery),
        "Submit a turn to an owned top-level session",
        SessionRootSubmitTurn,
        SessionSubmitTurnOutput
    ),
    HostOperationSpec {
        operation: HostOperation::SessionStateRead,
        name: "astrcode.session.state.read",
        required: None,
        description: "Read extension-namespaced session state",
        input_schema: CapabilitySchema::Object,
        output_schema: CapabilitySchema::Object,
        supports_stream: false,
        cancelable: false,
        catalog: false,
    },
    HostOperationSpec {
        operation: HostOperation::SessionStateWrite,
        name: "astrcode.session.state.write",
        required: None,
        description: "Write extension-namespaced session state",
        input_schema: CapabilitySchema::Object,
        output_schema: CapabilitySchema::Object,
        supports_stream: false,
        cancelable: false,
        catalog: false,
    },
    spec!(
        WorkspaceEdit,
        "astrcode.workspace.edit",
        Some(ExtensionCapability::WorkspaceWrite),
        "Replace an exact text fragment in a non-sensitive workspace file",
        WorkspaceEdit,
        WorkspaceEditOutput
    ),
    spec!(
        WorkspaceGlob,
        "astrcode.workspace.glob",
        Some(ExtensionCapability::WorkspaceRead),
        "Match bounded workspace paths by glob",
        WorkspaceGlob,
        WorkspaceGlobOutput
    ),
    spec!(
        WorkspaceGrep,
        "astrcode.workspace.grep",
        Some(ExtensionCapability::WorkspaceRead),
        "Regex-search bounded UTF-8 workspace files",
        WorkspaceGrep,
        WorkspaceGrepOutput
    ),
    spec!(
        WorkspaceList,
        "astrcode.workspace.list",
        Some(ExtensionCapability::WorkspaceRead),
        "List a bounded workspace directory tree",
        WorkspaceList,
        WorkspaceListOutput
    ),
    spec!(
        WorkspaceRead,
        "astrcode.workspace.read",
        Some(ExtensionCapability::WorkspaceRead),
        "Read a bounded UTF-8 workspace file",
        WorkspaceRead,
        WorkspaceReadOutput
    ),
    spec!(
        WorkspaceWrite,
        "astrcode.workspace.write",
        Some(ExtensionCapability::WorkspaceWrite),
        "Create or replace a non-sensitive file under the working directory",
        WorkspaceWrite,
        WorkspaceWriteOutput
    ),
];

fn capability_schema(schema: CapabilitySchema) -> Value {
    match schema {
        CapabilitySchema::Object => json!({ "type": "object" }),
        CapabilitySchema::EmptyObject => json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        CapabilitySchema::Acknowledgement => HostAcknowledgement::wire_schema(),
        CapabilitySchema::EventEmitRequest => json!({
            "type": "object",
            "properties": {
                "event_type": { "type": "string" },
                "schema_version": { "type": "integer", "minimum": 1, "default": 1 },
                "payload": {}
            },
            "required": ["event_type"],
            "additionalProperties": false
        }),
        CapabilitySchema::ExtensionHttpRequest => ExtensionHttpRequest::wire_schema(),
        CapabilitySchema::ExtensionHttpResponse => ExtensionHttpResponse::wire_schema(),
        CapabilitySchema::LlmChatRequest => HostLlmChatRequest::wire_schema(),
        CapabilitySchema::LlmChatOutput => host_llm_chat_response_schema(),
        CapabilitySchema::NetworkRequest => HostNetworkRequest::wire_schema(),
        CapabilitySchema::NetworkResponse => HostNetworkResponse::wire_schema(),
        CapabilitySchema::ProcessSpawn => HostProcessRequest::wire_schema(),
        CapabilitySchema::ProcessSpawnOutput => HostProcessOutput::wire_schema(),
        CapabilitySchema::SessionId => json!({
            "type": "object",
            "properties": { "session_id": { "type": "string" } },
            "required": ["session_id"],
            "additionalProperties": false
        }),
        CapabilitySchema::SessionEventsPage => HostSessionEventsPageRequest::wire_schema(),
        CapabilitySchema::SessionEventsPageOutput => HostSessionEventsPageOutput::wire_schema(),
        CapabilitySchema::SessionCancelOutput => HostSessionCancelOutput::wire_schema(),
        CapabilitySchema::SessionDeliveryOutput => HostSessionDeliveryOutput::wire_schema(),
        CapabilitySchema::SessionExecutionView => HostSessionExecutionView::wire_schema(),
        CapabilitySchema::SessionHistorySnapshotOutput => {
            SessionHistorySnapshotOutput::wire_schema()
        },
        CapabilitySchema::SessionInput => HostSessionInputRequest::wire_schema(),
        CapabilitySchema::SessionInspectListOutput => SessionInspectListOutput::wire_schema(),
        CapabilitySchema::SessionInspectProviderMessagesOutput => {
            SessionInspectProviderMessagesOutput::wire_schema()
        },
        CapabilitySchema::SessionInspectReadModelOutput => {
            SessionInspectReadModelOutput::wire_schema()
        },
        CapabilitySchema::SessionInspectSnapshotOutput => {
            SessionInspectSnapshotOutput::wire_schema()
        },
        CapabilitySchema::SessionSummariesOutput => HostSessionSummariesOutput::wire_schema(),
        CapabilitySchema::SessionTranscriptOutput => HostSessionTranscript::wire_schema(),
        CapabilitySchema::SessionProviderMessagesOutput => {
            HostSessionProviderMessagesOutput::wire_schema()
        },
        CapabilitySchema::SessionTokenUsageOutput => HostSessionTokenUsageOutput::wire_schema(),
        CapabilitySchema::SessionCreate => HostCreateSessionRequest::wire_schema(),
        CapabilitySchema::SessionCreateOutput => HostCreateSessionOutput::wire_schema(),
        CapabilitySchema::SessionTarget => HostSessionTargetRequest::wire_schema(),
        CapabilitySchema::SessionStateOutput => HostSessionStateOutput::wire_schema(),
        CapabilitySchema::SessionReactivateOutput => HostSessionReactivateOutput::wire_schema(),
        CapabilitySchema::SessionSubmitTurn => HostSubmitTurnRequest::wire_schema(),
        CapabilitySchema::SessionSubmitTurnOutput => HostSubmitTurnOutput::wire_schema(),
        CapabilitySchema::SessionRootSubmitTurn => HostRootSubmitTurnRequest::wire_schema(),
        CapabilitySchema::SessionRecycle => HostRecycleSessionRequest::wire_schema(),
        CapabilitySchema::SessionToolSelection => HostConfigureSessionToolsRequest::wire_schema(),
        CapabilitySchema::SessionToolSelectionOutput => {
            HostConfigureSessionToolsOutput::wire_schema()
        },
        CapabilitySchema::WorkspaceEdit => HostWorkspaceEditRequest::wire_schema(),
        CapabilitySchema::WorkspaceEditOutput => HostWorkspaceEditOutput::wire_schema(),
        CapabilitySchema::WorkspaceGlob => HostWorkspaceGlobRequest::wire_schema(),
        CapabilitySchema::WorkspaceGlobOutput => HostWorkspaceGlobOutput::wire_schema(),
        CapabilitySchema::WorkspaceGrep => HostWorkspaceGrepRequest::wire_schema(),
        CapabilitySchema::WorkspaceGrepOutput => HostWorkspaceGrepOutput::wire_schema(),
        CapabilitySchema::WorkspaceList => HostWorkspaceListRequest::wire_schema(),
        CapabilitySchema::WorkspaceListOutput => HostWorkspaceListOutput::wire_schema(),
        CapabilitySchema::WorkspaceRead => HostWorkspaceReadRequest::wire_schema(),
        CapabilitySchema::WorkspaceReadOutput => HostWorkspaceReadOutput::wire_schema(),
        CapabilitySchema::WorkspaceWrite => HostWorkspaceWriteRequest::wire_schema(),
        CapabilitySchema::WorkspaceWriteOutput => HostWorkspaceWriteOutput::wire_schema(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn assert_closed_record_schema(
        schema: &Value,
        operation: HostOperation,
        boundary: &str,
        path: &str,
    ) {
        let Some(object) = schema.as_object() else {
            return;
        };
        if let Some(properties) = object.get("properties").and_then(Value::as_object) {
            assert_eq!(
                object.get("additionalProperties"),
                Some(&Value::Bool(false)),
                "{operation:?} {boundary} record at {path} must reject unknown fields"
            );
            for (name, property) in properties {
                assert_closed_record_schema(
                    property,
                    operation,
                    boundary,
                    &format!("{path}/properties/{name}"),
                );
            }
        }
        if let Some(required) = object.get("required").and_then(Value::as_array) {
            let mut names = HashSet::new();
            for name in required {
                let name = name.as_str().unwrap_or_else(|| {
                    panic!("{operation:?} {boundary} required entry at {path} must be a string")
                });
                assert!(
                    names.insert(name),
                    "{operation:?} {boundary} schema at {path} repeats required field {name}"
                );
                if let Some(properties) = object.get("properties").and_then(Value::as_object) {
                    assert!(
                        properties.contains_key(name),
                        "{operation:?} {boundary} schema at {path} requires unknown field {name}"
                    );
                }
            }
        }
        if let Some(items) = object.get("items") {
            assert_closed_record_schema(items, operation, boundary, &format!("{path}/items"));
        }
        for keyword in ["oneOf", "anyOf", "allOf"] {
            if let Some(variants) = object.get(keyword).and_then(Value::as_array) {
                for (index, variant) in variants.iter().enumerate() {
                    assert_closed_record_schema(
                        variant,
                        operation,
                        boundary,
                        &format!("{path}/{keyword}/{index}"),
                    );
                }
            }
        }
        if let Some(definitions) = object.get("$defs").and_then(Value::as_object) {
            for (name, definition) in definitions {
                assert_closed_record_schema(
                    definition,
                    operation,
                    boundary,
                    &format!("{path}/$defs/{name}"),
                );
            }
        }
        if let Some(value_schema) = object
            .get("additionalProperties")
            .filter(|value| value.is_object())
        {
            assert_closed_record_schema(
                value_schema,
                operation,
                boundary,
                &format!("{path}/additionalProperties"),
            );
        }
    }

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
    fn catalog_operations_use_their_typed_wire_schemas() {
        use CapabilitySchema::*;
        use HostOperation::*;

        let cases = [
            (EventEmit, EventEmitRequest, Acknowledgement),
            (
                ExtensionHttpPublic,
                ExtensionHttpRequest,
                ExtensionHttpResponse,
            ),
            (LlmMainChat, LlmChatRequest, LlmChatOutput),
            (LlmSmallChat, LlmChatRequest, LlmChatOutput),
            (NetworkClient, NetworkRequest, NetworkResponse),
            (
                HostOperation::ProcessSpawn,
                CapabilitySchema::ProcessSpawn,
                ProcessSpawnOutput,
            ),
            (SessionControlCancelTurn, SessionTarget, SessionCancelOutput),
            (
                SessionControlConfigureTools,
                SessionToolSelection,
                SessionToolSelectionOutput,
            ),
            (SessionControlCreate, SessionCreate, SessionCreateOutput),
            (SessionControlDispose, SessionRecycle, Acknowledgement),
            (
                SessionControlExecutionView,
                SessionTarget,
                SessionExecutionView,
            ),
            (
                SessionControlInjectOrStart,
                SessionInput,
                SessionDeliveryOutput,
            ),
            (
                SessionControlInterruptAndSubmit,
                SessionInput,
                SessionDeliveryOutput,
            ),
            (
                SessionControlReactivate,
                SessionTarget,
                SessionReactivateOutput,
            ),
            (SessionControlState, SessionTarget, SessionStateOutput),
            (
                SessionControlSubmitTurn,
                SessionSubmitTurn,
                SessionSubmitTurnOutput,
            ),
            (SessionHistoryList, EmptyObject, SessionSummariesOutput),
            (
                SessionHistoryProviderMessages,
                SessionTarget,
                SessionProviderMessagesOutput,
            ),
            (
                SessionHistorySnapshot,
                SessionTarget,
                SessionHistorySnapshotOutput,
            ),
            (
                SessionHistoryTokenUsage,
                SessionTarget,
                SessionTokenUsageOutput,
            ),
            (
                SessionHistoryTranscript,
                SessionTarget,
                SessionTranscriptOutput,
            ),
            (SessionInspectList, EmptyObject, SessionInspectListOutput),
            (
                SessionInspectProviderMessages,
                SessionId,
                SessionInspectProviderMessagesOutput,
            ),
            (
                SessionInspectReadModel,
                SessionId,
                SessionInspectReadModelOutput,
            ),
            (
                SessionInspectSnapshot,
                SessionId,
                SessionInspectSnapshotOutput,
            ),
            (
                SessionReadEvents,
                SessionEventsPage,
                SessionEventsPageOutput,
            ),
            (
                HostOperation::SessionRootCreate,
                CapabilitySchema::EmptyObject,
                SessionCreateOutput,
            ),
            (SessionRootState, SessionTarget, SessionStateOutput),
            (
                HostOperation::SessionRootSubmitTurn,
                CapabilitySchema::SessionRootSubmitTurn,
                SessionSubmitTurnOutput,
            ),
            (
                HostOperation::WorkspaceEdit,
                CapabilitySchema::WorkspaceEdit,
                WorkspaceEditOutput,
            ),
            (
                HostOperation::WorkspaceGlob,
                CapabilitySchema::WorkspaceGlob,
                WorkspaceGlobOutput,
            ),
            (
                HostOperation::WorkspaceGrep,
                CapabilitySchema::WorkspaceGrep,
                WorkspaceGrepOutput,
            ),
            (
                HostOperation::WorkspaceList,
                CapabilitySchema::WorkspaceList,
                WorkspaceListOutput,
            ),
            (
                HostOperation::WorkspaceRead,
                CapabilitySchema::WorkspaceRead,
                WorkspaceReadOutput,
            ),
            (
                HostOperation::WorkspaceWrite,
                CapabilitySchema::WorkspaceWrite,
                WorkspaceWriteOutput,
            ),
        ];

        let catalog_operations = HOST_OPERATION_SPECS
            .iter()
            .filter(|spec| spec.catalog)
            .map(|spec| spec.operation)
            .collect::<HashSet<_>>();
        assert_eq!(cases.len(), catalog_operations.len());

        let mut covered = HashSet::new();
        for (operation, input_schema, output_schema) in cases {
            let spec = operation.spec();
            assert!(spec.catalog);
            assert!(covered.insert(operation), "duplicate case: {operation:?}");
            assert_eq!(spec.input_schema, input_schema, "{operation:?} input");
            assert_eq!(spec.output_schema, output_schema, "{operation:?} output");
            let input_schema = spec.input_schema();
            let output_schema = spec.output_schema();
            assert_eq!(input_schema, capability_schema(spec.input_schema));
            assert_eq!(output_schema, capability_schema(spec.output_schema));
            assert_closed_record_schema(&input_schema, operation, "input", "$");
            assert_closed_record_schema(&output_schema, operation, "output", "$");
        }
        assert_eq!(covered, catalog_operations);
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
            policy!(SessionStateRead, None, Session, false, false, false),
            policy!(SessionStateWrite, None, Session, false, false, false),
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
