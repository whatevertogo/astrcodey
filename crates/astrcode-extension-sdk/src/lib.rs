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
