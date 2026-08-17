//! Core types used across the hook system: enums, registration structs, and utility types.
//!
//! These are consumed by handler traits, context structs, and result enums.

use std::{collections::BTreeSet, sync::Arc};

use astrcode_core::event::EventSendError;
use serde::{Deserialize, Serialize};

use crate::WireErrorCode;
pub use crate::wire::manifest::{CompactEvent, ContinueAfterStopLimit, HookMode};

// ─── Tool hook target ──────────────────────────────────────────────────

/// Tool hook 作用范围。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolHookTarget {
    All,
    Names(BTreeSet<String>),
}

impl ToolHookTarget {
    pub fn all() -> Self {
        Self::All
    }

    pub fn names(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::Names(names.into_iter().map(Into::into).collect())
    }

    pub fn matches(&self, tool_name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Names(names) => names.contains(tool_name),
        }
    }
}

// ─── Registration structs ──────────────────────────────────────────────

#[derive(Clone)]
pub struct ToolHookRegistration<H: ?Sized> {
    pub mode: HookMode,
    pub priority: i32,
    pub target: ToolHookTarget,
    pub handler: Arc<H>,
}

/// 一个同步工具参数变换或准入处理器的注册声明。
#[derive(Clone)]
pub struct ToolUseRegistration<H: ?Sized> {
    pub priority: i32,
    pub target: ToolHookTarget,
    pub handler: Arc<H>,
}

#[derive(Clone)]
pub struct ContinueAfterStopRegistration<H: ?Sized> {
    pub priority: i32,
    pub options: ContinueAfterStopOptions,
    pub handler: Arc<H>,
}

#[derive(Clone)]
pub struct UserMessageEnvelopeRegistration<H: ?Sized> {
    pub priority: i32,
    pub handler: Arc<H>,
}

// ─── Contribution types ────────────────────────────────────────────────

/// 插件在 PromptBuild hook 中提供的 prompt 片段。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptContributions {
    #[serde(default)]
    pub system_prompts: Vec<String>,
    #[serde(default)]
    pub additional_instructions: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub agents: Vec<String>,
}

impl PromptContributions {
    pub fn merge(&mut self, other: PromptContributions) {
        self.system_prompts.extend(other.system_prompts);
        self.additional_instructions
            .extend(other.additional_instructions);
        self.skills.extend(other.skills);
        self.agents.extend(other.agents);
    }
}

/// Extension-owned context that must remain visible after a transcript rewrite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CompactRetainedContext {
    File { path: String, content: String },
    Note { title: String, body: String },
}

/// Contributions collected before compacting a transcript snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactContributions {
    pub instructions: Vec<String>,
    pub retained_context: Vec<CompactRetainedContext>,
}

impl CompactContributions {
    pub fn merge(&mut self, other: CompactContributions) {
        self.instructions.extend(other.instructions);
        self.retained_context.extend(other.retained_context);
    }
}

// ─── Event discriminants ───────────────────────────────────────────────

/// Provider hook 触发时机。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderEvent {
    BeforeRequest,
    AfterResponse,
}

/// Host identity for one concrete provider request attempt.
///
/// A retry after context compaction receives a new identity. Extensions must use this value only
/// to correlate a prepared contribution with its eventual durable-success acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderRequestId(String);

impl ProviderRequestId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProviderRequestId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Extension-owned identity for the exact pending state represented by a provider contribution.
///
/// The identity must change whenever that pending state is replaced. The host returns it only
/// after the corresponding provider request and assistant durable facts have committed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderContributionId(String);

impl ProviderContributionId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProviderContributionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

// ─── Extension error ───────────────────────────────────────────────────

/// 扩展操作产生的错误。
#[derive(Debug, thiserror::Error)]
pub enum ExtensionError {
    #[error(transparent)]
    Config(#[from] crate::extension::ExtensionConfigError),
    #[error(transparent)]
    Path(#[from] crate::extension::ExtensionPathError),
    #[error(transparent)]
    Host(#[from] crate::host::HostError),
    #[error(transparent)]
    EventSend(#[from] EventSendError),
    #[error("invalid extension input `{code}`: {message}")]
    InvalidInput {
        code: String,
        message: String,
        hint: Option<String>,
    },
    #[error("Extension not found: {0}")]
    NotFound(String),
    #[error("Hook timed out after {0}ms")]
    Timeout(u64),
    #[error("operation cancelled")]
    Cancelled,
    #[error("extension {extension_id} is draining")]
    Draining { extension_id: String },
    #[error("blocked by hook: {reason}")]
    Blocked { reason: String },
    #[error("extension {extension_id} registered {hook} without declaring {capability:?}")]
    MissingCapability {
        extension_id: String,
        hook: &'static str,
        capability: crate::extension::ExtensionCapability,
    },
    #[error(
        "extension {extension_id} registered lifecycle {event:?} with blocking mode, but this \
         event is observe-only"
    )]
    InvalidLifecycleMode {
        extension_id: String,
        event: crate::extension::LifecycleEvent,
    },
    #[error(
        "extension {extension_id} tool `{tool_name}` conflicts with extension \
         {conflicting_extension_id}"
    )]
    ToolConflict {
        extension_id: String,
        tool_name: String,
        conflicting_extension_id: String,
    },
    #[error("extension {extension_id} has an invalid registration: {reason}")]
    InvalidRegistration {
        extension_id: String,
        reason: String,
    },
    #[error("extension error: {0}")]
    Internal(String),
}

impl ExtensionError {
    /// 构造 [`InvalidInput`](Self::InvalidInput) 错误，wire code 固定为 `invalid_input`。
    pub fn invalid_input(message: impl Into<String>, hint: impl Into<Option<String>>) -> Self {
        Self::InvalidInput {
            code: WireErrorCode::InvalidInput.as_str().into(),
            message: message.into(),
            hint: hint.into(),
        }
    }
}

// ─── ContinueAfterStop limit ───────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContinueAfterStopOptions {
    pub max_per_turn: ContinueAfterStopLimit,
}

impl ContinueAfterStopOptions {
    pub const fn limited(max_per_turn: u32) -> Self {
        Self {
            max_per_turn: ContinueAfterStopLimit::limited(max_per_turn),
        }
    }

    pub const fn unlimited() -> Self {
        Self {
            max_per_turn: ContinueAfterStopLimit::unlimited(),
        }
    }

    pub const fn allows(self, continuations_this_turn: u32) -> bool {
        self.max_per_turn.allows(continuations_this_turn)
    }
}

impl Default for ContinueAfterStopOptions {
    fn default() -> Self {
        Self::unlimited()
    }
}

// ─── Status item update ────────────────────────────────────────────────

/// 命令结果附带的状态栏更新。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusItemUpdatePayload {
    pub id: String,
    pub text: String,
}

// ─── Exchange summary ──────────────────────────────────────────────────

/// 当轮 user/assistant 消息摘要，仅 TurnEnd 事件填充。
#[derive(Debug, Clone)]
pub struct ExchangeSummary {
    pub user_message: String,
    pub assistant_message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options_are_unlimited() {
        assert!(ContinueAfterStopOptions::default().allows(u32::MAX));
    }

    #[test]
    fn serializes_unlimited_as_negative_one() {
        let value = serde_json::to_value(ContinueAfterStopLimit::unlimited()).unwrap();
        assert_eq!(value, serde_json::json!(-1));
    }

    #[test]
    fn deserializes_non_negative_values_as_limited_limit() {
        let limit: ContinueAfterStopLimit = serde_json::from_value(serde_json::json!(7)).unwrap();
        assert_eq!(limit, ContinueAfterStopLimit::limited(7));
    }

    #[test]
    fn rejects_negative_values_other_than_unlimited_sentinel() {
        let error = serde_json::from_value::<ContinueAfterStopLimit>(serde_json::json!(-2))
            .expect_err("negative values other than -1 should be invalid");
        assert!(error.to_string().contains("must be -1"));
    }
}
