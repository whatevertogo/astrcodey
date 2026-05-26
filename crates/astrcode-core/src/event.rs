//! 统一的运行时事件与持久化事件类型。
//!
//! 本模块定义了 astrcode 平台中所有事件的核心数据结构，包括：
//! - [`Phase`]：会话执行阶段的枚举
//! - [`EventPayload`]：事件载荷的统一枚举类型
//! - [`Event`]：携带会话/轮次标识和存储序号的事件信封

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{extension::ChildToolPolicy, llm::LlmMessage, tool::ToolResult, types::*};

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
        parent_session_id: Option<SessionId>,
        /// 子会话出生时被注入的工具集策略。
        ///
        /// 写在子 session 的事件日志而不仅仅父 session 的 `AgentSessionSpawned`，
        /// 是为了让子 session resume 时能从自己的事件流里恢复 policy，不必跨 session 查父。
        /// 根会话始终为 `None`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_policy: Option<ChildToolPolicy>,
        /// 创建该子 session 的扩展 ID，用于按插件组织存储目录。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_extension: Option<String>,
    },

    /// 会话使用的模型已变更。
    ///
    /// 由 handler 在运行时配置的 model_id 与 session 创建时不同时写入，
    /// 确保 session 始终反映当前生效的模型。
    ModelIdChanged { model_id: String },

    /// 会话 system prompt 已固定。
    ///
    /// 这是 session 级事实：同一 session 后续回合复用这份提示词，
    /// 不再按轮次重新组装。
    SystemPromptConfigured {
        /// 完整 system prompt 文本。
        text: String,
        /// system prompt 文本的稳定指纹，用于调试 prompt 是否漂移。
        fingerprint: String,
        /// 额外注入的 system prompt（子会话场景）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extra_system_prompt: Option<String>,
    },

    /// 会话已删除。
    SessionDeleted,

    /// 父会话记录派生了子 Agent 会话。
    ///
    /// 写入父 Session 的事件日志，表达"从父看子"的关系。
    /// 子侧通过 `SessionStarted.parent_session_id` 表达"从子看父"。
    ///
    /// `child_session_id` 为最初委托的子 session，父侧投影用其作为稳定锚点。
    /// 若子 session 经跨 session continuation 产生 leaf，见
    /// [`AgentSessionCompleted::final_session_id`](EventPayload::AgentSessionCompleted)。
    AgentSessionSpawned {
        child_session_id: SessionId,
        agent_name: String,
        task: String,
        /// 子会话生效的工具集策略（`None` 表示继承父全集）。
        ///
        /// 持久化以便子 session resume 时重建相同的工具表。
        #[serde(default)]
        tool_policy: Option<ChildToolPolicy>,
        /// 触发此子会话的工具调用 ID（用于 TUI 路由子 session 事件）。
        tool_call_id: ToolCallId,
    },

    /// Agent 运行开始。
    AgentRunStarted,

    /// Agent 运行完成。
    AgentRunCompleted {
        /// 完成原因描述。
        reason: String,
    },

    /// 子 Agent 会话成功完成。
    ///
    /// 由 `ChildTurnGuard` / server 在子 turn 结束后追加到父会话。
    ///
    /// **双 `SessionId` 说明（勿删其一）**：`child_session_id` 锚定
    /// `AgentSessionSpawned`；`final_session_id` 为结果所在 leaf。当前 compact 为
    /// 同 session 原地续写（`append_compact_boundary` 不换 id），故二者恒等；
    /// 构造载荷请用 `astrcode_session::payload::agent_session_completed_payload`。
    AgentSessionCompleted {
        /// 初始子会话 ID（与 `AgentSessionSpawned` 一致；compact 不会改此锚点）。
        child_session_id: SessionId,
        /// 产出结果的 leaf session；**当前实现**与 `child_session_id` 相同。
        final_session_id: SessionId,
        /// 子 Agent 输出摘要。
        summary: String,
    },

    /// 子 Agent 会话失败。
    ///
    /// 由 `ChildTurnGuard` / server 在子 turn 结束后追加到父会话。
    /// 双 `SessionId` 语义同 [`AgentSessionCompleted`]。
    AgentSessionFailed {
        /// 初始子会话 ID（与 `AgentSessionSpawned` 一致）。
        child_session_id: SessionId,
        /// leaf session；**当前实现**与 `child_session_id` 相同。
        final_session_id: SessionId,
        /// 错误描述。
        error: String,
    },

    /// 子 Agent 会话已回收。
    ///
    /// 在 `recycle_session` 时追加到父会话，用于从父会话的 `agent_sessions` 投影中
    /// 移除对应条目，使前端不再显示已回收的子 agent。
    AgentSessionRecycled {
        /// 初始子会话 ID。
        child_session_id: SessionId,
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

    /// Recap 摘要已生成。
    ///
    /// 持久化事件，用于展示和事件溯源。不进入下一轮 LLM 对话历史。
    RecapGenerated {
        /// 摘要文本。
        text: String,
        /// 触发来源：`"manual"`（/recap 命令）或 `"auto"`（future away summary）。
        source: String,
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
        /// 完整的消息文本（不含 thinking）。
        text: String,
        /// 推理模型的思维链内容。
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
    },

    /// 思考过程的文本增量（用于推理模型的思维链）。
    ThinkingDelta {
        /// 所属助手消息唯一标识。
        message_id: MessageId,
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
        /// 原始调用参数的折叠摘要文本。
        #[serde(default)]
        arguments: String,
        /// 原始调用参数的 JSON 值。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arguments_json: Option<serde_json::Value>,
    },

    /// 上下文压缩已开始。
    ///
    /// 这是 live 状态事件，不持久化；恢复时只依赖 durable 的 compact
    /// boundary / continuation 事件。
    CompactionStarted,

    /// 上下文压缩已完成。
    ///
    /// 这是 live 状态事件；压缩结果由 durable compact boundary 记录。
    CompactionCompleted {
        /// 被移除的消息数量。
        messages_removed: usize,
    },

    /// 上下文压缩已跳过（compare-and-append 冲突或条件不满足）。
    ///
    /// live 状态事件，不持久化。
    CompactionSkipped {
        /// 跳过原因。
        reason: String,
    },

    /// 上下文压缩失败。
    ///
    /// live 状态事件，不持久化。
    CompactionFailed {
        /// 失败原因。
        reason: String,
    },

    /// compact 在父会话中创建了 continuation 边界。
    CompactBoundaryCreated {
        /// compact 触发来源，例如 `manual_command`。
        trigger: String,
        /// 压缩前的 token 数量。
        pre_tokens: usize,
        /// 压缩后的 token 数量。
        post_tokens: usize,
        /// 压缩生成的摘要文本。
        summary: String,
        /// compact 前 transcript snapshot 的可读路径。
        #[serde(skip_serializing_if = "Option::is_none")]
        transcript_path: Option<String>,
        /// 接续对话的 session id。生产路径 `append_compact_boundary` 中为 **当前 session**
        /// （原地 compact，不换 id）；仅跨 session continuation 设计下才指向另一 session。
        continued_session_id: SessionId,
        /// compact 基于的事件 seq（replay 后、compact 前锁定），用于幂等校验。
        base_event_seq: u64,
        /// compact 策略，记录用于 replay 和审计。
        strategy: crate::extension::CompactStrategy,
    },

    /// 同一条 session log 上的 compact 续写投影（摘要 + 保留消息替换 transcript）。
    ///
    /// 生产路径中 `parent_session_id` 为 **本 session**（与 `Event.session_id` 相同），
    /// 并非另开子 session；与 `CompactBoundaryCreated` 成对追加。
    SessionContinuedFromCompaction {
        /// compact 前的 session（原地续写时等于本 log 的 `session_id`）。
        parent_session_id: SessionId,
        /// 父会话 compact 前的 durable cursor。
        parent_cursor: Cursor,
        /// 压缩生成的摘要文本。
        summary: String,
        /// compact 前 transcript snapshot 的可读路径。
        #[serde(skip_serializing_if = "Option::is_none")]
        transcript_path: Option<String>,
        /// 注入 provider 的隐藏上下文消息。
        context_messages: Vec<LlmMessage>,
        /// compact 后保留在可见 transcript 中的近期消息。
        retained_messages: Vec<LlmMessage>,
    },

    /// 从源会话 fork 而来。
    ///
    /// fork 点之前的消息原样保留，保证 provider KV 缓存前缀命中。
    /// 与 `SessionContinuedFromCompaction` 区别：fork 不做摘要压缩，
    /// 只是在 fork 点截断后复制原始消息前缀。
    SessionForked {
        /// 源会话 ID。
        source_session_id: SessionId,
        /// 源会话 fork 点 durable cursor。
        source_cursor: Cursor,
        /// 注入 provider 的隐藏上下文消息（通常为空，为未来兼容 compact+fork 保留）。
        context_messages: Vec<LlmMessage>,
        /// fork 点之前保留的可见 transcript 消息（原样复制，保证 KV 前缀一致）。
        retained_messages: Vec<LlmMessage>,
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

    /// 工具调用已转入后台执行。
    ///
    /// 当工具执行超过其声明的后台化阈值时，agent loop 自动将调用
    /// 从同步等待转为后台运行，并返回占位结果给 LLM 继续推理。
    ToolCallBackgrounded {
        /// 工具调用唯一标识。
        call_id: ToolCallId,
        /// 工具名称。
        tool_name: String,
        /// 后台任务 ID，用于后续查询和取消。
        task_id: crate::types::BackgroundTaskId,
        /// 后台化原因（如 "auto_threshold"）。
        reason: String,
    },

    /// 后台任务的输出增量（stdout/stderr 流）。
    ///
    /// 这是 live 事件，不持久化到事件日志。
    BackgroundTaskOutput {
        /// 后台任务 ID。
        task_id: crate::types::BackgroundTaskId,
        /// 原始工具调用 ID，用于客户端将输出关联到对应的 tool-call block。
        call_id: ToolCallId,
        /// 输出流类型。
        stream: ToolOutputStream,
        /// 本次增量输出文本。
        delta: String,
    },

    /// 后台任务已完成。
    BackgroundTaskCompleted {
        /// 后台任务 ID。
        task_id: crate::types::BackgroundTaskId,
        /// 原始工具调用 ID。
        call_id: ToolCallId,
        /// 工具名称。
        tool_name: String,
        /// 工具执行的最终结果。
        result: ToolResult,
    },

    /// 插件命名空间事件。
    ///
    /// 由 [`crate::extension::ExtensionEventSink`] 发出，`extension_id` 由 runtime
    /// 在构造 sink 时注入，插件无法伪造。`event_type` 必须在 Registrar 中声明。
    ExtensionEvent {
        /// 插件 ID，充当事件命名空间。
        extension_id: String,
        /// 插件声明的事件类型名（如 `"memory.accepted"`）。
        event_type: String,
        /// payload schema 版本，用于向前兼容。
        #[serde(default)]
        schema_version: u32,
        /// 不透明事件载荷。
        payload: serde_json::Value,
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
            Self::ToolCallStarted { .. }
                | Self::AssistantTextDelta { .. }
                | Self::ThinkingDelta { .. }
                | Self::ToolCallArgumentsDelta { .. }
                | Self::ToolOutputDelta { .. }
                | Self::AgentRunStarted
                | Self::AgentRunCompleted { .. }
                | Self::ToolCallBackgrounded { .. }
                | Self::BackgroundTaskOutput { .. }
                | Self::BackgroundTaskCompleted { .. }
                | Self::CompactionStarted
                | Self::CompactionCompleted { .. }
                | Self::CompactionSkipped { .. }
                | Self::CompactionFailed { .. }
        )
    }
}

/// 事件信封，携带会话/轮次标识和存储序号。
///
/// 序号（`seq`）由存储层在追加事件时分配，用于事件日志的有序读取。
/// `payload` 使用 `#[serde(flatten)]` 与信封字段平铺在同一 JSON 对象中。
///
/// **维护约定**：新增 [`EventPayload`] 变体时，其 serde 字段名不得与信封字段冲突：
/// `seq`、`id`、`session_id`、`turn_id`、`timestamp`（`type` 由 payload 内部 tag 占用）。
/// 冲突不会在 Rust 编译期报错，可能导致 JSONL replay 静默错乱。见
/// `event_payload_fields_do_not_use_envelope_keys` 测试。
/// TODO: 更好的设计？
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

    /// 构造 session 级事件（不属于任何 turn）。
    pub fn session(session_id: SessionId, payload: EventPayload) -> Self {
        Self::new(session_id, None, payload)
    }

    /// 构造 turn 级事件。
    pub fn turn(session_id: SessionId, turn_id: TurnId, payload: EventPayload) -> Self {
        Self::new(session_id, Some(turn_id), payload)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn event_payload_fields_do_not_use_envelope_keys() {
        let event = Event {
            seq: Some(1),
            id: "event-1".into(),
            session_id: "parent-session".into(),
            turn_id: None,
            timestamp: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            payload: EventPayload::AgentSessionCompleted {
                child_session_id: "child-a".into(),
                final_session_id: "child-a".into(),
                summary: "ok".into(),
            },
        };
        let json = serde_json::to_value(&event).unwrap();
        for key in ["seq", "id", "session_id", "timestamp"] {
            assert!(
                json.get(key).is_some(),
                "flattened event must include envelope field `{key}`"
            );
        }
        assert!(json.get("turn_id").is_none());
        assert_eq!(json["session_id"], "parent-session");
        assert_eq!(json["child_session_id"], "child-a");
        assert_eq!(json["id"], "event-1");
        assert!(json.get("payload").is_none());
    }

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
                arguments: String::new(),
                arguments_json: None,
            }
            .is_durable()
        );
        assert!(
            !EventPayload::ThinkingDelta {
                message_id: "m1".into(),
                delta: "thinking".into(),
            }
            .is_durable(),
            "ThinkingDelta is live UI state only"
        );
        assert!(
            !EventPayload::ToolCallStarted {
                call_id: "c1".into(),
                tool_name: "shell".into(),
            }
            .is_durable(),
            "ToolCallStarted is live UI state only"
        );
        assert!(
            !EventPayload::ToolCallBackgrounded {
                call_id: "c1".into(),
                tool_name: "shell".into(),
                task_id: "t1".into(),
                reason: "auto_threshold".into(),
            }
            .is_durable(),
            "ToolCallBackgrounded is live UI state only"
        );
        assert!(
            !EventPayload::BackgroundTaskCompleted {
                task_id: "t1".into(),
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
            .is_durable(),
            "BackgroundTaskCompleted is live UI state only"
        );
        assert!(
            EventPayload::CompactBoundaryCreated {
                trigger: "manual_command".into(),
                pre_tokens: 10,
                post_tokens: 3,
                summary: "summary".into(),
                transcript_path: None,
                continued_session_id: "child".into(),
                base_event_seq: 0,
                strategy: crate::extension::CompactStrategy::Manual {
                    keep_recent_turns: None,
                },
            }
            .is_durable(),
            "CompactBoundaryCreated is the durable parent-session audit fact"
        );
        assert!(
            EventPayload::SessionContinuedFromCompaction {
                parent_session_id: "parent".into(),
                parent_cursor: "2".into(),
                summary: "summary".into(),
                transcript_path: None,
                context_messages: vec![LlmMessage::system("summary")],
                retained_messages: vec![LlmMessage::user("recent")],
            }
            .is_durable(),
            "SessionContinuedFromCompaction is the durable child-session projection fact"
        );
    }

    #[test]
    fn compact_boundary_created_serializes_continuation_target() {
        let payload = EventPayload::CompactBoundaryCreated {
            trigger: "manual_command".into(),
            pre_tokens: 100,
            post_tokens: 20,
            summary: "summary".into(),
            transcript_path: Some("compact.jsonl".into()),
            continued_session_id: "child-session".into(),
            base_event_seq: 42,
            strategy: crate::extension::CompactStrategy::Manual {
                keep_recent_turns: None,
            },
        };

        let value = serde_json::to_value(&payload).unwrap();
        let round_trip: EventPayload = serde_json::from_value(value.clone()).unwrap();

        assert_eq!(value["type"], "compact_boundary_created");
        assert_eq!(value["continued_session_id"], "child-session");
        assert_eq!(round_trip, payload);
    }

    #[test]
    fn session_continued_from_compaction_serializes_context() {
        let payload = EventPayload::SessionContinuedFromCompaction {
            parent_session_id: "parent-session".into(),
            parent_cursor: "7".into(),
            summary: "summary".into(),
            transcript_path: Some("compact.jsonl".into()),
            context_messages: vec![LlmMessage::system("hidden summary")],
            retained_messages: vec![LlmMessage::user("recent")],
        };

        let value = serde_json::to_value(&payload).unwrap();
        let round_trip: EventPayload = serde_json::from_value(value.clone()).unwrap();

        assert_eq!(value["type"], "session_continued_from_compaction");
        assert_eq!(value["parent_session_id"], "parent-session");
        assert_eq!(value["parent_cursor"], "7");
        assert_eq!(round_trip, payload);
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

        assert!(!start.is_durable(), "ToolCallStarted is live UI state only");
        assert!(request.is_durable());
        assert_ne!(start, request);
    }

    #[test]
    fn thinking_delta_serializes_message_owner() {
        let payload = EventPayload::ThinkingDelta {
            message_id: "assistant-1".into(),
            delta: "reasoning".into(),
        };

        let value = serde_json::to_value(&payload).unwrap();
        let round_trip: EventPayload = serde_json::from_value(value.clone()).unwrap();

        assert_eq!(value["type"], "thinking_delta");
        assert_eq!(value["message_id"], "assistant-1");
        assert_eq!(value["delta"], "reasoning");
        assert_eq!(round_trip, payload);
    }

    #[test]
    fn agent_session_spawned_serializes_and_is_durable() {
        let payload = EventPayload::AgentSessionSpawned {
            child_session_id: "child-1".into(),
            agent_name: "reviewer".into(),
            task: "review current diff".into(),
            tool_policy: None,
            tool_call_id: "call-42".into(),
        };

        assert!(payload.is_durable());

        let value = serde_json::to_value(&payload).unwrap();
        assert_eq!(value["type"], "agent_session_spawned");
        assert_eq!(value["child_session_id"], "child-1");
        assert_eq!(value["agent_name"], "reviewer");
        assert_eq!(value["task"], "review current diff");
        assert!(value["tool_policy"].is_null());
        assert_eq!(value["tool_call_id"], "call-42");

        let round_trip: EventPayload = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(round_trip, payload);
    }
}
