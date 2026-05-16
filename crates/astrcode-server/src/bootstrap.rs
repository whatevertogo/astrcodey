//! 服务器引导模块 — 从配置组装所有服务。
//!
//! 负责在启动时初始化所有核心组件：LLM 提供者、提示词组装器、
//! 会话管理器、扩展运行器和上下文窗口设置。

use std::{sync::Arc, time::Duration};

use astrcode_context::context_assembler::LlmContextAssembler;
use astrcode_core::{config::ConfigStore, storage::EventStore, tool::AgentSessionControl};
use astrcode_extensions::{loader::ExtensionLoader, runner::ExtensionRunner};
use astrcode_session::{SessionRuntimeRegistry, background::BackgroundTaskManager};
use astrcode_storage::config_store::FileConfigStore;
use parking_lot::{Mutex, RwLock};

pub use crate::config_manager::ConfigManager;
use crate::{session_manager::SessionManager, session_spawner::ServerSessionSpawner};

// ─── AgentSessionControl 延迟注入槽 ────────────────────────────────────

/// `AgentSessionControl` 的延迟注入容器。
///
/// Bootstrap 时创建空槽位，spawn_actor 后注入 `CommandHandle` 实现。
/// 消费者通过 `.read().clone()` 获取 `Option<Arc<...>>`。
pub(crate) type AgentSessionControlSlot = Arc<RwLock<Option<Arc<dyn AgentSessionControl>>>>;

// ─── ServerRuntime ───────────────────────────────────────────────────────

/// 启动时组装的所有服务集合，按领域分组。
///
/// 这是服务器运行时的核心容器，持有所有共享服务的引用。
/// 各组件通过 `Arc` 共享，支持并发访问。
pub struct ServerRuntime {
    /// 事件存储后端，用于创建/恢复/删除会话等集合操作
    pub event_store: Arc<dyn EventStore>,
    /// 配置与 LLM 提供者的联合管理器
    pub config: Arc<crate::config_manager::ConfigManager>,
    /// 上下文组装器，负责窗口估算和摘要压缩
    pub context_assembler: Arc<LlmContextAssembler>,
    /// 跨回合共享的后台任务管理器。
    pub background_tasks: Arc<Mutex<BackgroundTaskManager>>,
    /// server 侧的 session 生命周期门面。
    pub session_manager: Arc<SessionManager>,
    /// 扩展运行器，负责加载和分发扩展钩子事件
    pub extension_runner: Arc<ExtensionRunner>,
    /// 触发后通知 HTTP server 执行 graceful shutdown
    pub shutdown_token: tokio_util::sync::CancellationToken,
    /// AgentSessionControl 延迟注入槽。
    ///
    /// bootstrap 时为空；spawn_actor 后绑定 `CommandHandle` 实现。
    /// 通过 `RwLock` 允许所有持有 `Arc` 的消费者读取当前值。
    pub agent_session_control: AgentSessionControlSlot,
}

// ─── Bootstrap ───────────────────────────────────────────────────────────

/// 引导选项，支持自定义配置路径和工作目录，主要用于测试。
#[derive(Default)]
pub struct BootstrapOptions {
    /// 自定义配置文件路径，为 None 时使用默认路径
    pub config_path: Option<std::path::PathBuf>,
    /// 自定义工作目录，为 None 时使用当前目录
    pub working_dir: Option<std::path::PathBuf>,
}

/// 使用默认选项引导服务器运行时。
pub async fn bootstrap() -> Result<ServerRuntime, BootstrapError> {
    bootstrap_with(BootstrapOptions::default()).await
}

/// 使用指定选项引导服务器运行时。
///
/// 这个函数只负责“把长期共享服务装起来”，不会为某个会话创建工具表。
/// 工具表现在是 session 级快照，由 `SessionManager` 在创建/恢复 session 时
/// 按对应 working_dir 单独构建。
///
/// 启动顺序：
/// 1. 加载并解析配置
/// 2. 构建 LLM 提供者
/// 3. 构建提示词组装器
/// 4. 确定启动工作目录
/// 5. 初始化会话管理器和存储后端
/// 6. 加载扩展并创建扩展运行器
/// 7. 绑定扩展创建子会话的宿主能力
/// 8. 返回共享运行时容器
pub async fn bootstrap_with(opts: BootstrapOptions) -> Result<ServerRuntime, BootstrapError> {
    // 1. 读取配置并解析成 EffectiveConfig。
    //
    // `config_path` 只在测试或嵌入式启动时传入；正常运行使用默认配置路径。
    // `into_effective()` 会把默认值、用户配置和环境变量等合并成最终只读配置。
    let config_store = if let Some(ref path) = opts.config_path {
        FileConfigStore::new(path.clone())
    } else {
        FileConfigStore::default_path()
    };
    let config = config_store.load().await?;
    let effective = config.clone().into_effective()?;

    // 2. 构建配置管理器及其初始 LLM provider。
    //
    // 根据 `provider_kind` 路由到对应的 provider 实现。
    // 后续所有主会话和子会话都会共享这个 provider。
    let config_manager = Arc::new(crate::config_manager::ConfigManager::from_loaded_config(
        Arc::new(config_store),
        config,
        effective.clone(),
    ));

    // 3. 初始化上下文组装器。
    let context_settings = effective.context.clone();
    let context_assembler = Arc::new(LlmContextAssembler::new(context_settings));
    let background_tasks = Arc::new(Mutex::new(BackgroundTaskManager::default()));
    let session_runtime_registry = Arc::new(SessionRuntimeRegistry::default());

    // 4. 确定当前项目工作目录。
    //
    // 这个目录只用于启动期项目识别、扩展加载和默认隐式会话。
    // 显式创建 session 时，工具快照会使用 session 自己的 working_dir。
    let cwd = opts
        .working_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    // 5. 构建 session manager 和事件存储。
    //
    // 测试启动（config_path.is_some()）使用内存存储，避免污染真实会话目录；
    // 正常启动按项目路径选择文件系统会话仓库。
    //
    // InMemoryEventStore 仅在 `testing` feature 启用时可用；生产二进制始终使用
    // FileSystemSessionRepository，与 opts.config_path 无关。
    #[cfg(feature = "testing")]
    let store: Arc<dyn astrcode_core::storage::EventStore> = if opts.config_path.is_some() {
        Arc::new(astrcode_storage::in_memory::InMemoryEventStore::new())
    } else {
        Arc::new(
            astrcode_storage::session_repo::FileSystemSessionRepository::for_project_path(&cwd),
        )
    };
    #[cfg(not(feature = "testing"))]
    let store: Arc<dyn astrcode_core::storage::EventStore> = Arc::new(
        astrcode_storage::session_repo::FileSystemSessionRepository::for_project_path(&cwd),
    );
    let event_store = store;

    // 6. 加载扩展并创建 extension runner。
    //
    // 加载顺序：
    // - 先从磁盘加载项目/用户扩展；
    // - 再注册内置 agent/task 扩展；
    // - 最后注册磁盘扩展。
    //
    // 扩展工具不会在这里写入全局工具表；这里只保存扩展列表。
    // 每个 session 需要工具时，再从 runner 收集工具适配器并生成快照。
    let cwd_str = cwd.to_string_lossy().to_string();
    let load_result = ExtensionLoader::load_all(Some(&cwd_str)).await;
    let extension_runner = Arc::new(ExtensionRunner::new(Duration::from_secs(30)));
    extension_runner.register_builtins().await;
    for ext in load_result.extensions {
        extension_runner.register(ext).await;
    }
    for err in &load_result.errors {
        tracing::warn!("Extension load error: {err}");
    }

    // 共享的 agent_session_control slot，runtime 和 spawner 都读它。
    let agent_session_control_slot: AgentSessionControlSlot = Arc::new(RwLock::new(None));
    let session_manager = Arc::new(SessionManager::new(
        Arc::clone(&event_store),
        Arc::clone(&config_manager),
        Arc::clone(&extension_runner),
        session_runtime_registry,
        Arc::clone(&background_tasks),
    ));

    // 7. 给扩展运行时绑定”创建子会话”的宿主能力。
    //
    // 扩展本身不能直接拿到 EventStore；当扩展工具返回 RunSession 声明式结果时，
    // 绑定会话派生器，使扩展工具可通过 RunSession 声明式结果创建子 session。
    // 子 session 也会生成自己的工具快照，而不是复用父会话或启动期的工具表。
    extension_runner.bind(Arc::new(ServerSessionSpawner {
        config: Arc::clone(&config_manager),
        context_assembler: Arc::clone(&context_assembler),
        background_tasks: Arc::clone(&background_tasks),
        session_manager: Arc::clone(&session_manager),
        extension_runner: Arc::clone(&extension_runner),
        agent_session_control: Arc::clone(&agent_session_control_slot),
    }));

    // 8. 返回运行时容器。
    //
    // ServerRuntime 保存的是”共享基础设施”：session、LLM、prompt、扩展、
    // 配置和上下文预算。注意这里故意没有 tool_registry：
    // 工具表是 session 级别的快照，不再是 bootstrap 级全局单例。
    Ok(ServerRuntime {
        event_store,
        config: config_manager,
        context_assembler,
        background_tasks,
        session_manager,
        extension_runner,
        shutdown_token: tokio_util::sync::CancellationToken::new(),
        agent_session_control: agent_session_control_slot,
    })
}

/// 引导过程中可能出现的错误。
#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("Config: {0}")]
    Config(#[from] astrcode_core::config::ConfigStoreError),
    #[error("Resolve: {0}")]
    Resolve(#[from] astrcode_core::config::ResolveError),
}
