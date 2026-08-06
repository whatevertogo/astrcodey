//! Host-attributed context for one extension tool invocation.

use std::{path::Path, sync::Arc};

use astrcode_core::{tool::ToolDefinition, types::SessionId};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{
    ExtensionCallContext, ExtensionError, ExtensionEventEmitter, ExtensionPaths, ExtensionTasks,
};
use crate::host::ExtensionHost;

/// Immutable input and scoped host capabilities for one extension tool call.
///
/// The host owns construction and attribution. Authors cannot reach the core tool execution
/// context, raw session operations, or the event sink behind this value.
#[derive(Clone)]
pub struct ToolContext {
    call: ExtensionCallContext,
    tool_name: Arc<str>,
    call_id: Option<Arc<str>>,
    arguments: Value,
    main_model_id: Option<Arc<str>>,
    small_model_id: Option<Arc<str>>,
    available_tools: Arc<[ToolDefinition]>,
}

impl ToolContext {
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn from_runtime(
        call: ExtensionCallContext,
        tool_name: impl Into<String>,
        call_id: Option<String>,
        arguments: Value,
        main_model_id: Option<String>,
        small_model_id: Option<String>,
        available_tools: Vec<ToolDefinition>,
    ) -> Self {
        Self {
            call,
            tool_name: Arc::from(tool_name.into()),
            call_id: call_id.map(Arc::from),
            arguments,
            main_model_id: main_model_id.map(Arc::from),
            small_model_id: small_model_id.map(Arc::from),
            available_tools: available_tools.into(),
        }
    }

    pub fn call(&self) -> &ExtensionCallContext {
        &self.call
    }

    pub fn extension_id(&self) -> &str {
        self.call.extension_id()
    }

    pub fn session_id(&self) -> Option<&SessionId> {
        self.call.session_id()
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.call.turn_id()
    }

    pub fn working_dir(&self) -> Option<&Path> {
        self.call.working_dir()
    }

    pub fn paths(&self) -> &ExtensionPaths {
        self.call.paths()
    }

    pub fn host(&self) -> &ExtensionHost {
        self.call.host()
    }

    pub fn events(&self) -> &ExtensionEventEmitter {
        self.call.events()
    }

    pub fn tasks(&self) -> &ExtensionTasks {
        self.call.tasks()
    }

    pub fn cancellation(&self) -> &CancellationToken {
        self.call.cancellation()
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub fn call_id(&self) -> Option<&str> {
        self.call_id.as_deref()
    }

    pub fn raw_arguments(&self) -> &Value {
        &self.arguments
    }

    pub fn arguments<T: DeserializeOwned>(&self) -> Result<T, ExtensionError> {
        serde_path_to_error::deserialize(&self.arguments).map_err(|error| {
            let path = error.path().to_string();
            let path = if path.is_empty() { "$" } else { path.as_str() };
            ExtensionError::InvalidInput {
                code: "invalid_tool_arguments".into(),
                message: format!(
                    "tool `{}` arguments at `{path}`: {}",
                    self.tool_name,
                    error.into_inner()
                ),
                hint: Some(format!(
                    "check the `{}` tool arguments against its declared JSON schema",
                    self.tool_name
                )),
            }
        })
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
