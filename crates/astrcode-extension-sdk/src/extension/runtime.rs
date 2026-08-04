use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{Notify, watch},
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
#[derive(Clone, Debug, Default)]
pub struct ExtensionConfig(pub serde_json::Value);

impl ExtensionConfig {
    /// 将配置反序列化为具体类型。
    ///
    /// # 示例
    ///
    /// ```ignore
    /// #[derive(Deserialize)]
    /// struct MyConfig { timeout: u64, retry: bool }
    /// let cfg: MyConfig = ctx.config.deserialize()?;
    /// ```
    pub fn deserialize<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_value(self.0.clone())
    }

    /// 如果配置为空对象 `{}` 则返回 `true`。
    pub fn is_empty(&self) -> bool {
        self.0.as_object().is_some_and(|o| o.is_empty())
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

struct ExtensionTask {
    name: String,
    abort_handle: AbortHandle,
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

    pub fn shutdown(&self) -> CancellationToken {
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
                abort_handle: handle.abort_handle(),
            },
        );
    }

    pub fn cancel(&self) {
        let _state = self.lock_state();
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

    /// 等待所有托管任务退出，超时后请求中止并短暂等待回收。
    ///
    /// 仅在任务集合确实清空时返回 `true`；`false` 表示中止后仍有任务未回收。
    pub async fn wait(&self, timeout: Duration) -> bool {
        if self
            .wait_until_empty(tokio::time::Instant::now() + timeout)
            .await
        {
            return true;
        }

        let remaining = self
            .lock_state()
            .tasks
            .values()
            .map(|task| (task.name.clone(), task.abort_handle.clone()))
            .collect::<Vec<_>>();
        for (name, abort_handle) in remaining {
            tracing::warn!(
                extension_id = %self.extension_id,
                task = %name,
                "extension task did not stop before shared timeout; aborting"
            );
            abort_handle.abort();
        }

        self.wait_until_empty(tokio::time::Instant::now() + Duration::from_millis(100))
            .await
    }

    async fn wait_until_empty(&self, deadline: tokio::time::Instant) -> bool {
        loop {
            let notified = self.completed.notified();
            if self.lock_state().tasks.is_empty() {
                return true;
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return self.lock_state().tasks.is_empty();
            }
            let _ = tokio::time::timeout(deadline - now, notified).await;
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ExtensionTaskState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
}

// ─── Host Services ──────────────────────────────────────────────────────

/// 宿主出站网络请求的跳转处理方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkRedirectPolicy {
    /// 由统一网络服务在每次跳转前重新执行目标地址校验。
    Follow,
    /// 返回 3xx 响应，由调用方实现产品层的跳转规则。
    Manual,
}

/// 可信内置扩展调用宿主出站网络服务时使用的请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundNetworkRequest {
    pub url: String,
    pub method: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub max_bytes: usize,
    pub timeout: Duration,
    pub redirect_policy: NetworkRedirectPolicy,
}

/// 宿主出站网络服务的响应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundNetworkResponse {
    pub final_url: String,
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

/// 宿主出站网络服务的稳定错误分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundNetworkErrorKind {
    InvalidRequest,
    PermissionDenied,
    Unavailable,
    RequestFailed,
    Timeout,
    ResponseTooLarge,
    Cancelled,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct OutboundNetworkError {
    pub kind: OutboundNetworkErrorKind,
    pub message: String,
}

impl OutboundNetworkError {
    pub fn new(kind: OutboundNetworkErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// 宿主唯一的受限出站网络执行边界。
#[async_trait::async_trait]
pub trait OutboundNetworkService: Send + Sync {
    async fn request(
        &self,
        request: OutboundNetworkRequest,
        cancellation: Option<CancellationToken>,
    ) -> Result<OutboundNetworkResponse, OutboundNetworkError>;
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use tokio::sync::oneshot;

    use super::*;

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
    async fn wait_completes_when_task_observes_shutdown() {
        let tasks = ExtensionTasks::new("ext");
        let shutdown = tasks.shutdown();
        let finished = Arc::new(AtomicBool::new(false));
        let finished_clone = finished.clone();
        tasks.spawn("cooperative", async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
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
