//! 会话事件事实。
//!
//! 持久事件与实时事件使用不同的枚举。新增事件时必须在类型层面选择生命周期，
//! storage 因而不需要依赖运行时分类或“默认持久化”规则。

use serde::{Deserialize, Serialize};

use super::{PersistedSystemPrompt, SystemPromptSource, envelope::ToolOutputStream};
use crate::{
    compaction::CompactStrategy,
    llm::{LlmTokenUsage, TranscriptMessage},
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

/// Host-attributed audience for a custom event.
///
/// Persistence remains encoded by the outer durable/live payload type. The audience is carried
/// with the event so transport fan-out never has to infer product semantics from an extension id
/// or event name.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CustomEventAudience {
    Session,
    Global,
}

/// 自定义事件的公共事实；是否持久化由外层事件类型表达。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CustomEventData {
    pub extension_id: String,
    pub event_type: String,
    pub schema_version: u32,
    pub audience: CustomEventAudience,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<EventId>,
    pub cascade_depth: u8,
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
        source: SystemPromptSource,
    },
    AgentSessionSpawned {
        child_session_id: SessionId,
        agent_name: String,
        task: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_selection: Option<SessionToolSelection>,
        #[serde(skip_serializing_if = "Option::is_none")]
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
    StepStarted {
        step_index: u32,
        attempt: u32,
    },
    StepCompleted {
        step_index: u32,
        attempt: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        finish_reason: Option<String>,
    },
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
        #[serde(skip_serializing_if = "Option::is_none")]
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
        messages: Vec<TranscriptMessage>,
        reason: TranscriptRewriteReason,
    },
    SessionForked {
        source_session_id: SessionId,
        source_cursor: Cursor,
        #[serde(skip_serializing_if = "Option::is_none")]
        first_user_message: Option<String>,
        messages: Vec<TranscriptMessage>,
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
    fn durable_payload_uses_canonical_tags_and_requires_complete_facts() {
        let payload = DurableEventPayload::CustomEvent(CustomEventData {
            extension_id: "ext".into(),
            event_type: "thing".into(),
            schema_version: 1,
            audience: CustomEventAudience::Session,
            causation_id: None,
            cascade_depth: 0,
            payload: serde_json::json!({}),
        });
        let canonical = serde_json::to_value(&payload).unwrap();
        assert_eq!(canonical["type"], "custom_event");
        assert_eq!(canonical["audience"], "session");
        assert!(canonical.get("causation_id").is_none());
        assert_eq!(canonical["cascade_depth"], 0);
        assert_eq!(
            serde_json::from_value::<DurableEventPayload>(canonical.clone()).unwrap(),
            payload
        );

        let mut incomplete_custom_event = canonical.clone();
        incomplete_custom_event
            .as_object_mut()
            .unwrap()
            .remove("cascade_depth");
        assert!(serde_json::from_value::<DurableEventPayload>(incomplete_custom_event).is_err());

        let mut incomplete_custom_event = canonical;
        incomplete_custom_event
            .as_object_mut()
            .unwrap()
            .remove("audience");
        assert!(serde_json::from_value::<DurableEventPayload>(incomplete_custom_event).is_err());

        let mut configured = serde_json::json!({
            "type": "system_prompt_configured",
            "text": "hi",
            "fingerprint": "fp",
            "source": "native"
        });
        assert!(serde_json::from_value::<DurableEventPayload>(configured.clone()).is_ok());
        configured.as_object_mut().unwrap().remove("source");
        assert!(serde_json::from_value::<DurableEventPayload>(configured).is_err());

        let mut failed = serde_json::json!({
            "type": "tool_call_failed",
            "call_id": "c1",
            "tool_name": "t",
            "error": "boom",
            "metadata": {},
            "arguments": "{}"
        });
        let decoded = serde_json::from_value::<DurableEventPayload>(failed.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(decoded).unwrap()["metadata"],
            serde_json::json!({})
        );
        failed.as_object_mut().unwrap().remove("metadata");
        assert!(serde_json::from_value::<DurableEventPayload>(failed).is_err());
    }
}
