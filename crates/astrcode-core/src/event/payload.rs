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
    user_input::UserInput,
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

/// 自定义事件的公共事实；是否持久化由外层事件类型表达。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CustomEventData {
    pub extension_id: String,
    pub event_type: String,
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<EventId>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cascade_depth: u8,
    pub payload: serde_json::Value,
}

const fn is_zero(value: &u8) -> bool {
    *value == 0
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
        // `default` 兼容旧日志：Native 是默认来源，序列化时被省略，缺失等价于 Native。
        #[serde(default, skip_serializing_if = "SystemPromptSource::is_native")]
        source: SystemPromptSource,
    },
    AgentSessionSpawned {
        child_session_id: SessionId,
        agent_name: String,
        task: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_selection: Option<SessionToolSelection>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<ToolCallId>,
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
    UserInputAccepted {
        input: UserInput,
    },
    UserMessage {
        message_id: MessageId,
        text: String,
        attachments: Vec<MessageAttachment>,
        /// 对应 `UserInputAccepted` 的 durable seq；直接启动或 turn 中注入时为空。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accepted_seq: Option<u64>,
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
        // `default` 兼容旧日志：空 metadata 序列化时被省略，反序列化时缺失等价于空 map。
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
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
        /// 被替换前缀（system prompt + provider 视角消息）的 `transcript_prefix_fingerprint`；
        /// 提交时必须与 projection 重算结果一致。
        source_fingerprint: String,
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
    CustomEvent(CustomEventData),
}

/// 只在进程内事件流和客户端通知中存在的瞬态事实。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LiveEventPayload {
    AgentRunStarted,
    AgentRunCompleted {
        reason: String,
    },
    LlmRetrying {
        /// HTTP 状态码；连接或响应流中断时为空。
        status: Option<u16>,
        attempt: u32,
        max_retries: u32,
        delay_ms: u64,
    },
    LlmRetryRecovered,
    AssistantMessageStarted {
        message_id: MessageId,
    },
    /// 丢弃同一消息在失败流中产生的临时文本与思考内容。
    AssistantMessageReset {
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
    CustomEvent(CustomEventData),
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_event_serializes_with_new_tag() {
        let payload = DurableEventPayload::CustomEvent(CustomEventData {
            extension_id: "ext".into(),
            event_type: "thing".into(),
            schema_version: 1,
            causation_id: None,
            cascade_depth: 0,
            payload: serde_json::json!({}),
        });
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains(r#""type":"custom_event""#));
        assert!(!json.contains("causation_id"));
    }

    #[test]
    fn tool_call_failed_accepts_missing_empty_metadata() {
        // 旧日志写入时空 metadata 被省略；缺失必须等价于空 map，否则整个会话无法重放。
        let json = r#"{"type":"tool_call_failed","call_id":"c1","tool_name":"t","error":"boom","arguments":"{}"}"#;
        let payload = serde_json::from_str::<DurableEventPayload>(json).unwrap();
        let DurableEventPayload::ToolCallFailed { ref metadata, .. } = payload else {
            panic!("expected tool_call_failed");
        };
        assert!(metadata.is_empty());

        // 写读往返：空 metadata 序列化时省略，再读回仍为空 map。
        let roundtrip = serde_json::to_string(&payload).unwrap();
        assert!(!roundtrip.contains("metadata"));
        let payload = serde_json::from_str::<DurableEventPayload>(&roundtrip).unwrap();
        let DurableEventPayload::ToolCallFailed { metadata, .. } = payload else {
            panic!("expected tool_call_failed");
        };
        assert!(metadata.is_empty());
    }

    #[test]
    fn system_prompt_configured_accepts_missing_native_source() {
        // 旧日志写入 Native 来源时省略 source 字段；缺失必须等价于 Native。
        let json = r#"{"type":"system_prompt_configured","text":"hi","fingerprint":"fp"}"#;
        let payload = serde_json::from_str::<DurableEventPayload>(json).unwrap();
        let DurableEventPayload::SystemPromptConfigured { source, .. } = payload else {
            panic!("expected system_prompt_configured");
        };
        assert_eq!(source, SystemPromptSource::Native);
    }
}
