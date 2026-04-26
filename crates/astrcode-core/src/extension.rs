//! Extension and hook system types.
//!
//! Extensions are the primary extensibility mechanism.
//! Skills, agent profiles, custom tools, slash commands — all are extensions.

use serde::{Deserialize, Serialize};

use crate::config::ModelSelection;
use crate::prompt::BlockSpec;
use crate::tool::{CapabilitySpec, ToolDefinition};
use crate::types::SessionId;

// ─── Extension Trait ─────────────────────────────────────────────────────

/// An extension that hooks into the astrcode lifecycle.
///
/// Extensions are loaded from `~/.astrcode/extensions/` (global)
/// and `.astrcode/extensions/` (project-level). They can subscribe to
/// lifecycle events, register tools, slash commands, and context providers.
#[async_trait::async_trait]
pub trait Extension: Send + Sync {
    /// Unique extension identifier.
    fn id(&self) -> &str;

    /// Events this extension subscribes to, with their hook modes.
    fn subscriptions(&self) -> Vec<(ExtensionEvent, HookMode)>;

    /// Handle an event.
    ///
    /// Returns `HookEffect` to allow, block, or modify the action.
    async fn on_event(
        &self,
        event: ExtensionEvent,
        ctx: &dyn ExtensionContext,
    ) -> Result<HookEffect, ExtensionError>;

    /// Optional: tools registered by this extension.
    fn tools(&self) -> Vec<ToolDefinition> {
        vec![]
    }

    /// Optional: slash commands registered by this extension.
    fn slash_commands(&self) -> Vec<SlashCommand> {
        vec![]
    }

    /// Optional: context providers (contributors) registered by this extension.
    fn context_contributions(&self) -> Vec<BlockSpec> {
        vec![]
    }

    /// Optional: capabilities registered by this extension.
    fn capabilities(&self) -> Vec<CapabilitySpec> {
        vec![]
    }
}

// ─── Lifecycle Events ────────────────────────────────────────────────────

/// Core lifecycle events that extensions can subscribe to.
///
/// 12 events covering the full session/agent/turn/message/tool lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionEvent {
    // Session-level
    SessionStart,
    SessionBeforeFork,
    SessionBeforeCompact,
    SessionShutdown,

    // Agent-level
    AgentStart,
    AgentEnd,

    // Turn-level
    TurnStart,
    TurnEnd,

    // Message-level
    MessageDelta,

    // Tool-level (primary hook points)
    BeforeToolCall,
    AfterToolCall,

    // User input
    UserPromptSubmit,
}

// ─── Hook Mode ───────────────────────────────────────────────────────────

/// Execution mode for a hook subscription.
///
/// Determines whether the hook can block the action, runs asynchronously,
/// or is purely advisory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookMode {
    /// Hook runs synchronously and can block the action.
    /// Used for: security review, permission enforcement.
    Blocking,

    /// Hook runs asynchronously (fire-and-forget), cannot block.
    /// Used for: logging, analytics, notifications.
    NonBlocking,

    /// Hook runs but its result is informational only.
    /// Used for: style suggestions, optional guidance.
    Advisory,
}

// ─── Hook Effect ─────────────────────────────────────────────────────────

/// The result of a hook execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEffect {
    /// Allow the action to proceed normally.
    Allow,

    /// Block the action with a reason.
    /// Only valid from Blocking hooks.
    Block { reason: String },

    /// Modify the action's data.
    /// For AfterToolCall: patches tool result content.
    Modify { patches: Vec<Modification> },
}

/// A modification made by a hook.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Modification {
    /// Replace text in the target.
    Replace {
        /// Old text to find.
        old: String,
        /// New text to substitute.
        new: String,
    },
    /// Truncate the target to a maximum length.
    Truncate { max_len: usize },
    /// Prepend text.
    Prepend { text: String },
}

// ─── Extension Capabilities Summary ──────────────────────────────────────

/// Summary of what an extension provides.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionCapabilities {
    /// Extension ID.
    pub id: String,
    /// Events subscribed to with their modes.
    pub events: Vec<(ExtensionEvent, HookMode)>,
    /// Number of tools registered.
    pub tool_count: usize,
    /// Number of slash commands registered.
    pub command_count: usize,
}

// ─── Slash Command ───────────────────────────────────────────────────────

/// A slash command registered by an extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommand {
    /// Command name (without the leading slash).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Argument schema (JSON Schema).
    pub args_schema: Option<serde_json::Value>,
}

// ─── Extension Context ───────────────────────────────────────────────────

/// Restricted view of session + services available to extension handlers.
///
/// Extensions get a limited API surface to prevent them from
/// destabilizing the core system.
#[async_trait::async_trait]
pub trait ExtensionContext: Send + Sync {
    /// Get the current session ID.
    fn session_id(&self) -> &str;

    /// Get the working directory for this session.
    fn working_dir(&self) -> &str;

    /// Get the current model selection.
    fn model_selection(&self) -> ModelSelection;

    /// Read a configuration value by key.
    fn config_value(&self, key: &str) -> Option<String>;

    /// Emit a custom event to the session log.
    async fn emit_custom_event(&self, name: &str, data: serde_json::Value);

    /// Look up a tool definition by name from the tool registry.
    fn find_tool(&self, name: &str) -> Option<ToolDefinition>;
}

// ─── Extension Error ─────────────────────────────────────────────────────

/// Error from extension operations.
#[derive(Debug, thiserror::Error)]
pub enum ExtensionError {
    #[error("Extension not found: {0}")]
    NotFound(String),
    #[error("Hook timed out after {0}ms")]
    Timeout(u64),
    #[error("Extension error: {0}")]
    Internal(String),
}

// ─── Agent Profile (basic type for collaboration tools) ──────────────────

/// Agent profile — a named agent configuration.
///
/// Core only defines the type. Loading and management is done by extensions.
/// The agent collaboration tools (spawn/send/observe/close) use this type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProfile {
    /// Profile identifier.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Description of what this agent does.
    pub description: String,
    /// Guide/instructions for this agent type.
    pub guide: String,
    /// Tools this agent can use (empty = all available).
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Preferred model for this agent type.
    pub model_preference: Option<String>,
}
