//! 会话事件信封。

use std::ops::Deref;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

use super::payload::{DurableEventPayload, LiveEventPayload};
use crate::types::*;

/// 会话的执行阶段。
///
/// phase 是 reducer 的派生状态，不是 event log 中的权威事实。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Idle,
    Thinking,
    Streaming,
    CallingTool,
    Compacting,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutputStream {
    Stdout,
    Stderr,
}

/// 尚未分配存储序号的事件信封。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventEnvelope<P> {
    pub id: EventId,
    pub session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    pub timestamp: DateTime<Utc>,
    pub payload: P,
}

impl<P> EventEnvelope<P> {
    pub fn new(session_id: SessionId, turn_id: Option<TurnId>, payload: P) -> Self {
        Self {
            id: new_event_id(),
            session_id,
            turn_id,
            timestamp: Utc::now(),
            payload,
        }
    }

    pub fn session(session_id: SessionId, payload: P) -> Self {
        Self::new(session_id, None, payload)
    }

    pub fn turn(session_id: SessionId, turn_id: TurnId, payload: P) -> Self {
        Self::new(session_id, Some(turn_id), payload)
    }
}

pub type DurableEvent = EventEnvelope<DurableEventPayload>;
pub type LiveEvent = EventEnvelope<LiveEventPayload>;

/// 已提交到 event log 的 durable 事件。
///
/// `seq` 在类型上必定存在，projection 不需要再处理未落盘事件或默认序号。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredEvent {
    pub seq: u64,
    #[serde(flatten)]
    pub event: DurableEvent,
}

impl StoredEvent {
    pub fn new(seq: u64, event: DurableEvent) -> Self {
        Self { seq, event }
    }
}

impl Deref for StoredEvent {
    type Target = DurableEvent;

    fn deref(&self) -> &Self::Target {
        &self.event
    }
}

/// 进程内 fan-out 与 protocol 边界使用的事件联合。
///
/// storage 和 projection 不使用这个类型；它们只接受 [`DurableEvent`] /
/// [`StoredEvent`]。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Event {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    pub id: EventId,
    pub session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    pub timestamp: DateTime<Utc>,
    pub payload: EventPayload,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(untagged)]
pub enum EventPayload {
    Durable(DurableEventPayload),
    Live(LiveEventPayload),
}

impl EventPayload {
    pub fn as_durable(&self) -> Option<&DurableEventPayload> {
        match self {
            Self::Durable(payload) => Some(payload),
            Self::Live(_) => None,
        }
    }

    pub fn as_live(&self) -> Option<&LiveEventPayload> {
        match self {
            Self::Durable(_) => None,
            Self::Live(payload) => Some(payload),
        }
    }
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireEvent {
            seq: Option<u64>,
            id: EventId,
            session_id: SessionId,
            turn_id: Option<TurnId>,
            timestamp: DateTime<Utc>,
            payload: serde_json::Value,
        }

        let event = WireEvent::deserialize(deserializer)?;
        let payload = if event.seq.is_some() {
            serde_json::from_value(event.payload)
                .map(EventPayload::Durable)
                .map_err(D::Error::custom)?
        } else {
            serde_json::from_value(event.payload)
                .map(EventPayload::Live)
                .map_err(D::Error::custom)?
        };

        Ok(Self {
            seq: event.seq,
            id: event.id,
            session_id: event.session_id,
            turn_id: event.turn_id,
            timestamp: event.timestamp,
            payload,
        })
    }
}

impl From<StoredEvent> for Event {
    fn from(stored: StoredEvent) -> Self {
        let StoredEvent { seq, event } = stored;
        Self {
            seq: Some(seq),
            id: event.id,
            session_id: event.session_id,
            turn_id: event.turn_id,
            timestamp: event.timestamp,
            payload: EventPayload::Durable(event.payload),
        }
    }
}

impl From<&StoredEvent> for Event {
    fn from(stored: &StoredEvent) -> Self {
        Self {
            seq: Some(stored.seq),
            id: stored.id.clone(),
            session_id: stored.session_id.clone(),
            turn_id: stored.turn_id.clone(),
            timestamp: stored.timestamp,
            payload: EventPayload::Durable(stored.payload.clone()),
        }
    }
}

impl From<LiveEvent> for Event {
    fn from(event: LiveEvent) -> Self {
        Self {
            seq: None,
            id: event.id,
            session_id: event.session_id,
            turn_id: event.turn_id,
            timestamp: event.timestamp,
            payload: EventPayload::Live(event.payload),
        }
    }
}
