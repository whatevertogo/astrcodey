//! Public authoring surface for AstrCode extensions.
//!
//! Bundled and external Rust extensions depend on this crate instead of the
//! host's internal crates. The runtime remains responsible for adapting these
//! contracts to session, storage, and provider implementations.

pub mod extension;
pub mod frontmatter;
pub mod hostpaths;
pub mod shell;

/// Typed access to the host's single restricted outbound-network service.
pub mod network {
    pub use crate::extension::{
        NetworkRedirectPolicy, OutboundNetworkError, OutboundNetworkErrorKind,
        OutboundNetworkRequest, OutboundNetworkResponse, OutboundNetworkService,
    };
}

pub mod trusted {
    pub use crate::authoring_runtime::ExtensionHostServices;
}

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
    pub use astrcode_core::event::{Event, EventPayload, EventSendError, EventSender};
}

pub mod session_query;

pub mod tool {
    pub use astrcode_core::tool::{
        CreateRootSessionRequest, CreateSessionRequest, ExecutionMode, LlmModelIds, SessionAccess,
        SessionAccessPair, SessionApiError, SessionDeliveryOutcome, SessionHandle,
        SessionOperations, SessionStatus, SubmitTurnRequest, SubmitTurnResult, Tool, ToolCallScope,
        ToolCapabilities, ToolDefinition, ToolError, ToolExecutionContext, ToolExecutionResult,
        ToolFileServices, ToolHostServices, ToolModelAccess, ToolOrigin, ToolPromptMetadata,
        ToolPromptTag, ToolResult, ToolSessionControl, ToolSessionPaths, tool_metadata,
    };

    pub use crate::extension::ExtensionToolContext;
}

pub mod types {
    pub use astrcode_core::types::{SessionId, project_key_from_path};
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

mod authoring_runtime;
pub mod builder;
pub mod manifest;
pub mod runtime;
pub mod runtime_ports;
pub mod s5r;
pub mod session;
pub mod session_inspect;
pub mod worker;

/// Namespaced persistence locations for session-scoped extension data.
pub mod state {
    use std::path::{Path, PathBuf};

    /// Returns the only directory an extension should use for session-local state.
    pub fn session_data_dir(session_base: &Path, extension_id: &str) -> PathBuf {
        session_base.join("extension_data").join(extension_id)
    }
}

/// 进程内（bundled）扩展：实现 [`extension::Extension`] trait，使用 [`builder::handler_fn`]。
pub mod prelude {
    pub use crate::{
        builder::{continue_after_stop_handler_fn, handler_fn, tool},
        extension::{
            CommandContext, CommandHandler, CompactContext, CompactContributions, CompactEvent,
            CompactHandler, CompactResult, ContinueAfterStopContext, ContinueAfterStopHandler,
            ContinueAfterStopLimit, ContinueAfterStopOptions, ContinueAfterStopResult, Extension,
            ExtensionCapability, ExtensionCommandResult, ExtensionConfig, ExtensionCtx,
            ExtensionError, ExtensionEvent, ExtensionHttpHandler, ExtensionHttpMethod,
            ExtensionHttpRequest, ExtensionHttpResponse, ExtensionHttpRoute, ExtensionManifest,
            HookMode, HookResult, LifecycleContext, LifecycleHandler, PostToolUseContext,
            PostToolUseHandler, PostToolUseResult, PreToolUseContext, PreToolUseHandler,
            PreToolUseResult, PromptBuildContext, PromptBuildHandler, PromptContributions,
            ProviderContext, ProviderEvent, ProviderHandler, ProviderResult, Registrar,
            SlashCommand, StatusItemUpdatePayload, StopReason, ToolHandler,
            UserMessageEnvelopeContext, UserMessageEnvelopeHandler, UserMessageEnvelopeResult,
        },
        manifest::validate_manifest,
        s5r::effects::HandlerResult,
        session::SessionToolSelectionDto,
        session_inspect::{
            SessionInspectListItem, SessionInspectListOutput, SessionInspectProviderMessagesOutput,
            SessionInspectReadModel, SessionInspectReadModelOutput, SessionInspectSnapshot,
            SessionInspectSnapshotOutput,
        },
        tool::{
            ExecutionMode, ExtensionToolContext, ToolCallScope, ToolCapabilities, ToolDefinition,
            ToolResult,
        },
        worker::{
            HostClient, HostConfigureSessionToolsOutput, HostConfigureSessionToolsRequest,
            HostCreateSessionOutput, HostCreateSessionRequest, HostNetworkRequest,
            HostNetworkResponse, HostProcessOutput, HostProcessRequest, HostSessionDeliveryOutput,
            HostSessionExecutionView, HostSessionInputRequest, HostSessionTargetRequest,
            HostSubmitTurnOutput, HostSubmitTurnRequest, HostWorkspaceEditOutput,
            HostWorkspaceEditRequest, HostWorkspaceGlobOutput, HostWorkspaceGlobRequest,
            HostWorkspaceGrepMatch, HostWorkspaceGrepOutput, HostWorkspaceGrepRequest,
            HostWorkspaceListEntry, HostWorkspaceListOutput, HostWorkspaceListRequest,
            HostWorkspaceWriteOutput, HostWorkspaceWriteRequest, HttpHandlerFn, Worker,
            WorkerCallContext, tool_text,
        },
    };
}

/// s5r 子进程磁盘扩展：[`Worker`]、handler 辅助函数、[`HostClient`]。
pub mod worker_prelude {
    pub use crate::{
        builder::tool,
        s5r::{
            ErrorPayload,
            effects::{CallContinuation, HandlerResult},
        },
        session::SessionToolSelectionDto,
        session_inspect::{
            SessionInspectListItem, SessionInspectListOutput, SessionInspectProviderMessagesOutput,
            SessionInspectReadModel, SessionInspectReadModelOutput, SessionInspectSnapshot,
            SessionInspectSnapshotOutput,
        },
        worker::{
            HostApi, HostClient, HostConfigureSessionToolsOutput, HostConfigureSessionToolsRequest,
            HostCreateSessionOutput, HostCreateSessionRequest, HostNetworkRequest,
            HostNetworkResponse, HostProcessOutput, HostProcessRequest, HostSessionDeliveryOutput,
            HostSessionExecutionView, HostSessionInputRequest, HostSessionTargetRequest,
            HostSubmitTurnOutput, HostSubmitTurnRequest, HostWorkspaceEditOutput,
            HostWorkspaceEditRequest, HostWorkspaceGlobOutput, HostWorkspaceGlobRequest,
            HostWorkspaceGrepMatch, HostWorkspaceGrepOutput, HostWorkspaceGrepRequest,
            HostWorkspaceListEntry, HostWorkspaceListOutput, HostWorkspaceListRequest,
            HostWorkspaceWriteOutput, HostWorkspaceWriteRequest, HttpHandlerFn, Worker,
            WorkerCallContext, command_handler, handler_err, hook_handler, hook_handler_args,
            http_handler, inject_host_api, parse_hook_input, parse_tool_arguments, tool_handler,
            tool_handler_args, tool_text,
        },
    };
}
