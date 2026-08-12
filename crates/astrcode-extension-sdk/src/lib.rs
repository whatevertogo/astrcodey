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
        Event, EventDeliveryReceipt, EventPayload, EventSendError, EventSender,
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

/// Tool Gate 权限类型（扩展只读 `PreToolUseContext::approval_mode`）。
pub mod permission {
    pub use astrcode_core::permission::{ApprovalDecision, ApprovalMode};
}

pub mod builder;
pub mod host;
pub mod manifest;
pub mod model_stream;
pub mod runtime_ports;
pub mod s5r;
pub mod session;

pub use astrcode_extension_contract::WireErrorCode;
pub mod testing;

/// In-process bundled extension authoring surface.
pub mod prelude {
    pub use astrcode_extension_contract::session_inspect::{
        HostSessionInspectRequest, SessionHistorySnapshotOutput, SessionInspectContent,
        SessionInspectListItem, SessionInspectListOutput, SessionInspectProviderMessagesOutput,
        SessionInspectReadModel, SessionInspectReadModelOutput, SessionInspectSnapshot,
        SessionInspectSnapshotOutput,
    };

    pub use crate::{
        builder::{
            CustomEventDeclarationBuilder, ExtensionHttpRouteBuilder, ExtensionManifestBuilder,
            ExtensionToolDefinition, KeybindingBuilder, SlashCommandBuilder, StatusItemBuilder,
            command, command_handler, continue_after_stop_handler_fn, custom_event, http_handler,
            http_route, keybinding, manifest, status_item, tool, tool_handler, tool_handler_args,
        },
        event::EventDeliveryReceipt,
        extension::{
            CommandCompletionContext, CommandContext, CommandDiscovery, CommandDiscoveryContext,
            CommandDiscoveryHandler, CommandHandler, CompactContext, CompactContributions,
            CompactEvent, CompactHandler, CompactResult, ContinueAfterStopContext,
            ContinueAfterStopHandler, ContinueAfterStopLimit, ContinueAfterStopOptions,
            ContinueAfterStopResult, CustomEventContext, CustomEventDisposition,
            CustomEventEmitError, CustomEventEmitter, CustomEventHandler, CustomEventSubscription,
            DiscoveredCommand, DiscoveredTool, Extension, ExtensionCall, ExtensionCallContext,
            ExtensionCapability, ExtensionCommandResult, ExtensionConfig, ExtensionConfigError,
            ExtensionError, ExtensionHttpDispatchRequest, ExtensionHttpHandler,
            ExtensionHttpMethod, ExtensionHttpRequest, ExtensionHttpResponse, ExtensionHttpRoute,
            ExtensionManifest, ExtensionPathError, ExtensionPaths, ExtensionStartContext,
            ExtensionTaskError, ExtensionTasks, HookMode, HookResult, HttpContext,
            LifecycleContext, LifecycleEvent, LifecycleHandler, PostToolUseContext,
            PostToolUseHandler, PostToolUseResult, PreToolUseContext, PreToolUseHandler,
            PreToolUseResult, PromptBuildContext, PromptBuildHandler, PromptContributions,
            ProviderContext, ProviderEvent, ProviderHandler, ProviderResult, Registrar,
            SlashCommand, StatusItemUpdatePayload, StopReason, ToolContext, ToolDiscovery,
            ToolDiscoveryContext, ToolDiscoveryHandler, ToolHandler, UserMessageEnvelopeContext,
            UserMessageEnvelopeHandler, UserMessageEnvelopeResult,
        },
        host::{
            ExtensionHost, ExtensionHttpClient, HostConfigureSessionToolsOutput,
            HostConfigureSessionToolsRequest, HostError, HostLlmChatOutput, HostLlmChatRequest,
            HostLlmCollectedStreamOutput, HostLlmContent, HostLlmMessage, HostLlmRole,
            HostNetworkRedirectPolicy, HostNetworkRequest, HostNetworkResponse, HostProcessOutput,
            HostProcessRequest, HostSessionCancelOutput, HostSessionDeliveryOutput,
            HostSessionExecutionView, HostSessionInputRequest, HostSessionProviderMessagesOutput,
            HostSessionStateReadOutput, HostSessionStateReadRequest, HostSessionStateWriteRequest,
            HostSessionSummariesOutput, HostSessionSummary, HostSessionTokenUsage,
            HostSessionTokenUsageOutput, HostSessionTranscript, HostSessionTranscriptMessage,
            HostWorkspaceEditOutput, HostWorkspaceEditRequest, HostWorkspaceGlobOutput,
            HostWorkspaceGlobRequest, HostWorkspaceGrepMatch, HostWorkspaceGrepOutput,
            HostWorkspaceGrepRequest, HostWorkspaceListEntry, HostWorkspaceListOutput,
            HostWorkspaceListRequest, HostWorkspaceReadOutput, HostWorkspaceReadRequest,
            HostWorkspaceWriteOutput, HostWorkspaceWriteRequest, ModelClient, NetworkClient,
            ProcessClient, SessionControlClient, SessionHistoryClient, SessionInspectClient,
            SessionStateClient, WorkspaceClient,
        },
        llm::LlmMessage,
        model_stream::{ModelStream, ModelStreamEvent},
        session::{
            HostCreateSessionOutput, HostCreateSessionRequest, HostRecycleSessionRequest,
            HostRootSubmitTurnRequest, HostSessionEvent, HostSessionEventsPageOutput,
            HostSessionEventsPageRequest, HostSessionReactivateOutput, HostSessionStateOutput,
            HostSessionTargetRequest, HostSubmitTurnOutput, HostSubmitTurnRequest, SessionPhaseDto,
            SessionToolSelectionDto,
        },
        tool::{
            ExecutionMode, ToolDefinition, ToolExecutionResult, ToolPromptMetadata, ToolResult,
        },
        types::{SessionId, ToolCallId},
    };
}
