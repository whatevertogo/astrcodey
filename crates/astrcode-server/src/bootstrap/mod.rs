//! 服务器引导模块 — 从配置组装所有服务。
//!
//! 负责在启动时初始化所有核心组件：LLM 提供者、提示词组装器、
//! 会话管理器、扩展运行器和上下文窗口设置。

use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use astrcode_context::context_assembler::LlmContextAssembler;
use astrcode_core::{
    config::ConfigStore, extension::ExtensionHostServices, lifecycle::SessionResourceCleanup,
    storage::EventStore, tool::SessionOperations,
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

pub use server_system::{ServerSystem, spawn_server_system, spawn_server_system_without_legacy};

fn approval_mode_wire(mode: astrcode_core::permission::ApprovalMode) -> String {
    match mode {
        astrcode_core::permission::ApprovalMode::Manual => "manual".into(),
        astrcode_core::permission::ApprovalMode::Yolo => "yolo".into(),
    }
}

fn apply_approval_mode_bootstrap_options(
    config: &mut astrcode_core::config::Config,
    opts: &BootstrapOptions,
) {
    if let Some(mode) = opts.approval_mode_override {
        config.runtime.approval_mode = Some(approval_mode_wire(mode));
        return;
    }
    if config.runtime.approval_mode.is_none() {
        if let Some(mode) = opts.default_approval_mode_if_unset {
            config.runtime.approval_mode = Some(approval_mode_wire(mode));
        }
    }
}

/// 加载全局配置、合并项目 overlay、应用启动选项（与 [`bootstrap_with`] / 热重载共用）。
pub(crate) async fn load_merged_config(
    config_store: &dyn ConfigStore,
    opts: &BootstrapOptions,
) -> Result<astrcode_core::config::Config, astrcode_core::config::ConfigStoreError> {
    let mut config = config_store.load().await?;
    let cwd = opts
        .working_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    if let Some(overlay) = config_store.load_overlay(&cwd.to_string_lossy()).await? {
        config = astrcode_core::config::merge_overlay(config, overlay);
    }
    apply_approval_mode_bootstrap_options(&mut config, opts);
    Ok(config)
}

pub use crate::config_manager::ConfigManager;
use crate::{
    child_session::ChildSessionCoordinator, session_manager::SessionManager,
    session_operations::ServerSessionOperations, turn_registry::TurnRegistry,
    turn_scheduler::TurnScheduler,
};

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
    pub(crate) scheduler: Arc<TurnScheduler>,
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

    pub fn scheduler(&self) -> &Arc<TurnScheduler> {
        &self.scheduler
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

    /// 配置热更新后同步所有 session runtime 的 LLM binding。
    pub fn sync_session_model_bindings(&self) {
        self.session_manager.sync_all_model_bindings_from_config();
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
    /// 当 `runtime.approvalMode` 未设置时使用的审批模式（CLI/TUI 进程内启动默认为 Yolo）。
    pub default_approval_mode_if_unset: Option<astrcode_core::permission::ApprovalMode>,
    /// 强制覆盖 `runtime.approvalMode`（如 CLI `--yolo` / `--manual`）。
    pub approval_mode_override: Option<astrcode_core::permission::ApprovalMode>,
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
/// 7. 创建 turn scheduler 与 session ops
/// 8. 加载扩展（从 capabilities 获取 LLM 与 session ops）
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
    let config = load_merged_config(&config_store, &opts).await?;

    // 2. 确定当前项目工作目录（用于项目级 config 覆盖与扩展发现）。
    let cwd = opts
        .working_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let effective = config_resolve::resolve_effective_config(&config_store, &config).await;

    // 3. 构建提示词组装器。
    let context_settings = effective.context.clone();
    let context_assembler = Arc::new(LlmContextAssembler::new(context_settings));

    // 4. 初始化事件存储（步骤号延续上文「构建提示词组装器」之后）。
    //
    // 测试启动（config_path.is_some()）使用内存存储，避免污染真实会话目录；
    // 正常启动按项目路径选择文件系统会话仓库。
    #[cfg(feature = "testing")]
    let store: Arc<dyn astrcode_core::storage::EventStore> = if opts.config_path.is_some() {
        Arc::new(astrcode_storage::in_memory::InMemoryEventStore::new())
    } else {
        Arc::new(astrcode_storage::session_repo::FileSystemSessionRepository::new())
    };
    #[cfg(not(feature = "testing"))]
    let store: Arc<dyn astrcode_core::storage::EventStore> =
        Arc::new(astrcode_storage::session_repo::FileSystemSessionRepository::new());
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
    )?;
    let config_manager = Arc::new(config_manager);

    let session_manager = Arc::new(SessionManager::new(
        Arc::clone(&event_store),
        Arc::clone(&config_manager),
        Arc::clone(&capabilities),
        vec![Arc::new(TerminalCleanup), Arc::new(BackgroundShellCleanup)],
    ));

    let child_sessions = Arc::new(ChildSessionCoordinator::new(Arc::clone(&session_manager)));
    let scheduler = Arc::new(TurnScheduler::new(
        Arc::clone(&session_manager),
        Arc::new(TurnRegistry::new()),
        Arc::clone(&child_sessions),
    ));
    child_sessions.spawn_completion_watcher(Arc::clone(&scheduler));
    let session_ops: Arc<dyn SessionOperations> = Arc::new(ServerSessionOperations {
        session_manager: Arc::clone(&session_manager),
        scheduler: Arc::clone(&scheduler),
        child_sessions,
    });
    extension_runner.bind_session_ops(Arc::clone(&session_ops));
    session_manager.add_resource_cleanup(Arc::new(TurnSchedulerCleanup {
        scheduler: Arc::clone(&scheduler),
    }));

    // 7. 加载扩展。
    //
    // HostServices 从 capabilities 获取 LLM，并携带 session ops 给声明了
    // SessionControl 的 trusted bundled extension。不传给磁盘 IPC 扩展。
    let host_services = Arc::new(
        ExtensionHostServices::new(
            Arc::clone(&event_store),
            Some(capabilities.llm()),
            Some(capabilities.small_llm()),
        )
        .with_session_ops(session_ops),
    );
    extension_runner.bind_host_services(Arc::clone(&host_services));
    let load_errors =
        load_extensions_into_runner(&extension_runner, &capabilities, &host_services, &cwd).await;
    for err in &load_errors {
        tracing::warn!("Extension load error: {err}");
    }

    // 9. 返回运行时容器。
    Ok(ServerRuntime {
        event_store,
        config_manager,
        context_assembler,
        session_manager,
        scheduler,
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
    #[error("LLM provider: {0}")]
    Llm(#[from] astrcode_core::llm::LlmError),
}

#[cfg(feature = "testing")]
impl ServerRuntime {
    /// 集成测试用：从已组装的部件构造运行时（避免测试直接访问私有字段）。
    #[allow(clippy::too_many_arguments)] // 字段与 `ServerRuntime` 一一对应，拆 struct 无收益
    pub fn assemble_for_test(
        event_store: Arc<dyn EventStore>,
        config_manager: Arc<ConfigManager>,
        context_assembler: Arc<LlmContextAssembler>,
        session_manager: Arc<SessionManager>,
        scheduler: Arc<TurnScheduler>,
        extension_runner: Arc<ExtensionRunner>,
        capabilities: Arc<SessionRuntimeServices>,
        startup_working_dir: PathBuf,
    ) -> Self {
        Self {
            event_store,
            config_manager,
            context_assembler,
            session_manager,
            scheduler,
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
        let caps = self.capabilities();
        let mut host_services = ExtensionHostServices::new(
            Arc::clone(self.event_store()),
            Some(caps.llm()),
            Some(caps.small_llm()),
        );
        if let Some(session_ops) = caps.session_ops() {
            host_services = host_services.with_session_ops(session_ops);
        }
        let host_services = Arc::new(host_services);
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

/// session 销毁/回收时终止后台 shell 子进程。
struct BackgroundShellCleanup;

impl SessionResourceCleanup for BackgroundShellCleanup {
    fn cleanup(&self, session_id: &astrcode_core::types::SessionId) {
        astrcode_tools::background_shell::cleanup_background_shells_for_session(
            session_id.as_str(),
        );
    }
}

/// session 销毁/回收时清理 turn scheduler 中的待处理输入和活跃记录。
struct TurnSchedulerCleanup {
    scheduler: Arc<TurnScheduler>,
}

impl SessionResourceCleanup for TurnSchedulerCleanup {
    fn cleanup(&self, session_id: &astrcode_core::types::SessionId) {
        let scheduler = Arc::clone(&self.scheduler);
        let sid = session_id.clone();
        crate::task_utils::spawn_traced("turn_scheduler_cleanup", async move {
            scheduler.abort_and_cleanup(&sid).await;
            tracing::debug!(session_id = %sid, "turn scheduler cleanup finished");
        });
    }
}

#[cfg(test)]
mod tests {
    use astrcode_storage::config_store::FileConfigStore;

    use super::*;

    fn isolated_test_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("astrcode-{name}-{}", uuid::Uuid::new_v4()))
    }

    #[tokio::test]
    async fn load_merged_config_applies_toml_project_overlay() {
        let root = isolated_test_dir("config-overlay");
        let home = root.join("home");
        let workspace = root.join("workspace");
        let config_path = home.join(".astrcode").join("config.toml");
        let overlay_path = workspace.join(".astrcode").join("config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(overlay_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            r#"version = "1"
activeProfile = "base"
activeModel = "base-model"

[[profiles]]
name = "base"
providerKind = "openai"
wireFormat = "openai_chat_completions"
authScheme = "bearer"
baseUrl = "https://example.com"
apiKey = "test-key"

[[profiles.models]]
id = "base-model"
"#,
        )
        .unwrap();
        std::fs::write(
            &overlay_path,
            r#"activeProfile = "overlay"
activeModel = "overlay-model"

[[profiles]]
name = "overlay"
providerKind = "openai"
wireFormat = "openai_chat_completions"
authScheme = "bearer"
baseUrl = "https://overlay.example.com"
apiKey = "overlay-key"

[[profiles.models]]
id = "overlay-model"
"#,
        )
        .unwrap();
        let store = FileConfigStore::new(config_path);
        let opts = BootstrapOptions {
            working_dir: Some(workspace),
            ..BootstrapOptions::default()
        };

        let config = load_merged_config(&store, &opts).await.unwrap();

        assert_eq!(config.active_profile, "overlay");
        assert_eq!(config.active_model, "overlay-model");
        assert_eq!(config.profiles[0].name, "overlay");

        std::fs::remove_dir_all(root).unwrap();
    }
}
