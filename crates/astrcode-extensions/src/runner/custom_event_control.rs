use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use astrcode_core::{event::DurableEventPayload, types::SessionId};
use astrcode_extension_sdk::extension::{
    CustomEventHandler, CustomEventSubscription, internal::custom_event_subscription_matches,
};
use astrcode_storage::{EventConsumerCheckpointReset, EventConsumerState, StorageError};
use tokio::sync::Notify;

use super::{CustomEventSession, ExtensionRunner, custom_event_consumer_id};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomEventConsumerAction {
    Pause,
    Resume,
    ReplayFromBeginning,
    SkipToStreamHead,
}

#[derive(Debug, Clone)]
pub struct CustomEventConsumerStatus {
    pub extension_id: String,
    pub subscription: CustomEventSubscription,
    pub paused: bool,
    pub checkpoint: Option<u64>,
    pub stream_head: Option<u64>,
    pub pending_events: u64,
    pub in_flight: bool,
    pub failed_attempts: u64,
    pub consecutive_failures: u64,
    pub quarantined_events: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum CustomEventConsumerControlError {
    #[error("custom event subscription {extension_id}:{subscription_id} was not found")]
    SubscriptionNotFound {
        extension_id: String,
        subscription_id: String,
    },
    #[error("custom event consumer {0} is not available")]
    ConsumerUnavailable(String),
    #[error("custom event consumer {0} did not become idle before the control timeout")]
    ConsumerBusy(String),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct CustomEventConsumerKey {
    session_id: SessionId,
    consumer_id: String,
}

impl CustomEventConsumerKey {
    pub(super) fn new(session_id: &SessionId, consumer_id: &str) -> Self {
        Self {
            session_id: session_id.clone(),
            consumer_id: consumer_id.to_owned(),
        }
    }
}

pub(super) type CustomEventConsumerMetricsMap =
    parking_lot::Mutex<HashMap<CustomEventConsumerKey, Arc<CustomEventConsumerMetrics>>>;

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct CustomEventConsumerMetricsSnapshot {
    pub(super) in_flight: bool,
    pub(super) failed_attempts: u64,
    pub(super) consecutive_failures: u64,
}

#[derive(Default)]
pub(super) struct CustomEventConsumerMetrics {
    active_deliveries: AtomicU64,
    failed_attempts: AtomicU64,
    consecutive_failures: AtomicU64,
    became_idle: Notify,
}

impl CustomEventConsumerMetrics {
    pub(super) fn track_delivery(&self) -> ActiveDelivery<'_> {
        self.active_deliveries.fetch_add(1, Ordering::AcqRel);
        ActiveDelivery(self)
    }

    pub(super) fn record_failure(&self) {
        self.failed_attempts.fetch_add(1, Ordering::Relaxed);
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    fn snapshot(&self) -> CustomEventConsumerMetricsSnapshot {
        CustomEventConsumerMetricsSnapshot {
            in_flight: self.active_deliveries.load(Ordering::Acquire) != 0,
            failed_attempts: self.failed_attempts.load(Ordering::Relaxed),
            consecutive_failures: self.consecutive_failures.load(Ordering::Relaxed),
        }
    }

    async fn wait_until_idle(&self, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, async {
            loop {
                let became_idle = self.became_idle.notified();
                if self.active_deliveries.load(Ordering::Acquire) == 0 {
                    return;
                }
                became_idle.await;
            }
        })
        .await
        .is_ok()
    }
}

pub(super) struct ActiveDelivery<'a>(&'a CustomEventConsumerMetrics);

impl Drop for ActiveDelivery<'_> {
    fn drop(&mut self) {
        if self.0.active_deliveries.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.became_idle.notify_waiters();
        }
    }
}

struct CustomEventConsumerControlTarget<'a> {
    view: &'a Arc<super::ExtensionView>,
    extension_id: &'a str,
    subscription: &'a CustomEventSubscription,
    handler: &'a Arc<dyn CustomEventHandler>,
    session_id: &'a SessionId,
    consumer_id: &'a str,
    session: &'a CustomEventSession,
}

impl ExtensionRunner {
    pub async fn custom_event_consumer_statuses(
        &self,
        session_id: &SessionId,
        session: &CustomEventSession,
    ) -> Result<Vec<CustomEventConsumerStatus>, CustomEventConsumerControlError> {
        let view = self.extension_view().await;
        let events = session.event_store.replay_events(session_id).await?;
        let stream_head = events.last().map(|event| event.seq);
        let mut statuses = Vec::with_capacity(view.index.custom_event.len());
        for (extension_id, subscription, _) in &view.index.custom_event {
            let consumer_id = custom_event_consumer_id(extension_id, subscription);
            let state = session
                .event_store
                .event_consumer_state(session_id, &consumer_id)
                .await?;
            statuses.push(self.custom_event_consumer_status(
                session_id,
                extension_id,
                subscription,
                state,
                stream_head,
                &events,
            ));
        }
        Ok(statuses)
    }

    pub async fn control_custom_event_consumer(
        &self,
        session_id: &SessionId,
        extension_id: &str,
        subscription_id: &str,
        action: CustomEventConsumerAction,
        session: &CustomEventSession,
    ) -> Result<CustomEventConsumerStatus, CustomEventConsumerControlError> {
        let view = self.extension_view().await;
        let (subscription, handler) = view
            .index
            .custom_event
            .iter()
            .find(|(registered_extension_id, subscription, _)| {
                registered_extension_id == extension_id && subscription.id == subscription_id
            })
            .map(|(_, subscription, handler)| (subscription.clone(), Arc::clone(handler)))
            .ok_or_else(|| CustomEventConsumerControlError::SubscriptionNotFound {
                extension_id: extension_id.to_owned(),
                subscription_id: subscription_id.to_owned(),
            })?;
        let consumer_id = custom_event_consumer_id(extension_id, &subscription);
        let target = CustomEventConsumerControlTarget {
            view: &view,
            extension_id,
            subscription: &subscription,
            handler: &handler,
            session_id,
            consumer_id: &consumer_id,
            session,
        };

        match action {
            CustomEventConsumerAction::Pause => {
                session
                    .event_store
                    .set_event_consumer_paused(session_id, &consumer_id, true)
                    .await?;
            },
            CustomEventConsumerAction::Resume => {
                session
                    .event_store
                    .set_event_consumer_paused(session_id, &consumer_id, false)
                    .await?;
                self.wake_custom_event_consumer(&target)?;
            },
            CustomEventConsumerAction::ReplayFromBeginning => {
                self.reset_custom_event_consumer(&target, EventConsumerCheckpointReset::Beginning)
                    .await?;
            },
            CustomEventConsumerAction::SkipToStreamHead => {
                self.reset_custom_event_consumer(&target, EventConsumerCheckpointReset::StreamHead)
                    .await?;
            },
        }

        let events = session.event_store.replay_events(session_id).await?;
        let stream_head = events.last().map(|event| event.seq);
        let state = session
            .event_store
            .event_consumer_state(session_id, &consumer_id)
            .await?;
        Ok(self.custom_event_consumer_status(
            session_id,
            extension_id,
            &subscription,
            state,
            stream_head,
            &events,
        ))
    }

    pub fn forget_custom_event_session(&self, session_id: &SessionId) {
        let mut lanes = self.custom_event_lanes.lock();
        lanes.retain(|lane_id, lane| {
            if lane_id.session_id == *session_id {
                if let Some(lane) = lane.upgrade() {
                    lane.consumer.cancellation.cancel();
                }
                false
            } else {
                lane.strong_count() > 0
            }
        });
        drop(lanes);
        self.custom_event_metrics
            .lock()
            .retain(|key, _| key.session_id != *session_id);
    }

    pub(super) fn custom_event_consumer_metrics(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
    ) -> Arc<CustomEventConsumerMetrics> {
        Arc::clone(
            self.custom_event_metrics
                .lock()
                .entry(CustomEventConsumerKey::new(session_id, consumer_id))
                .or_default(),
        )
    }

    fn custom_event_consumer_status(
        &self,
        session_id: &SessionId,
        extension_id: &str,
        subscription: &CustomEventSubscription,
        state: EventConsumerState,
        stream_head: Option<u64>,
        events: &[astrcode_core::event::StoredEvent],
    ) -> CustomEventConsumerStatus {
        let consumer_id = custom_event_consumer_id(extension_id, subscription);
        let metrics = self
            .custom_event_metrics
            .lock()
            .get(&CustomEventConsumerKey::new(session_id, &consumer_id))
            .map(|metrics| metrics.snapshot())
            .unwrap_or_default();
        let pending_events = u64::try_from(
            events
                .iter()
                .filter(|event| {
                    state
                        .checkpoint
                        .is_none_or(|checkpoint| event.seq > checkpoint)
                })
                .filter_map(|event| match &event.payload {
                    DurableEventPayload::CustomEvent(event) => Some(event),
                    _ => None,
                })
                .filter(|event| {
                    event.cascade_depth <= super::MAX_CUSTOM_EVENT_CASCADE_DEPTH
                        && custom_event_subscription_matches(
                            subscription,
                            &event.extension_id,
                            &event.event_type,
                        )
                })
                .count(),
        )
        .unwrap_or(u64::MAX);
        CustomEventConsumerStatus {
            extension_id: extension_id.to_owned(),
            subscription: subscription.clone(),
            paused: state.paused,
            checkpoint: state.checkpoint,
            stream_head,
            pending_events,
            in_flight: metrics.in_flight,
            failed_attempts: metrics.failed_attempts,
            consecutive_failures: u64::from(state.consecutive_failures)
                .max(metrics.consecutive_failures),
            quarantined_events: u64::try_from(state.quarantined.len()).unwrap_or(u64::MAX),
        }
    }

    fn wake_custom_event_consumer(
        &self,
        target: &CustomEventConsumerControlTarget<'_>,
    ) -> Result<(), CustomEventConsumerControlError> {
        let lane = self
            .custom_event_lane(
                target.view,
                target.extension_id,
                target.subscription,
                target.handler,
                target.session_id,
            )
            .ok_or_else(|| {
                CustomEventConsumerControlError::ConsumerUnavailable(custom_event_consumer_id(
                    target.extension_id,
                    target.subscription,
                ))
            })?;
        if Self::signal_durable_custom_events(&lane, target.session.clone()) {
            Ok(())
        } else {
            Err(CustomEventConsumerControlError::ConsumerUnavailable(
                custom_event_consumer_id(target.extension_id, target.subscription),
            ))
        }
    }

    async fn reset_custom_event_consumer(
        &self,
        target: &CustomEventConsumerControlTarget<'_>,
        reset: EventConsumerCheckpointReset,
    ) -> Result<(), CustomEventConsumerControlError> {
        let previous = target
            .session
            .event_store
            .event_consumer_state(target.session_id, target.consumer_id)
            .await?;
        target
            .session
            .event_store
            .set_event_consumer_paused(target.session_id, target.consumer_id, true)
            .await?;
        let metrics = self.custom_event_consumer_metrics(target.session_id, target.consumer_id);
        let result = if metrics.wait_until_idle(self.operation_timeout).await {
            target
                .session
                .event_store
                .reset_event_consumer_checkpoint(target.session_id, target.consumer_id, reset)
                .await
                .map(|_| ())
                .map_err(CustomEventConsumerControlError::from)
        } else {
            Err(CustomEventConsumerControlError::ConsumerBusy(
                target.consumer_id.to_owned(),
            ))
        };
        if let Err(error) = result {
            self.recover_custom_event_consumer(target, previous.paused)
                .await;
            return Err(error);
        }

        target
            .session
            .event_store
            .set_event_consumer_paused(target.session_id, target.consumer_id, previous.paused)
            .await?;
        if !previous.paused {
            self.wake_custom_event_consumer(target)?;
        }
        Ok(())
    }

    async fn recover_custom_event_consumer(
        &self,
        target: &CustomEventConsumerControlTarget<'_>,
        paused: bool,
    ) {
        let restored = target
            .session
            .event_store
            .set_event_consumer_paused(target.session_id, target.consumer_id, paused)
            .await;
        if let Err(error) = restored {
            tracing::error!(session_id = %target.session_id, consumer_id = target.consumer_id, %error, "failed to restore custom event consumer pause state");
            return;
        }
        if !paused && let Err(error) = self.wake_custom_event_consumer(target) {
            tracing::error!(session_id = %target.session_id, consumer_id = target.consumer_id, %error, "failed to wake restored custom event consumer");
        }
    }
}
