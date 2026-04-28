//! 服务器引导模块 — 从配置组装所有服务。
//!
//! 负责在启动时初始化所有核心组件：LLM 提供者、提示词组装器、
//! 会话管理器、扩展运行器和上下文窗口管理。

use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use astrcode_ai::openai::OpenAiProvider;
use astrcode_context::{
    budget::ToolResultBudget, file_access::FileAccessTracker, settings::ContextWindowSettings,
};
use astrcode_core::{
    config::{ConfigStore, EffectiveConfig, ModelSelection},
    extension::ExtensionError,
    llm::{LlmClientConfig, LlmProvider},
    prompt::{PromptContext, PromptProvider},
    tool::ToolDefinition,
};
use astrcode_extensions::{
    context::ServerExtensionContext, loader::ExtensionLoader, runner::ExtensionRunner,
};
use astrcode_storage::config_store::FileConfigStore;
use astrcode_support::shell::resolve_shell;
use astrcode_tools::registry::ToolRegistry;

use crate::{session::SessionManager, session_spawner::ServerSessionSpawner};

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
    /// 提示词组装器，负责构建发送给 LLM 的系统提示词
    pub prompt_provider: Arc<dyn PromptProvider>,
    /// 扩展运行器，负责加载和分发扩展钩子事件
    pub extension_runner: Arc<ExtensionRunner>,
    /// 已解析的最终配置（只读快照）
    pub effective: EffectiveConfig,
    /// 上下文窗口管理设置
    pub context_settings: ContextWindowSettings,
    /// 工具结果预算控制器，限制工具返回数据的大小
    pub tool_result_budget: Arc<ToolResultBudget>,
    /// 文件访问追踪器，记录 Agent 访问过的文件
    pub file_access_tracker: Arc<std::sync::Mutex<FileAccessTracker>>,
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
        extra_headers: Default::default(),
    };
    let llm_provider: Arc<dyn LlmProvider> = Arc::new(OpenAiProvider::new(
        llm_config,
        effective.llm.api_mode,
        effective.llm.model_id.clone(),
        Some(effective.llm.max_tokens),
        Some(effective.llm.context_limit),
    ));

    // 3. 构建 prompt provider。
    //
    // PromptComposer 只负责组装系统提示词和上下文消息，不持有会话状态。
    let prompt_provider: Arc<dyn PromptProvider> =
        Arc::new(astrcode_prompt::composer::PromptComposer::new());

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
    // 测试启动使用内存空存储，避免污染真实会话目录；
    // 正常启动按项目路径 hash 选择文件系统会话仓库。
    let project_hash = astrcode_core::types::project_hash_from_path(&cwd);
    let store: Arc<dyn astrcode_core::storage::EventStore> = if opts.config_path.is_some() {
        Arc::new(astrcode_storage::noop::NoopEventStore::new())
    } else {
        Arc::new(astrcode_storage::session_repo::FileSystemSessionRepository::new(project_hash))
    };
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
        .register(astrcode_extension_task_tools::extension())
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
        prompt: Arc::clone(&prompt_provider),
        extension_runner: Arc::clone(&extension_runner),
        read_timeout_secs: effective.llm.read_timeout_secs,
    }));

    // 8. 初始化上下文窗口相关的共享状态。
    //
    // 这些对象用于控制工具结果裁剪、文件访问追踪和上下文恢复预算。
    // 它们是跨会话共享的服务，但内部会按具体 Agent 回合使用。
    let context_settings = ContextWindowSettings::default();
    let tool_result_budget = Arc::new(ToolResultBudget::new(
        context_settings.summary_reserve_tokens * 3, // aggregate
        context_settings.max_tracked_files * 1024,   // inline
        context_settings.recovery_token_budget * 3,  // preview
    ));
    let file_access_tracker = Arc::new(std::sync::Mutex::new(FileAccessTracker::new(
        context_settings.max_tracked_files,
    )));

    // 9. 返回运行时容器。
    //
    // ServerRuntime 保存的是“共享基础设施”：session、LLM、prompt、扩展、
    // 配置和上下文预算。注意这里故意没有 tool_registry：
    // 工具表是 session 级别的快照，不再是 bootstrap 级全局单例。
    Ok(ServerRuntime {
        session_manager,
        llm_provider,
        prompt_provider,
        extension_runner,
        effective,
        context_settings,
        tool_result_budget,
        file_access_tracker,
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
    tool_registry.register_builtins(PathBuf::from(working_dir), timeout_secs);

    let extension_tools = extension_runner.collect_tool_adapters(working_dir).await;
    // Preserve the old precedence: extension tools override built-ins, and
    // earlier extensions win when duplicate extension tool names exist.
    for tool in extension_tools.into_iter().rev() {
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
    prompt_provider: &dyn PromptProvider,
    session_id: &str,
    working_dir: &str,
    model_id: &str,
    tools: &[ToolDefinition],
    extra_system_prompt: Option<&str>,
) -> Result<(String, String), ExtensionError> {
    let mut custom =
        collect_prompt_custom_sections(extension_runner, session_id, working_dir, model_id, tools)
            .await?;

    if let Some(extra) = non_empty(extra_system_prompt.unwrap_or_default()) {
        append_custom_section(&mut custom, "system_prompts", extra);
    }

    let prompt_ctx = PromptContext {
        working_dir: working_dir.to_string(),
        os: std::env::consts::OS.into(),
        shell: resolve_shell().name,
        date: chrono::Utc::now().format("%Y-%m-%d").to_string(),
        available_tools: tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>()
            .join(", "),
        custom,
    };

    let plan = prompt_provider.assemble(prompt_ctx).await;
    let system_prompt = plan.system_prompt.unwrap_or_default();
    let fingerprint = prompt_fingerprint(&system_prompt);
    Ok((system_prompt, fingerprint))
}

/// 构建一次 prompt composer 可消费的插件提示 section。
///
/// 这里是 extensions 与 prompt 系统之间的桥接层：插件返回结构化的
/// `PromptContributions`，server 将其映射成 prompt crate 能消费的通用
/// `custom` section 文本。prompt crate 不直接知道这些文本来自插件。
async fn collect_prompt_custom_sections(
    extension_runner: &ExtensionRunner,
    session_id: &str,
    working_dir: &str,
    model_id: &str,
    tools: &[ToolDefinition],
) -> Result<BTreeMap<String, String>, ExtensionError> {
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

    let prompt_contributions = extension_runner
        .collect_prompt_contributions(&ext_ctx)
        .await?;
    let mut custom_sections = BTreeMap::new();
    // 这些 key 是 server 到 prompt composer 的内部约定，不属于 extension API。
    if let Some(system_prompts) = join_prompt_parts(prompt_contributions.system_prompts) {
        custom_sections.insert("system_prompts".to_string(), system_prompts);
    }
    if let Some(skills) = join_prompt_parts(prompt_contributions.skills) {
        custom_sections.insert("skills".to_string(), skills);
    }
    if let Some(agents) = join_prompt_parts(prompt_contributions.agents) {
        custom_sections.insert("agents".to_string(), agents);
    }

    Ok(custom_sections)
}

fn append_custom_section(custom: &mut BTreeMap<String, String>, key: &str, value: String) {
    custom
        .entry(key.to_string())
        .and_modify(|existing| {
            if !existing.trim().is_empty() {
                existing.push_str("\n\n");
            }
            existing.push_str(&value);
        })
        .or_insert(value);
}

fn join_prompt_parts(parts: Vec<String>) -> Option<String> {
    let text = parts
        .into_iter()
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    (!text.is_empty()).then_some(text)
}

fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn prompt_fingerprint(text: &str) -> String {
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

    use astrcode_core::prompt::{PromptContext, PromptPlan};

    use super::*;

    struct EchoSystemPrompts;

    #[async_trait::async_trait]
    impl PromptProvider for EchoSystemPrompts {
        async fn assemble(&self, context: PromptContext) -> PromptPlan {
            PromptPlan::from_system_prompt(
                context
                    .custom
                    .get("system_prompts")
                    .cloned()
                    .unwrap_or_default(),
            )
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
            &EchoSystemPrompts,
            "session-1",
            ".",
            "mock",
            &[],
            Some("child body"),
        )
        .await
        .unwrap();

        assert_eq!(system_prompt, "child body");
        assert!(!fingerprint.is_empty());
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
