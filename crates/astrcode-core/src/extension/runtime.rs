use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{ExtensionError, ExtensionEventSink, Registrar};
use crate::{
    llm::LlmProvider,
    storage::{EventReader, EventStore},
    tool::SessionOperations,
};

// ─── Extension Trait ─────────────────────────────────────────────────────

/// 扩展 trait，定义了挂入 astrcode 生命周期的核心接口。
///
/// 扩展从 `~/.astrcode/extensions/`（全局）和 `.astrcode/extensions/`（项目级）加载。
/// 它们可以订阅生命周期事件、注册工具、斜杠命令和上下文提供者。
#[async_trait::async_trait]
pub trait Extension: Send + Sync {
    /// 返回扩展的唯一标识符。
    fn id(&self) -> &str;

    /// 声明扩展需要宿主授予的能力。
    ///
    /// 宿主以此限制注入到扩展工具和生命周期上下文中的敏感能力。
    fn capabilities(&self) -> &[ExtensionCapability] {
        &[]
    }

    /// 一次性调用。扩展通过 registrar 注册工具、命令和事件处理器。
    fn register(&self, _reg: &mut Registrar) {}

    /// 扩展进入运行态。默认 no-op。
    ///
    /// 在此处通过 `ctx.config.deserialize::<T>()` 读取用户配置。
    async fn start(&self, _ctx: ExtensionCtx) -> Result<(), ExtensionError> {
        Ok(())
    }

    /// 扩展退出运行态。默认 no-op。
    ///
    /// [`StopReason::StartupFailed`] 可能在 `start` 只完成部分初始化时调用；
    /// 实现必须容忍资源尚未创建，并保持清理幂等。
    async fn stop(&self, _reason: StopReason) -> Result<(), ExtensionError> {
        Ok(())
    }

    /// 检查扩展当前是否可用。宿主可周期性调用用于健康观测。
    ///
    /// 默认认为不持有外部运行态资源的扩展始终健康。
    async fn health(&self) -> Result<(), ExtensionError> {
        Ok(())
    }

    /// 扩展配置发生热更新时调用。
    ///
    /// 当用户修改 `config.toml` 中的 `extensions.<id>` 并触发重载时，
    /// 运行器会调用此方法通知扩展更新内部状态。
    /// 默认 no-op（兼容不支持热更新的扩展）。
    async fn on_config_changed(&self, _config: ExtensionConfig) -> Result<(), ExtensionError> {
        Ok(())
    }
}

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

/// 插件运行态上下文。
#[derive(Clone)]
pub struct ExtensionCtx {
    tasks: ExtensionTasks,
    /// 扩展专有配置。用户配置文件中 `extensions.<id>` 对应的 JSON 值。
    /// 若用户未配置该扩展，则为空对象 `{}`。
    pub config: ExtensionConfig,
    /// 宿主启动时绑定的工作目录；不绑定工作区的宿主可为 `None`。
    startup_working_dir: Option<String>,
    /// 启动阶段可用的扩展事件发送端；由宿主显式绑定。
    event_sink: Option<Arc<dyn ExtensionEventSink>>,
    /// 由宿主统一绑定的受信运行态服务。
    ///
    /// 扩展只能在标准启动生命周期内取得这些能力，组合根不得为单个扩展
    /// 另开构造参数注入路径。
    host_services: Option<Arc<ExtensionHostServices>>,
}

impl ExtensionCtx {
    pub fn new(tasks: ExtensionTasks) -> Self {
        Self {
            tasks,
            config: ExtensionConfig::default(),
            startup_working_dir: None,
            event_sink: None,
            host_services: None,
        }
    }

    pub fn with_config(tasks: ExtensionTasks, config: ExtensionConfig) -> Self {
        Self::with_startup_working_dir(tasks, config, None)
    }

    pub fn with_startup_working_dir(
        tasks: ExtensionTasks,
        config: ExtensionConfig,
        startup_working_dir: Option<String>,
    ) -> Self {
        Self::with_startup_services(tasks, config, startup_working_dir, None)
    }

    pub fn with_startup_services(
        tasks: ExtensionTasks,
        config: ExtensionConfig,
        startup_working_dir: Option<String>,
        event_sink: Option<Arc<dyn ExtensionEventSink>>,
    ) -> Self {
        Self::with_host_services(tasks, config, startup_working_dir, event_sink, None)
    }

    pub fn with_host_services(
        tasks: ExtensionTasks,
        config: ExtensionConfig,
        startup_working_dir: Option<String>,
        event_sink: Option<Arc<dyn ExtensionEventSink>>,
        host_services: Option<Arc<ExtensionHostServices>>,
    ) -> Self {
        Self {
            tasks,
            config,
            startup_working_dir,
            event_sink,
            host_services,
        }
    }

    pub fn tasks(&self) -> &ExtensionTasks {
        &self.tasks
    }

    /// 启动时宿主已知的工作目录，供扩展预加载该项目的资源。
    pub fn startup_working_dir(&self) -> Option<&str> {
        self.startup_working_dir.as_deref()
    }

    /// 返回启动阶段由宿主绑定的扩展事件发送端。
    pub fn event_sink(&self) -> Option<&Arc<dyn ExtensionEventSink>> {
        self.event_sink.as_ref()
    }

    /// 返回宿主授予扩展的运行态服务。
    pub fn host_services(&self) -> Option<&Arc<ExtensionHostServices>> {
        self.host_services.as_ref()
    }

    pub fn shutdown(&self) -> CancellationToken {
        self.tasks.shutdown()
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
}

#[derive(Default)]
struct ExtensionTaskState {
    shutdown: bool,
    tasks: Vec<ExtensionTask>,
}

struct ExtensionTask {
    name: String,
    handle: JoinHandle<()>,
}

impl ExtensionTasks {
    pub fn new(extension_id: impl Into<String>) -> Self {
        Self {
            extension_id: Arc::from(extension_id.into()),
            shutdown: CancellationToken::new(),
            state: Arc::new(Mutex::new(ExtensionTaskState::default())),
        }
    }

    pub fn shutdown(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    pub fn spawn<F>(&self, name: impl Into<String>, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut state = self.lock_state();
        if state.shutdown {
            tracing::debug!(
                extension_id = %self.extension_id,
                "skip spawning extension task after shutdown"
            );
            return;
        }

        let name = name.into();
        let handle = tokio::spawn(fut);
        state.tasks.push(ExtensionTask { name, handle });
    }

    pub fn cancel(&self) {
        let mut state = self.lock_state();
        state.shutdown = true;
        self.shutdown.cancel();
    }

    pub async fn wait(&self, timeout: Duration) {
        let tasks = std::mem::take(&mut self.lock_state().tasks);

        let deadline = tokio::time::Instant::now() + timeout;
        for task in tasks {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                self.abort_one(task).await;
            } else {
                self.wait_one(task, deadline - now).await;
            }
        }
    }

    async fn wait_one(&self, task: ExtensionTask, timeout: Duration) {
        let ExtensionTask { name, mut handle } = task;
        match tokio::time::timeout(timeout, &mut handle).await {
            Ok(Ok(())) => {},
            Ok(Err(join_err)) if join_err.is_cancelled() => {
                tracing::debug!(
                    extension_id = %self.extension_id,
                    task = %name,
                    "extension task cancelled"
                );
            },
            Ok(Err(join_err)) if join_err.is_panic() => {
                tracing::error!(
                    extension_id = %self.extension_id,
                    task = %name,
                    "extension task panicked"
                );
            },
            Ok(Err(join_err)) => {
                tracing::warn!(
                    extension_id = %self.extension_id,
                    task = %name,
                    error = %join_err,
                    "extension task failed"
                );
            },
            Err(_) => {
                tracing::warn!(
                    extension_id = %self.extension_id,
                    task = %name,
                    "extension task did not stop before timeout; aborting"
                );
                handle.abort();
                let _ = tokio::time::timeout(Duration::from_millis(100), handle).await;
            },
        }
    }

    async fn abort_one(&self, task: ExtensionTask) {
        let ExtensionTask { name, handle } = task;
        tracing::warn!(
            extension_id = %self.extension_id,
            task = %name,
            "extension task did not stop before shared timeout; aborting"
        );
        handle.abort();
        let _ = tokio::time::timeout(Duration::from_millis(100), handle).await;
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

/// 扩展运行时可用的宿主服务。
///
/// 只注入给 trusted bundled extension，不暴露给 untrusted source（磁盘 IPC 扩展）。
pub struct ExtensionHostServices {
    /// 可信内置扩展可用的只读会话投影数据源。
    ///
    /// 由 `Arc<dyn EventStore>` 通过 trait upcasting 转换而来
    /// （Rust 1.86+，`EventStore: EventReader` 建立 supertrait 关系）。
    pub session_read: Option<Arc<dyn EventReader>>,
    /// 主模型 provider（当前 session active model）。
    pub main_llm: Option<Arc<dyn LlmProvider>>,
    /// 小模型 provider（`activeSmallModel`；未配置时与主模型相同）。
    pub small_llm: Option<Arc<dyn LlmProvider>>,
    /// 会话原子操作能力。
    ///
    /// 只注入给声明了 [`ExtensionCapability::SessionControl`] 的 trusted bundled
    /// extension。磁盘 IPC 扩展仍通过 HostRouter 的能力门控访问子集。
    pub session_ops: Option<Arc<dyn SessionOperations>>,
    /// 统一的受限出站网络服务。
    pub outbound_network: Option<Arc<dyn OutboundNetworkService>>,
}

impl ExtensionHostServices {
    pub fn new(
        event_store: Arc<dyn EventStore>,
        main_llm: Option<Arc<dyn LlmProvider>>,
        small_llm: Option<Arc<dyn LlmProvider>>,
    ) -> Self {
        Self {
            // Arc<dyn EventStore> → Arc<dyn EventReader> 由 trait upcasting 自动完成。
            session_read: Some(event_store),
            main_llm,
            small_llm,
            session_ops: None,
            outbound_network: None,
        }
    }

    pub fn with_session_ops(mut self, session_ops: Arc<dyn SessionOperations>) -> Self {
        self.session_ops = Some(session_ops);
        self
    }

    pub fn with_outbound_network(
        mut self,
        outbound_network: Arc<dyn OutboundNetworkService>,
    ) -> Self {
        self.outbound_network = Some(outbound_network);
        self
    }

    /// 按扩展已声明的能力裁剪 trusted host services。
    pub fn scoped_to(&self, capabilities: &[ExtensionCapability]) -> Option<Self> {
        let session_read = capabilities
            .iter()
            .any(|capability| {
                matches!(
                    capability,
                    ExtensionCapability::SessionHistory | ExtensionCapability::SessionInspect
                )
            })
            .then(|| self.session_read.clone())
            .flatten();
        let scoped = Self {
            session_read,
            main_llm: capabilities
                .contains(&ExtensionCapability::MainModel)
                .then(|| self.main_llm.clone())
                .flatten(),
            small_llm: capabilities
                .contains(&ExtensionCapability::SmallModel)
                .then(|| self.small_llm.clone())
                .flatten(),
            session_ops: capabilities
                .contains(&ExtensionCapability::SessionControl)
                .then(|| self.session_ops.clone())
                .flatten(),
            outbound_network: capabilities
                .contains(&ExtensionCapability::NetworkClient)
                .then(|| self.outbound_network.clone())
                .flatten(),
        };
        (scoped.session_read.is_some()
            || scoped.main_llm.is_some()
            || scoped.small_llm.is_some()
            || scoped.session_ops.is_some()
            || scoped.outbound_network.is_some())
        .then_some(scoped)
    }
}
