//! Result enums returned by extension hook handlers.

use super::types::{CompactContributions, ProviderContributionId};

/// Request-local message effect attached to a contribution that needs durable-success
/// acknowledgement.
#[derive(Debug, Clone)]
pub enum PreparedProviderEffect {
    Unchanged,
    ReplaceMessages(Vec<crate::llm::LlmMessage>),
    AppendMessages(Vec<crate::llm::LlmMessage>),
}

/// Exact pending extension state and its request-local message effect.
#[derive(Debug, Clone)]
pub struct PreparedProviderContribution {
    contribution_id: ProviderContributionId,
    effect: PreparedProviderEffect,
}

impl PreparedProviderContribution {
    pub fn new(contribution_id: ProviderContributionId, effect: PreparedProviderEffect) -> Self {
        Self {
            contribution_id,
            effect,
        }
    }

    pub fn into_parts(self) -> (ProviderContributionId, PreparedProviderEffect) {
        (self.contribution_id, self.effect)
    }
}

/// 通用钩子结果。
#[derive(Debug, Clone)]
pub enum HookResult {
    Allow,
    Block { reason: String },
}

/// 工具参数变换结果。
#[derive(Debug, Clone)]
pub enum ToolInputTransformResult {
    Unchanged,
    Replace { tool_input: serde_json::Value },
}

/// 单个 PreToolUse 准入处理器的决策。
#[derive(Debug, Clone)]
pub enum PreToolUseResult {
    Allow,
    Block {
        reason: String,
    },
    /// 请求用户审批后再执行（扩展 Gate Ask）。
    Ask {
        prompt: String,
        rule_key: Option<String>,
    },
}

/// 一个 Extension 准入条件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreToolUseRequirement {
    pub prompt: String,
    pub rule_key: Option<String>,
}

/// 所有匹配 PreToolUse 处理器的组合决策。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreToolUseAdmission {
    Allow,
    Ask {
        requirements: Vec<PreToolUseRequirement>,
    },
    Block {
        reason: String,
    },
}

/// PostToolUse 钩子结果。
///
/// `ModifyResult` 仅替换 ToolResult 的文本内容（`content` 字段）；其它结构化
/// 字段——`is_error` / `error` / `metadata` / `duration_ms`——保持不变。
#[derive(Debug, Clone)]
pub enum PostToolUseResult {
    Allow,
    Block { reason: String },
    ModifyResult { content: String },
}

/// LLM 自然结束（无 tool call）后，扩展是否再跑一个 agent step。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinueAfterStopResult {
    EndTurn,
    ContinueOneStep,
}

/// Provider 钩子结果。
#[derive(Debug, Clone)]
pub enum ProviderResult {
    Allow,
    Block {
        reason: String,
    },
    ReplaceMessages {
        messages: Vec<crate::llm::LlmMessage>,
    },
    AppendMessages {
        messages: Vec<crate::llm::LlmMessage>,
    },
}

/// PreCompact hook result.
#[derive(Debug, Clone)]
pub enum PreCompactResult {
    Allow,
    Block { reason: String },
    Contributions(CompactContributions),
}

/// 用户消息 envelope 变换结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserMessageEnvelopeResult {
    Allow,
    ReplaceText { text: String },
    AppendText { text: String },
    Block { reason: String },
}
