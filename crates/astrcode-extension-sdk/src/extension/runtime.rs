use std::{
    collections::HashMap,
    future::Future,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::FutureExt;
use serde::{Deserialize, Serialize, de::IntoDeserializer};
use tokio::{
    sync::{Notify, oneshot, watch},
    task::AbortHandle,
};
use tokio_util::sync::CancellationToken;

/// 扩展可以显式申请的宿主能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionCapability {
    /// 创建子 session、提交 turn 与回收 session。
    SessionControl,
    /// 宿主级全局读取授权：跨会话读取宿主可见的 session 投影。
    ///
    /// 此能力不受当前 session lineage 限制，只应授予需要全局观察或后台接续会话的扩展。
    SessionInspect,
    /// 注册无需宿主 bearer token 的公开 HTTP 路由。
    PublicHttp,
    /// 注册复用宿主 bearer token 的扩展 HTTP 路由。
    AuthenticatedHttp,
    /// 从插件内部调用其他插件的公开 HTTP 路由。
    PublicHttpDispatch,
    /// 调用宿主配置的主模型（当前 session 的 active model）。
    MainModel,
    /// 调用宿主配置的小模型。
    SmallModel,
    /// 只读查询历史 session 投影。
    SessionHistory,
    /// 发射已声明的扩展事件。
    EmitEvents,
    /// 消费其他扩展发射的事件。
    ConsumeEvents,
    /// 读取工作区或扩展发现目录。
    WorkspaceRead,
    /// 写入或编辑工作区内的非敏感文件。
    WorkspaceWrite,
    /// 启动受扩展管理的子进程。
    ProcessSpawn,
    /// 发起网络客户端请求。
    NetworkClient,
    /// 读取或改写 provider 请求边界。
    ProviderRequest,
    /// 决定外部输入的投递策略。
    InputDelivery,
    /// 阻断或改写工具执行。
    ToolIntercept,
    /// 决定工具结果或自然停止后 turn 是否继续。
    TurnContinuationControl,
    /// 观察临时的实时会话增量。
    LiveConversation,
}

/// 扩展专有配置的包装类型。
///
/// 包装用户 `config.toml` 中 `extensions.<id>` 下的扩展配置，
/// 扩展在 `start()` 或 `on_config_changed()` 时通过 `deserialize::<T>()` 获取。
#[derive(Clone, Debug)]
pub struct ExtensionConfig {
    extension_id: Arc<str>,
    value: serde_json::Value,
}

impl Default for ExtensionConfig {
    fn default() -> Self {
        Self {
            extension_id: Arc::from("unknown-extension"),
            value: serde_json::Value::Null,
        }
    }
}

impl ExtensionConfig {
    #[doc(hidden)]
    pub fn from_runtime(extension_id: impl Into<String>, value: serde_json::Value) -> Self {
        Self {
            extension_id: Arc::from(extension_id.into()),
            value,
        }
    }

    /// 将配置反序列化为具体类型。
    ///
    /// # 示例
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

    /// 如果配置为 `null` 或空对象 `{}` 则返回 `true`。
    pub fn is_empty(&self) -> bool {
        self.value.is_null()
            || self
                .value
                .as_object()
                .is_some_and(|object| object.is_empty())
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

/// 插件退出原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// 同一个扩展 id 被重新加载的新实例替换。
    Reload,
    /// 配置关闭或 source 不再提供该扩展。
    Disabled,
    /// 宿主进程关闭。
    Shutdown,
    /// `start` 失败或超时，回滚已经取得的资源。
    StartupFailed,
}

/// 宿主管理的插件后台任务集合。
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

/// `run_to_completion` 无法启动或取得结果时的结构化错误。
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
    pub fn new(extension_id: impl Into<String>) -> Self {
        Self::with_lifecycle(extension_id, ExtensionTaskLifecycle::Active)
    }

    #[doc(hidden)]
    pub fn new_suspended(extension_id: impl Into<String>) -> Self {
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

    #[doc(hidden)]
    pub fn activate(&self) {
        self.lifecycle.send_if_modified(|lifecycle| {
            if *lifecycle == ExtensionTaskLifecycle::Suspended {
                *lifecycle = ExtensionTaskLifecycle::Active;
                true
            } else {
                false
            }
        });
    }

    /// 登记由扩展生命周期托管的后台任务。
    ///
    /// 宿主可在扩展启动阶段暂停任务，直到 registration 对其他运行时组件可见。
    /// `Extension::start()` 不应等待这里登记的任务完成。
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

    /// 运行不可取消的持久化临界区，并等待其结果。
    ///
    /// 与 [`Self::spawn`] 不同，任务即使在扩展仍处于 suspended 状态也会立即开始。调用方被取消
    /// 只会放弃等待结果；任务仍由扩展生命周期持有，并在 retirement 时完成后才允许继续。
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

    pub fn cancel(&self) {
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

    /// 等待所有托管任务退出，超时后只中止普通后台任务，并短暂等待回收。
    ///
    /// must-finish 任务会在超过共享预算后继续等待；普通后台任务只使用同一个绝对截止时间的
    /// 剩余预算。调用方应先调用 [`Self::cancel`]，使等待期间不能再登记新任务。
    /// 仅在任务集合确实清空时返回 `true`；`false` 表示中止后仍有后台任务未回收。
    pub async fn wait(&self, timeout: Duration) -> bool {
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
            // 永不观察共享 shutdown token,只有被 abort 才会停止。
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
        // 注意:被 abort 的 panic 仍会经默认 panic hook 打到 stderr,这是预期噪声,
        // 此处只验证 wait 不会把 panic 传播给调用方。
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
        // 手动毒化内部 mutex:一个 OS 线程持锁后 panic。
        let state = tasks.state.clone();
        let join = std::thread::spawn(move || {
            let _guard = state.lock().unwrap();
            panic!("poison the mutex");
        });
        assert!(join.join().is_err(), "helper thread should have panicked");

        // 尽管已毒化,所有经由 lock_state() 的操作都应恢复,而非 panic。
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
