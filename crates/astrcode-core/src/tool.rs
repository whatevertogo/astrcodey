//! Tool trait and associated types.
//!
//! Tools are the primary way the agent interacts with the world.
//! Extensions can register additional tools beyond the built-in set.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Definition of a tool, sent to the LLM as part of the function calling schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Unique tool name (e.g., "readFile", "shell").
    pub name: String,
    /// Human-readable description of what the tool does.
    pub description: String,
    /// JSON Schema for the tool's parameters.
    pub parameters: serde_json::Value,
    /// Whether this tool is built-in (true) or registered by an extension (false).
    pub is_builtin: bool,
}

/// Result of a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// The tool call ID this result corresponds to.
    pub call_id: String,
    /// Content output of the tool.
    pub content: String,
    /// Whether this result represents an error.
    pub is_error: bool,
    /// Optional metadata (e.g., file path, line count).
    pub metadata: BTreeMap<String, serde_json::Value>,
}

/// Error that can occur during tool execution.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("Tool not found: {0}")]
    NotFound(String),
    #[error("Invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("Execution error: {0}")]
    Execution(String),
    #[error("Tool execution blocked by hook: {0}")]
    Blocked(String),
    #[error("Timeout after {0}ms")]
    Timeout(u64),
}

/// Execution mode for a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionMode {
    /// Execute sequentially — one tool at a time.
    Sequential,
    /// Execute in parallel with other parallel-mode tools.
    Parallel,
}

/// The `Tool` trait that all tools (built-in and extension-registered) must implement.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// Returns the tool's definition for LLM function calling.
    fn definition(&self) -> ToolDefinition;

    /// Returns the tool's execution mode preference.
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    /// Executes the tool with the given arguments.
    async fn execute(&self, arguments: serde_json::Value) -> Result<ToolResult, ToolError>;
}

/// Capability specification for tool/skill metadata, used by extensions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitySpec {
    /// Unique capability name.
    pub name: String,
    /// Kind of capability.
    pub kind: CapabilityKind,
    /// Human-readable description.
    pub description: String,
}

/// Kind of capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    Tool,
    Skill,
    SlashCommand,
    ContextProvider,
    Hook,
}
