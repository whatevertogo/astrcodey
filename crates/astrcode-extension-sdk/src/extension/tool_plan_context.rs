//! Side-effect-free context for planning one extension tool invocation.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use astrcode_core::types::SessionId;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::ExtensionError;
use crate::WireErrorCode;

/// Immutable facts available while an extension plans a tool invocation.
///
/// This context intentionally has no Host client, event emitter, task owner, or persistence path.
/// Planning must only interpret the already-final arguments into a resource set.
#[derive(Clone)]
pub struct ToolPlanContext {
    extension_id: Arc<str>,
    session_id: SessionId,
    turn_id: Option<Arc<str>>,
    working_dir: PathBuf,
    tool_name: Arc<str>,
    call_id: Option<Arc<str>>,
    arguments: Value,
    cancellation: CancellationToken,
}

impl ToolPlanContext {
    pub(crate) fn from_runtime(
        extension_id: impl Into<String>,
        session_id: SessionId,
        working_dir: PathBuf,
        tool_name: impl Into<String>,
        arguments: Value,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            extension_id: Arc::from(extension_id.into()),
            session_id,
            turn_id: None,
            working_dir,
            tool_name: Arc::from(tool_name.into()),
            call_id: None,
            arguments,
            cancellation,
        }
    }

    pub(crate) fn with_turn_id(mut self, turn_id: Option<String>) -> Self {
        self.turn_id = turn_id.map(Arc::from);
        self
    }

    pub(crate) fn with_call_id(mut self, call_id: Option<String>) -> Self {
        self.call_id = call_id.map(Arc::from);
        self
    }

    pub fn extension_id(&self) -> &str {
        &self.extension_id
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.turn_id.as_deref()
    }

    pub fn working_dir(&self) -> &Path {
        &self.working_dir
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
                code: WireErrorCode::InvalidInput.as_str().into(),
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

    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }
}

impl std::fmt::Debug for ToolPlanContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolPlanContext")
            .field("extension_id", &self.extension_id)
            .field("session_id", &self.session_id)
            .field("turn_id", &self.turn_id)
            .field("working_dir", &self.working_dir)
            .field("tool_name", &self.tool_name)
            .field("call_id", &self.call_id)
            .field("cancelled", &self.cancellation.is_cancelled())
            .finish_non_exhaustive()
    }
}
