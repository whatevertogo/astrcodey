//! 服务器引导模块 — 从配置组装所有服务。
//!
//! 负责在启动时初始化所有核心组件：LLM 提供者、提示词组装器、
//! 会话管理器、扩展运行器和上下文窗口设置。

use std::{path::PathBuf, sync::Arc, time::Duration};

use astrcode_context::context_assembler::LlmContextAssembler;
use astrcode_core::{
    config::ConfigStore,
    extension::ExtensionHostServices,
    storage::EventStore,
};
use astrcode_extensions::{
    loader::{DiskExtensionSource, ExtensionLoadContext, ExtensionRuntime, WasmLimits},
    runner::ExtensionRunner,
};
use astrcode_session::SessionRuntimeServices;
use astrcode_storage::config_store::FileConfigStore;

pub use crate::config_manager::ConfigManager;
use crate::session_manager::SessionManager;

// ─── ServerRuntime ───────────────────────────────────────────────────────

/// 启动时组装的所有服务集合，按领域分组。
///
/// 这是服务器运行时的核心容器，持有所有共享服务的引用。
/// 各组件通过 `Arc` 共享，支持并发访问。
pub struct ServerRuntime {
    /// 事件存储后端，用于创建/恢复/删除会话等集合操作
    pub event_store: Arc<dyn EventStore>,
    /// 配置与 LLM 提供者的联合管理器
    pub config_manager: Arc<crate::config_manager::ConfigManager>,
    /// 上下文组装器，负责窗口估算和摘要压缩
    pub context_assembler: Arc<LlmContextAssembler>,
    /// server 侧的 session 生命周期门面。
    pub session_manager: Arc<SessionManager>,
    /// 扩展运行器，负责加载和分发扩展钩子事件
    pub extension_runner: Arc<ExtensionRunner>,
    /// 跨 session 共享的运行时能力（LLM / 扩展 / 上下文 / 配置）。
    ///
    /// 是 session crate 不依赖 server 类型的视图，所有 session 通过它读取 LLM
    /// provider 与生效配置。`ConfigManager` 是配置写入入口，但 `effective` 与
    /// `llm_provider` 的存储位置只在这里，二者共享同一个 `Arc`。
    pub capabilities: Arc<SessionRuntimeServices>,
    /// 启动时使用的工作目录，用于项目级扩展发现与后续重载。
    pub startup_working_dir: PathBuf,
    /// 触发后通知 HTTP server 执行 graceful shutdown
    pub shutdown_token: tokio_util::sync::CancellationToken,
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
    let effective = match config.clone().into_effective() {
        Ok(e) => {
            if let Err(err) = config_store.save_last_known_good(&config).await {
                tracing::warn!("Failed to save last-known-good config snapshot: {err}");
            }
            e
        },
        Err(error) => {
            tracing::warn!("Config resolution failed: {error}");
            match config_store.load_last_known_good().await {
                Ok(Some(snapshot)) => match snapshot.clone().into_effective() {
                    Ok(e) => {
                        tracing::warn!(
                            "Loaded last-known-good config snapshot as fallback. Fix your config \
                             via Settings or POST /api/config/active-selection."
                        );
                        e
                    },
                    Err(snapshot_err) => {
                        tracing::warn!("Last-known-good snapshot also invalid: {snapshot_err}");
                        fallback_default_effective()
                    },
                },
                Ok(None) => {
                    tracing::warn!(
                        "No last-known-good snapshot found. Using built-in defaults. Fix your \
                         config via Settings or POST /api/config/active-selection."
                    );
                    fallback_default_effective()
                },
                Err(err) => {
                    tracing::warn!("Failed to load last-known-good snapshot: {err}");
                    fallback_default_effective()
                },
            }
        },
    };

    // 2. 构建提示词组装器。
    let context_settings = effective.context.clone();
    let context_assembler = Arc::new(LlmContextAssembler::new(context_settings));

    // 3. 确定当前项目工作目录。
    //
    // 这个目录只用于启动期项目识别、扩展加载和默认隐式会话。
    // 显式创建 session 时，工具快照会使用 session 自己的 working_dir。
    let cwd = opts
        .working_dir
        .clone()
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
    ));

    // 7. 加载扩展。
    //
    // HostServices 从 capabilities 获取 small_llm，为 trusted bundled extension
    // 提供运行时依赖（EventStore、small_llm）。不传给 disk/wasm source。
    let host_services = Arc::new(ExtensionHostServices::new(
        Arc::clone(&event_store),
        Some(capabilities.small_llm()),
    ));
    let load_errors = load_extensions_into_runner(
        &extension_runner,
        &capabilities,
        &cwd,
        Some(host_services),
    )
    .await;
    for err in &load_errors {
        tracing::warn!("Extension load error: {err}");
    }

    // 8. 给扩展运行时绑定"创建子会话"的宿主能力。
    extension_runner.bind_session_ops(Arc::new(
        crate::session_operations::ServerSessionOperations {
            session_manager: Arc::clone(&session_manager),
        },
    ));

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

/// 构造一个兜底的 `EffectiveConfig`，用于所有配置来源均失败时。
///
/// 返回的 LLM 配置使用空连接信息，LLM 功能不可用，但 HTTP API 仍然正常工作。
fn fallback_default_effective() -> astrcode_core::config::EffectiveConfig {
    use astrcode_core::config::{
        AgentSettings, ContextSettings, EffectiveConfig, ExtensionSettings, WasmSettings,
    };

    EffectiveConfig {
        llm: dummy_llm_settings(),
        small_llm: dummy_llm_settings(),
        context: ContextSettings::default(),
        agent: AgentSettings::default(),
        wasm: WasmSettings::default(),
        extensions: ExtensionSettings::default(),
    }
}

fn dummy_llm_settings() -> astrcode_core::config::LlmSettings {
    use astrcode_core::config::{LlmSettings, raw::OpenAiApiMode};

    LlmSettings {
        provider_kind: "openai".into(),
        base_url: String::new(),
        api_key: String::new(),
        api_mode: OpenAiApiMode::ChatCompletions,
        model_id: "fallback".into(),
        max_tokens: 1024,
        context_limit: 4096,
        connect_timeout_secs: 10,
        read_timeout_secs: 90,
        max_retries: 0,
        retry_base_delay_ms: 250,
        temperature: None,
        supports_prompt_cache_key: false,
        prompt_cache_retention: None,
        reasoning: false,
        reasoning_split: false,
    }
}

impl ServerRuntime {
    /// 停止所有扩展运行态任务。可重复调用。
    pub async fn shutdown_extensions(&self) {
        for error in self.extension_runner.shutdown().await {
            tracing::warn!("extension shutdown error: {error}");
        }
    }

    /// 按当前配置重载扩展集合，并让已打开 session 的工具快照在下一次 turn 重建。
    pub async fn reload_extensions(&self) -> Vec<String> {
        let small_llm = self.capabilities.small_llm();
        let host_services = Arc::new(ExtensionHostServices::new(
            Arc::clone(&self.event_store),
            Some(small_llm),
        ));
        let load_errors = load_extensions_into_runner(
            &self.extension_runner,
            &self.capabilities,
            &self.startup_working_dir,
            Some(host_services),
        )
        .await;
        self.session_manager.invalidate_tool_registries();
        load_errors
    }
}

/// 将扩展加载到已有的 runner 中。
async fn load_extensions_into_runner(
    runner: &Arc<ExtensionRunner>,
    capabilities: &SessionRuntimeServices,
    cwd: &std::path::Path,
    host_services: Option<Arc<ExtensionHostServices>>,
) -> Vec<String> {
    let effective = capabilities.read_effective();
    let bundled_source = astrcode_bundled_extensions::BundledExtensionSource::new(
        effective.extensions.extension_states.clone(),
        host_services,
    );
    let disk_source = DiskExtensionSource::new(effective.extensions.extension_states.clone());
    ExtensionRuntime::sync_sources(
        runner,
        &ExtensionLoadContext {
            working_dir: Some(cwd.to_string_lossy().to_string()),
            wasm_limits: WasmLimits {
                fuel: effective.wasm.fuel,
                memory_bytes: effective.wasm.memory_bytes,
            },
        },
        &[&bundled_source, &disk_source],
    )
    .await
}
