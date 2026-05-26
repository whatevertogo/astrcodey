//! Public authoring surface for AstrCode extensions.
//!
//! Bundled and external Rust extensions depend on this crate instead of the
//! host's internal crates. The runtime remains responsible for adapting these
//! contracts to session, storage, and provider implementations.

pub mod extension {
    pub use astrcode_core::extension::*;

    // TODO: ExtensionHostServices 通过 pub use * 对所有 SDK 消费者可见，
    //       但它只应给 trusted bundled extension 使用。未来需要过滤这个类型的公开可见性，
    //       或者改用 capped re-export 代替通配符。
}

pub mod config {
    pub use astrcode_core::config::ModelSelection;
}

pub mod llm {
    pub use astrcode_core::llm::{
        LlmContent, LlmEvent, LlmMessage, LlmProvider, LlmRole, ModelLimits,
    };
}

pub mod render {
    pub use astrcode_core::render::*;
}

pub mod storage {
    pub use astrcode_core::storage::{EventReader, SessionReadModel, SessionSummary};
}

pub mod tool {
    pub use astrcode_core::tool::{
        CreateSessionRequest, DEFERRED_TOOLS_METADATA_KEY, ExecutionMode, SessionApiError,
        SessionHandle, SessionOperations, SessionStatus, SubmitTurnRequest, SubmitTurnResult, Tool,
        ToolCapabilities, ToolDefinition, ToolError, ToolExecutionContext, ToolOrigin,
        ToolPromptMetadata, ToolPromptTag, ToolResult, tool_metadata,
    };
}

pub mod types {
    pub use astrcode_core::types::project_key_from_path;
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
        builder::{handler_fn, tool},
        extension::{
            CommandContext, CommandHandler, CompactContext, CompactContributions, CompactEvent,
            CompactHandler, CompactResult, Extension, ExtensionCapability, ExtensionCommandResult,
            ExtensionConfig, ExtensionCtx, ExtensionError, ExtensionEvent, ExtensionManifest,
            HookMode, HookResult, LifecycleContext, LifecycleHandler, PostToolUseContext,
            PostToolUseHandler, PostToolUseResult, PreToolUseContext, PreToolUseHandler,
            PreToolUseResult, PromptBuildContext, PromptBuildHandler, PromptContributions,
            ProviderContext, ProviderEvent, ProviderHandler, ProviderResult, Registrar,
            SlashCommand, StatusItemUpdatePayload, StopReason, ToolHandler,
        },
        manifest::validate_manifest,
        s5r::effects::HandlerResult,
        tool::{ExecutionMode, ToolCapabilities, ToolDefinition, ToolExecutionContext, ToolResult},
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
            HostApi, HostClient, ManifestCatalog, Worker, WorkerCallContext, command_handler,
            handler_err, hook_handler, hook_handler_args, inject_host_api, parse_hook_input,
            parse_tool_arguments, tool_handler, tool_handler_args, tool_text,
        },
    };
}
