//! Public authoring surface for AstrCode extensions.
//!
//! Bundled and external Rust extensions depend on this crate instead of the
//! host's internal crates. The runtime remains responsible for adapting these
//! contracts to session, storage, and provider implementations.

pub mod discovery;
pub mod extension;
pub mod frontmatter;
pub mod hostpaths;
pub mod shell;

pub mod config {
    pub use astrcode_core::config::ModelSelection;
}

pub mod llm {
    pub use astrcode_core::llm::{
        LlmContent, LlmEvent, LlmMessage, LlmProvider, LlmRole, LlmTokenUsage, ModelLimits,
        collect_stream_text,
    };
}

pub mod event {
    pub use astrcode_core::event::{
        Event, EventPayload, EventPublishReceipt, EventSendError, EventSender,
    };
}

pub mod tool {
    pub use astrcode_core::tool::{
        CreateRootSessionRequest, CreateSessionRequest, ExecutionMode, LlmModelIds, SessionAccess,
        SessionAccessPair, SessionApiError, SessionDeliveryOutcome, SessionHandle,
        SessionLifecycleState, SessionOperations, SessionReactivation, SessionState, SessionStatus,
        SubmitTurnRequest, SubmitTurnResult, Tool, ToolCallScope, ToolCapabilities, ToolDefinition,
        ToolError, ToolExecutionContext, ToolExecutionResult, ToolFileServices, ToolHostServices,
        ToolModelAccess, ToolOrigin, ToolPromptMetadata, ToolPromptTag, ToolResult,
        ToolSessionControl, ToolSessionPaths, tool_metadata,
    };

    pub use crate::extension::ToolContext;
}

pub mod types {
    pub use astrcode_core::types::{SessionId, ToolCallId, project_key_from_path};
}

/// Protocol types needed by extensions.
pub mod protocol {
    use serde::{Deserialize, Serialize};

    /// S5R JSON-RPC 边界使用的错误对象。
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct JsonRpcError {
        pub code: i32,
        pub message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub data: Option<serde_json::Value>,
    }
}

/// Tool Gate 权限类型（扩展只读 `PreToolUseContext::approval_mode`）。
pub mod permission {
    pub use astrcode_core::permission::{ApprovalDecision, ApprovalMode};
}

pub mod builder;
pub mod host;
pub mod manifest;
pub mod runtime;
pub mod runtime_ports;
pub mod s5r;
pub mod session;
pub mod session_inspect;

pub use astrcode_core::wire::{WireError, WireErrorCode};
pub mod testing;
pub mod worker;

/// In-process bundled extension authoring surface.
pub mod prelude {
    pub use crate::{
        builder::{
            ExtensionEventDeclarationBuilder, ExtensionHttpRouteBuilder, ExtensionManifestBuilder,
            ExtensionToolDefinition, KeybindingBuilder, SlashCommandBuilder, StatusItemBuilder,
            command, command_handler, continue_after_stop_handler_fn, extension_event,
            http_handler, http_route, keybinding, manifest, status_item, tool, tool_handler,
            tool_handler_args,
        },
        extension::{
            CommandCompletionContext, CommandContext, CommandDiscovery, CommandDiscoveryContext,
            CommandDiscoveryHandler, CommandHandler, CompactContext, CompactContributions,
            CompactEvent, CompactHandler, CompactResult, ContinueAfterStopContext,
            ContinueAfterStopHandler, ContinueAfterStopLimit, ContinueAfterStopOptions,
            ContinueAfterStopResult, DiscoveredCommand, DiscoveredTool, Extension,
            ExtensionCallContext, ExtensionCapability, ExtensionCommandResult, ExtensionConfig,
            ExtensionConfigError, ExtensionError, ExtensionEvent, ExtensionEventEmitter,
            ExtensionEventError, ExtensionHttpDispatchRequest, ExtensionHttpHandler,
            ExtensionHttpMethod, ExtensionHttpRequest, ExtensionHttpResponse, ExtensionHttpRoute,
            ExtensionManifest, ExtensionPathError, ExtensionPaths, ExtensionStartContext,
            ExtensionTaskError, ExtensionTasks, HookMode, HookResult, HttpContext,
            LifecycleContext, LifecycleHandler, PostToolUseContext, PostToolUseHandler,
            PostToolUseResult, PreToolUseContext, PreToolUseHandler, PreToolUseResult,
            PromptBuildContext, PromptBuildHandler, PromptContributions, ProviderContext,
            ProviderEvent, ProviderHandler, ProviderResult, Registrar, SlashCommand,
            StatusItemUpdatePayload, StopReason, ToolContext, ToolDiscovery, ToolDiscoveryContext,
            ToolDiscoveryHandler, ToolHandler, UserMessageEnvelopeContext,
            UserMessageEnvelopeHandler, UserMessageEnvelopeResult,
        },
        host::{
            ExtensionHost, ExtensionHttpClient, HostConfigureSessionToolsOutput,
            HostConfigureSessionToolsRequest, HostError, HostErrorClass, HostLlmChatOutput,
            HostLlmChatRequest, HostLlmCollectedStreamOutput, HostLlmContent, HostLlmMessage,
            HostLlmRole, HostLlmTextDelta, HostNetworkRedirectPolicy, HostNetworkRequest,
            HostNetworkResponse, HostProcessOutput, HostProcessRequest, HostSessionCancelOutput,
            HostSessionDeliveryOutput, HostSessionExecutionView, HostSessionInputRequest,
            HostSessionProviderMessagesOutput, HostSessionStateReadOutput,
            HostSessionStateReadRequest, HostSessionStateWriteRequest, HostSessionSummariesOutput,
            HostSessionSummary, HostSessionTokenUsage, HostSessionTokenUsageOutput,
            HostSessionTranscript, HostSessionTranscriptMessage, HostWorkspaceEditOutput,
            HostWorkspaceEditRequest, HostWorkspaceGlobOutput, HostWorkspaceGlobRequest,
            HostWorkspaceGrepMatch, HostWorkspaceGrepOutput, HostWorkspaceGrepRequest,
            HostWorkspaceListEntry, HostWorkspaceListOutput, HostWorkspaceListRequest,
            HostWorkspaceReadOutput, HostWorkspaceReadRequest, HostWorkspaceWriteOutput,
            HostWorkspaceWriteRequest, ModelClient, NetworkClient, ProcessClient,
            SessionControlClient, SessionHistoryClient, SessionInspectClient, SessionStateClient,
            WorkspaceClient,
        },
        llm::LlmMessage,
        session::{
            HostCreateSessionOutput, HostCreateSessionRequest, HostRecycleSessionRequest,
            HostRootSubmitTurnRequest, HostSessionEvent, HostSessionEventsPageOutput,
            HostSessionEventsPageRequest, HostSessionReactivateOutput, HostSessionStateOutput,
            HostSessionTargetRequest, HostSubmitTurnOutput, HostSubmitTurnRequest, SessionPhaseDto,
            SessionToolSelectionDto,
        },
        session_inspect::{
            HostSessionInspectRequest, SessionHistorySnapshotOutput, SessionInspectListItem,
            SessionInspectListOutput, SessionInspectProviderMessagesOutput,
            SessionInspectReadModel, SessionInspectReadModelOutput, SessionInspectSnapshot,
            SessionInspectSnapshotOutput,
        },
        tool::{
            ExecutionMode, ToolDefinition, ToolExecutionResult, ToolPromptMetadata, ToolResult,
        },
        types::{SessionId, ToolCallId},
    };
}

/// s5r 子进程磁盘扩展：[`Worker`]、handler 辅助函数、[`HostClient`]。
pub mod worker_prelude {
    pub use crate::{
        builder::worker_tool as tool,
        extension::{
            ExtensionCapability, ExtensionEvent, ExtensionEventDecl, ExtensionHttpDispatchRequest,
            ExtensionHttpMethod, ExtensionHttpRequest, ExtensionHttpResponse, ExtensionHttpRoute,
            HookMode,
        },
        llm::LlmMessage,
        s5r::{
            ErrorPayload,
            effects::{CallContinuation, HandlerResult},
        },
        session::{SessionPhaseDto, SessionToolSelectionDto},
        session_inspect::{
            HostSessionInspectRequest, SessionHistorySnapshotOutput, SessionInspectListItem,
            SessionInspectListOutput, SessionInspectProviderMessagesOutput,
            SessionInspectReadModel, SessionInspectReadModelOutput, SessionInspectSnapshot,
            SessionInspectSnapshotOutput,
        },
        worker::{
            EventClient, ExtensionHttpClient, HostClient, HostConfigureSessionToolsOutput,
            HostConfigureSessionToolsRequest, HostCreateSessionOutput, HostCreateSessionRequest,
            HostEventEmitOutput, HostEventEmitRequest, HostLlmChatOutput,
            HostLlmCollectedStreamOutput, HostLlmContent, HostLlmMessage, HostLlmRole,
            HostLlmTextDelta, HostNetworkRedirectPolicy, HostNetworkRequest, HostNetworkResponse,
            HostProcessOutput, HostProcessRequest, HostRecycleSessionRequest,
            HostRootSubmitTurnRequest, HostSessionCancelOutput, HostSessionDeliveryOutput,
            HostSessionEvent, HostSessionEventsPageOutput, HostSessionEventsPageRequest,
            HostSessionExecutionView, HostSessionInputRequest, HostSessionProviderMessagesOutput,
            HostSessionReactivateOutput, HostSessionStateOutput, HostSessionStateReadOutput,
            HostSessionStateReadRequest, HostSessionStateWriteRequest, HostSessionSummariesOutput,
            HostSessionSummary, HostSessionTargetRequest, HostSessionTokenUsage,
            HostSessionTokenUsageOutput, HostSessionTranscript, HostSessionTranscriptMessage,
            HostSubmitTurnOutput, HostSubmitTurnRequest, HostWorkspaceEditOutput,
            HostWorkspaceEditRequest, HostWorkspaceGlobOutput, HostWorkspaceGlobRequest,
            HostWorkspaceGrepMatch, HostWorkspaceGrepOutput, HostWorkspaceGrepRequest,
            HostWorkspaceListEntry, HostWorkspaceListOutput, HostWorkspaceListRequest,
            HostWorkspaceReadOutput, HostWorkspaceReadRequest, HostWorkspaceWriteOutput,
            HostWorkspaceWriteRequest, HttpHandlerFn, ModelClient, NetworkClient, ProcessClient,
            SessionControlClient, SessionHistoryClient, SessionInspectClient, SessionStateClient,
            Worker, WorkerCallContext, WorkspaceClient, command_handler, handler_err, hook_handler,
            hook_handler_args, http_handler, parse_hook_input, parse_tool_arguments, tool_handler,
            tool_handler_args, tool_text,
        },
    };
}
