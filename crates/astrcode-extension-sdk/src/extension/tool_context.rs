//! Host-attributed context for one extension tool invocation.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use astrcode_core::{tool::ToolDefinition, types::SessionId};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{
    ExtensionCall, ExtensionCallContext, ExtensionError, SessionCallContext, parse_tool_arguments,
};
use crate::{
    WireErrorCode,
    host::{HostError, HostSessionDeliveryOutput, HostSessionInputRequest},
};

/// Immutable input and scoped host capabilities for one extension tool call.
///
/// The host owns construction and attribution. Authors cannot reach the core tool execution
/// context, raw session operations, or the event sink behind this value.
#[derive(Clone)]
pub struct ToolContext {
    call: SessionCallContext,
    working_dir: PathBuf,
    tool_name: Arc<str>,
    call_id: Option<Arc<str>>,
    arguments: Value,
    main_model_id: Option<Arc<str>>,
    small_model_id: Option<Arc<str>>,
    available_tools: Arc<[ToolDefinition]>,
}

impl ToolContext {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_runtime(
        call: SessionCallContext,
        working_dir: PathBuf,
        tool_name: impl Into<String>,
        call_id: Option<String>,
        arguments: Value,
        main_model_id: Option<String>,
        small_model_id: Option<String>,
        available_tools: Vec<ToolDefinition>,
    ) -> Self {
        Self {
            call,
            working_dir,
            tool_name: Arc::from(tool_name.into()),
            call_id: call_id.map(Arc::from),
            arguments,
            main_model_id: main_model_id.map(Arc::from),
            small_model_id: small_model_id.map(Arc::from),
            available_tools: available_tools.into(),
        }
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub fn call_id(&self) -> Option<&str> {
        self.call_id.as_deref()
    }

    /// Returns the host-attributed tool call id or a stable context error.
    pub fn require_call_id(&self) -> Result<&str, HostError> {
        self.call_id().ok_or_else(|| {
            HostError::new(
                WireErrorCode::ContextUnavailable,
                "tool handler requires a tool-call-scoped context",
            )
        })
    }

    pub fn raw_arguments(&self) -> &Value {
        &self.arguments
    }

    pub fn arguments<T: DeserializeOwned>(&self) -> Result<T, ExtensionError> {
        parse_tool_arguments(&self.tool_name, &self.arguments)
    }

    pub fn main_model_id(&self) -> Option<&str> {
        self.main_model_id.as_deref()
    }

    pub fn small_model_id(&self) -> Option<&str> {
        self.small_model_id.as_deref()
    }

    pub fn available_tools(&self) -> &[ToolDefinition] {
        &self.available_tools
    }

    pub fn session_id(&self) -> &SessionId {
        self.call.session_id()
    }

    /// Appends a user message to this tool's own session, to be absorbed into the model
    /// context at the next agent step boundary of the active turn.
    ///
    /// The call never wakes the session, queues input, or starts a new turn: when no turn
    /// is active it fails with a typed `no_active_turn` host error. Requires the
    /// `session_control` capability, like the other session input delivery operations.
    pub async fn defer_context(
        &self,
        content: impl Into<String>,
    ) -> Result<HostSessionDeliveryOutput, HostError> {
        self.host()
            .session_control()?
            .defer_context(HostSessionInputRequest {
                target_session_id: self.session_id().to_string(),
                content: content.into(),
            })
            .await
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.call.turn_id()
    }

    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }
}

impl ExtensionCall for ToolContext {
    fn call(&self) -> &ExtensionCallContext {
        self.call.call()
    }
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolContext")
            .field("call", &self.call)
            .field("tool_name", &self.tool_name)
            .field("call_id", &self.call_id)
            .field("main_model_id", &self.main_model_id)
            .field("small_model_id", &self.small_model_id)
            .field("available_tool_count", &self.available_tools.len())
            .finish_non_exhaustive()
    }
}
