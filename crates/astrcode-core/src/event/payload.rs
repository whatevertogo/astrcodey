//! 会话事件事实。
//!
//! 持久事件与实时事件使用不同的枚举。新增事件时必须在类型层面选择生命周期，
//! storage 因而不需要依赖运行时分类或“默认持久化”规则。

use serde::{Deserialize, Serialize};

use super::{PersistedSystemPrompt, SystemPromptSource, envelope::ToolOutputStream};
use crate::{
    compaction::CompactStrategy,
    llm::{LlmMessage, LlmTokenUsage},
    message_attachment::MessageAttachment,
    permission::{ApprovalDecision, ApprovalSource},
    tool::{SessionToolSelection, ToolResult},
    types::*,
};

/// 子会话与父会话之间的稳定关系。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParentSessionRef {
    pub session_id: SessionId,
}

/// 创建会话时一次性确定的初始状态。
///
/// 它是首条 durable 事件的载荷。必需字段不使用 `Option`，因此成功创建的会话
/// 天然具备可投影的 identity、工具边界和 system prompt。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionStarted {
    pub working_dir: String,
    pub model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<ParentSessionRef>,
    pub tool_selection: SessionToolSelection,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_extension: Option<String>,
    pub initial_system_prompt: PersistedSystemPrompt,
}

/// 扩展事件的公共事实；是否持久化由外层事件类型表达。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtensionEventData {
    pub extension_id: String,
    pub event_type: String,
    pub schema_version: u32,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompactionDetails {
    pub trigger: String,
    pub pre_tokens: usize,
    pub post_tokens: usize,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    pub strategy: CompactStrategy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TranscriptRewriteReason {
    Compaction(CompactionDetails),
}

/// 可写入 session event log 的事实。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DurableEventPayload {
    SessionStarted(SessionStarted),
    ModelIdChanged {
        model_id: String,
    },
    SessionToolsConfigured {
        selection: SessionToolSelection,
    },
    SystemPromptConfigured {
        text: String,
        fingerprint: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        extra_system_prompt: Option<String>,
        #[serde(skip_serializing_if = "SystemPromptSource::is_native")]
        source: SystemPromptSource,
    },
    AgentSessionSpawned {
        child_session_id: SessionId,
        agent_name: String,
        task: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_selection: Option<SessionToolSelection>,
        tool_call_id: ToolCallId,
    },
    AgentSessionCompleted {
        child_session_id: SessionId,
        final_session_id: SessionId,
        summary: String,
    },
    AgentSessionFailed {
        child_session_id: SessionId,
        final_session_id: SessionId,
        error: String,
    },
    AgentSessionRecycled {
        child_session_id: SessionId,
    },
    TurnStarted,
    TurnCompleted {
        finish_reason: String,
    },
    TurnAbortedContext,
    UserMessage {
        message_id: MessageId,
        text: String,
        attachments: Vec<MessageAttachment>,
    },
    RecapGenerated {
        text: String,
        source: String,
    },
    AssistantMessageCompleted {
        message_id: MessageId,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
    },
    TokenUsageRecorded {
        usage: LlmTokenUsage,
        model_context_window: usize,
    },
    ToolCallRequested {
        call_id: ToolCallId,
        tool_name: String,
        arguments: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_arguments: Option<String>,
    },
    ToolApprovalRequested {
        call_id: ToolCallId,
        tool_name: String,
        prompt: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        rule_key: Option<String>,
        source: ApprovalSource,
        arguments: serde_json::Value,
    },
    ToolApprovalResolved {
        call_id: ToolCallId,
        decision: ApprovalDecision,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    ToolCallCompleted {
        call_id: ToolCallId,
        tool_name: String,
        result: ToolResult,
        arguments: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments_json: Option<serde_json::Value>,
    },
    ToolCallFailed {
        call_id: ToolCallId,
        tool_name: String,
        error: String,
        #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        metadata: std::collections::BTreeMap<String, serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        arguments: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments_json: Option<serde_json::Value>,
    },
    ToolCallCancelled {
        call_id: ToolCallId,
        tool_name: String,
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        arguments: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments_json: Option<serde_json::Value>,
    },
    /// 当前 projection 仍位于 `source_seq` 时，原子替换 provider transcript。
    TranscriptRewritten {
        source_seq: u64,
        messages: Vec<LlmMessage>,
        reason: TranscriptRewriteReason,
    },
    SessionForked {
        source_session_id: SessionId,
        source_cursor: Cursor,
        #[serde(skip_serializing_if = "Option::is_none")]
        first_user_message: Option<String>,
        messages: Vec<LlmMessage>,
    },
    ErrorOccurred {
        code: i32,
        message: String,
        recoverable: bool,
    },
    ExtensionEvent(ExtensionEventData),
}

/// 只在进程内事件流和客户端通知中存在的瞬态事实。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LiveEventPayload {
    AgentRunStarted,
    AgentRunCompleted {
        reason: String,
    },
    AssistantMessageStarted {
        message_id: MessageId,
    },
    AssistantTextDelta {
        message_id: MessageId,
        delta: String,
    },
    ThinkingDelta {
        message_id: MessageId,
        delta: String,
    },
    ToolCallStarted {
        call_id: ToolCallId,
        tool_name: String,
    },
    ToolCallArgumentsDelta {
        call_id: ToolCallId,
        delta: String,
    },
    ToolOutputDelta {
        call_id: ToolCallId,
        stream: ToolOutputStream,
        delta: String,
    },
    CompactionStarted,
    CompactionCompleted {
        messages_removed: usize,
    },
    CompactionSkipped {
        reason: String,
    },
    CompactionFailed {
        reason: String,
    },
    ErrorOccurred {
        code: i32,
        message: String,
        recoverable: bool,
    },
    ExtensionEvent(ExtensionEventData),
}
