//! 服务器引导模块 — 从配置组装所有服务。
//!
//! 负责在启动时初始化所有核心组件：LLM 提供者、提示词组装器、
//! 会话管理器、扩展运行器和上下文窗口设置。

use std::{sync::Arc, time::Duration};

use astrcode_ai::openai::OpenAiProvider;
use astrcode_context::{manager::LlmContextAssembler, settings::ContextWindowSettings};
use astrcode_core::{
    config::{ConfigStore, EffectiveConfig, ModelSelection},
    extension::ExtensionError,
    llm::{LlmClientConfig, LlmProvider},
    prompt::{ExtensionPromptBlock, ExtensionSection, PromptProvider, SystemPromptInput},
    tool::ToolDefinition,
};
use astrcode_extensions::{
    context::ServerExtensionContext, loader::ExtensionLoader, runner::ExtensionRunner,
};
use astrcode_prompt::{composer::PromptComposer, pipeline};
use astrcode_storage::config_store::FileConfigStore;
use astrcode_support::shell::resolve_shell;
use astrcode_tools::registry::{ToolRegistry, builtin_tools};

use crate::{
    agent::AutoCompactFailureTracker,
    session::{SessionManager, spawner::ServerSessionSpawner},
};

// ─── ServerRuntime ───────────────────────────────────────────────────────

/// 启动时组装的所有服务集合，按领域分组。
///
/// 这是服务器运行时的核心容器，持有所有共享服务的引用。
/// 各组件通过 `Arc` 共享，支持并发访问。
pub struct ServerRuntime {
    /// 会话管理器，负责会话的创建、恢复、事件追加和删除
    pub session_manager: Arc<SessionManager>,
    /// LLM 提供者，用于生成 AI 回复
    pub llm_provider: Arc<dyn LlmProvider>,
    /// 上下文组装器，负责窗口估算和摘要压缩
    pub context_assembler: Arc<LlmContextAssembler>,
    /// Auto compact provider 连续失败熔断状态。
    pub auto_compact_failures: Arc<AutoCompactFailureTracker>,
    /// 扩展运行器，负责加载和分发扩展钩子事件
    pub extension_runner: Arc<ExtensionRunner>,
    /// 已解析的最终配置（只读快照）
    pub effective: EffectiveConfig,
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
/// 8. 初始化上下文窗口管理
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
    let effective = config.into_effective()?;

    // 2. 构建 LLM provider。
    //
    // 这里把 EffectiveConfig 中的 LLM 参数转换成底层 OpenAI 兼容客户端配置。
    // 后续所有主会话和子会话都会共享这个 provider。
    let llm_config = LlmClientConfig {
        base_url: effective.llm.base_url.clone(),
        api_key: effective.llm.api_key.clone(),
        connect_timeout_secs: effective.llm.connect_timeout_secs,
        read_timeout_secs: effective.llm.read_timeout_secs,
        max_retries: effective.llm.max_retries,
        retry_base_delay_ms: effective.llm.retry_base_delay_ms,
        temperature: effective.llm.temperature,
        supports_prompt_cache_key: effective.llm.supports_prompt_cache_key,
        prompt_cache_retention: effective.llm.prompt_cache_retention,
        extra_headers: Default::default(),
    };
    let llm_provider: Arc<dyn LlmProvider> = Arc::new(OpenAiProvider::new(
        llm_config,
        effective.llm.api_mode,
        effective.llm.model_id.clone(),
        Some(effective.llm.max_tokens),
        Some(effective.llm.context_limit),
    ));

    // 3. 初始化上下文组装器。
    let context_settings = ContextWindowSettings::default();
    let context_assembler = Arc::new(LlmContextAssembler::new(context_settings.clone()));
    let auto_compact_failures = Arc::new(AutoCompactFailureTracker::default());

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
    // 正常启动按项目路径 hash 选择文件系统会话仓库。
    //
    // InMemoryEventStore 仅在 `testing` feature 启用时可用；生产二进制始终使用
    // FileSystemSessionRepository，与 opts.config_path 无关。
    let project_hash = astrcode_core::types::project_hash_from_path(&cwd);
    #[cfg(feature = "testing")]
    let store: Arc<dyn astrcode_core::storage::EventStore> = if opts.config_path.is_some() {
        Arc::new(astrcode_storage::in_memory::InMemoryEventStore::new())
    } else {
        Arc::new(astrcode_storage::session_repo::FileSystemSessionRepository::new(project_hash))
    };
    #[cfg(not(feature = "testing"))]
    let store: Arc<dyn astrcode_core::storage::EventStore> =
        Arc::new(astrcode_storage::session_repo::FileSystemSessionRepository::new(project_hash));
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
    let extension_runner = Arc::new(ExtensionRunner::new(
        Duration::from_secs(30),
        load_result.runtime,
    ));
    extension_runner
        .register(astrcode_extension_agent_tools::extension())
        .await;
    extension_runner
        .register(astrcode_extension_todo_tool::extension())
        .await;
    for ext in load_result.extensions {
        extension_runner.register(ext).await;
    }
    for err in &load_result.errors {
        tracing::warn!("Extension load error: {err}");
    }

    // 7. 给扩展运行时绑定“创建子会话”的宿主能力。
    //
    // 扩展本身不能直接拿到 SessionManager；当扩展工具返回 RunSession 声明式结果时，
    // ExtensionRuntime 会回调这个 ServerSessionSpawner，让服务器创建子 session。
    // 子 session 也会生成自己的工具快照，而不是复用父会话或启动期的工具表。
    extension_runner.bind(Arc::new(ServerSessionSpawner {
        session_manager: Arc::clone(&session_manager),
        llm: Arc::clone(&llm_provider),
        context_assembler: Arc::clone(&context_assembler),
        auto_compact_failures: Arc::clone(&auto_compact_failures),
        extension_runner: Arc::clone(&extension_runner),
        read_timeout_secs: effective.llm.read_timeout_secs,
    }));

    // 9. 返回运行时容器。
    //
    // ServerRuntime 保存的是“共享基础设施”：session、LLM、prompt、扩展、
    // 配置和上下文预算。注意这里故意没有 tool_registry：
    // 工具表是 session 级别的快照，不再是 bootstrap 级全局单例。
    Ok(ServerRuntime {
        session_manager,
        llm_provider,
        context_assembler,
        auto_compact_failures,
        extension_runner,
        effective,
    })
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
        .collect_tool_adapters(working_dir)
        .await
        .into_iter()
        .rev()
    {
        tool_registry.register(tool);
    }

    Arc::new(tool_registry)
}

/// 构建完整的 session 级 system prompt 快照。
///
/// `PromptBuild` 扩展钩子、内置 prompt composer 和可选的子 agent 指令
/// 都只在这里汇合一次。调用方应把结果写入 eventlog，后续回合直接复用。
pub(crate) async fn build_system_prompt_snapshot(
    extension_runner: &ExtensionRunner,
    session_id: &str,
    working_dir: &str,
    model_id: &str,
    tools: &[ToolDefinition],
    extra_system_prompt: Option<&str>,
) -> Result<(String, String), ExtensionError> {
    let mut ext_ctx = ServerExtensionContext::new(
        session_id.to_string(),
        working_dir.to_string(),
        ModelSelection {
            profile_name: String::new(),
            model: model_id.to_string(),
            provider_kind: String::new(),
        },
    );
    ext_ctx.set_tools(
        tools
            .iter()
            .map(|tool| (tool.name.clone(), tool.clone()))
            .collect(),
    );

    let contributions = extension_runner
        .collect_prompt_contributions(&ext_ctx)
        .await?;

    let mut extension_blocks = Vec::new();
    for sp in contributions.system_prompts {
        extension_blocks.push(ExtensionPromptBlock {
            section: ExtensionSection::PlatformInstructions,
            content: sp,
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
    let extra_instructions = non_empty(extra_system_prompt.unwrap_or_default());

    let identity = pipeline::load_identity_md(&pipeline::user_identity_md_path());
    let project_rules = pipeline::load_project_rules(std::path::Path::new(working_dir));

    let input = SystemPromptInput {
        working_dir: working_dir.to_string(),
        os: std::env::consts::OS.into(),
        shell: resolve_shell().name,
        date: chrono::Utc::now().format("%Y-%m-%d").to_string(),
        identity,
        user_rules: None,
        project_rules,
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

fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub(crate) fn prompt_fingerprint(text: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use astrcode_core::{
        extension::{Extension, ExtensionContext, ExtensionError, ExtensionEvent, HookEffect},
        tool::{ToolDefinition, ToolOrigin},
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

        fn hook_subscriptions(&self) -> Vec<astrcode_core::extension::HookSubscription> {
            Vec::new()
        }

        async fn on_event(
            &self,
            _event: ExtensionEvent,
            _ctx: &dyn ExtensionContext,
        ) -> Result<HookEffect, ExtensionError> {
            Ok(HookEffect::Allow)
        }

        fn tools(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: self.tool_name.into(),
                description: self.description.into(),
                parameters: serde_json::json!({"type": "object"}),
                origin: ToolOrigin::Extension,
            }]
        }
    }

    #[tokio::test]
    async fn child_extra_system_prompt_participates_in_snapshot_build() {
        let runner = ExtensionRunner::new(
            Duration::from_secs(1),
            Arc::new(astrcode_extensions::runtime::ExtensionRuntime::new()),
        );
        let (system_prompt, fingerprint) = build_system_prompt_snapshot(
            &runner,
            "session-1",
            ".",
            "mock",
            &[],
            Some("child body"),
        )
        .await
        .unwrap();

        assert!(system_prompt.contains("child body"));
        assert!(!fingerprint.is_empty());
    }

    #[tokio::test]
    async fn tool_snapshot_precedence_is_explicit() {
        let runner = ExtensionRunner::new(
            Duration::from_secs(1),
            Arc::new(astrcode_extensions::runtime::ExtensionRuntime::new()),
        );
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
