//! 统一的运行时事件与持久化事件类型。
//!
//! 本模块定义了 astrcode 平台中所有事件的核心数据结构，包括：
//! - [`Phase`]：会话执行阶段的枚举
//! - [`EventPayload`]：事件载荷的统一枚举类型
//! - [`Event`]：携带会话/轮次标识和存储序号的事件信封

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{tool::ToolResult, types::*};

/// 会话的执行阶段。
///
/// 该枚举由 reducer 从事件流中推导得出，而非事件日志的权威来源，
/// 因为工具并发需要依赖 reducer 的状态。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// 空闲状态，无活跃操作。
    #[default]
    Idle,
    /// 正在思考（LLM 推理中）。
    Thinking,
    /// 正在流式输出文本。
    Streaming,
    /// 正在调用工具。
    CallingTool,
    /// 正在压缩上下文。
    Compacting,
    /// 发生错误。
    Error,
}

/// 工具调用过程中的输出流类型。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutputStream {
    /// 标准输出流。
    Stdout,
    /// 标准错误流。
    Stderr,
}

/// 统一的 astrcode 事件载荷。
///
/// 使用 `#[serde(tag = "type")]` 实现扁平化的 JSON 序列化，
/// 每个变体在序列化时会自动带上 `"type"` 字段用于区分。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventPayload {
    /// 会话已创建。
    SessionStarted {
        /// 工作目录路径。
        working_dir: String,
        /// 使用的模型标识。
        model_id: String,
        /// 父会话 ID，用于子会话场景。根会话为 `None`。
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_session_id: Option<String>,
    },

    /// 会话已删除。
    SessionDeleted,

    /// Agent 运行开始。
    AgentRunStarted,

    /// Agent 运行完成。
    AgentRunCompleted {
        /// 完成原因描述。
        reason: String,
    },

    /// 用户轮次开始。
    TurnStarted,

    /// 用户轮次完成。
    TurnCompleted {
        /// 完成原因（如 "stop"、"tool_use" 等）。
        finish_reason: String,
    },

    /// 用户发送的消息。
    UserMessage {
        /// 消息唯一标识。
        message_id: MessageId,
        /// 消息文本内容。
        text: String,
    },

    /// 助手消息开始（流式输出的起始标记）。
    AssistantMessageStarted {
        /// 消息唯一标识。
        message_id: MessageId,
    },

    /// 助手消息的文本增量（流式输出片段）。
    AssistantTextDelta {
        /// 消息唯一标识。
        message_id: MessageId,
        /// 本次增量文本。
        delta: String,
    },

    /// 助手消息已完成（流式输出结束）。
    AssistantMessageCompleted {
        /// 消息唯一标识。
        message_id: MessageId,
        /// 完整的消息文本。
        text: String,
    },

    /// 思考过程的文本增量（用于推理模型的思维链）。
    ThinkingDelta {
        /// 本次增量文本。
        delta: String,
    },

    /// 工具调用已开始。
    ToolCallStarted {
        /// 工具调用唯一标识。
        call_id: ToolCallId,
        /// 工具名称。
        tool_name: String,
    },

    /// 工具调用参数的增量数据（流式解析）。
    ToolCallArgumentsDelta {
        /// 工具调用唯一标识。
        call_id: ToolCallId,
        /// 本次增量参数 JSON 片段。
        delta: String,
    },

    /// 工具调用请求已完成（参数解析完毕，准备执行）。
    ToolCallRequested {
        /// 工具调用唯一标识。
        call_id: ToolCallId,
        /// 工具名称。
        tool_name: String,
        /// 完整的工具调用参数（JSON 值）。
        arguments: serde_json::Value,
    },

    /// 工具执行过程中的输出增量（stdout/stderr 流）。
    ToolOutputDelta {
        /// 工具调用唯一标识。
        call_id: ToolCallId,
        /// 输出流类型（标准输出或标准错误）。
        stream: ToolOutputStream,
        /// 本次增量输出文本。
        delta: String,
    },

    /// 工具调用已完成。
    ToolCallCompleted {
        /// 工具调用唯一标识。
        call_id: ToolCallId,
        /// 工具名称。
        tool_name: String,
        /// 工具执行结果。
        result: ToolResult,
    },

    /// 上下文压缩已开始。
    CompactionStarted,

    /// 上下文压缩已完成。
    CompactionCompleted {
        /// 压缩前的 token 数量。
        pre_tokens: usize,
        /// 压缩后的 token 数量。
        post_tokens: usize,
        /// 压缩生成的摘要文本。
        summary: String,
    },

    /// 发生错误。
    ErrorOccurred {
        /// 错误码。
        code: i32,
        /// 错误消息。
        message: String,
        /// 是否可恢复（可恢复的错误允许继续会话）。
        recoverable: bool,
    },

    /// 自定义事件（由扩展或外部系统发出）。
    Custom {
        /// 事件名称。
        name: String,
        /// 事件数据（任意 JSON 值）。
        data: serde_json::Value,
    },
}

impl EventPayload {
    /// 判断该事件是否应持久化到会话的 JSONL 事件日志中。
    ///
    /// 增量类事件（如文本增量、参数增量、输出增量）和临时控制事件
    /// 不需要持久化，因为它们可以从持久化事件中重建。
    pub fn is_durable(&self) -> bool {
        !matches!(
            self,
            Self::AssistantTextDelta { .. }
                | Self::ThinkingDelta { .. }
                | Self::ToolCallArgumentsDelta { .. }
                | Self::ToolOutputDelta { .. }
                | Self::CompactionStarted
                | Self::AgentRunStarted
                | Self::AgentRunCompleted { .. }
        )
    }
}

/// 事件信封，携带会话/轮次标识和存储序号。
///
/// 序号（`seq`）由存储层在追加事件时分配，用于事件日志的有序读取。
/// `payload` 使用 `#[serde(flatten)]` 与信封字段平铺在同一 JSON 对象中。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    /// 存储层分配的递增序号，新创建时为 `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// 事件唯一标识。
    pub id: EventId,
    /// 所属会话标识。
    pub session_id: SessionId,
    /// 所属轮次标识，会话级别事件为 `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    /// 事件时间戳（UTC）。
    pub timestamp: DateTime<Utc>,
    /// 事件载荷（使用 flatten 平铺序列化）。
    #[serde(flatten)]
    pub payload: EventPayload,
}

impl Event {
    /// 创建一个新事件，自动生成 ID 和当前时间戳。
    ///
    /// - `session_id`: 所属会话 ID
    /// - `turn_id`: 所属轮次 ID（可为 `None`）
    /// - `payload`: 事件载荷
    pub fn new(session_id: SessionId, turn_id: Option<TurnId>, payload: EventPayload) -> Self {
        Self {
            seq: None,
            id: new_event_id(),
            session_id,
            turn_id,
            timestamp: Utc::now(),
            payload,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn event_serializes_as_flat_json() {
        let event = Event {
            seq: Some(0),
            id: "event-1".into(),
            session_id: "session-1".into(),
            turn_id: Some("turn-1".into()),
            timestamp: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            payload: EventPayload::UserMessage {
                message_id: "message-1".into(),
                text: "hello".into(),
            },
        };

        let json = serde_json::to_value(event).unwrap();
        assert_eq!(json["seq"], 0);
        assert_eq!(json["type"], "user_message");
        assert_eq!(json["session_id"], "session-1");
        assert_eq!(json["message_id"], "message-1");
        assert!(json.get("payload").is_none());
    }

    #[test]
    fn durable_classification_matches_event_log_policy() {
        assert!(
            !EventPayload::AssistantTextDelta {
                message_id: "m1".into(),
                delta: "hi".into(),
            }
            .is_durable()
        );
        assert!(
            !EventPayload::ToolCallArgumentsDelta {
                call_id: "c1".into(),
                delta: "{}".into(),
            }
            .is_durable()
        );
        assert!(
            EventPayload::ToolCallRequested {
                call_id: "c1".into(),
                tool_name: "shell".into(),
                arguments: serde_json::json!({"cmd": "pwd"}),
            }
            .is_durable()
        );
        assert!(
            EventPayload::ToolCallCompleted {
                call_id: "c1".into(),
                tool_name: "shell".into(),
                result: ToolResult {
                    call_id: "c1".into(),
                    content: "ok".into(),
                    is_error: false,
                    error: None,
                    metadata: BTreeMap::new(),
                    duration_ms: Some(10),
                },
            }
            .is_durable()
        );
    }

    #[test]
    fn tool_call_start_and_request_have_separate_meaning() {
        let start = EventPayload::ToolCallStarted {
            call_id: "c1".into(),
            tool_name: "shell".into(),
        };
        let request = EventPayload::ToolCallRequested {
            call_id: "c1".into(),
            tool_name: "shell".into(),
            arguments: serde_json::json!({"cmd": "pwd"}),
        };

        assert!(start.is_durable());
        assert!(request.is_durable());
        assert_ne!(start, request);
    }
}
