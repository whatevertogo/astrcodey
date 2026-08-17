//! 持久事件、实时事件及其信封类型。

mod envelope;
mod fingerprint;
mod payload;

use std::sync::Arc;

use async_trait::async_trait;
pub use envelope::{
    DurableEvent, Event, EventEnvelope, EventPayload, LiveEvent, Phase, StoredEvent,
    ToolOutputStream,
};
pub use fingerprint::{stable_hash_hex, transcript_prefix_fingerprint};
pub use payload::{
    CompactionDetails, CustomEventAudience, CustomEventData, DurableEventPayload, LiveEventPayload,
    ParentSessionRef, SessionStarted, TranscriptRewriteReason,
};
use serde::{Deserialize, Serialize};

use crate::types::EventId;

/// Result of submitting an event through a runtime event ingress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventDeliveryReceipt {
    /// The receiving boundary accepted the event but does not expose publication completion.
    Accepted,
    /// A live event was accepted into the session's ordered lane without durable storage.
    ///
    /// Observer dispatch happens asynchronously after acceptance; this receipt does not
    /// mean any observer has seen the event yet.
    LivePublished { event_id: EventId },
    /// A durable event was stored and assigned its session sequence.
    Persisted { event_id: EventId, seq: u64 },
}

/// Runtime-owned event ingress behind [`EventSender`].
///
/// `try_send` 对 durable payload 的契约由实现方定义：turn 路径
/// （astrcode-session 的 `TurnEventPublisher`）接受并入队，由 ingress worker 异步
/// 持久化；session 路径（`SessionScopedEventPublisher`）直接拒绝并返回
/// [`EventSendError::PublishFailed`]，durable 必须走 [`EventPublisher::send_confirmed`]。
#[async_trait]
#[doc(hidden)]
pub trait EventPublisher: Send + Sync {
    fn try_send(&self, payload: EventPayload) -> Result<(), EventSendError>;

    async fn send_confirmed(
        &self,
        payload: EventPayload,
    ) -> Result<EventDeliveryReceipt, EventSendError>;
}

/// Cloneable event ingress used by turn-scoped tools and extensions.
///
/// The publisher boundary lets the session runtime put event and barrier commands on the same
/// FIFO without exposing its internal command type through core contracts.
#[derive(Clone)]
pub struct EventSender {
    publisher: Arc<dyn EventPublisher>,
}

impl EventSender {
    pub fn new(
        send: impl Fn(EventPayload) -> Result<(), EventSendError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            publisher: Arc::new(UnconfirmedEventPublisher { send }),
        }
    }

    #[doc(hidden)]
    pub fn from_publisher(publisher: Arc<dyn EventPublisher>) -> Self {
        Self { publisher }
    }

    pub fn send(&self, payload: EventPayload) -> Result<(), EventSendError> {
        self.publisher.try_send(payload)
    }

    pub async fn send_confirmed(
        &self,
        payload: EventPayload,
    ) -> Result<EventDeliveryReceipt, EventSendError> {
        self.publisher.send_confirmed(payload).await
    }
}

struct UnconfirmedEventPublisher<F> {
    send: F,
}

#[async_trait]
impl<F> EventPublisher for UnconfirmedEventPublisher<F>
where
    F: Fn(EventPayload) -> Result<(), EventSendError> + Send + Sync,
{
    fn try_send(&self, payload: EventPayload) -> Result<(), EventSendError> {
        (self.send)(payload)
    }

    async fn send_confirmed(
        &self,
        payload: EventPayload,
    ) -> Result<EventDeliveryReceipt, EventSendError> {
        self.try_send(payload)?;
        Ok(EventDeliveryReceipt::Accepted)
    }
}

impl From<tokio::sync::mpsc::UnboundedSender<EventPayload>> for EventSender {
    fn from(sender: tokio::sync::mpsc::UnboundedSender<EventPayload>) -> Self {
        Self::new(move |payload| sender.send(payload).map_err(|_| EventSendError::Closed))
    }
}

impl std::fmt::Debug for EventSender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("EventSender")
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum EventSendError {
    #[error("event ingress is closed")]
    Closed,
    #[error("event ingress is full")]
    Full,
    #[error("event publication failed: {0}")]
    PublishFailed(String),
}

/// 持久化的 system prompt 及其恢复语义。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedSystemPrompt {
    pub text: String,
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_system_prompt: Option<String>,
    pub source: SystemPromptSource,
}

/// system prompt 的生命周期来源。
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SystemPromptSource {
    #[default]
    Native,
    Inherited,
}

#[cfg(test)]
mod tests;
