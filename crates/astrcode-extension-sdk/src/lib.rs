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
        LlmContent, LlmEvent, LlmMessage, LlmProvider, LlmRequest, LlmRole, LlmTokenUsage,
        ModelLimits, collect_stream_text,
    };
}

pub mod event {
    pub use astrcode_core::event::{
        Event, EventDeliveryReceipt, EventPayload, EventSendError, EventSender,
    };
}

pub mod tool {
    pub use astrcode_core::tool::{
        ExecutionMode, ToolDefinition, ToolExecutionPolicy, ToolExecutionResult, ToolOrigin,
        ToolPresentation, ToolPromptMetadata, ToolPromptTag, ToolResult,
        access::{FileOperation, HostResource, ResourceAccess, ToolPlan},
        read_image::ReadToolInlinePayload,
        tool_metadata,
    };

    pub use crate::extension::{ToolContext, ToolPlanContext};
}

pub mod types {
    pub use astrcode_core::types::{SessionId, ToolCallId, project_key_from_path};
}

/// Tool Gate permission types (extensions only read `PreToolUseContext::approval_mode`).
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
pub mod transport;
pub mod wire;

pub use wire::WireErrorCode;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

/// In-process bundled extension authoring surface.
pub mod prelude {
    pub use crate::{
        builder::{
            CustomEventDeclarationBuilder, ExtensionHttpRouteBuilder, ExtensionManifestBuilder,
            ExtensionToolDefinition, KeybindingBuilder, SlashCommandBuilder, StatusItemBuilder,
            command, command_handler, continue_after_stop_handler_fn, custom_event, http_handler,
            http_route, keybinding, manifest, status_item, tool, tool_handler, tool_handler_args,
        },
        event::EventDeliveryReceipt,
        extension::{
            CommandAvailability, CommandCompletionContext, CommandContext, CommandDiscovery,
            CommandDiscoveryContext, CommandDiscoveryHandler, CommandExecution, CommandHandler,
            CompactContributions, CompactEvent, CompactRetainedContext, ContinueAfterStopContext,
            ContinueAfterStopHandler, ContinueAfterStopLimit, ContinueAfterStopOptions,
            ContinueAfterStopResult, CustomEventContext, CustomEventDelivery,
            CustomEventDisposition, CustomEventEmitError, CustomEventEmitter, CustomEventHandler,
            CustomEventSubscription, DiscoveredCommand, DiscoveredTool, Extension, ExtensionCall,
            ExtensionCallContext, ExtensionCapability, ExtensionCommandResult, ExtensionConfig,
            ExtensionConfigError, ExtensionError, ExtensionHttpDispatchRequest,
            ExtensionHttpHandler, ExtensionHttpMethod, ExtensionHttpRequest, ExtensionHttpResponse,
            ExtensionHttpRoute, ExtensionManifest, ExtensionPathError, ExtensionPaths,
            ExtensionStartContext, ExtensionTaskError, ExtensionTasks, HookMode, HookResult,
            HttpContext, LifecycleContext, LifecycleEvent, LifecycleHandler, PostCompactContext,
            PostCompactHandler, PostToolUseContext, PostToolUseHandler, PostToolUseResult,
            PreCompactContext, PreCompactHandler, PreCompactResult, PreToolUseContext,
            PreToolUseHandler, PreToolUseResult, PreparedProviderContribution,
            PreparedProviderEffect, PromptBuildContext, PromptBuildHandler, PromptContributions,
            ProviderContext, ProviderContributionHandler, ProviderContributionId, ProviderEvent,
            ProviderHandler, ProviderRequestId, ProviderResult, ProviderSettlementContext,
            Registrar, SessionCommandKind, SlashCommand, StatusItemUpdatePayload, StopReason,
            ToolContext, ToolDiscovery, ToolDiscoveryContext, ToolDiscoveryHandler, ToolHandler,
            ToolInputTransformHandler, ToolInputTransformResult, ToolPlanContext, TransportFeature,
            UserMessageEnvelopeContext, UserMessageEnvelopeHandler, UserMessageEnvelopeResult,
        },
        host::{
            ExtensionHost, ExtensionHttpClient, HostConfigureSessionToolsOutput,
            HostConfigureSessionToolsRequest, HostError, HostLlmChatOutput, HostLlmChatRequest,
            HostLlmContent, HostLlmMessage, HostLlmRole, HostNetworkRedirectPolicy,
            HostNetworkRequest, HostNetworkResponse, HostProcessHandleOutput,
            HostProcessInputAction, HostProcessInputRequest, HostProcessListOutput,
            HostProcessOutput, HostProcessReadOutput, HostProcessReadRequest, HostProcessRequest,
            HostProcessStartRequest, HostProcessState, HostProcessStatusOutput,
            HostProcessTargetRequest, HostSessionCancelOutput, HostSessionDeliveryOutput,
            HostSessionExecutionView, HostSessionInputRequest, HostSessionProviderMessagesOutput,
            HostSessionStateReadOutput, HostSessionStateReadRequest, HostSessionStateWriteRequest,
            HostSessionSummariesOutput, HostSessionSummary, HostSessionTokenUsage,
            HostSessionTokenUsageOutput, HostSessionTranscript, HostSessionTranscriptMessage,
            HostToolResultReadOutput, HostToolResultReadRequest, HostWorkspaceEditOutput,
            HostWorkspaceEditRequest, HostWorkspaceGlobOutput, HostWorkspaceGlobRequest,
            HostWorkspaceGrepContextLine, HostWorkspaceGrepEntry, HostWorkspaceGrepMode,
            HostWorkspaceGrepOutput, HostWorkspaceGrepRequest, HostWorkspaceListEntry,
            HostWorkspaceListOutput, HostWorkspaceListRequest, HostWorkspaceReadOutput,
            HostWorkspaceReadRequest, HostWorkspaceTextChange, HostWorkspaceWriteOutput,
            HostWorkspaceWriteRequest, ModelClient, NetworkClient, ProcessClient,
            SessionControlClient, SessionHistoryClient, SessionInspectClient, SessionStateClient,
            ToolResultClient, WorkspaceClient, llm_chat_request,
        },
        llm::LlmMessage,
        model_stream::{ModelStream, ModelStreamEvent},
        session::{
            HostCreateSessionOutput, HostCreateSessionRequest, HostRecycleSessionRequest,
            HostRootSubmitTurnRequest, HostSessionEvent, HostSessionEventsPageOutput,
            HostSessionEventsPageRequest, HostSessionReactivateOutput, HostSessionStateOutput,
            HostSessionTargetRequest, HostSubmitTurnOutput, HostSubmitTurnRequest,
            SessionMessageOriginDto, SessionPhaseDto, SessionToolSelectionDto,
        },
        tool::{
            ExecutionMode, HostResource, ReadToolInlinePayload, ResourceAccess, ToolDefinition,
            ToolExecutionResult, ToolPlan, ToolPromptMetadata, ToolResult,
        },
        types::{SessionId, ToolCallId},
        wire::session_inspect::{
            HostSessionInspectRequest, SessionHistorySnapshotOutput, SessionInspectContent,
            SessionInspectListItem, SessionInspectListOutput, SessionInspectProviderMessagesOutput,
            SessionInspectReadModel, SessionInspectReadModelOutput, SessionInspectSnapshot,
            SessionInspectSnapshotOutput,
        },
    };
}
