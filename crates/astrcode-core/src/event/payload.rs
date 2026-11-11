//! 会话事件事实。
//!
//! 持久事件与实时事件使用不同的枚举。新增事件时必须在类型层面选择生命周期，
//! storage 因而不需要依赖运行时分类或“默认持久化”规则。

use serde::{Deserialize, Serialize};

use super::{
    AgentSessionOutcome, AssistantAttemptOutcome, CompactionOutcome, ContextTokenAnchor,
    PersistedSystemPrompt, StepEndReason, ToolCallOutcome, TurnEndReason,
    envelope::ToolOutputStream,
};
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
    SystemPromptConfigured(PersistedSystemPrompt),
    AgentSessionSpawned {
        child_session_id: SessionId,
        agent_name: String,
        task: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_selection: Option<SessionToolSelection>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<ToolCallId>,
    },
    AgentSessionSettled {
        child_session_id: SessionId,
        outcome: AgentSessionOutcome,
    },
    AgentSessionRecycled {
        child_session_id: SessionId,
    },
    TurnStarted,
    StepStarted {
        step_index: u32,
        attempt: u32,
    },
    StepEnded {
        step_index: u32,
        attempt: u32,
        reason: StepEndReason,
    },
    TurnEnded {
        reason: TurnEndReason,
    },
    TurnAbortedContext,
    UserInputAccepted {
        input: UserInput,
    },
    UserInputDiscarded {
        accepted_seq: u64,
        reason: String,
    },
    UserMessage {
        message_id: MessageId,
        text: String,
        attachments: Vec<MessageAttachment>,
        /// 对应 `UserInputAccepted` 的 durable seq；仅 turn 启动直接提交时为空。
        #[serde(skip_serializing_if = "Option::is_none")]
        accepted_seq: Option<u64>,
    },
    RecapGenerated {
        text: String,
        source: String,
    },
    AssistantMessage {
        message_id: MessageId,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        outcome: AssistantAttemptOutcome,
    },
    AssistantAttemptSettled {
        message_id: MessageId,
        outcome: AssistantAttemptOutcome,
    },
    TokenUsageRecorded {
        usage: LlmTokenUsage,
        model_context_window: usize,
    },
    ToolCallRequested {
        call_id: ToolCallId,
        /// 仅供出站模型请求配对；运行时与 UI 使用 `call_id`。
        provider_call_id: String,
        tool_name: String,
        /// 模型原始 JSON 文本；解析修复和执行 hook 不覆盖这个事实。
        arguments: String,
    },
    ToolApprovalRequested {
        call_id: ToolCallId,
        tool_name: String,
        prompt: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        rule_key: Option<String>,
        source: ApprovalSource,
        /// 审批针对执行前回调与归一化后的参数，不替换模型原文。
        arguments: serde_json::Value,
    },
    ToolApprovalResolved {
        call_id: ToolCallId,
        decision: ApprovalDecision,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    ToolCallSettled {
        call_id: ToolCallId,
        outcome: ToolCallOutcome,
        result: ToolResult,
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
        #[serde(skip)]
        input_token_anchor: Option<ContextTokenAnchor>,
    },
    ErrorOccurred {
        code: i32,
        message: String,
        recoverable: bool,
    },
    CustomEvent(CustomEventData),
}


#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "delta", rename_all = "snake_case")]
pub enum AssistantDelta {
    Text(String),
    Reasoning(String),
}

/// 只在进程内事件流和客户端通知中存在的瞬态事实。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LiveEventPayload {
    AgentRunStarted,
    AgentRunEnded {
        reason: TurnEndReason,
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
    AssistantMessageDelta {
        message_id: MessageId,
        delta: AssistantDelta,
    },
    ToolCallPreviewStarted {
        message_id: MessageId,
        call_id: ToolCallId,
        tool_name: String,
    },
    ToolExecutionStarted {
        call_id: ToolCallId,
        tool_name: String,
    },
    ToolCallArgumentsDelta {
        message_id: MessageId,
        call_id: ToolCallId,
        delta: String,
    },
    ToolCallPreviewDiscarded {
        call_id: ToolCallId,
    },
    ToolOutputDelta {
        call_id: ToolCallId,
        stream: ToolOutputStream,
        delta: String,
    },
    CompactionStarted,
    CompactionEnded {
        outcome: CompactionOutcome,
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
            "type": "tool_call_settled",
            "outcome": "failed",
            "call_id": "c1",
            "result": { "content": "boom", "is_error": true, "error": "boom", "metadata": {} }
        });
        let decoded = serde_json::from_value::<DurableEventPayload>(failed.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(decoded).unwrap()["result"]["metadata"],
            serde_json::json!({})
        );
        failed["result"].as_object_mut().unwrap().remove("metadata");
        assert!(serde_json::from_value::<DurableEventPayload>(failed).is_err());
    }
}
