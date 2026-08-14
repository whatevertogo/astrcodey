use std::{
    collections::HashMap,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use astrcode_extension_sdk::extension::{
    Extension, ExtensionError, ExtensionTasks, StopReason,
    internal::{cancel_extension_tasks, extension_stop_context, wait_extension_tasks},
};
use futures_util::FutureExt;
use tokio::{
    sync::{Mutex as AsyncMutex, Notify, OwnedMutexGuard, oneshot},
    task::JoinSet,
};
use tracing::Instrument;

use super::{HostedExtension, supervisor::ExtensionSupervisor};
use crate::host_router::{ExtensionGenerationGate, ExtensionInstanceId, HostRouter};

/// Tracks published indexes that can still dispatch into one extension instance.
pub(super) struct ExtensionPublicationLease {
    published_indexes: AtomicUsize,
    released: Notify,
}

impl ExtensionPublicationLease {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            published_indexes: AtomicUsize::new(0),
            released: Notify::new(),
        })
    }

    pub(super) fn acquire(self: &Arc<Self>) -> ExtensionIndexLease {
        self.published_indexes.fetch_add(1, Ordering::AcqRel);
        ExtensionIndexLease(Arc::clone(self))
    }

    async fn wait_until_unpublished(&self, timeout: std::time::Duration) -> Result<(), usize> {
        let wait = async {
            loop {
                let released = self.released.notified();
                if self.published_indexes.load(Ordering::Acquire) == 0 {
                    return;
                }
                released.await;
            }
        };
        if tokio::time::timeout(timeout, wait).await.is_ok() {
            return Ok(());
        }
        let published_indexes = self.published_indexes.load(Ordering::Acquire);
        if published_indexes == 0 {
            Ok(())
        } else {
            Err(published_indexes)
        }
    }

    async fn wait_until_fully_unpublished(&self) {
        loop {
            let released = self.released.notified();
            if self.published_indexes.load(Ordering::Acquire) == 0 {
                return;
            }
            released.await;
        }
    }
}

pub(super) struct ExtensionIndexLease(Arc<ExtensionPublicationLease>);

impl Drop for ExtensionIndexLease {
    fn drop(&mut self) {
        if self.0.published_indexes.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.released.notify_waiters();
        }
    }
}

pub(super) struct RetirementSupervisor {
    tasks: parking_lot::Mutex<JoinSet<()>>,
    pending: Arc<AtomicUsize>,
    completed: Arc<Notify>,
    operation_gates: parking_lot::Mutex<HashMap<String, Weak<AsyncMutex<()>>>>,
    next_retirement_id: AtomicU64,
    completed_errors: Arc<parking_lot::Mutex<Vec<RecordedRetirementError>>>,
}

struct RetirementCompletion {
    retirement_id: u64,
    extension_id: String,
    pending: Arc<AtomicUsize>,
    completed: Arc<Notify>,
    completed_errors: Arc<parking_lot::Mutex<Vec<RecordedRetirementError>>>,
    finished: bool,
    _operation_guard: OwnedMutexGuard<()>,
}

struct RetirementWork {
    extension_id: String,
    extension: Arc<dyn Extension>,
    tasks: ExtensionTasks,
    generation_gate: ExtensionGenerationGate,
    publication_lease: Option<Arc<ExtensionPublicationLease>>,
    supervisor: Option<ExtensionSupervisor>,
    cleanup_host_resources: Option<(Arc<HostRouter>, ExtensionInstanceId)>,
}

/// Owns a registration after its runtime resources exist but before publication.
///
/// Dropping an armed instance synchronously transfers those resources and the keyed lifecycle
/// guard to the retirement supervisor, so cancellation cannot abandon startup rollback.
pub(super) struct PendingRegistration<'a> {
    supervisor: &'a RetirementSupervisor,
    resources: Option<PendingRegistrationResources>,
    operation_guard: Option<OwnedMutexGuard<()>>,
    operation_timeout: std::time::Duration,
}

pub(super) struct PendingRegistrationResources {
    pub(super) extension_id: String,
    pub(super) instance_id: ExtensionInstanceId,
    pub(super) extension: Arc<dyn Extension>,
    pub(super) tasks: ExtensionTasks,
    pub(super) generation_gate: ExtensionGenerationGate,
    pub(super) host_router: Arc<HostRouter>,
}

pub(crate) struct RetirementTicket {
    retirement_id: u64,
    extension_id: String,
    outcome: oneshot::Receiver<RetirementTicketOutcome>,
    completed_errors: Arc<parking_lot::Mutex<Vec<RecordedRetirementError>>>,
}

struct RetirementTicketOutcome {
    _completion: RetirementCompletion,
}

struct RecordedRetirementError {
    retirement_id: u64,
    error: ExtensionRetirementError,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct ExtensionRetirementError {
    message: String,
}

impl ExtensionRetirementError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl RetirementCompletion {
    fn finish(&mut self, error: Option<ExtensionRetirementError>) {
        if let Some(error) = error {
            record_error(&self.completed_errors, self.retirement_id, error);
        }
        self.finished = true;
    }
}

impl PendingRegistration<'_> {
    fn take_work(&mut self) -> Option<RetirementWork> {
        let resources = self.resources.take()?;
        Some(RetirementWork {
            extension_id: resources.extension_id,
            extension: resources.extension,
            tasks: resources.tasks,
            generation_gate: resources.generation_gate,
            publication_lease: None,
            supervisor: None,
            cleanup_host_resources: Some((resources.host_router, resources.instance_id)),
        })
    }

    pub(super) fn retire(mut self) -> Result<RetirementTicket, ExtensionRetirementError> {
        let Some(work) = self.take_work() else {
            return Err(ExtensionRetirementError::new(
                "pending registration lost startup resources",
            ));
        };
        let Some(operation_guard) = self.operation_guard.take() else {
            return Err(ExtensionRetirementError::new(format!(
                "pending registration lost its lifecycle gate for {}",
                work.extension_id
            )));
        };
        Ok(self
            .supervisor
            .retire_registration(work, self.operation_timeout, operation_guard))
    }

    pub(super) fn disarm(mut self) -> Result<OwnedMutexGuard<()>, ExtensionRetirementError> {
        let operation_guard = self.operation_guard.take().ok_or_else(|| {
            ExtensionRetirementError::new("pending registration lost its lifecycle gate")
        })?;
        self.resources.take();
        Ok(operation_guard)
    }
}

impl Drop for PendingRegistration<'_> {
    fn drop(&mut self) {
        let Some(work) = self.take_work() else {
            return;
        };
        let Some(operation_guard) = self.operation_guard.take() else {
            return;
        };
        self.supervisor
            .abandon_registration(work, self.operation_timeout, operation_guard);
    }
}

impl RetirementTicket {
    pub(crate) async fn wait(self) -> Result<(), ExtensionRetirementError> {
        let Self {
            retirement_id,
            extension_id,
            outcome,
            completed_errors,
        } = self;
        match outcome.await {
            Ok(outcome) => {
                let result =
                    take_recorded_error(&completed_errors, retirement_id).map_or(Ok(()), Err);
                drop(outcome);
                result
            },
            Err(_) => Err(
                take_recorded_error(&completed_errors, retirement_id).unwrap_or_else(|| {
                    ExtensionRetirementError::new(format!(
                        "retirement outcome channel closed for {extension_id}"
                    ))
                }),
            ),
        }
    }
}

impl Drop for RetirementCompletion {
    fn drop(&mut self) {
        if !self.finished {
            record_error(
                &self.completed_errors,
                self.retirement_id,
                ExtensionRetirementError::new(format!(
                    "extension retirement task stopped before completion for {}",
                    self.extension_id
                )),
            );
        }
        if self.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.completed.notify_waiters();
        }
    }
}

impl RetirementSupervisor {
    pub(super) fn new() -> Self {
        Self {
            tasks: parking_lot::Mutex::new(JoinSet::new()),
            pending: Arc::new(AtomicUsize::new(0)),
            completed: Arc::new(Notify::new()),
            operation_gates: parking_lot::Mutex::new(HashMap::new()),
            next_retirement_id: AtomicU64::new(1),
            completed_errors: Arc::new(parking_lot::Mutex::new(Vec::new())),
        }
    }

    pub(super) fn operation_gate(&self, extension_id: &str) -> Arc<AsyncMutex<()>> {
        let mut gates = self.operation_gates.lock();
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(extension_id).and_then(Weak::upgrade) {
            return gate;
        }

        let gate = Arc::new(AsyncMutex::new(()));
        gates.insert(extension_id.to_owned(), Arc::downgrade(&gate));
        gate
    }

    pub(super) fn pending_registration(
        &self,
        resources: PendingRegistrationResources,
        operation_guard: OwnedMutexGuard<()>,
        operation_timeout: std::time::Duration,
    ) -> PendingRegistration<'_> {
        PendingRegistration {
            supervisor: self,
            resources: Some(resources),
            operation_guard: Some(operation_guard),
            operation_timeout,
        }
    }

    pub(super) fn retire(
        &self,
        hosted: HostedExtension,
        reason: StopReason,
        operation_timeout: std::time::Duration,
        operation_guard: OwnedMutexGuard<()>,
        cleanup_host_resources: Arc<HostRouter>,
    ) -> RetirementTicket {
        let work = RetirementWork {
            extension_id: hosted.manifest.id().to_owned(),
            extension: hosted.extension,
            tasks: hosted.tasks,
            generation_gate: hosted.generation_gate,
            publication_lease: Some(hosted.publication_lease),
            supervisor: Some(hosted.supervisor),
            cleanup_host_resources: Some((cleanup_host_resources, hosted.instance_id)),
        };
        self.spawn_ticketed_retirement(work, reason, operation_timeout, operation_guard)
    }

    fn retire_registration(
        &self,
        work: RetirementWork,
        operation_timeout: std::time::Duration,
        operation_guard: OwnedMutexGuard<()>,
    ) -> RetirementTicket {
        self.spawn_ticketed_retirement(
            work,
            StopReason::StartupFailed,
            operation_timeout,
            operation_guard,
        )
    }

    fn abandon_registration(
        &self,
        work: RetirementWork,
        operation_timeout: std::time::Duration,
        operation_guard: OwnedMutexGuard<()>,
    ) {
        self.spawn_retirement(
            work,
            StopReason::StartupFailed,
            operation_timeout,
            operation_guard,
            None,
            false,
        );
    }

    pub(super) fn retire_replaced(
        &self,
        hosted: HostedExtension,
        reason: StopReason,
        operation_timeout: std::time::Duration,
        operation_guard: OwnedMutexGuard<()>,
        cleanup_host_resources: Arc<HostRouter>,
    ) {
        let work = RetirementWork {
            extension_id: hosted.manifest.id().to_owned(),
            extension: hosted.extension,
            tasks: hosted.tasks,
            generation_gate: hosted.generation_gate,
            publication_lease: Some(hosted.publication_lease),
            supervisor: Some(hosted.supervisor),
            cleanup_host_resources: Some((cleanup_host_resources, hosted.instance_id)),
        };
        self.spawn_retirement(work, reason, operation_timeout, operation_guard, None, true);
    }

    fn spawn_ticketed_retirement(
        &self,
        work: RetirementWork,
        reason: StopReason,
        operation_timeout: std::time::Duration,
        operation_guard: OwnedMutexGuard<()>,
    ) -> RetirementTicket {
        let extension_id = work.extension_id.clone();
        let (outcome_tx, outcome) = oneshot::channel();
        let retirement_id = self.spawn_retirement(
            work,
            reason,
            operation_timeout,
            operation_guard,
            Some(outcome_tx),
            false,
        );
        RetirementTicket {
            retirement_id,
            extension_id,
            outcome,
            completed_errors: Arc::clone(&self.completed_errors),
        }
    }

    fn spawn_retirement(
        &self,
        work: RetirementWork,
        reason: StopReason,
        operation_timeout: std::time::Duration,
        operation_guard: OwnedMutexGuard<()>,
        outcome: Option<oneshot::Sender<RetirementTicketOutcome>>,
        drain_publication_first: bool,
    ) -> u64 {
        let mut tasks = self.tasks.lock();
        self.collect_ready(&mut tasks);
        self.pending.fetch_add(1, Ordering::AcqRel);
        let retirement_id = self.next_retirement_id.fetch_add(1, Ordering::Relaxed);
        let extension_id = work.extension_id.clone();
        let mut completion = RetirementCompletion {
            retirement_id,
            extension_id: extension_id.clone(),
            pending: Arc::clone(&self.pending),
            completed: Arc::clone(&self.completed),
            completed_errors: Arc::clone(&self.completed_errors),
            finished: false,
            _operation_guard: operation_guard,
        };
        let span = tracing::info_span!("extension.retirement", %extension_id, ?reason);
        let retirement = async move {
            let mut work = work;
            let supervisor = work.supervisor.take();
            let supervisor_control = supervisor.as_ref().map(ExtensionSupervisor::control);
            let cleanup_host_resources = work.cleanup_host_resources.take();
            let result = AssertUnwindSafe(async move {
                let mut errors = Vec::new();
                if let Some(publication_lease) = work.publication_lease {
                    if drain_publication_first {
                        publication_lease.wait_until_fully_unpublished().await;
                    } else if let Err(published_indexes) = publication_lease
                        .wait_until_unpublished(operation_timeout)
                        .await
                    {
                        errors.push(format!(
                            "timed out waiting for {published_indexes} published extension \
                             index(es)"
                        ));
                    }
                }
                work.generation_gate.deactivate();
                cancel_extension_tasks(&work.tasks);
                if drain_publication_first
                    && let Some(supervisor) = supervisor_control
                    && let Err(error) = supervisor.begin_draining().await
                {
                    errors.push(error.to_string());
                }
                let tasks_stopped = wait_extension_tasks(&work.tasks, operation_timeout).await;
                let stop_result = match tokio::time::timeout(
                    operation_timeout,
                    work.extension.stop(extension_stop_context(reason)),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(ExtensionError::Timeout(operation_timeout.as_millis() as u64)),
                };
                if !tasks_stopped {
                    errors.push("managed extension tasks remained active after abort".into());
                }
                if let Err(error) = stop_result {
                    errors.push(format!("stop failed: {error}"));
                }
                if errors.is_empty() {
                    Ok(())
                } else {
                    Err(ExtensionError::Internal(errors.join("; ")))
                }
            })
            .catch_unwind()
            .await;
            if let Some((host_router, instance_id)) = cleanup_host_resources {
                host_router.cleanup_extension_resources(instance_id);
            }
            let supervisor_failure = match &result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(_) => Some(format!(
                    "extension retirement task panicked for {extension_id}"
                )),
            };
            if let Some(supervisor) = supervisor {
                supervisor.finish(supervisor_failure).await;
            }
            match result {
                Ok(Ok(())) => {
                    tracing::debug!(%extension_id, "extension retirement completed");
                    completion.finish(None);
                },
                Ok(Err(error)) => completion.finish(Some(ExtensionRetirementError::new(format!(
                    "failed to stop extension {extension_id}: {error}"
                )))),
                Err(_) => completion.finish(Some(ExtensionRetirementError::new(format!(
                    "extension retirement task panicked for {extension_id}"
                )))),
            };
            if let Some(outcome) = outcome {
                let _ = outcome.send(RetirementTicketOutcome {
                    _completion: completion,
                });
            }
        };
        tasks.spawn(retirement.instrument(span));
        retirement_id
    }

    pub(super) async fn drain(&self) -> Vec<String> {
        loop {
            let completed = self.completed.notified();
            if self.pending.load(Ordering::Acquire) == 0 {
                break;
            }
            completed.await;
        }

        // All retirement side effects and error recording have completed. Moving the
        // now-ready handles is cancellation-safe: dropping them cannot skip stop().
        let mut tasks = {
            let mut supervised = self.tasks.lock();
            self.collect_ready(&mut supervised);
            std::mem::take(&mut *supervised)
        };
        while let Some(result) = tasks.join_next().await {
            self.record_join_result(result);
        }
        std::mem::take(&mut *self.completed_errors.lock())
            .into_iter()
            .map(|recorded| recorded.error.to_string())
            .collect()
    }

    fn collect_ready(&self, tasks: &mut JoinSet<()>) {
        while let Some(result) = tasks.try_join_next() {
            self.record_join_result(result);
        }
    }

    fn record_join_result(&self, result: Result<(), tokio::task::JoinError>) {
        if let Err(error) = result {
            tracing::debug!(
                error = %error,
                "extension retirement join failed after its completion guard recorded the outcome"
            );
        }
    }
}

fn record_error(
    errors: &parking_lot::Mutex<Vec<RecordedRetirementError>>,
    retirement_id: u64,
    error: ExtensionRetirementError,
) {
    tracing::warn!(error = %error, "extension retirement failed");
    errors.lock().push(RecordedRetirementError {
        retirement_id,
        error,
    });
}

fn take_recorded_error(
    errors: &parking_lot::Mutex<Vec<RecordedRetirementError>>,
    retirement_id: u64,
) -> Option<ExtensionRetirementError> {
    let mut recorded = errors.lock();
    let index = recorded
        .iter()
        .position(|error| error.retirement_id == retirement_id)?;
    Some(recorded.remove(index).error)
}
