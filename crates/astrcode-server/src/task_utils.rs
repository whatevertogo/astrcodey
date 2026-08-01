use std::{
    future::Future,
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use futures_util::FutureExt;
use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio_util::task::TaskTracker;

#[derive(Debug, thiserror::Error)]
#[error("server background tasks are shutting down")]
pub(crate) struct OwnedTaskSetClosed;

/// Server-owned task lifetime.
///
/// An admission reserves the right to spawn one task. Shutdown first rejects
/// new admissions, waits for admitted callers to register their task, then
/// closes and drains the tracker.
pub(crate) struct OwnedTaskSet {
    accepting: AtomicBool,
    active_admissions: Mutex<usize>,
    admissions_drained: Notify,
    tracker: TaskTracker,
}

impl OwnedTaskSet {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            accepting: AtomicBool::new(true),
            active_admissions: Mutex::new(0),
            admissions_drained: Notify::new(),
            tracker: TaskTracker::new(),
        })
    }

    pub(crate) fn admit(self: &Arc<Self>) -> Result<OwnedTaskAdmission, OwnedTaskSetClosed> {
        let mut active = self.active_admissions.lock();
        if !self.accepting.load(Ordering::Acquire) {
            return Err(OwnedTaskSetClosed);
        }
        *active += 1;
        Ok(OwnedTaskAdmission {
            tasks: Arc::clone(self),
        })
    }

    pub(crate) fn spawn<F>(
        self: &Arc<Self>,
        task: F,
    ) -> Result<tokio::task::JoinHandle<F::Output>, OwnedTaskSetClosed>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        Ok(self.admit()?.spawn(task))
    }

    pub(crate) fn spawn_named(
        self: &Arc<Self>,
        name: &'static str,
        task: impl Future<Output = ()> + Send + 'static,
    ) -> Result<tokio::task::JoinHandle<()>, OwnedTaskSetClosed> {
        Ok(self.admit()?.spawn_named(name, task))
    }

    pub(crate) fn stop_accepting(&self) {
        let _active = self.active_admissions.lock();
        self.accepting.store(false, Ordering::Release);
    }

    pub(crate) async fn wait_for_admissions(&self) {
        loop {
            let notified = self.admissions_drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if *self.active_admissions.lock() == 0 {
                break;
            }
            notified.await;
        }
    }

    pub(crate) async fn close_admission(&self) {
        self.stop_accepting();
        self.wait_for_admissions().await;
        self.tracker.close();
    }

    pub(crate) async fn close_and_wait(&self) {
        self.close_admission().await;
        self.tracker.wait().await;
    }

    #[cfg(feature = "testing")]
    pub(crate) fn task_count(&self) -> usize {
        self.tracker.len()
    }

    #[cfg(feature = "testing")]
    pub(crate) fn is_accepting(&self) -> bool {
        self.accepting.load(Ordering::Acquire)
    }
}

pub(crate) struct OwnedTaskAdmission {
    tasks: Arc<OwnedTaskSet>,
}

impl OwnedTaskAdmission {
    pub(crate) fn spawn<F>(self, task: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let handle = self.tasks.tracker.spawn(task);
        drop(self);
        handle
    }

    pub(crate) fn spawn_named(
        self,
        name: &'static str,
        task: impl Future<Output = ()> + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        self.spawn(async move {
            if AssertUnwindSafe(task).catch_unwind().await.is_err() {
                tracing::error!(task = name, "owned background task panicked");
            }
        })
    }
}

impl Drop for OwnedTaskAdmission {
    fn drop(&mut self) {
        let mut active = self.tasks.active_admissions.lock();
        *active -= 1;
        if *active == 0 {
            self.tasks.admissions_drained.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{future, sync::Arc};

    use tokio::sync::oneshot;

    use super::OwnedTaskSet;

    struct DropSignal(Option<oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }

    #[tokio::test]
    async fn admission_closes_without_losing_owned_tasks() {
        let tasks = OwnedTaskSet::new();
        let (started_tx, started_rx) = oneshot::channel();
        let (dropped_tx, dropped_rx) = oneshot::channel();
        let admission = tasks.admit().unwrap();
        let closing_tasks = Arc::clone(&tasks);
        let mut closing = tokio::spawn(async move {
            closing_tasks.close_and_wait().await;
        });
        tokio::task::yield_now().await;
        assert!(tasks.admit().is_err());
        assert!(!closing.is_finished());

        let handle = admission.spawn(async move {
            let _drop_signal = DropSignal(Some(dropped_tx));
            let _ = started_tx.send(());
            future::pending::<()>().await;
        });

        started_rx.await.unwrap();
        assert!(!closing.is_finished());
        handle.abort();
        assert!(handle.await.unwrap_err().is_cancelled());
        dropped_rx.await.unwrap();
        (&mut closing).await.unwrap();
        assert!(tasks.spawn(async {}).is_err());
    }
}
