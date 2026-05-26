//! 服务器引导模块 — 从配置组装所有服务。
//!
//! 负责在启动时初始化所有核心组件：LLM 提供者、提示词组装器、
//! 会话管理器、扩展运行器和上下文窗口设置。

use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use astrcode_context::context_assembler::LlmContextAssembler;
use astrcode_core::{
    config::ConfigStore, extension::ExtensionHostServices, lifecycle::SessionResourceCleanup,
    storage::EventStore,
};
use astrcode_extensions::{
    build_host_router,
    loader::{DiskExtensionSource, ExtensionLoadContext, ExtensionRuntime},
    runner::ExtensionRunner,
};
use astrcode_session::SessionRuntimeServices;
use astrcode_storage::config_store::FileConfigStore;

mod config_resolve;
mod server_system;

pub use server_system::{ServerSystem, spawn_server_system};

pub use crate::config_manager::ConfigManager;
use crate::session_manager::SessionManager;

// ─── ServerRuntime ───────────────────────────────────────────────────────

/// 启动时组装的所有服务集合，按领域分组。
///
/// 这是服务器运行时的核心容器，持有所有共享服务的引用。
/// 各组件通过 `Arc` 共享，支持并发访问。
pub struct ServerRuntime {
    pub(crate) event_store: Arc<dyn EventStore>,
    pub(crate) config_manager: Arc<ConfigManager>,
    pub(crate) context_assembler: Arc<LlmContextAssembler>,
    pub(crate) session_manager: Arc<SessionManager>,
    pub(crate) extension_runner: Arc<ExtensionRunner>,
    pub(crate) capabilities: Arc<SessionRuntimeServices>,
    pub(crate) startup_working_dir: PathBuf,
    pub(crate) shutdown_token: tokio_util::sync::CancellationToken,
}

impl ServerRuntime {
    pub fn event_store(&self) -> &Arc<dyn EventStore> {
        &self.event_store
    }

    pub fn config_manager(&self) -> &Arc<ConfigManager> {
        &self.config_manager
    }

    pub fn context_assembler(&self) -> &Arc<LlmContextAssembler> {
        &self.context_assembler
    }

    pub fn session_manager(&self) -> &Arc<SessionManager> {
        &self.session_manager
    }

    pub fn extension_runner(&self) -> &Arc<ExtensionRunner> {
        &self.extension_runner
    }

    pub fn capabilities(&self) -> &Arc<SessionRuntimeServices> {
        &self.capabilities
    }

    pub fn startup_working_dir(&self) -> &PathBuf {
        &self.startup_working_dir
    }

    pub fn shutdown_token(&self) -> &tokio_util::sync::CancellationToken {
        &self.shutdown_token
    }
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
/// 2. 构建提示词组装器
/// 3. 确定启动工作目录
/// 4. 初始化存储后端
/// 5. 创建空的扩展运行器
/// 6. 组装 ConfigManager（内部构建 providers）
/// 7. 加载扩展（从 capabilities 获取 small_llm）
/// 8. 绑定扩展创建子会话的宿主能力
/// 9. 返回共享运行时容器
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
    let effective = config_resolve::resolve_effective_config(&config_store, &config).await;

    // 2. 构建提示词组装器。
    let context_settings = effective.context.clone();
    let context_assembler = Arc::new(LlmContextAssembler::new(context_settings));

    // 3. 确定当前项目工作目录。
    //
    // 这个目录只用于启动期项目识别、扩展加载和默认隐式会话。
    // 显式创建 session 时，工具快照会使用 session 自己的 working_dir。
    let cwd = opts
        .working_dir
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    // 4. 初始化事件存储。
    //
    // 测试启动（config_path.is_some()）使用内存存储，避免污染真实会话目录；
    // 正常启动按项目路径选择文件系统会话仓库。
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

    // 5. 创建空的扩展运行器。
    //
    // 先创建空 runner，后续加载 extensions 填充它。
    // ConfigManager 持有 Arc 引用，加载后的扩展对已创建的 session 立即可见。
    let extension_runner = Arc::new(ExtensionRunner::new(Duration::from_secs(30)));

    // 6. 组装 ConfigManager 与 Capabilities。
    //
    // ConfigManager 内部从 effective 构建 providers，不需要外部注入。
    // 二者共享同一份 effective/llm_provider 存储，配置写入直接更新 Capabilities。
    let (config_manager, capabilities) = crate::config_manager::ConfigManager::from_loaded_config(
        Arc::new(config_store),
        config,
        effective,
        Arc::clone(&extension_runner),
        Arc::clone(&context_assembler),
    );
    let config_manager = Arc::new(config_manager);

    let session_manager = Arc::new(SessionManager::new(
        Arc::clone(&event_store),
        Arc::clone(&config_manager),
        Arc::clone(&capabilities),
        vec![Arc::new(TerminalCleanup)],
    ));

    // 7. 加载扩展。
    //
    // HostServices 从 capabilities 获取 small_llm，为 trusted bundled extension
    // 提供运行时依赖（EventStore、small_llm）。不传给磁盘 IPC 扩展。
    let host_services = Arc::new(ExtensionHostServices::new(
        Arc::clone(&event_store),
        Some(capabilities.small_llm()),
    ));
    extension_runner.bind_host_services(Arc::clone(&host_services));
    let load_errors =
        load_extensions_into_runner(&extension_runner, &capabilities, &host_services, &cwd).await;
    for err in &load_errors {
        tracing::warn!("Extension load error: {err}");
    }

    // 8. 子会话操作能力绑定移至 spawn_server_system（需要 scheduler）。

    // 9. 返回运行时容器。
    Ok(ServerRuntime {
        event_store,
        config_manager,
        context_assembler,
        session_manager,
        extension_runner,
        capabilities,
        startup_working_dir: cwd,
        shutdown_token: tokio_util::sync::CancellationToken::new(),
    })
}

/// 引导过程中可能出现的错误。
#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("Config: {0}")]
    Config(#[from] astrcode_core::config::ConfigStoreError),
}

#[cfg(feature = "testing")]
impl ServerRuntime {
    /// 集成测试用：从已组装的部件构造运行时（避免测试直接访问私有字段）。
    pub fn assemble_for_test(
        event_store: Arc<dyn EventStore>,
        config_manager: Arc<ConfigManager>,
        context_assembler: Arc<LlmContextAssembler>,
        session_manager: Arc<SessionManager>,
        extension_runner: Arc<ExtensionRunner>,
        capabilities: Arc<SessionRuntimeServices>,
        startup_working_dir: PathBuf,
    ) -> Self {
        Self {
            event_store,
            config_manager,
            context_assembler,
            session_manager,
            extension_runner,
            capabilities,
            startup_working_dir,
            shutdown_token: tokio_util::sync::CancellationToken::new(),
        }
    }
}

impl ServerRuntime {
    /// 停止所有扩展运行态任务。可重复调用。
    pub async fn shutdown_extensions(&self) {
        for error in self.extension_runner().shutdown().await {
            tracing::warn!("extension shutdown error: {error}");
        }
    }

    /// 按当前配置重载扩展集合，并让已打开 session 的工具快照在下一次 turn 重建。
    pub async fn reload_extensions(&self) -> Vec<String> {
        let small_llm = self.capabilities().small_llm();
        let host_services = Arc::new(ExtensionHostServices::new(
            Arc::clone(self.event_store()),
            Some(small_llm),
        ));
        self.extension_runner()
            .bind_host_services(Arc::clone(&host_services));
        let load_errors = load_extensions_into_runner(
            self.extension_runner(),
            self.capabilities(),
            &host_services,
            self.startup_working_dir(),
        )
        .await;
        self.session_manager().invalidate_tool_registries();
        load_errors
    }
}

/// 将扩展加载到已有的 runner 中。
async fn load_extensions_into_runner(
    runner: &Arc<ExtensionRunner>,
    capabilities: &SessionRuntimeServices,
    host_services: &Arc<ExtensionHostServices>,
    cwd: &std::path::Path,
) -> Vec<String> {
    let effective = capabilities.read_effective();

    // 先将扩展配置注入运行器，这样 register 时可查到
    let configs: BTreeMap<_, _> = effective
        .extensions
        .extension_configs
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    runner.update_extension_configs(configs);

    let bundled_source = astrcode_bundled_extensions::BundledExtensionSource::new(
        effective.extensions.extension_states.clone(),
    );
    let disk_source = DiskExtensionSource::new(effective.extensions.extension_states.clone());
    ExtensionRuntime::sync_sources(
        runner,
        &ExtensionLoadContext {
            working_dir: Some(cwd.to_string_lossy().to_string()),
            host_router: Some(build_host_router(
                Arc::clone(host_services),
                Some(cwd.to_string_lossy().to_string()),
            )),
        },
        &[&bundled_source, &disk_source],
    )
    .await
}

// ─── SessionResourceCleanup 实现 ────────────────────────────────────────

/// session 销毁/回收时清理 PTY 终端资源。
struct TerminalCleanup;

impl SessionResourceCleanup for TerminalCleanup {
    fn cleanup(&self, session_id: &astrcode_core::types::SessionId) {
        astrcode_tools::terminal_tool::cleanup_terminals_for_session(session_id.as_str());
    }
}
