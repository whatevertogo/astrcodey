//! 事件信封 —— [`Event`]、[`Phase`]、[`ToolOutputStream`] 与自定义序列化。
//!
//! 从 `event` 根模块拆出。`Event` 的 v2 wire format 把事实放入 `payload` 子对象,
//! 顶层只保留 envelope;自定义 [`Serialize`] 校验动态载荷不得占用保留顶层字段。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, Serializer};

use super::payload::EventPayload;
use crate::types::*;

/// Event 顶层保留字段名集合。
///
/// v2 wire format 将事件事实放入 `payload` 子对象，顶层只保留 envelope。
/// 该常量用于测试与插件动态载荷的保留字文档；新增顶层字段请同步更新。
pub(crate) const EVENT_ENVELOPE_KEYS: &[&str] =
    &["seq", "id", "session_id", "turn_id", "timestamp", "payload"];
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
/// 事件信封，携带会话/轮次标识和存储序号。
///
/// 序号（`seq`）由存储层在追加事件时分配，用于事件日志的有序读取。
/// v2 wire format 将 `payload` 作为子对象序列化，避免 envelope 字段与
/// [`EventPayload`] 字段共享同一 JSON 命名空间。
#[derive(Debug, Clone, Deserialize, PartialEq)]
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
    /// 事件载荷。
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

impl Serialize for Event {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_dynamic_payload_reserved_keys(&self.payload).map_err(serde::ser::Error::custom)?;
        EventRef {
            seq: self.seq,
            id: &self.id,
            session_id: &self.session_id,
            turn_id: self.turn_id.as_ref(),
            timestamp: &self.timestamp,
            payload: &self.payload,
        }
        .serialize(serializer)
    }
}

#[derive(Serialize)]
struct EventRef<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    seq: Option<u64>,
    id: &'a EventId,
    session_id: &'a SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_id: Option<&'a TurnId>,
    timestamp: &'a DateTime<Utc>,
    payload: &'a EventPayload,
}

fn validate_dynamic_payload_reserved_keys(payload: &EventPayload) -> Result<(), String> {
    let (label, value) = match payload {
        EventPayload::Custom { data, .. } => ("Custom data", data),
        EventPayload::ExtensionEvent { payload, .. } => ("ExtensionEvent payload", payload),
        _ => return Ok(()),
    };

    let Some(key) = first_reserved_object_key(value) else {
        return Ok(());
    };

    Err(format!(
        "{label} contains reserved Event envelope key `{key}` at its top level"
    ))
}

fn first_reserved_object_key(value: &serde_json::Value) -> Option<&str> {
    value.as_object()?.keys().find_map(|key| {
        EVENT_ENVELOPE_KEYS
            .contains(&key.as_str())
            .then_some(key.as_str())
    })
}
