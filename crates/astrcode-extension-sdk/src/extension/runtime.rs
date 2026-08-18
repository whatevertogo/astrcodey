use std::{
    collections::HashMap,
    future::Future,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::FutureExt;
use serde::de::IntoDeserializer;
use tokio::{
    sync::{Notify, oneshot, watch},
    task::AbortHandle,
};
use tokio_util::sync::CancellationToken;

pub use crate::wire::ExtensionCapability;

/// Wrapper type for extension-specific configuration.
///
/// Wraps the extension configuration under `extensions.<id>` in the user's `config.toml`;
/// extensions obtain it at `start()` via `deserialize::<T>()`. Configuration changes create a
/// new extension instance.
#[derive(Clone, Debug)]
pub struct ExtensionConfig {
    extension_id: Arc<str>,
    value: serde_json::Value,
}

#[cfg(any(test, feature = "testing"))]
impl Default for ExtensionConfig {
    fn default() -> Self {
        Self {
            extension_id: Arc::from("unknown-extension"),
            value: serde_json::Value::Null,
        }
    }
}

impl ExtensionConfig {
    pub(crate) fn from_runtime(extension_id: impl Into<String>, value: serde_json::Value) -> Self {
        Self {
            extension_id: Arc::from(extension_id.into()),
            value,
        }
    }

    /// Deserialize the configuration into a concrete type.
    ///
    /// # Example
    ///
    /// ```ignore
    /// #[derive(Deserialize)]
    /// struct MyConfig { timeout: u64, retry: bool }
    /// let cfg: MyConfig = ctx.config().deserialize()?;
    /// ```
    pub fn deserialize<T: serde::de::DeserializeOwned>(&self) -> Result<T, ExtensionConfigError> {
        serde_path_to_error::deserialize(self.value.clone().into_deserializer()).map_err(|error| {
            ExtensionConfigError {
                extension_id: self.extension_id.to_string(),
                path: error.path().to_string(),
                source: error.into_inner(),
            }
        })
    }

    pub fn deserialize_or_default<T>(&self) -> Result<T, ExtensionConfigError>
    where
        T: serde::de::DeserializeOwned + Default,
    {
        if self.is_empty() {
            Ok(T::default())
        } else {
            self.deserialize()
        }
    }

    /// Returns `true` if the configuration is `null` or an empty object `{}`.
    pub fn is_empty(&self) -> bool {
        self.value.is_null()
            || self
                .value
                .as_object()
                .is_some_and(|object| object.is_empty())
    }

    pub(crate) fn value(&self) -> &serde_json::Value {
        &self.value
    }
}

#[derive(Debug, thiserror::Error)]
#[error("extension {extension_id} config at {path}: {source}")]
pub struct ExtensionConfigError {
    extension_id: String,
    path: String,
    #[source]
    source: serde_json::Error,
}

impl ExtensionConfigError {
    pub fn extension_id(&self) -> &str {
        &self.extension_id
    }

    pub fn path(&self) -> &str {
        &self.path
    }
}

/// Reason an extension stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// A new instance replaced the same extension id on reload.
    Reload,
    /// Configuration disabled it, or the source no longer provides the extension.
    Disabled,
    /// The host process is shutting down.
    Shutdown,
    /// `start` failed or timed out; already-acquired resources were rolled back.
    StartupFailed,
}

/// Facts supplied when an extension generation is stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtensionStopContext {
    reason: StopReason,
}

impl ExtensionStopContext {
    pub(crate) const fn from_runtime(reason: StopReason) -> Self {
        Self { reason }
    }

    pub const fn reason(self) -> StopReason {
        self.reason
    }
}

/// Host-managed set of extension-generation tasks.
///
/// Extensions can only obtain it from [`super::ExtensionStartContext`]. Regular handlers should
/// submit work to workers created during startup rather than letting turn/call contexts spawn
/// generation tasks.
#[derive(Clone)]
pub struct ExtensionTasks {
    extension_id: Arc<str>,
    shutdown: CancellationToken,
    state: Arc<Mutex<ExtensionTaskState>>,
    lifecycle: watch::Sender<ExtensionTaskLifecycle>,
    completed: Arc<Notify>,
}

#[derive(Default)]
struct ExtensionTaskState {
    next_task_id: u64,
    tasks: HashMap<u64, ExtensionTask>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExtensionTaskLifecycle {
    Suspended,
    Active,
    Shutdown { was_active: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExtensionTaskKind {
    Background,
    MustFinish,
}

struct ExtensionTask {
    name: String,
    kind: ExtensionTaskKind,
    abort_handle: AbortHandle,
}

/// Structured error when `run_to_completion` cannot start or obtain a result.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExtensionTaskError {
    #[error("extension {extension_id} is shutting down; task `{task}` was not started")]
    ShuttingDown { extension_id: String, task: String },
    #[error("extension task `{task}` panicked for {extension_id}")]
    Panicked { extension_id: String, task: String },
    #[error("extension task `{task}` stopped before producing a result for {extension_id}")]
    RuntimeStopped { extension_id: String, task: String },
}

struct ExtensionTaskCompletion {
    task_id: u64,
    state: Arc<Mutex<ExtensionTaskState>>,
    completed: Arc<Notify>,
}

impl Drop for ExtensionTaskCompletion {
    fn drop(&mut self) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .tasks
            .remove(&self.task_id);
        self.completed.notify_waiters();
    }
}

impl ExtensionTasks {
    pub(crate) fn new(extension_id: impl Into<String>) -> Self {
        Self::with_lifecycle(extension_id, ExtensionTaskLifecycle::Active)
    }

    pub(crate) fn new_suspended(extension_id: impl Into<String>) -> Self {
        Self::with_lifecycle(extension_id, ExtensionTaskLifecycle::Suspended)
    }

    fn with_lifecycle(extension_id: impl Into<String>, lifecycle: ExtensionTaskLifecycle) -> Self {
        Self {
            extension_id: Arc::from(extension_id.into()),
            shutdown: CancellationToken::new(),
            state: Arc::new(Mutex::new(ExtensionTaskState::default())),
            lifecycle: watch::channel(lifecycle).0,
            completed: Arc::new(Notify::new()),
        }
    }

    /// Returns the shared signal used by extension tasks to observe lifecycle cancellation.
    pub fn cancellation(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    pub(crate) fn activate(&self) {
        self.lifecycle.send_if_modified(|lifecycle| {
            if *lifecycle == ExtensionTaskLifecycle::Suspended {
                *lifecycle = ExtensionTaskLifecycle::Active;
                true
            } else {
                false
            }
        });
    }

    /// Register a background task managed by the extension lifecycle.
    ///
    /// The host may suspend tasks during extension startup until the registration is visible to
    /// other runtime components. `Extension::start()` should not wait for tasks registered here
    /// to complete.
    pub fn spawn<F>(&self, name: impl Into<String>, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let name = name.into();
        let mut state = self.lock_state();
        if matches!(
            *self.lifecycle.borrow(),
            ExtensionTaskLifecycle::Shutdown { .. }
        ) {
            tracing::debug!(
                extension_id = %self.extension_id,
                "skip spawning extension task after shutdown"
            );
            return;
        }

        let task_id = state.next_task_id;
        state.next_task_id = state.next_task_id.wrapping_add(1);
        let mut lifecycle = self.lifecycle.subscribe();
        let extension_id = Arc::clone(&self.extension_id);
        let task_name = name.clone();
        let task_state = Arc::clone(&self.state);
        let completed = Arc::clone(&self.completed);
        let completion = ExtensionTaskCompletion {
            task_id,
            state: task_state,
            completed,
        };
        let handle = tokio::spawn(async move {
            let _completion = completion;
            let run = async move {
                loop {
                    let current_lifecycle = *lifecycle.borrow_and_update();
                    match current_lifecycle {
                        ExtensionTaskLifecycle::Active
                        | ExtensionTaskLifecycle::Shutdown { was_active: true } => {
                            future.await;
                            return;
                        },
                        ExtensionTaskLifecycle::Shutdown { was_active: false } => return,
                        ExtensionTaskLifecycle::Suspended => {
                            if lifecycle.changed().await.is_err() {
                                return;
                            }
                        },
                    }
                }
            };
            if AssertUnwindSafe(run).catch_unwind().await.is_err() {
                tracing::error!(
                    extension_id = %extension_id,
                    task = %task_name,
                    "extension task panicked"
                );
            }
        });
        state.tasks.insert(
            task_id,
            ExtensionTask {
                name,
                kind: ExtensionTaskKind::Background,
                abort_handle: handle.abort_handle(),
            },
        );
    }

    /// Run an uncancellable persistent critical section and wait for its result.
    ///
    /// Unlike [`Self::spawn`], the task starts immediately even while the extension is still
    /// suspended. Cancelling the caller only abandons waiting for the result; the task remains
    /// held by the extension lifecycle and must finish before retirement may proceed.
    pub async fn run_to_completion<F, T>(
        &self,
        name: impl Into<String>,
        future: F,
    ) -> Result<T, ExtensionTaskError>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let name = name.into();
        let extension_id = self.extension_id.to_string();
        let result_rx = {
            let mut state = self.lock_state();
            if matches!(
                *self.lifecycle.borrow(),
                ExtensionTaskLifecycle::Shutdown { .. }
            ) {
                return Err(ExtensionTaskError::ShuttingDown {
                    extension_id,
                    task: name,
                });
            }

            let task_id = state.next_task_id;
            state.next_task_id = state.next_task_id.wrapping_add(1);
            let task_state = Arc::clone(&self.state);
            let completed = Arc::clone(&self.completed);
            let completion = ExtensionTaskCompletion {
                task_id,
                state: task_state,
                completed,
            };
            let (result_tx, result_rx) = oneshot::channel();
            let panic_extension_id = extension_id.clone();
            let panic_task_name = name.clone();
            let handle = tokio::spawn(async move {
                let result = match AssertUnwindSafe(future).catch_unwind().await {
                    Ok(value) => Ok(value),
                    Err(_) => {
                        tracing::error!(
                            extension_id = %panic_extension_id,
                            task = %panic_task_name,
                            "extension must-finish task panicked"
                        );
                        Err(ExtensionTaskError::Panicked {
                            extension_id: panic_extension_id,
                            task: panic_task_name,
                        })
                    },
                };
                drop(completion);
                let _ = result_tx.send(result);
            });
            state.tasks.insert(
                task_id,
                ExtensionTask {
                    name: name.clone(),
                    kind: ExtensionTaskKind::MustFinish,
                    abort_handle: handle.abort_handle(),
                },
            );
            result_rx
        };

        result_rx
            .await
            .map_err(|_| ExtensionTaskError::RuntimeStopped {
                extension_id,
                task: name,
            })?
    }

    pub(crate) fn cancel(&self) {
        // Keep the state-first lock order used by `spawn` so shutdown and admission linearize.
        let _state_guard = self.lock_state();
        self.shutdown.cancel();
        self.lifecycle.send_if_modified(|lifecycle| {
            let shutdown = match lifecycle {
                ExtensionTaskLifecycle::Suspended => {
                    ExtensionTaskLifecycle::Shutdown { was_active: false }
                },
                ExtensionTaskLifecycle::Active => {
                    ExtensionTaskLifecycle::Shutdown { was_active: true }
                },
                ExtensionTaskLifecycle::Shutdown { .. } => return false,
            };
            *lifecycle = shutdown;
            true
        });
    }

    /// Wait for all managed tasks to exit; after the timeout, only ordinary background tasks
    /// are aborted, with a brief wait for reaping.
    ///
    /// Must-finish tasks keep waiting after the shared budget is exhausted; ordinary background
    /// tasks only use the remaining budget of the same absolute deadline. Callers should call
    /// [`Self::cancel`] first so no new tasks can be registered while waiting.
    /// Returns `true` only when the task set is actually empty; `false` means background tasks
    /// remain unreaped after aborting.
    pub(crate) async fn wait(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        if !self
            .wait_until_no_tasks(Some(ExtensionTaskKind::MustFinish), Some(deadline))
            .await
        {
            let remaining = self.remaining_tasks(ExtensionTaskKind::MustFinish);
            for (name, _) in remaining {
                tracing::warn!(
                    extension_id = %self.extension_id,
                    task = %name,
                    "extension must-finish task overran the shutdown budget; continuing to wait"
                );
            }
            self.wait_until_no_tasks(Some(ExtensionTaskKind::MustFinish), None)
                .await;
        }

        if self.wait_until_no_tasks(None, Some(deadline)).await {
            return true;
        }

        for (name, abort_handle) in self.remaining_tasks(ExtensionTaskKind::Background) {
            tracing::warn!(
                extension_id = %self.extension_id,
                task = %name,
                "extension task did not stop before shared timeout; aborting"
            );
            abort_handle.abort();
        }

        self.wait_until_no_tasks(
            None,
            Some(tokio::time::Instant::now() + Duration::from_millis(100)),
        )
        .await
    }

    async fn wait_until_no_tasks(
        &self,
        kind: Option<ExtensionTaskKind>,
        deadline: Option<tokio::time::Instant>,
    ) -> bool {
        loop {
            let notified = self.completed.notified();
            if !self.has_tasks(kind) {
                return true;
            }
            let Some(deadline) = deadline else {
                notified.await;
                continue;
            };
            if tokio::time::Instant::now() >= deadline {
                return !self.has_tasks(kind);
            }
            let _ = tokio::time::timeout_at(deadline, notified).await;
        }
    }

    fn has_tasks(&self, kind: Option<ExtensionTaskKind>) -> bool {
        self.lock_state()
            .tasks
            .values()
            .any(|task| kind.is_none_or(|kind| task.kind == kind))
    }

    fn remaining_tasks(&self, kind: ExtensionTaskKind) -> Vec<(String, AbortHandle)> {
        self.lock_state()
            .tasks
            .values()
            .filter(|task| task.kind == kind)
            .map(|task| (task.name.clone(), task.abort_handle.clone()))
            .collect()
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ExtensionTaskState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use serde::Deserialize;
    use tokio::sync::oneshot;

    use super::*;

    #[test]
    fn extension_config_defaults_only_when_empty_and_reports_attributed_field_paths() {
        #[derive(Debug, Default, Deserialize, PartialEq, Eq)]
        struct Config {
            #[serde(default)]
            enabled: bool,
            #[serde(default)]
            nested: Nested,
        }

        #[derive(Debug, Default, Deserialize, PartialEq, Eq)]
        struct Nested {
            #[serde(default)]
            count: u64,
        }

        let empty = ExtensionConfig::from_runtime("config-probe", serde_json::Value::Null);
        assert!(empty.is_empty());
        assert_eq!(
            empty.deserialize_or_default::<Config>().unwrap(),
            Config::default()
        );

        let invalid = ExtensionConfig::from_runtime(
            "config-probe",
            serde_json::json!({ "nested": { "count": "many" } }),
        );
        let error = invalid.deserialize::<Config>().unwrap_err();
        assert_eq!(error.extension_id(), "config-probe");
        assert_eq!(error.path(), "nested.count");
    }

    #[tokio::test]
    async fn spawn_after_cancel_is_skipped() {
        let tasks = ExtensionTasks::new("ext");
        tasks.cancel();
        tasks.spawn("late", async {});
        assert!(
            tasks.lock_state().tasks.is_empty(),
            "no task should be recorded after shutdown"
        );
    }

    #[tokio::test]
    async fn suspended_tasks_start_only_after_activation() {
        let tasks = ExtensionTasks::new_suspended("ext");
        let started = Arc::new(AtomicBool::new(false));
        let task_started = Arc::clone(&started);
        tasks.spawn("deferred", async move {
            task_started.store(true, Ordering::SeqCst);
        });

        tokio::task::yield_now().await;
        assert!(!started.load(Ordering::SeqCst));

        tasks.activate();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tasks.cancel();
        assert!(tasks.wait(Duration::from_secs(1)).await);
    }

    #[tokio::test]
    async fn wait_completes_when_task_observes_cancellation() {
        let tasks = ExtensionTasks::new("ext");
        let cancellation = tasks.cancellation();
        let finished = Arc::new(AtomicBool::new(false));
        let finished_clone = finished.clone();
        tasks.spawn("cooperative", async move {
            loop {
                tokio::select! {
                    _ = cancellation.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {}
                }
            }
            finished_clone.store(true, Ordering::SeqCst);
        });
        tasks.cancel();
        assert!(tasks.wait(Duration::from_secs(1)).await);
        assert!(
            finished.load(Ordering::SeqCst),
            "cooperative task should run to completion before the timeout"
        );
    }

    #[tokio::test]
    async fn must_finish_work_outlives_its_caller_and_shutdown_budget() {
        let tasks = ExtensionTasks::new("ext");
        let panic_error = tasks
            .run_to_completion("panic", async { panic!("critical write exploded") })
            .await
            .unwrap_err();
        assert_eq!(
            panic_error,
            ExtensionTaskError::Panicked {
                extension_id: "ext".into(),
                task: "panic".into(),
            }
        );

        tasks.spawn("stalled-background", std::future::pending());

        let finished = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let tracked_tasks = tasks.clone();
        let tracked_finished = Arc::clone(&finished);
        let caller = tokio::spawn(async move {
            tracked_tasks
                .run_to_completion("write-transaction", async move {
                    let _ = started_tx.send(());
                    let _ = release_rx.await;
                    tracked_finished.store(true, Ordering::SeqCst);
                })
                .await
        });
        started_rx.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());

        tasks.cancel();
        let late_work_ran = Arc::new(AtomicBool::new(false));
        let late_work_ran_in_task = Arc::clone(&late_work_ran);
        assert!(matches!(
            tasks
                .run_to_completion("late-write", async move {
                    late_work_ran_in_task.store(true, Ordering::SeqCst);
                })
                .await,
            Err(ExtensionTaskError::ShuttingDown { .. })
        ));
        assert!(!late_work_ran.load(Ordering::SeqCst));

        let draining_tasks = tasks.clone();
        let drain =
            tokio::spawn(async move { draining_tasks.wait(Duration::from_millis(20)).await });
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            !drain.is_finished(),
            "must-finish work must not be aborted when the budget expires"
        );
        assert!(!finished.load(Ordering::SeqCst));

        release_tx.send(()).unwrap();
        assert!(drain.await.unwrap());
        assert!(finished.load(Ordering::SeqCst));
        assert!(tasks.lock_state().tasks.is_empty());
    }

    #[tokio::test]
    async fn completed_tasks_are_reaped_before_shutdown() {
        let tasks = ExtensionTasks::new("ext");
        for _ in 0..64 {
            tasks.spawn("short", async {});
        }

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if tasks.lock_state().tasks.is_empty() {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn wait_aborts_task_that_ignores_shutdown() {
        let tasks = ExtensionTasks::new("ext");
        tasks.spawn("stuck", async move {
            // Never observes the shared shutdown token; only an abort stops it.
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        tasks.cancel();
        let start = tokio::time::Instant::now();
        assert!(tasks.wait(Duration::from_millis(50)).await);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "wait should abort the stuck task instead of blocking; elapsed={:?}",
            start.elapsed()
        );
        assert!(tasks.lock_state().tasks.is_empty());
    }

    #[tokio::test]
    async fn immediate_abort_reaps_a_task_before_its_first_poll() {
        let tasks = ExtensionTasks::new("ext");
        tasks.spawn("unpolled", std::future::pending());
        tasks.cancel();
        assert!(tasks.wait(Duration::ZERO).await);

        assert!(tasks.lock_state().tasks.is_empty());
    }

    #[tokio::test]
    async fn wait_absorbs_panicking_task() {
        // Note: the panic from the abort still reaches stderr through the default panic hook;
        // that is expected noise. This test only verifies that `wait` does not propagate the
        // panic to callers.
        let tasks = ExtensionTasks::new("ext");
        tasks.spawn("boom", async {
            panic!("extension task exploded");
        });
        tasks.cancel();
        assert!(tasks.wait(Duration::from_secs(1)).await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_reports_task_that_cannot_be_reaped_after_abort() {
        let tasks = ExtensionTasks::new("ext");
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        tasks.spawn("blocking", async move {
            tokio::task::block_in_place(|| {
                let _ = entered_tx.send(());
                let _ = release_rx.recv();
            });
        });
        entered_rx.await.unwrap();

        tasks.cancel();
        assert!(!tasks.wait(Duration::ZERO).await);

        release_tx.send(()).unwrap();
        assert!(tasks.wait(Duration::from_secs(1)).await);
    }

    #[tokio::test]
    async fn operations_recover_from_poisoned_state() {
        let tasks = ExtensionTasks::new("ext");
        // Manually poison the internal mutex: an OS thread panics while holding the lock.
        let state = tasks.state.clone();
        let join = std::thread::spawn(move || {
            let _guard = state.lock().unwrap();
            panic!("poison the mutex");
        });
        assert!(join.join().is_err(), "helper thread should have panicked");

        // Even though it is poisoned, all operations going through lock_state() should recover
        // rather than panic.
        tasks.spawn("after-poison", std::future::pending());
        assert_eq!(tasks.lock_state().tasks.len(), 1);
        tasks.cancel();
        assert!(tasks.wait(Duration::from_millis(10)).await);
        assert!(matches!(
            *tasks.lifecycle.borrow(),
            ExtensionTaskLifecycle::Shutdown { .. }
        ));
    }
}
