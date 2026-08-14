use std::{collections::HashMap, sync::Arc};

use astrcode_core::{
    event::{EventDeliveryReceipt, EventSendError},
    types::{EventId, SessionId},
};
use async_trait::async_trait;
use serde::Serialize;

use super::{
    ExtensionCall, ExtensionCallContext, ExtensionError, SessionCallContext,
    internal::CustomEventSink,
};
pub use crate::wire::custom_event::{
    CustomEventDeclaration, CustomEventSourceFilter, CustomEventSubscription,
    DEFAULT_CUSTOM_EVENT_DURABLE, DEFAULT_CUSTOM_EVENT_MAX_PAYLOAD_BYTES,
    DEFAULT_CUSTOM_EVENT_SCHEMA_VERSION, MAX_CUSTOM_EVENT_PAYLOAD_BYTES,
    MAX_CUSTOM_EVENT_SUBSCRIPTION_ID_LEN,
};
use crate::wire::effects::{HandlerEffect, HandlerResult};
// ─── Lifecycle Events ────────────────────────────────────────────────────
/// 扩展可订阅的核心生命周期事件。
///
/// 覆盖会话/轮次/工具/LLM 提供者/prompt 组装的完整生命周期。
pub use crate::wire::manifest::LifecycleEvent;

// ─── Custom Event System ───────────────────────────────────────────────────

/// Host-attributed input for a custom-event consumer.
#[derive(Clone)]
pub struct CustomEventContext {
    call: SessionCallContext,
    event_id: EventId,
    seq: Option<u64>,
    source_extension_id: String,
    event_type: String,
    schema_version: u32,
    causation_id: Option<EventId>,
    cascade_depth: u8,
    payload: serde_json::Value,
}

impl CustomEventContext {
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn from_runtime(
        call: ExtensionCallContext,
        session_id: SessionId,
        turn_id: Option<String>,
        event_id: EventId,
        seq: Option<u64>,
        source_extension_id: String,
        event_type: String,
        schema_version: u32,
        causation_id: Option<EventId>,
        cascade_depth: u8,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            call: SessionCallContext::from_runtime(call, session_id, turn_id),
            event_id,
            seq,
            source_extension_id,
            event_type,
            schema_version,
            causation_id,
            cascade_depth,
            payload,
        }
    }

    pub fn event_id(&self) -> &EventId {
        &self.event_id
    }

    pub fn session_id(&self) -> &SessionId {
        self.call.session_id()
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.call.turn_id()
    }

    pub fn seq(&self) -> Option<u64> {
        self.seq
    }

    pub fn source_extension_id(&self) -> &str {
        &self.source_extension_id
    }

    pub fn event_type(&self) -> &str {
        &self.event_type
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn causation_id(&self) -> Option<&EventId> {
        self.causation_id.as_ref()
    }

    pub fn cascade_depth(&self) -> u8 {
        self.cascade_depth
    }

    pub fn is_durable(&self) -> bool {
        self.seq.is_some()
    }

    pub fn payload(&self) -> &serde_json::Value {
        &self.payload
    }
}

impl ExtensionCall for CustomEventContext {
    fn call(&self) -> &ExtensionCallContext {
        self.call.call()
    }
}

/// Durable-consumer decision for one custom-event delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomEventDisposition {
    Ack,
    Retry { reason: String },
    DeadLetter { reason: String },
}

impl CustomEventDisposition {
    pub fn retry(reason: impl Into<String>) -> Self {
        Self::Retry {
            reason: reason.into(),
        }
    }

    pub fn dead_letter(reason: impl Into<String>) -> Self {
        Self::DeadLetter {
            reason: reason.into(),
        }
    }
}

impl From<CustomEventDisposition> for HandlerResult {
    fn from(disposition: CustomEventDisposition) -> Self {
        match disposition {
            CustomEventDisposition::Ack => {
                Self::effect(HandlerEffect::CustomEventAck, serde_json::Value::Null)
            },
            CustomEventDisposition::Retry { reason } => Self::effect(
                HandlerEffect::CustomEventRetry,
                serde_json::json!({ "reason": reason }),
            ),
            CustomEventDisposition::DeadLetter { reason } => Self::effect(
                HandlerEffect::CustomEventDeadLetter,
                serde_json::json!({ "reason": reason }),
            ),
        }
    }
}

#[async_trait]
pub trait CustomEventHandler: Send + Sync {
    async fn handle(
        &self,
        ctx: CustomEventContext,
    ) -> Result<CustomEventDisposition, ExtensionError>;
}

/// Extension-scoped event emitter with immutable declaration attribution.
///
/// The runtime constructs this value from the same registration aggregate used by dispatch.
/// Authors choose only the event name and payload; schema version and durability come from the
/// declaration and cannot be changed per emission.
#[derive(Clone, Default)]
pub struct CustomEventEmitter {
    declarations: Arc<HashMap<String, CustomEventDeclaration>>,
    sink: Option<Arc<dyn CustomEventSink>>,
}

impl CustomEventEmitter {
    pub(super) fn from_runtime(
        declarations: impl IntoIterator<Item = CustomEventDeclaration>,
        sink: Option<Arc<dyn CustomEventSink>>,
    ) -> Self {
        Self {
            declarations: Arc::new(
                declarations
                    .into_iter()
                    .map(|declaration| (declaration.event_type.clone(), declaration))
                    .collect(),
            ),
            sink,
        }
    }

    /// Emit an event and wait until the host reports its publication state.
    ///
    /// Session-scoped durable events return [`EventDeliveryReceipt::Persisted`] only after they
    /// have a storage sequence. Unscoped hosts that cannot expose completion return
    /// [`EventDeliveryReceipt::Accepted`].
    pub async fn emit<T: Serialize + ?Sized>(
        &self,
        event_type: &str,
        payload: &T,
    ) -> Result<EventDeliveryReceipt, CustomEventEmitError> {
        let (declaration, payload) = self.prepare(event_type, payload)?;
        self.sink()?
            .emit(
                event_type,
                declaration.schema_version,
                declaration.durable,
                payload,
            )
            .await
            .map_err(|error| map_send_error(event_type, error))
    }

    /// Try to enqueue an event from a synchronous lifecycle boundary such as a cancellation
    /// guard's `Drop`.
    ///
    /// Success confirms queue admission only. Async handlers should use [`Self::emit`] so queue
    /// pressure and publication failures are observable.
    pub fn try_emit<T: Serialize + ?Sized>(
        &self,
        event_type: &str,
        payload: &T,
    ) -> Result<(), CustomEventEmitError> {
        let (declaration, payload) = self.prepare(event_type, payload)?;
        self.sink()?
            .try_emit(
                event_type,
                declaration.schema_version,
                declaration.durable,
                payload,
            )
            .map_err(|error| map_send_error(event_type, error))
    }

    fn prepare<T: Serialize + ?Sized>(
        &self,
        event_type: &str,
        payload: &T,
    ) -> Result<(&CustomEventDeclaration, serde_json::Value), CustomEventEmitError> {
        let declaration =
            self.declarations
                .get(event_type)
                .ok_or_else(|| CustomEventEmitError::Undeclared {
                    event_type: event_type.to_owned(),
                })?;
        let payload = serde_json::to_value(payload).map_err(|error| {
            CustomEventEmitError::InvalidPayload {
                event_type: event_type.to_owned(),
                message: error.to_string(),
            }
        })?;
        let payload_bytes = serde_json::to_vec(&payload)
            .map_err(|error| CustomEventEmitError::InvalidPayload {
                event_type: event_type.to_owned(),
                message: error.to_string(),
            })?
            .len();
        if payload_bytes > declaration.max_payload_bytes {
            return Err(CustomEventEmitError::PayloadTooLarge {
                event_type: event_type.to_owned(),
                actual_bytes: payload_bytes,
                max_bytes: declaration.max_payload_bytes,
            });
        }
        Ok((declaration, payload))
    }

    fn sink(&self) -> Result<&dyn CustomEventSink, CustomEventEmitError> {
        self.sink
            .as_deref()
            .ok_or(CustomEventEmitError::ContextUnavailable)
    }
}

impl std::fmt::Debug for CustomEventEmitter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CustomEventEmitter")
            .field("declarations", &self.declarations.keys())
            .field("sink", &self.sink.as_ref().map(|_| "<event_sink>"))
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CustomEventEmitError {
    #[error("custom event `{event_type}` was not declared")]
    Undeclared { event_type: String },
    #[error("custom event emission is unavailable in this call context")]
    ContextUnavailable,
    #[error("custom event `{event_type}` payload is invalid: {message}")]
    InvalidPayload { event_type: String, message: String },
    #[error(
        "custom event `{event_type}` payload is {actual_bytes} bytes, exceeding {max_bytes} bytes"
    )]
    PayloadTooLarge {
        event_type: String,
        actual_bytes: usize,
        max_bytes: usize,
    },
    #[error("custom event `{event_type}` ingress is full")]
    QueueFull { event_type: String },
    #[error("custom event `{event_type}` ingress is closed")]
    IngressClosed { event_type: String },
    #[error("custom event `{event_type}` publication failed: {message}")]
    Publication { event_type: String, message: String },
}

fn map_send_error(event_type: &str, error: EventSendError) -> CustomEventEmitError {
    match error {
        EventSendError::Full => CustomEventEmitError::QueueFull {
            event_type: event_type.to_owned(),
        },
        EventSendError::Closed => CustomEventEmitError::IngressClosed {
            event_type: event_type.to_owned(),
        },
        EventSendError::PublishFailed(message) => CustomEventEmitError::Publication {
            event_type: event_type.to_owned(),
            message,
        },
    }
}

#[cfg(test)]
mod emitter_tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<(String, u32, serde_json::Value)>>);

    impl RecordingSink {
        fn record(&self, event_type: &str, schema_version: u32, payload: serde_json::Value) {
            self.0
                .lock()
                .unwrap()
                .push((event_type.to_owned(), schema_version, payload));
        }
    }

    #[async_trait::async_trait]
    impl CustomEventSink for RecordingSink {
        async fn emit(
            &self,
            event_type: &str,
            schema_version: u32,
            _durable: bool,
            payload: serde_json::Value,
        ) -> Result<EventDeliveryReceipt, EventSendError> {
            self.record(event_type, schema_version, payload);
            Ok(EventDeliveryReceipt::Accepted)
        }

        fn try_emit(
            &self,
            event_type: &str,
            schema_version: u32,
            _durable: bool,
            payload: serde_json::Value,
        ) -> Result<(), EventSendError> {
            self.record(event_type, schema_version, payload);
            Ok(())
        }
    }

    #[tokio::test]
    async fn emitter_owns_declaration_version_and_reports_missing_declaration_or_sink() {
        let sink = Arc::new(RecordingSink::default());
        let emitter = CustomEventEmitter::from_runtime(
            [CustomEventDeclaration {
                event_type: "review.completed".into(),
                schema_version: 3,
                durable: true,
                max_payload_bytes: 1024,
            }],
            Some(sink.clone()),
        );
        assert_eq!(
            emitter
                .emit("review.completed", &serde_json::json!({ "status": "ok" }))
                .await
                .unwrap(),
            EventDeliveryReceipt::Accepted
        );
        emitter
            .try_emit(
                "review.completed",
                &serde_json::json!({ "status": "cancelled" }),
            )
            .unwrap();
        emitter
            .try_emit(
                "review.completed",
                &serde_json::json!({ "status": "published" }),
            )
            .unwrap();
        assert_eq!(
            sink.0.lock().unwrap().as_slice(),
            &[
                (
                    "review.completed".into(),
                    3,
                    serde_json::json!({ "status": "ok" })
                ),
                (
                    "review.completed".into(),
                    3,
                    serde_json::json!({ "status": "cancelled" })
                ),
                (
                    "review.completed".into(),
                    3,
                    serde_json::json!({ "status": "published" })
                )
            ]
        );
        assert!(matches!(
            emitter.try_emit("review.failed", &()),
            Err(CustomEventEmitError::Undeclared { .. })
        ));

        let detached = CustomEventEmitter::from_runtime(
            [CustomEventDeclaration {
                event_type: "review.completed".into(),
                schema_version: 1,
                durable: false,
                max_payload_bytes: 1024,
            }],
            None,
        );
        assert!(matches!(
            detached.try_emit("review.completed", &()),
            Err(CustomEventEmitError::ContextUnavailable)
        ));

        let bounded = CustomEventEmitter::from_runtime(
            [CustomEventDeclaration {
                event_type: "review.completed".into(),
                schema_version: 1,
                durable: false,
                max_payload_bytes: 2,
            }],
            Some(sink),
        );
        assert!(matches!(
            bounded.try_emit(
                "review.completed",
                &serde_json::json!({ "status": "too-large" })
            ),
            Err(CustomEventEmitError::PayloadTooLarge { .. })
        ));
        assert!(matches!(
            map_send_error("review.completed", EventSendError::Full),
            CustomEventEmitError::QueueFull { .. }
        ));
        assert!(matches!(
            map_send_error("review.completed", EventSendError::Closed),
            CustomEventEmitError::IngressClosed { .. }
        ));
        assert!(matches!(
            map_send_error(
                "review.completed",
                EventSendError::PublishFailed("storage unavailable".into())
            ),
            CustomEventEmitError::Publication { .. }
        ));
    }
}
