//! Session-scoped custom-event delivery, replay, retry, and quiescence.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use astrcode_core::{
    event::{DurableEventPayload, Event, EventSender},
    types::SessionId,
};
use astrcode_extension_sdk::extension::{
    CustomEventContext, CustomEventDisposition, CustomEventHandler, CustomEventSubscription,
    internal::custom_event_subscription_matches,
};
use astrcode_storage::{EventConsumerCheckpointOutcome, EventConsumerFailureOutcome, SessionStore};
use tokio::sync::{Notify, OwnedSemaphorePermit, mpsc};

use super::{ExtensionRunner, ExtensionView, host_invoker::ExtensionCallContextInput};

pub(super) const CUSTOM_EVENT_CONCURRENCY: usize = 64;
pub(super) const MAX_CUSTOM_EVENT_CASCADE_DEPTH: u8 = 8;
const CUSTOM_EVENT_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(250);
const CUSTOM_EVENT_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);
const CUSTOM_EVENT_QUARANTINE_AFTER: u32 = 20;

pub(super) type CustomEventLanes =
    parking_lot::Mutex<HashMap<CustomEventLaneId, Weak<CustomEventLane>>>;
pub(super) type CustomEventQuiescing = parking_lot::Mutex<HashSet<SessionId>>;

type CustomEventSenderFactory =
    Arc<dyn Fn(Option<astrcode_core::types::TurnId>) -> EventSender + Send + Sync>;

pub(super) fn custom_event_consumer_id(
    extension_id: &str,
    subscription: &CustomEventSubscription,
) -> String {
    format!(
        "{extension_id}:{}:v{}",
        subscription.id, subscription.consumer_version
    )
}

#[derive(Clone)]
#[doc(hidden)]
pub struct CustomEventSession {
    pub(super) event_store: Arc<dyn SessionStore>,
    event_sender: CustomEventSenderFactory,
}

impl CustomEventSession {
    pub fn new(
        event_store: Arc<dyn SessionStore>,
        event_sender: impl Fn(Option<astrcode_core::types::TurnId>) -> EventSender
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            event_store,
            event_sender: Arc::new(event_sender),
        }
    }

    fn event_sender(&self, turn_id: Option<astrcode_core::types::TurnId>) -> EventSender {
        (self.event_sender)(turn_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct CustomEventLaneId {
    generation: u64,
    session_id: astrcode_core::types::SessionId,
    consumer_id: String,
}

pub(super) struct CustomEventLane {
    sender: mpsc::UnboundedSender<CustomEventLaneCommand>,
    durable_reconciliation_queued: AtomicBool,
    consumer: Arc<CustomEventConsumer>,
    stopped: tokio_util::sync::CancellationToken,
}

struct CustomEventLaneStopGuard(tokio_util::sync::CancellationToken);

impl Drop for CustomEventLaneStopGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

enum CustomEventLaneCommand {
    Live {
        _lane: Arc<CustomEventLane>,
        _permit: OwnedSemaphorePermit,
        event: Arc<Event>,
        session: CustomEventSession,
    },
    ReconcileDurable {
        _lane: Arc<CustomEventLane>,
        session: CustomEventSession,
    },
}

struct CustomEventConsumer {
    view: Arc<ExtensionView>,
    extension_id: String,
    consumer_id: String,
    subscription: CustomEventSubscription,
    cancellation: tokio_util::sync::CancellationToken,
    handler: Arc<dyn CustomEventHandler>,
    metrics: Arc<CustomEventConsumerMetrics>,
    session_id: astrcode_core::types::SessionId,
}

#[derive(Clone)]
struct CustomEventInvocation {
    _lane: Arc<CustomEventLane>,
    view: Arc<ExtensionView>,
    extension_id: String,
    consumer_id: String,
    cancellation: tokio_util::sync::CancellationToken,
    handler: Arc<dyn CustomEventHandler>,
    metrics: Arc<CustomEventConsumerMetrics>,
    context: CustomEventContext,
    event_store: Arc<dyn SessionStore>,
    session_id: astrcode_core::types::SessionId,
    seq: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CustomEventInvocationOutcome {
    Consumed,
    Paused,
    Retry,
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
    fn track_delivery(&self) -> ActiveDelivery<'_> {
        self.active_deliveries.fetch_add(1, Ordering::AcqRel);
        ActiveDelivery(self)
    }

    fn record_failure(&self) {
        self.failed_attempts.fetch_add(1, Ordering::Relaxed);
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
    }

    fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    pub(super) fn snapshot(&self) -> CustomEventConsumerMetricsSnapshot {
        CustomEventConsumerMetricsSnapshot {
            in_flight: self.active_deliveries.load(Ordering::Acquire) != 0,
            failed_attempts: self.failed_attempts.load(Ordering::Relaxed),
            consecutive_failures: self.consecutive_failures.load(Ordering::Relaxed),
        }
    }

    pub(super) async fn wait_until_idle(&self, timeout: Duration) -> bool {
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

struct ActiveDelivery<'a>(&'a CustomEventConsumerMetrics);

impl Drop for ActiveDelivery<'_> {
    fn drop(&mut self) {
        if self.0.active_deliveries.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.became_idle.notify_waiters();
        }
    }
}

async fn run_custom_event_lane(
    consumer: Arc<CustomEventConsumer>,
    mut receiver: mpsc::UnboundedReceiver<CustomEventLaneCommand>,
    _stopped: CustomEventLaneStopGuard,
) {
    loop {
        let command = tokio::select! {
            biased;
            () = consumer.cancellation.cancelled() => break,
            command = receiver.recv() => match command {
                Some(command) => command,
                None => break,
            },
        };
        match command {
            CustomEventLaneCommand::Live {
                _lane,
                _permit,
                event,
                session,
                ..
            } => {
                if let Some(invocation) = consumer.invocation(_lane, &event, &session).await {
                    let _ = run_custom_event_invocation(&invocation).await;
                }
                drop(_permit);
            },
            CustomEventLaneCommand::ReconcileDurable { _lane, session } => {
                _lane
                    .durable_reconciliation_queued
                    .store(false, Ordering::Release);
                reconcile_durable_custom_events(&consumer, &_lane, &session).await;
            },
        }
    }
}

async fn reconcile_durable_custom_events(
    consumer: &CustomEventConsumer,
    lane: &Arc<CustomEventLane>,
    session: &CustomEventSession,
) {
    let mut retry_delay = CUSTOM_EVENT_RETRY_INITIAL_DELAY;
    loop {
        if reconcile_durable_custom_events_once(consumer, lane, session).await {
            return;
        }
        tokio::select! {
            () = consumer.cancellation.cancelled() => return,
            () = tokio::time::sleep(retry_delay) => {},
        }
        retry_delay = retry_delay
            .saturating_mul(2)
            .min(CUSTOM_EVENT_RETRY_MAX_DELAY);
    }
}

async fn reconcile_durable_custom_events_once(
    consumer: &CustomEventConsumer,
    lane: &Arc<CustomEventLane>,
    session: &CustomEventSession,
) -> bool {
    let state = match session
        .event_store
        .event_consumer_state(&consumer.session_id, &consumer.consumer_id)
        .await
    {
        Ok(state) => state,
        Err(error) => {
            consumer.metrics.record_failure();
            tracing::warn!(
                extension_id = consumer.extension_id,
                session_id = %consumer.session_id,
                %error,
                "failed to read custom event consumer checkpoint"
            );
            return false;
        },
    };
    if state.paused {
        return true;
    }
    let stored_events = match state.checkpoint {
        Some(seq) => {
            let cursor = seq.to_string();
            session
                .event_store
                .replay_from(&consumer.session_id, &cursor)
                .await
        },
        None => {
            session
                .event_store
                .replay_events(&consumer.session_id)
                .await
        },
    };
    let stored_events = match stored_events {
        Ok(events) => events,
        Err(error) => {
            consumer.metrics.record_failure();
            tracing::warn!(
                extension_id = consumer.extension_id,
                session_id = %consumer.session_id,
                %error,
                "failed to replay durable custom events"
            );
            return false;
        },
    };

    let replay_head = stored_events.last().map(|event| event.seq);
    for stored in stored_events {
        let custom_event = match &stored.payload {
            DurableEventPayload::CustomEvent(custom_event) => custom_event,
            _ => continue,
        };
        if !custom_event_subscription_matches(
            &consumer.subscription,
            &custom_event.extension_id,
            &custom_event.event_type,
        ) {
            continue;
        }
        if custom_event.cascade_depth > MAX_CUSTOM_EVENT_CASCADE_DEPTH {
            tracing::warn!(
                event_id = %stored.id,
                cascade_depth = custom_event.cascade_depth,
                "custom event cascade depth exceeded"
            );
            continue;
        }

        let event = Arc::new(Event::from(stored));
        let Some(invocation) = consumer.invocation(Arc::clone(lane), &event, session).await else {
            return false;
        };
        let mut retry_delay = CUSTOM_EVENT_RETRY_INITIAL_DELAY;
        loop {
            let permit = tokio::select! {
                result = Arc::clone(&consumer.view.custom_event_permits).acquire_owned() => {
                    match result {
                        Ok(permit) => permit,
                        Err(_) => return false,
                    }
                },
                () = consumer.cancellation.cancelled() => return false,
            };
            let outcome = run_custom_event_invocation(&invocation).await;
            drop(permit);
            match outcome {
                CustomEventInvocationOutcome::Consumed => break,
                CustomEventInvocationOutcome::Paused => return true,
                CustomEventInvocationOutcome::Retry => {},
            }
            tokio::select! {
                () = consumer.cancellation.cancelled() => return false,
                () = tokio::time::sleep(retry_delay) => {},
            }
            retry_delay = retry_delay
                .saturating_mul(2)
                .min(CUSTOM_EVENT_RETRY_MAX_DELAY);
        }
    }
    if let Some(replay_head) = replay_head {
        match session
            .event_store
            .checkpoint_event_consumer(
                &consumer.session_id,
                &consumer.consumer_id,
                state.revision,
                replay_head,
            )
            .await
        {
            Ok(EventConsumerCheckpointOutcome::Accepted) => {},
            Ok(EventConsumerCheckpointOutcome::StaleRevision) => return false,
            Err(error) => {
                consumer.metrics.record_failure();
                tracing::warn!(
                    extension_id = consumer.extension_id,
                    %error,
                    "failed to checkpoint inspected custom events"
                );
                return false;
            },
        }
    }
    consumer.metrics.record_success();
    true
}

impl CustomEventConsumer {
    async fn invocation(
        &self,
        lane: Arc<CustomEventLane>,
        event: &Arc<Event>,
        session: &CustomEventSession,
    ) -> Option<CustomEventInvocation> {
        let custom_event = event.payload.custom_event()?;
        let session_store_dir = match session
            .event_store
            .session_store_dir(&event.session_id)
            .await
        {
            Ok(session_store_dir) => session_store_dir,
            Err(error) => {
                tracing::warn!(
                    event_id = %event.id,
                    extension_id = self.extension_id,
                    %error,
                    "failed to resolve custom event session storage"
                );
                return None;
            },
        };
        let call = match self.view.make_registered_extension_call_context(
            &self.extension_id,
            ExtensionCallContextInput {
                session_id: Some(event.session_id.clone()),
                tool_call_id: None,
                working_dir: None,
                session_store_dir,
                event_tx: Some(session.event_sender(event.turn_id.clone())),
                event_causation: Some((event.id.clone(), custom_event.cascade_depth)),
                resource_lease: None,
                file_observation_store: None,
                tool_result_reader: None,
                cancellation: self.cancellation.clone(),
            },
        ) {
            Ok(call) => call,
            Err(error) => {
                tracing::warn!(
                    event_id = %event.id,
                    extension_id = self.extension_id,
                    %error,
                    "failed to build custom event context"
                );
                return None;
            },
        };
        let context = CustomEventContext::from_runtime(
            call,
            event.session_id.clone(),
            event.turn_id.as_ref().map(ToString::to_string),
            event.id.clone(),
            event.seq,
            custom_event.extension_id.clone(),
            custom_event.event_type.clone(),
            custom_event.schema_version,
            custom_event.causation_id.clone(),
            custom_event.cascade_depth,
            custom_event.payload.clone(),
        );
        Some(CustomEventInvocation {
            _lane: lane,
            view: Arc::clone(&self.view),
            extension_id: self.extension_id.clone(),
            consumer_id: self.consumer_id.clone(),
            cancellation: self.cancellation.clone(),
            handler: Arc::clone(&self.handler),
            metrics: Arc::clone(&self.metrics),
            context,
            event_store: Arc::clone(&session.event_store),
            session_id: event.session_id.clone(),
            seq: event.seq,
        })
    }
}

#[tracing::instrument(
    name = "custom_event.delivery",
    skip(invocation),
    fields(
        extension_id = %invocation.extension_id,
        consumer_id = %invocation.consumer_id,
        session_id = %invocation.session_id,
        seq = ?invocation.seq,
    )
)]
async fn run_custom_event_invocation(
    invocation: &CustomEventInvocation,
) -> CustomEventInvocationOutcome {
    let _active_delivery = invocation.metrics.track_delivery();
    let state = match invocation
        .event_store
        .event_consumer_state(&invocation.session_id, &invocation.consumer_id)
        .await
    {
        Ok(state) => state,
        Err(error) => {
            invocation.metrics.record_failure();
            tracing::warn!(
                extension_id = invocation.extension_id,
                %error,
                "failed to read custom event consumer state"
            );
            return CustomEventInvocationOutcome::Retry;
        },
    };
    if state.paused {
        return CustomEventInvocationOutcome::Paused;
    }
    if let Some(seq) = invocation.seq
        && state.checkpoint.is_some_and(|checkpoint| checkpoint >= seq)
    {
        return CustomEventInvocationOutcome::Consumed;
    }
    let result = invocation
        .view
        .run_recorded_hook(
            &invocation.extension_id,
            "custom_event",
            invocation.cancellation.clone(),
            invocation.handler.handle(invocation.context.clone()),
        )
        .await;
    let disposition = match result {
        Ok(disposition) => disposition,
        Err(error) => {
            tracing::warn!(
                extension_id = invocation.extension_id,
                %error,
                "custom event handler failed"
            );
            CustomEventDisposition::retry(error.to_string())
        },
    };
    let failure = match &disposition {
        CustomEventDisposition::Ack => None,
        CustomEventDisposition::Retry { reason } => Some((reason.as_str(), false)),
        CustomEventDisposition::DeadLetter { reason } => Some((reason.as_str(), true)),
    };
    if let Some((reason, explicit_dead_letter)) = failure {
        invocation.metrics.record_failure();
        let quarantine_after = if explicit_dead_letter {
            1
        } else {
            CUSTOM_EVENT_QUARANTINE_AFTER
        };
        if let Some(seq) = invocation.seq {
            match invocation
                .event_store
                .record_event_consumer_failure(
                    &invocation.session_id,
                    &invocation.consumer_id,
                    state.revision,
                    seq,
                    reason,
                    quarantine_after,
                )
                .await
            {
                Ok(EventConsumerFailureOutcome::Quarantined { attempts }) => {
                    invocation.metrics.record_success();
                    tracing::error!(
                        extension_id = invocation.extension_id,
                        session_id = %invocation.session_id,
                        seq,
                        attempts,
                        explicit_dead_letter,
                        "custom event moved to quarantine"
                    );
                    return CustomEventInvocationOutcome::Consumed;
                },
                Ok(EventConsumerFailureOutcome::AlreadyConsumed) => {
                    invocation.metrics.record_success();
                    return CustomEventInvocationOutcome::Consumed;
                },
                Ok(EventConsumerFailureOutcome::Recorded { .. }) => {},
                Ok(EventConsumerFailureOutcome::StaleRevision) => {
                    return CustomEventInvocationOutcome::Retry;
                },
                Err(storage_error) => {
                    tracing::warn!(
                        extension_id = invocation.extension_id,
                        session_id = %invocation.session_id,
                        seq,
                        %storage_error,
                        "failed to persist custom event delivery failure"
                    );
                    return CustomEventInvocationOutcome::Retry;
                },
            }
        }
        if explicit_dead_letter {
            invocation.metrics.record_success();
            return CustomEventInvocationOutcome::Consumed;
        }
        return CustomEventInvocationOutcome::Retry;
    }
    if let Some(seq) = invocation.seq {
        match invocation
            .event_store
            .checkpoint_event_consumer(
                &invocation.session_id,
                &invocation.consumer_id,
                state.revision,
                seq,
            )
            .await
        {
            Ok(EventConsumerCheckpointOutcome::Accepted) => {},
            Ok(EventConsumerCheckpointOutcome::StaleRevision) => {
                return CustomEventInvocationOutcome::Retry;
            },
            Err(error) => {
                invocation.metrics.record_failure();
                tracing::warn!(
                    extension_id = invocation.extension_id,
                    %error,
                    "failed to checkpoint custom event consumer"
                );
                return CustomEventInvocationOutcome::Retry;
            },
        }
    }
    invocation.metrics.record_success();
    CustomEventInvocationOutcome::Consumed
}

impl ExtensionRunner {
    pub fn observe_custom_event(&self, event: Arc<Event>, session: CustomEventSession) -> bool {
        let Some(custom_event) = event.payload.custom_event() else {
            return true;
        };
        let durable = event.payload.as_durable().is_some();
        if custom_event.cascade_depth > MAX_CUSTOM_EVENT_CASCADE_DEPTH {
            tracing::warn!(
                event_id = %event.id,
                cascade_depth = custom_event.cascade_depth,
                "custom event cascade depth exceeded"
            );
            return true;
        }

        let view = self.turn_extension_view_with_lease();
        let mut fully_admitted = true;
        for (extension_id, subscription, handler) in &view.index.custom_event {
            if !custom_event_subscription_matches(
                subscription,
                &custom_event.extension_id,
                &custom_event.event_type,
            ) {
                continue;
            }
            let Some(lane) = self.custom_event_lane(
                &view,
                extension_id,
                subscription,
                handler,
                &event.session_id,
            ) else {
                fully_admitted = false;
                continue;
            };
            if durable {
                if !Self::signal_durable_custom_events(&lane, session.clone()) {
                    fully_admitted = false;
                }
                continue;
            }

            let permit = match Arc::clone(&view.custom_event_permits).try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    tracing::warn!(
                        event_id = %event.id,
                        extension_id,
                        "live custom event dispatch capacity exhausted"
                    );
                    fully_admitted = false;
                    continue;
                },
            };
            if lane
                .sender
                .send(CustomEventLaneCommand::Live {
                    _lane: Arc::clone(&lane),
                    _permit: permit,
                    event: Arc::clone(&event),
                    session: session.clone(),
                })
                .is_err()
            {
                tracing::warn!(
                    event_id = %event.id,
                    extension_id,
                    "custom event lane stopped before admission"
                );
                fully_admitted = false;
            }
        }
        fully_admitted
    }

    /// Wakes every durable consumer for one session after startup or extension reload.
    #[doc(hidden)]
    pub fn reconcile_custom_events(
        &self,
        session_id: &astrcode_core::types::SessionId,
        session: CustomEventSession,
    ) -> bool {
        let view = self.turn_extension_view_with_lease();
        let mut fully_admitted = true;
        for (extension_id, subscription, handler) in &view.index.custom_event {
            let Some(lane) =
                self.custom_event_lane(&view, extension_id, subscription, handler, session_id)
            else {
                fully_admitted = false;
                continue;
            };
            if !Self::signal_durable_custom_events(&lane, session.clone()) {
                fully_admitted = false;
            }
        }
        fully_admitted
    }

    pub(super) fn custom_event_lane(
        &self,
        view: &Arc<ExtensionView>,
        extension_id: &str,
        subscription: &CustomEventSubscription,
        handler: &Arc<dyn CustomEventHandler>,
        session_id: &astrcode_core::types::SessionId,
    ) -> Option<Arc<CustomEventLane>> {
        let consumer_id = custom_event_consumer_id(extension_id, subscription);
        let lane_id = CustomEventLaneId {
            generation: view.generation,
            session_id: session_id.clone(),
            consumer_id: consumer_id.clone(),
        };
        let mut lanes = view.custom_event_lanes.lock();
        if view.custom_event_quiescing.lock().contains(session_id) {
            return None;
        }
        lanes.retain(|_, lane| lane.strong_count() > 0);
        if let Some(lane) = lanes
            .get(&lane_id)
            .and_then(Weak::upgrade)
            .filter(|lane| !lane.sender.is_closed())
        {
            return Some(lane);
        }

        let consumer = Arc::new(self.custom_event_consumer(
            view,
            extension_id,
            subscription,
            handler,
            session_id,
        )?);
        let (sender, receiver) = mpsc::unbounded_channel();
        let stopped = tokio_util::sync::CancellationToken::new();
        let lane = Arc::new(CustomEventLane {
            sender,
            durable_reconciliation_queued: AtomicBool::new(false),
            consumer: Arc::clone(&consumer),
            stopped: stopped.clone(),
        });
        lanes.insert(lane_id, Arc::downgrade(&lane));
        view.spawn_extension_task(
            extension_id,
            "custom-event-lane",
            run_custom_event_lane(consumer, receiver, CustomEventLaneStopGuard(stopped)),
        );
        Some(lane)
    }

    fn custom_event_consumer(
        &self,
        view: &Arc<ExtensionView>,
        extension_id: &str,
        subscription: &CustomEventSubscription,
        handler: &Arc<dyn CustomEventHandler>,
        session_id: &astrcode_core::types::SessionId,
    ) -> Option<CustomEventConsumer> {
        let Some(generation) = view.index.extensions.get(extension_id) else {
            tracing::warn!(extension_id, "custom event consumer has no task owner");
            return None;
        };
        let consumer_id = custom_event_consumer_id(extension_id, subscription);
        Some(CustomEventConsumer {
            view: Arc::clone(view),
            extension_id: extension_id.to_owned(),
            consumer_id: consumer_id.clone(),
            subscription: subscription.clone(),
            cancellation: generation.tasks.cancellation().child_token(),
            handler: Arc::clone(handler),
            metrics: self.custom_event_consumer_metrics(session_id, &consumer_id),
            session_id: session_id.clone(),
        })
    }

    pub(super) fn signal_durable_custom_events(
        lane: &Arc<CustomEventLane>,
        session: CustomEventSession,
    ) -> bool {
        if lane
            .durable_reconciliation_queued
            .swap(true, Ordering::AcqRel)
        {
            return true;
        }
        if lane
            .sender
            .send(CustomEventLaneCommand::ReconcileDurable {
                _lane: Arc::clone(lane),
                session,
            })
            .is_ok()
        {
            return true;
        }
        lane.durable_reconciliation_queued
            .store(false, Ordering::Release);
        false
    }

    /// Stops admission and waits until every delivery lane for one session has exited.
    ///
    /// The caller must either [`Self::forget_custom_event_session`] after removing the
    /// session, or [`Self::resume_custom_event_session`] if the storage operation fails.
    pub async fn quiesce_custom_event_session(&self, session_id: &SessionId) {
        self.custom_event_quiescing
            .lock()
            .insert(session_id.clone());
        let lanes = self.take_custom_event_session_lanes(session_id);
        for (_, lane) in &lanes {
            lane.consumer.cancellation.cancel();
        }
        for (_, lane) in lanes {
            lane.stopped.cancelled().await;
        }
    }

    /// Re-opens custom-event admission after a failed session storage transition.
    pub fn resume_custom_event_session(&self, session_id: &SessionId, session: CustomEventSession) {
        self.custom_event_quiescing.lock().remove(session_id);
        self.reconcile_custom_events(session_id, session);
    }

    pub fn forget_custom_event_session(&self, session_id: &SessionId) {
        for (_, lane) in self.take_custom_event_session_lanes(session_id) {
            lane.consumer.cancellation.cancel();
        }
        self.custom_event_quiescing.lock().remove(session_id);
        self.custom_event_metrics
            .lock()
            .retain(|key, _| key.session_id != *session_id);
    }

    fn take_custom_event_session_lanes(
        &self,
        session_id: &SessionId,
    ) -> Vec<(CustomEventLaneId, Arc<CustomEventLane>)> {
        let mut lanes = self.custom_event_lanes.lock();
        let mut removed = Vec::new();
        lanes.retain(|lane_id, lane| {
            if lane_id.session_id == *session_id {
                if let Some(lane) = lane.upgrade() {
                    removed.push((lane_id.clone(), lane));
                }
                false
            } else {
                lane.strong_count() > 0
            }
        });
        removed
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
}
