//! Core types used across the hook system: enums, registration structs, and utility types.
//!
//! These are consumed by handler traits, context structs, and result enums.

use std::{collections::BTreeSet, sync::Arc};

use serde::{Deserialize, Serialize};

// ─── Hook mode ─────────────────────────────────────────────────────────

/// 钩子订阅的执行模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookMode {
    /// 同步执行，可以阻止操作。
    Blocking,
    /// 异步执行（即发即弃），不能阻止操作。
    NonBlocking,
    /// 执行但结果仅供参考。
    Advisory,
}

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

/// 插件在 PreCompact hook 中提供的 compact 摘要指令。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompactContributions {
    #[serde(default)]
    pub instructions: Vec<String>,
}

impl CompactContributions {
    pub fn merge(&mut self, other: CompactContributions) {
        self.instructions.extend(other.instructions);
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

/// Compact hook 触发时机。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactEvent {
    PreCompact,
    PostCompact,
}

// ─── Extension error ───────────────────────────────────────────────────

/// 扩展操作产生的错误。
#[derive(Debug, thiserror::Error)]
pub enum ExtensionError {
    #[error("Extension not found: {0}")]
    NotFound(String),
    #[error("Hook timed out after {0}ms")]
    Timeout(u64),
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
        event: crate::extension::ExtensionEvent,
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

// ─── ContinueAfterStop limit ───────────────────────────────────────────

/// 单个 `ContinueAfterStop` hook 在同一个 turn 内可请求的续跑上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
pub enum ContinueAfterStopLimit {
    Limited { max_per_turn: u32 },
    Unlimited,
}

impl ContinueAfterStopLimit {
    pub const fn limited(max_per_turn: u32) -> Self {
        Self::Limited { max_per_turn }
    }

    pub const fn unlimited() -> Self {
        Self::Unlimited
    }

    pub const fn allows(self, continuations_this_turn: u32) -> bool {
        match self {
            Self::Limited { max_per_turn } => continuations_this_turn < max_per_turn,
            Self::Unlimited => true,
        }
    }
}

impl TryFrom<i64> for ContinueAfterStopLimit {
    type Error = String;

    fn try_from(max_per_turn: i64) -> Result<Self, Self::Error> {
        match max_per_turn {
            -1 => Ok(Self::Unlimited),
            value if (0..=i64::from(u32::MAX)).contains(&value) => Ok(Self::limited(value as u32)),
            _ => {
                Err("continue_after_stop max_per_turn must be -1 or a non-negative integer".into())
            },
        }
    }
}

impl From<ContinueAfterStopLimit> for i64 {
    fn from(budget: ContinueAfterStopLimit) -> Self {
        match budget {
            ContinueAfterStopLimit::Limited { max_per_turn } => i64::from(max_per_turn),
            ContinueAfterStopLimit::Unlimited => -1,
        }
    }
}

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
