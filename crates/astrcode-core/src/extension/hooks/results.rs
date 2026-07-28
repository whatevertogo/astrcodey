//! Result enums returned by extension hook handlers.

use super::types::CompactContributions;

/// 通用钩子结果。
#[derive(Debug, Clone)]
pub enum HookResult {
    Allow,
    Block { reason: String },
}

/// PreToolUse 钩子结果。
#[derive(Debug, Clone)]
pub enum PreToolUseResult {
    Allow,
    Block {
        reason: String,
    },
    ModifyInput {
        tool_input: serde_json::Value,
    },
    /// 请求用户审批后再执行（扩展 Gate Ask）。
    Ask {
        prompt: String,
        rule_key: Option<String>,
    },
}

/// PostToolUse 钩子结果。
///
/// `ModifyResult` 仅替换 ToolResult 的文本内容（`content` 字段）；其它结构化
/// 字段——`is_error` / `metadata` / `artifact_ref` / `duration_ms`——保持不变。
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

/// Compact 钩子结果。
#[derive(Debug, Clone)]
pub enum CompactResult {
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

/// 工具结果批次落盘后的继续/结束决策结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterToolResultsResult {
    Continue,
    EndTurn { reason: String },
}
