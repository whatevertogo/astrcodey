use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use astrcode_extension_sdk::extension::{ExtensionError, StopReason};
use futures_util::FutureExt;
use tokio::{sync::Notify, task::JoinSet};

use super::HostedExtension;

/// Tracks turn-scoped views that may still dispatch through their captured handlers.
pub(super) struct ActiveTurnViews {
    active: AtomicUsize,
    released: Notify,
}

impl ActiveTurnViews {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            active: AtomicUsize::new(0),
            released: Notify::new(),
        })
    }

    pub(super) fn acquire(self: &Arc<Self>) -> ActiveTurnViewLease {
        self.active.fetch_add(1, Ordering::AcqRel);
        ActiveTurnViewLease {
            views: Arc::clone(self),
        }
    }

    pub(super) async fn wait_until_idle(&self, timeout: std::time::Duration) -> Result<(), usize> {
        let wait = async {
            loop {
                let released = self.released.notified();
                if self.active.load(Ordering::Acquire) == 0 {
                    return;
                }
                released.await;
            }
        };
        if tokio::time::timeout(timeout, wait).await.is_ok() {
            return Ok(());
        }
        let active = self.active.load(Ordering::Acquire);
        if active == 0 { Ok(()) } else { Err(active) }
    }
}

/// RAII 租约：持有期间计入活跃 turn 视图数，drop 时递减并在归零时唤醒等待者。
pub(super) struct ActiveTurnViewLease {
    views: Arc<ActiveTurnViews>,
}

impl Drop for ActiveTurnViewLease {
    fn drop(&mut self) {
        if self.views.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.views.released.notify_waiters();
        }
    }
}

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

    async fn wait_until_unpublished(&self) {
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
    completed_errors: Arc<parking_lot::Mutex<Vec<String>>>,
}

struct RetirementCompletion {
    pending: Arc<AtomicUsize>,
    completed: Arc<Notify>,
}

impl Drop for RetirementCompletion {
    fn drop(&mut self) {
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
            completed_errors: Arc::new(parking_lot::Mutex::new(Vec::new())),
        }
    }

    pub(super) fn retire(
        &self,
        hosted: HostedExtension,
        reason: StopReason,
        operation_timeout: std::time::Duration,
    ) {
        let mut tasks = self.tasks.lock();
        self.collect_ready(&mut tasks);
        self.pending.fetch_add(1, Ordering::AcqRel);
        let completion = RetirementCompletion {
            pending: Arc::clone(&self.pending),
            completed: Arc::clone(&self.completed),
        };
        let completed_errors = Arc::clone(&self.completed_errors);
        tasks.spawn(async move {
            let _completion = completion;
            let extension_id = hosted.manifest.id.clone();
            let result = AssertUnwindSafe(async move {
                hosted.publication_lease.wait_until_unpublished().await;
                hosted.tasks.cancel();
                let tasks_stopped = hosted.tasks.wait(operation_timeout).await;
                let stop_result =
                    match tokio::time::timeout(operation_timeout, hosted.extension.stop(reason))
                        .await
                    {
                        Ok(result) => result,
                        Err(_) => {
                            Err(ExtensionError::Timeout(operation_timeout.as_millis() as u64))
                        },
                    };
                if !tasks_stopped {
                    return match stop_result {
                        Ok(()) => Err(ExtensionError::Internal(
                            "managed extension tasks remained active after abort".into(),
                        )),
                        Err(stop_error) => Err(ExtensionError::Internal(format!(
                            "managed extension tasks remained active after abort; stop also \
                             failed: {stop_error}"
                        ))),
                    };
                }
                stop_result
            })
            .catch_unwind()
            .await;
            match result {
                Ok(Ok(())) => {
                    tracing::debug!(%extension_id, "extension retirement completed");
                },
                Ok(Err(error)) => record_error(
                    &completed_errors,
                    format!("failed to stop extension {extension_id}: {error}"),
                ),
                Err(_) => record_error(
                    &completed_errors,
                    format!("extension retirement task panicked for {extension_id}"),
                ),
            };
        });
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
    }

    fn collect_ready(&self, tasks: &mut JoinSet<()>) {
        while let Some(result) = tasks.try_join_next() {
            self.record_join_result(result);
        }
    }

    fn record_join_result(&self, result: Result<(), tokio::task::JoinError>) {
        if let Err(error) = result {
            record_error(
                &self.completed_errors,
                format!("extension retirement task failed: {error}"),
            );
        }
    }
}

fn record_error(errors: &parking_lot::Mutex<Vec<String>>, error: String) {
    tracing::warn!(error = %error, "extension retirement failed");
    errors.lock().push(error);
}
