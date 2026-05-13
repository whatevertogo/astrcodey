//! 服务器引导模块 — 从配置组装所有服务。
//!
//! 负责在启动时初始化所有核心组件：LLM 提供者、提示词组装器、
//! 会话管理器、扩展运行器和上下文窗口设置。

use std::{path::Path, sync::Arc, time::Duration};

use astrcode_ai::create_provider;
use astrcode_context::manager::LlmContextAssembler;
use astrcode_core::{
    config::{ConfigStore, EffectiveConfig, ModelSelection},
    extension::{ExtensionError, PromptBuildContext},
    llm::{LlmClientConfig, LlmProvider},
    prompt::{ExtensionPromptBlock, ExtensionSection, PromptProvider, SystemPromptInput},
    tool::{AgentSessionControl, ToolDefinition},
};
use astrcode_extensions::{loader::ExtensionLoader, runner::ExtensionRunner};
use astrcode_prompt::{composer::PromptComposer, pipeline};
use astrcode_storage::config_store::FileConfigStore;
use astrcode_support::shell::resolve_shell;
use astrcode_tools::registry::{ToolRegistry, builtin_tools};
use parking_lot::{Mutex, RwLock};

use crate::{
    agent::{AutoCompactFailureTracker, BackgroundTaskManager},
    session::{SessionManager, spawner::ServerSessionSpawner},
};

#[derive(Clone, Default)]
pub(crate) struct PromptFiles {
    pub(crate) identity: Option<String>,
    pub(crate) user_rules: Option<String>,
    pub(crate) project_rules: Option<String>,
}

pub(crate) struct SystemPromptSnapshotInput<'a> {
    pub(crate) extension_runner: &'a ExtensionRunner,
    pub(crate) session_id: &'a str,
    pub(crate) working_dir: &'a str,
    pub(crate) model_id: &'a str,
    pub(crate) tools: &'a [ToolDefinition],
    pub(crate) extra_system_prompt: Option<&'a str>,
    pub(crate) tool_prompt_metadata:
        std::collections::HashMap<String, astrcode_core::tool::ToolPromptMetadata>,
    pub(crate) prompt_files: PromptFiles,
}

// ─── ServerRuntime ───────────────────────────────────────────────────────

/// 启动时组装的所有服务集合，按领域分组。
///
/// 这是服务器运行时的核心容器，持有所有共享服务的引用。
/// 各组件通过 `Arc` 共享，支持并发访问。
pub struct ServerRuntime {
    /// 会话管理器，负责会话的创建、恢复、事件追加和删除
    pub session_manager: Arc<SessionManager>,
    /// LLM 提供者，用于生成 AI 回复（运行时可重建）
    pub llm_provider: Arc<RwLock<Arc<dyn LlmProvider>>>,
    /// 上下文组装器，负责窗口估算和摘要压缩
    pub context_assembler: Arc<LlmContextAssembler>,
    /// Auto compact provider 连续失败熔断状态。
    pub auto_compact_failures: Arc<AutoCompactFailureTracker>,
    /// 跨回合共享的后台任务管理器。
    pub background_tasks: Arc<Mutex<BackgroundTaskManager>>,
    /// 扩展运行器，负责加载和分发扩展钩子事件
    pub extension_runner: Arc<ExtensionRunner>,
    /// 已解析的最终配置（运行时可通过 `sync_effective` 刷新）
    pub effective: RwLock<EffectiveConfig>,
    /// 配置持久化存储，用于运行时读写配置
    pub config_store: Arc<dyn astrcode_core::config::ConfigStore>,
    /// 原始配置（用于设置面板展示 profile 列表等）
    pub raw_config: RwLock<astrcode_core::config::Config>,
    /// 触发后通知 HTTP server 执行 graceful shutdown
    pub shutdown_token: tokio_util::sync::CancellationToken,
    /// AgentSessionControl 共享引用（延迟注入：spawn_actor 后绑定 CommandHandle）。
    pub agent_session_control: Arc<RwLock<Option<Arc<dyn AgentSessionControl>>>>,
}

impl ServerRuntime {
    pub fn read_effective(&self) -> parking_lot::RwLockReadGuard<'_, EffectiveConfig> {
        self.effective.read()
    }

    pub fn write_raw_config(
        &self,
    ) -> parking_lot::RwLockWriteGuard<'_, astrcode_core::config::Config> {
        self.raw_config.write()
    }

    pub fn read_raw_config(
        &self,
    ) -> parking_lot::RwLockReadGuard<'_, astrcode_core::config::Config> {
        self.raw_config.read()
    }

    pub fn read_llm_provider(&self) -> Arc<dyn LlmProvider> {
        self.llm_provider.read().clone()
    }

    pub fn rebuild_provider_from_effective(&self) -> Result<(), String> {
        let new_provider = {
            let effective = self.read_effective();
            build_provider_from_effective(&effective)
        };
        let mut guard = self.llm_provider.write();
        *guard = new_provider;
        Ok(())
    }

    pub fn sync_effective(&self) -> Result<(), astrcode_core::config::ResolveError> {
        let new_effective = {
            let raw = self.raw_config.read();
            raw.clone().into_effective()?
        };
        let mut guard = self.effective.write();
        *guard = new_effective;
        Ok(())
    }

    /// Write raw config, re-resolve effective config, and rebuild the LLM provider.
    ///
    /// Returns `Err` if the config fails validation (`into_effective`). On success,
    /// all three state updates are applied atomically. A provider rebuild failure
    /// is logged but does not propagate — the raw and effective configs remain updated.
    pub fn apply_raw_config_and_rebuild(
        &self,
        config: astrcode_core::config::Config,
    ) -> Result<(), astrcode_core::config::ResolveError> {
        let new_effective = config.clone().into_effective()?;
        {
            let mut guard = self.write_raw_config();
            *guard = config;
        }
        {
            let mut guard = self.effective.write();
            *guard = new_effective;
        }
        if let Err(e) = self.rebuild_provider_from_effective() {
            tracing::warn!("provider rebuild after config update failed: {e}");
        }
        Ok(())
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
/// 工具表现在是 session 级快照，由 [`build_tool_registry_snapshot`] 在
/// 创建/恢复 session 时按对应 working_dir 单独构建。
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

    // 2. 构建 LLM provider。
    //
    // 根据 `provider_kind` 路由到对应的 provider 实现。
    // 后续所有主会话和子会话都会共享这个 provider。
    let llm_provider = Arc::new(RwLock::new(build_provider_from_effective(&effective)));

    // 3. 初始化上下文组装器。
    let context_settings = effective.context.clone();
    let context_assembler = Arc::new(LlmContextAssembler::new(context_settings));
    let auto_compact_failures = Arc::new(AutoCompactFailureTracker::default());
    let background_tasks = Arc::new(Mutex::new(BackgroundTaskManager::default()));

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
    let session_manager = Arc::new(SessionManager::new(store));

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
    extension_runner
        .register(astrcode_extension_agent_tools::extension())
        .await;
    extension_runner
        .register(astrcode_extension_mcp::extension())
        .await;
    extension_runner
        .register(astrcode_extension_skill::extension())
        .await;
    extension_runner
        .register(astrcode_extension_todo_tool::extension())
        .await;
    extension_runner
        .register(astrcode_extension_mode::extension())
        .await;
    for ext in load_result.extensions {
        extension_runner.register(ext).await;
    }
    for err in &load_result.errors {
        tracing::warn!("Extension load error: {err}");
    }

    // 共享的 agent_session_control slot，runtime 和 spawner 都读它。
    let agent_session_control_slot: Arc<RwLock<Option<Arc<dyn AgentSessionControl>>>> =
        Arc::new(RwLock::new(None));

    // 7. 给扩展运行时绑定”创建子会话”的宿主能力。
    //
    // 扩展本身不能直接拿到 SessionManager；当扩展工具返回 RunSession 声明式结果时，
    // 绑定会话派生器，使扩展工具可通过 RunSession 声明式结果创建子 session。
    // 子 session 也会生成自己的工具快照，而不是复用父会话或启动期的工具表。
    extension_runner.bind(Arc::new(ServerSessionSpawner {
        session_manager: Arc::clone(&session_manager),
        llm_provider: Arc::clone(&llm_provider),
        context_assembler: Arc::clone(&context_assembler),
        auto_compact_failures: Arc::clone(&auto_compact_failures),
        background_tasks: Arc::clone(&background_tasks),
        extension_runner: Arc::clone(&extension_runner),
        read_timeout_secs: effective.llm.read_timeout_secs,
        agent_session_control: Arc::clone(&agent_session_control_slot),
    }));

    // 8. 返回运行时容器。
    //
    // ServerRuntime 保存的是”共享基础设施”：session、LLM、prompt、扩展、
    // 配置和上下文预算。注意这里故意没有 tool_registry：
    // 工具表是 session 级别的快照，不再是 bootstrap 级全局单例。
    Ok(ServerRuntime {
        session_manager,
        llm_provider,
        context_assembler,
        auto_compact_failures,
        background_tasks,
        extension_runner,
        effective: RwLock::new(effective),
        config_store: Arc::new(config_store),
        raw_config: RwLock::new(config),
        shutdown_token: tokio_util::sync::CancellationToken::new(),
        agent_session_control: agent_session_control_slot,
    })
}

fn build_provider_from_effective(effective: &EffectiveConfig) -> Arc<dyn LlmProvider> {
    let llm_config = LlmClientConfig {
        base_url: effective.llm.base_url.clone(),
        api_key: effective.llm.api_key.clone(),
        connect_timeout_secs: effective.llm.connect_timeout_secs,
        read_timeout_secs: effective.llm.read_timeout_secs,
        max_retries: effective.llm.max_retries,
        retry_base_delay_ms: effective.llm.retry_base_delay_ms,
        temperature: effective.llm.temperature,
        reasoning: effective.llm.reasoning,
        supports_prompt_cache_key: effective.llm.supports_prompt_cache_key,
        prompt_cache_retention: effective.llm.prompt_cache_retention,
        extra_headers: Default::default(),
    };
    create_provider(
        &effective.llm.provider_kind,
        llm_config,
        effective.llm.api_mode,
        effective.llm.model_id.clone(),
        Some(effective.llm.max_tokens),
        Some(effective.llm.context_limit),
    )
}

/// 构建一个工作目录绑定的工具表快照。
///
/// 每次新建/恢复 session 时调用一次；工具执行期间只读取这份快照，
/// 不再维护运行中的动态工具层。
pub(crate) async fn build_tool_registry_snapshot(
    extension_runner: &ExtensionRunner,
    working_dir: &str,
    timeout_secs: u64,
) -> Arc<ToolRegistry> {
    let mut tool_registry = ToolRegistry::new();

    for tool in builtin_tools(std::path::PathBuf::from(working_dir), timeout_secs) {
        tool_registry.register(tool);
    }

    // Extensions override builtins, and earlier registered extensions keep
    // precedence over later registered extensions with the same tool name.
    for tool in extension_runner
        .collect_tool_adapters_typed(working_dir)
        .await
        .into_iter()
        .rev()
    {
        tool_registry.register(tool);
    }

    Arc::new(tool_registry)
}

pub(crate) async fn build_system_prompt_snapshot_with_files(
    input: SystemPromptSnapshotInput<'_>,
) -> Result<(String, String), ExtensionError> {
    let SystemPromptSnapshotInput {
        extension_runner,
        session_id,
        working_dir,
        model_id,
        tools,
        extra_system_prompt,
        tool_prompt_metadata,
        prompt_files,
    } = input;

    let prompt_ctx = PromptBuildContext {
        session_id: session_id.to_string(),
        working_dir: working_dir.to_string(),
        model: ModelSelection::simple(model_id),
        tools: tools.to_vec(),
    };

    let contributions = extension_runner
        .collect_prompt_contributions_typed(prompt_ctx)
        .await?;

    let mut extension_blocks = Vec::new();
    for sp in contributions.system_prompts {
        extension_blocks.push(ExtensionPromptBlock {
            section: ExtensionSection::PlatformInstructions,
            content: sp,
        });
    }
    for instruction in contributions.additional_instructions {
        extension_blocks.push(ExtensionPromptBlock {
            section: ExtensionSection::AdditionalInstructions,
            content: instruction,
        });
    }
    for s in contributions.skills {
        extension_blocks.push(ExtensionPromptBlock {
            section: ExtensionSection::Skills,
            content: s,
        });
    }
    for a in contributions.agents {
        extension_blocks.push(ExtensionPromptBlock {
            section: ExtensionSection::Agents,
            content: a,
        });
    }
    let extra_instructions = extra_system_prompt.and_then(|s| {
        let trimmed = s.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    });

    // Merge extension prompt metadata with caller-provided metadata.
    let mut merged_metadata = tool_prompt_metadata;
    merged_metadata.extend(extension_runner.collect_tool_prompt_metadata_typed().await);

    let input = SystemPromptInput {
        working_dir: working_dir.to_string(),
        os: std::env::consts::OS.into(),
        shell: resolve_shell().name,
        date: chrono::Utc::now().format("%Y-%m-%d").to_string(),
        identity: prompt_files.identity,
        user_rules: prompt_files.user_rules,
        project_rules: prompt_files.project_rules,
        tools: tools.to_vec(),
        tool_prompt_metadata: merged_metadata,
        extension_blocks,
        extra_instructions,
    };

    let system_prompt = PromptComposer::new()
        .assemble(input)
        .await
        .system_prompt
        .unwrap_or_default();
    let fingerprint = prompt_fingerprint(&system_prompt);
    Ok((system_prompt, fingerprint))
}

pub(crate) async fn load_system_prompt_files(working_dir: &str) -> PromptFiles {
    let working_dir = std::path::PathBuf::from(working_dir);
    let fallback_dir = working_dir.clone();
    tokio::task::spawn_blocking(move || read_system_prompt_files(&working_dir))
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(error = %error, "prompt file preload task failed; reading inline");
            read_system_prompt_files(&fallback_dir)
        })
}

fn read_system_prompt_files(working_dir: &Path) -> PromptFiles {
    PromptFiles {
        identity: pipeline::load_identity_md(&pipeline::user_identity_md_path()),
        user_rules: pipeline::load_user_rules(&pipeline::user_agents_md_path()),
        project_rules: pipeline::load_project_rules(working_dir),
    }
}

pub(crate) fn fnv1a_hash_bytes(data: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub(crate) fn prompt_fingerprint(text: &str) -> String {
    format!("{:016x}", fnv1a_hash_bytes(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use astrcode_core::{
        extension::{Extension, Registrar, ToolHandler},
        tool::{ExecutionMode, ToolDefinition, ToolOrigin, ToolResult},
    };

    use super::*;

    struct StaticToolExtension {
        id: &'static str,
        tool_name: &'static str,
        description: &'static str,
    }

    #[async_trait::async_trait]
    impl Extension for StaticToolExtension {
        fn id(&self) -> &str {
            self.id
        }

        fn register(&self, reg: &mut Registrar) {
            reg.tool(
                ToolDefinition {
                    name: self.tool_name.into(),
                    description: self.description.into(),
                    parameters: serde_json::json!({"type": "object"}),
                    origin: ToolOrigin::Extension,
                    execution_mode: ExecutionMode::Sequential,
                },
                Arc::new(StaticToolHandler),
            );
        }
    }

    struct StaticToolHandler;

    #[async_trait::async_trait]
    impl ToolHandler for StaticToolHandler {
        async fn execute(
            &self,
            tool_name: &str,
            _arguments: serde_json::Value,
            _working_dir: &str,
            _ctx: &astrcode_core::tool::ToolExecutionContext,
        ) -> Result<ToolResult, astrcode_core::extension::ExtensionError> {
            Err(astrcode_core::extension::ExtensionError::NotFound(
                tool_name.into(),
            ))
        }
    }

    #[tokio::test]
    async fn child_extra_system_prompt_participates_in_snapshot_build() {
        let runner = ExtensionRunner::new(Duration::from_secs(1));
        let prompt_files = load_system_prompt_files(".").await;
        let (system_prompt, fingerprint) =
            build_system_prompt_snapshot_with_files(SystemPromptSnapshotInput {
                extension_runner: &runner,
                session_id: "session-1",
                working_dir: ".",
                model_id: "mock",
                tools: &[],
                extra_system_prompt: Some("child body"),
                tool_prompt_metadata: std::collections::HashMap::new(),
                prompt_files,
            })
            .await
            .unwrap();

        assert!(system_prompt.contains("child body"));
        assert!(!fingerprint.is_empty());
    }

    #[tokio::test]
    async fn tool_snapshot_precedence_is_explicit() {
        let runner = ExtensionRunner::new(Duration::from_secs(1));
        runner
            .register(Arc::new(StaticToolExtension {
                id: "first",
                tool_name: "shell",
                description: "first extension shell",
            }))
            .await;
        runner
            .register(Arc::new(StaticToolExtension {
                id: "second",
                tool_name: "shell",
                description: "second extension shell",
            }))
            .await;

        let registry = build_tool_registry_snapshot(&runner, ".", 1).await;
        let shell = registry.find_definition("shell").unwrap();

        assert_eq!(shell.origin, ToolOrigin::Extension);
        assert_eq!(shell.description, "first extension shell");
    }
}

/// 引导过程中可能出现的错误。
#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("Config: {0}")]
    Config(#[from] astrcode_core::config::ConfigStoreError),
    #[error("Resolve: {0}")]
    Resolve(#[from] astrcode_core::config::ResolveError),
}
