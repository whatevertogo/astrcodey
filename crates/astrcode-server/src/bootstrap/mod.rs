//! 服务器引导模块 — 从配置组装所有服务。
//!
//! 负责在启动时初始化所有核心组件：LLM 提供者、提示词组装器、
//! 会话管理器、扩展运行器和上下文窗口设置。

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use astrcode_core::{config::ConfigStore, tool::SessionOperations};
use astrcode_extensions::{
    host_router::{HostBackends, build_host_router_with_public_http_dispatcher},
    runner::ExtensionRunner,
};
use astrcode_session::SessionRuntimeServices;
use astrcode_storage::{EventReader, SessionReader, SessionStore, config_store::FileConfigStore};

use crate::session_resource_cleanup::SessionResourceCleanup;

mod config_resolve;
mod server_system;

pub use server_system::ServerApp;

fn apply_approval_mode_bootstrap_options(
    config: &mut astrcode_core::config::Config,
    opts: &BootstrapOptions,
) {
    if let Some(mode) = opts.approval_mode_override {
        config.runtime.approval_mode = Some(mode.as_str().into());
        return;
    }
    if config.runtime.approval_mode.is_none()
        && let Some(mode) = opts.default_approval_mode_if_unset
    {
        config.runtime.approval_mode = Some(mode.as_str().into());
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
    if !opts.disabled_extension_ids.is_empty() {
        let states = config
            .runtime
            .extension_states
            .get_or_insert_with(BTreeMap::new);
        for extension_id in &opts.disabled_extension_ids {
            states.insert(extension_id.clone(), false);
        }
    }
    Ok(config)
}

use crate::{
    child_session::ChildSessionCoordinator, config_manager::ConfigManager,
    session_manager::SessionManager, session_operations::ServerSessionOperations,
    turn_registry::TurnRegistry, turn_scheduler::TurnScheduler,
};

// ─── ServerRuntime ───────────────────────────────────────────────────────

/// 启动时组装的所有服务集合，按领域分组。
///
/// 这是服务器运行时的核心容器，持有所有共享服务的引用。
/// 各组件通过 `Arc` 共享，支持并发访问。
pub struct ServerRuntime {
    pub(crate) event_store: Arc<dyn SessionStore>,
    pub(crate) config_manager: Arc<ConfigManager>,
    pub(crate) session_manager: Arc<SessionManager>,
    pub(crate) scheduler: Arc<TurnScheduler>,
    pub(crate) extension_runner: Arc<ExtensionRunner>,
    pub(crate) runtime_services: Arc<SessionRuntimeServices>,
    pub(crate) startup_working_dir: PathBuf,
    pub(crate) shutdown_token: tokio_util::sync::CancellationToken,
}

impl ServerRuntime {
    pub(crate) fn event_store(&self) -> &Arc<dyn SessionStore> {
        &self.event_store
    }

    pub(crate) fn config_manager(&self) -> &Arc<ConfigManager> {
        &self.config_manager
    }

    pub(crate) fn session_manager(&self) -> &Arc<SessionManager> {
        &self.session_manager
    }

    pub(crate) fn scheduler(&self) -> &Arc<TurnScheduler> {
        &self.scheduler
    }

    pub(crate) fn extension_runner(&self) -> &Arc<ExtensionRunner> {
        &self.extension_runner
    }

    pub(crate) fn runtime_services(&self) -> &Arc<SessionRuntimeServices> {
        &self.runtime_services
    }

    pub(crate) fn startup_working_dir(&self) -> &PathBuf {
        &self.startup_working_dir
    }

    pub(crate) fn shutdown_token(&self) -> &tokio_util::sync::CancellationToken {
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
    /// 当 `runtime.approvalMode` 未设置时使用的审批模式（CLI/TUI 进程内启动默认为 Yolo）。
    pub default_approval_mode_if_unset: Option<astrcode_core::permission::ApprovalMode>,
    /// 强制覆盖 `runtime.approvalMode`（如 CLI `--yolo` / `--manual`）。
    pub approval_mode_override: Option<astrcode_core::permission::ApprovalMode>,
    /// 当前 transport 无法完成交互契约时强制禁用的扩展。
    pub disabled_extension_ids: BTreeSet<String>,
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
/// 2. 确定启动工作目录
/// 3. 初始化存储后端
/// 4. 创建空的扩展运行器
/// 5. 组装 ConfigManager（内部构建 providers 与 context assembler）
/// 6. 创建 turn scheduler 与 session ops
/// 7. 加载扩展（从 runtime services 获取 LLM 与 session ops）
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
    let config = load_merged_config(&config_store, &opts).await?;

    // 2. 确定当前项目工作目录（用于项目级 config 覆盖与扩展发现）。
    let cwd = opts
        .working_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let effective = config_resolve::resolve_effective_config(&config_store, &config).await;

    // 3. 初始化事件存储。
    //
    // 测试启动（config_path.is_some()）使用内存存储，避免污染真实会话目录；
    // 正常启动按项目路径选择文件系统会话仓库。
    #[cfg(feature = "testing")]
    let store: Arc<dyn SessionStore> = if opts.config_path.is_some() {
        Arc::new(astrcode_storage::in_memory::InMemoryEventStore::new())
    } else {
        Arc::new(astrcode_storage::session_repo::FileSystemSessionRepository::new())
    };
    #[cfg(not(feature = "testing"))]
    let store: Arc<dyn SessionStore> =
        Arc::new(astrcode_storage::session_repo::FileSystemSessionRepository::new());
    let event_store = store;

    // 4. 创建空的扩展运行器。
    //
    // 先创建空 runner，后续加载 extensions 填充它。
    // ConfigManager 持有 Arc 引用，加载后的扩展对已创建的 session 立即可见。
    let extension_runner = Arc::new(ExtensionRunner::new(Duration::from_secs(30)));

    // 5. 组装 ConfigManager 与 session runtime services。
    //
    // ConfigManager 内部从 effective 构建 providers，不需要外部注入。
    // 二者共享同一份 effective/llm_provider 存储，配置写入直接更新 runtime services。
    let (config_manager, runtime_services) =
        crate::config_manager::ConfigManager::from_loaded_config(
            Arc::new(config_store),
            config,
            effective,
            Arc::clone(&extension_runner),
            cwd.clone(),
        )?;
    let config_manager = Arc::new(config_manager);

    // 6. 创建 session manager、turn scheduler 与 session ops。
    let session_manager = Arc::new(SessionManager::new(
        Arc::clone(&event_store),
        Arc::clone(&runtime_services),
        vec![Arc::new(HostResourceCleanup {
            runner: Arc::downgrade(&extension_runner),
        })],
    ));
    session_manager.bind_custom_event_runner(Arc::clone(&extension_runner));

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

    // 7. 加载扩展。
    bind_extension_host_router(
        &extension_runner,
        &runtime_services,
        Arc::clone(&event_store),
        &cwd,
    );
    config_manager
        .initialize_extensions::<std::convert::Infallible>()
        .await
        .map_err(|error| BootstrapError::Extension(error.to_string()))?;
    if let Err(error) = session_manager
        .replay_custom_events(&extension_runner)
        .await
    {
        tracing::warn!(%error, "failed to replay durable custom events");
    }

    // 8. 返回运行时容器。
    Ok(ServerRuntime {
        event_store,
        config_manager,
        session_manager,
        scheduler,
        extension_runner,
        runtime_services,
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
    #[error("Extension runtime: {0}")]
    Extension(String),
}

impl ServerRuntime {
    /// 停止所有扩展运行态任务。可重复调用。
    pub async fn shutdown_extensions(&self) {
        self.config_manager().drain_transactions().await;
        for error in self.extension_runner().shutdown().await {
            tracing::warn!("extension shutdown error: {error}");
        }
    }

    /// 按当前配置重载扩展集合；新 turn 会直接解析新的工具快照。
    pub async fn reload_extensions(&self) -> Vec<String> {
        let errors = self
            .config_manager()
            .reload_extensions::<std::convert::Infallible>()
            .await
            .err()
            .map(|error| vec![error.to_string()])
            .unwrap_or_default();
        if let Err(error) = self
            .session_manager()
            .replay_custom_events(self.extension_runner())
            .await
        {
            tracing::warn!(%error, "failed to replay durable custom events after reload");
        }
        errors
    }
}

/// 将扩展加载到已有的 runner 中。
fn bind_extension_host_router(
    runner: &Arc<ExtensionRunner>,
    runtime_services: &SessionRuntimeServices,
    session_store: Arc<dyn SessionStore>,
    cwd: &std::path::Path,
) {
    let working_dir = cwd.to_string_lossy().into_owned();
    let event_reader: Arc<dyn EventReader> = session_store.clone();
    let session_reader: Arc<dyn SessionReader> = session_store;
    let outbound_network = runner
        .outbound_network_service()
        .unwrap_or_else(astrcode_extensions::host_router::default_outbound_network_service);
    let host_router = build_host_router_with_public_http_dispatcher(
        HostBackends {
            main_llm: Some(runtime_services.live_llm()),
            small_llm: Some(runtime_services.live_small_llm()),
            event_reader: Some(event_reader),
            session_reader: Some(session_reader),
            default_working_dir: Some(working_dir.clone()),
            public_http_dispatcher: None,
            outbound_network: Some(outbound_network),
        },
        runner.public_http_dispatcher(),
    );
    runner.bind_host_router(Arc::clone(&host_router));
}

// ─── SessionResourceCleanup 实现 ────────────────────────────────────────

/// Session durable close 后统一释放 Extension Runtime 持有的瞬态资源。
struct HostResourceCleanup {
    runner: std::sync::Weak<ExtensionRunner>,
}

impl SessionResourceCleanup for HostResourceCleanup {
    fn cleanup(&self, session_id: &astrcode_core::types::SessionId) {
        if let Some(runner) = self.runner.upgrade() {
            runner.cleanup_session_resources(session_id);
        }
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
            disabled_extension_ids: BTreeSet::from(["astrcode-ask-user".into()]),
            ..BootstrapOptions::default()
        };

        let config = load_merged_config(&store, &opts).await.unwrap();

        assert_eq!(config.active_profile, "overlay");
        assert_eq!(config.active_model, "overlay-model");
        assert_eq!(config.profiles[0].name, "overlay");
        assert!(!config.runtime.extension_states.as_ref().unwrap()["astrcode-ask-user"]);

        std::fs::remove_dir_all(root).unwrap();
    }
}
