//! Public authoring surface for AstrCode extensions.
//!
//! Bundled and external Rust extensions depend on this crate instead of the
//! host's internal crates. The runtime remains responsible for adapting these
//! contracts to session, storage, and provider implementations.

pub mod extension {
    pub use astrcode_core::extension::{
        AfterToolResult, AfterToolResultsContext, AfterToolResultsHandler,
        AfterToolResultsRegistration, AfterToolResultsResult, ChildToolPolicy,
        CommandCompletionItem, CommandCompletions, CommandContext, CommandDiscoveryHandler,
        CommandHandler, CompactContext, CompactContributions, CompactEvent, CompactHandler,
        CompactResult, CompactStrategy, CompactTrigger, ContinueAfterStopContext,
        ContinueAfterStopHandler, ContinueAfterStopLimit, ContinueAfterStopOptions,
        ContinueAfterStopRegistration, ContinueAfterStopResult, DiscoveredTool,
        EXTENSION_TOOL_OUTCOME_KEY, ExchangeSummary, Extension, ExtensionCapability,
        ExtensionCommandResult, ExtensionConfig, ExtensionCtx, ExtensionError, ExtensionEvent,
        ExtensionEventDecl, ExtensionEventDeclBuilder, ExtensionEventSink, ExtensionManifest,
        ExtensionTasks, ExtensionToolOutcome, HookMode, HookResult, Keybinding, LifecycleContext,
        LifecycleHandler, PostToolUseContext, PostToolUseFailureContext, PostToolUseFailureHandler,
        PostToolUseHandler, PostToolUseResult, PreToolUseContext, PreToolUseHandler,
        PreToolUseResult, PromptBuildContext, PromptBuildHandler, PromptContributions,
        ProviderContext, ProviderEvent, ProviderHandler, ProviderResult, Registrar, SlashCommand,
        StatusItem, StatusItemUpdatePayload, StopReason, ToolDiscoveryHandler, ToolHandler,
        ToolHookRegistration, ToolHookTarget, UserMessageEnvelopeContext,
        UserMessageEnvelopeHandler, UserMessageEnvelopeRegistration, UserMessageEnvelopeResult,
    };
}

#[cfg(feature = "trusted-bundled")]
pub mod trusted {
    /// Host services are only for trusted bundled extensions started in-process.
    /// Disk/IPC extensions must use the capability-gated host API instead.
    pub use astrcode_core::extension::ExtensionHostServices;
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

pub mod render {
    pub use astrcode_core::render::*;
}

pub mod event {
    pub use astrcode_core::event::{Event, EventPayload};
}

pub mod storage {
    pub use astrcode_core::storage::{EventReader, SessionReadModel, SessionSummary};
}

pub mod tool {
    pub use astrcode_core::{
        tool::{
            CreateRootSessionRequest, CreateSessionRequest, DEFERRED_TOOLS_METADATA_KEY,
            ExecutionMode, SessionAccess, SessionAccessPair, SessionApiError, SessionHandle,
            SessionOperations, SessionStatus, SubmitTurnRequest, SubmitTurnResult, Tool,
            ToolCallScope, ToolCapabilities, ToolDefinition, ToolError, ToolExecutionContext,
            ToolFileServices, ToolHostServices, ToolModelAccess, ToolOrigin, ToolPromptMetadata,
            ToolPromptTag, ToolResult, ToolSessionControl, ToolSessionPaths, tool_metadata,
        },
        tool_ui::{
            TOOL_UI_METADATA_KEY, TOOL_UI_PHASE_METADATA_KEY, ToolApprovalUiWire, ToolInputUiWire,
            ToolResultUiWire, ToolUiWire,
        },
    };
}

pub mod types {
    pub use astrcode_core::types::{SessionId, project_key_from_path};
}

/// Host path utilities usable by extensions.
pub mod hostpaths {
    pub use astrcode_support::hostpaths::*;
}

/// Frontmatter parsing helpers usable by extensions.
pub mod frontmatter {
    pub use astrcode_support::frontmatter::*;
}

/// Text formatting helpers usable by extensions.
pub mod text {
    pub use astrcode_support::text::*;
}

/// Shell detection helpers usable by extensions.
pub mod shell {
    pub use astrcode_support::shell::*;
}

/// Protocol types needed by extensions.
pub mod protocol {
    pub use astrcode_protocol::framing::JsonRpcError;
}

/// Tool Gate 权限类型（扩展只读 `PreToolUseContext::approval_mode`）。
pub mod permission {
    pub use astrcode_core::permission::{ApprovalDecision, ApprovalMode};
}

pub mod builder;
pub mod manifest;
pub mod runtime;
pub mod s5r;
pub mod session;
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
            AfterToolResult, AfterToolResultsContext, AfterToolResultsHandler,
            AfterToolResultsResult, CommandContext, CommandHandler, CompactContext,
            CompactContributions, CompactEvent, CompactHandler, CompactResult,
            ContinueAfterStopContext, ContinueAfterStopHandler, ContinueAfterStopLimit,
            ContinueAfterStopOptions, ContinueAfterStopResult, Extension, ExtensionCapability,
            ExtensionCommandResult, ExtensionConfig, ExtensionCtx, ExtensionError, ExtensionEvent,
            ExtensionManifest, HookMode, HookResult, LifecycleContext, LifecycleHandler,
            PostToolUseContext, PostToolUseHandler, PostToolUseResult, PreToolUseContext,
            PreToolUseHandler, PreToolUseResult, PromptBuildContext, PromptBuildHandler,
            PromptContributions, ProviderContext, ProviderEvent, ProviderHandler, ProviderResult,
            Registrar, SlashCommand, StatusItemUpdatePayload, StopReason, ToolHandler,
            UserMessageEnvelopeContext, UserMessageEnvelopeHandler, UserMessageEnvelopeResult,
        },
        manifest::validate_manifest,
        s5r::effects::HandlerResult,
        tool::{
            ExecutionMode, ToolCallScope, ToolCapabilities, ToolDefinition, ToolExecutionContext,
            ToolResult,
        },
        worker::{HostClient, Worker, WorkerCallContext, tool_text},
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
        worker::{
            HostApi, HostClient, Worker, WorkerCallContext, command_handler, handler_err,
            hook_handler, hook_handler_args, inject_host_api, parse_hook_input,
            parse_tool_arguments, tool_handler, tool_handler_args, tool_text,
        },
    };
}
