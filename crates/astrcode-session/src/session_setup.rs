//! Session 启动期资源构建：工具表快照与 system prompt 装配。
//!
//! 这两个动作以前在 server crate 的 `SessionManager` 内，但因为它们直接依赖
//! `Session` 自己的 runtime（写回 tool_registry）和事件日志（追加 `SystemPromptConfigured`），
//! 把它们搬到 session crate 后能让 Session 真正掌控自己的运行时。

use std::collections::HashMap;

use astrcode_core::{
    config::ModelSelection,
    extension::{ChildToolPolicy, ExtensionError, PromptBuildContext},
    prompt::{
        ExtensionPromptBlock, ExtensionSection, PromptFileProvider, PromptProvider,
        SystemPromptInput,
    },
    tool::{ToolDefinition, ToolPromptMetadata},
};
use astrcode_kernel::{ExtensionRuntime, ToolPack, ToolPackScope, ToolRegistry};
use astrcode_support::{hash::hex_fingerprint, shell::resolve_shell};

use crate::session::normalize_extra_system_prompt;

/// 构建一个工作目录绑定的工具表快照。
///
/// 每次新建/恢复 session 时调用一次；工具执行期间只读取这份快照，
/// 不再维护运行中的动态工具层。
///
/// `tool_policy` 用于子 session 的工具裁剪：
/// - `None`：保留父全集（即所有 builtin + extension 工具）。
/// - `Some(Deny)`：从全集排除指定工具。
/// - `Some(Allow)`：仅保留指定工具。空白名单视为非法配置（spawner 应在调用前拦截）。
///
/// 过滤在表构建末尾一次完成，确保 LLM schema、prompt 渲染、运行时白名单三处
/// 都看到同一份工具集。
pub async fn build_tool_registry_snapshot(
    extension_runner: &dyn ExtensionRuntime,
    tool_packs: &[std::sync::Arc<dyn ToolPack>],
    working_dir: &str,
    timeout_secs: u64,
    tool_policy: Option<&ChildToolPolicy>,
) -> ToolRegistry {
    let mut tool_registry = ToolRegistry::new();
    let scope = ToolPackScope {
        working_dir,
        shell_timeout_secs: timeout_secs,
    };

    for pack in tool_packs {
        for tool in pack.tools(&scope) {
            tool_registry.register(tool);
        }
    }

    // Extensions override host tool packs, and earlier registered extensions
    // keep precedence over later registered extensions with the same tool name.
    for tool in extension_runner
        .collect_tool_adapters(working_dir)
        .await
        .into_iter()
        .rev()
    {
        tool_registry.register(tool);
    }

    if let Some(policy) = tool_policy {
        apply_child_tool_policy(&mut tool_registry, policy);
    }

    tool_registry
}

/// 按 [`ChildToolPolicy`] 裁剪工具表。
/// TODO: 更好的方式？支持子智能体自定义工具？
/// `Deny` 直接 `unregister`；`Allow` 把不在白名单里的工具全部 `unregister`。
/// 命中不存在的工具名只打 debug 日志，不报错——插件可能针对多版本宿主写策略。
fn apply_child_tool_policy(registry: &mut ToolRegistry, policy: &ChildToolPolicy) {
    *registry = registry.clone_with_child_policy(Some(policy));
}

pub struct SystemPromptSnapshotInput<'a> {
    pub extension_runner: &'a dyn ExtensionRuntime,
    pub prompt_provider: &'a dyn PromptProvider,
    pub prompt_file_provider: &'a dyn PromptFileProvider,
    pub session_id: &'a str,
    pub working_dir: &'a str,
    pub model_id: &'a str,
    pub tools: &'a [ToolDefinition],
    pub extra_system_prompt: Option<&'a str>,
    pub tool_prompt_metadata: HashMap<String, ToolPromptMetadata>,
    pub include_agents_rules: bool,
}

/// 扩展动态贡献的收集结果（extension blocks + merged tool metadata）。
pub struct ExtensionPromptData {
    pub extension_blocks: Vec<ExtensionPromptBlock>,
    pub merged_tool_metadata: HashMap<String, ToolPromptMetadata>,
}

/// 收集扩展的 prompt 贡献（extension blocks + tool prompt metadata）。
///
/// 纯数据收集函数，不组装 prompt。调用方可自行决定如何与稳定前缀组合。
pub async fn collect_extension_prompt_data(
    extension_runner: &dyn ExtensionRuntime,
    session_id: &str,
    working_dir: &str,
    model_id: &str,
    tools: &[ToolDefinition],
    base_tool_prompt_metadata: HashMap<String, ToolPromptMetadata>,
) -> Result<ExtensionPromptData, ExtensionError> {
    let prompt_ctx = PromptBuildContext {
        session_id: session_id.to_string(),
        working_dir: working_dir.to_string(),
        model: ModelSelection::simple(model_id),
        tools: tools.to_vec(),
    };
    let contributions = extension_runner
        .collect_prompt_contributions(prompt_ctx)
        .await?;

    let mut extension_blocks = Vec::new();
    for content in contributions.system_prompts {
        extension_blocks.push(ExtensionPromptBlock {
            section: ExtensionSection::PlatformInstructions,
            content,
        });
    }
    for content in contributions.additional_instructions {
        extension_blocks.push(ExtensionPromptBlock {
            section: ExtensionSection::AdditionalInstructions,
            content,
        });
    }
    for content in contributions.skills {
        extension_blocks.push(ExtensionPromptBlock {
            section: ExtensionSection::Skills,
            content,
        });
    }
    for content in contributions.agents {
        extension_blocks.push(ExtensionPromptBlock {
            section: ExtensionSection::Agents,
            content,
        });
    }

    let mut merged_metadata = base_tool_prompt_metadata;
    merged_metadata.extend(extension_runner.collect_tool_prompt_metadata().await);

    Ok(ExtensionPromptData {
        extension_blocks,
        merged_tool_metadata: merged_metadata,
    })
}

/// 构建 system prompt 文本与指纹。
///
/// 调用方决定是否要把结果写成 `SystemPromptConfigured` 事件。
pub async fn build_system_prompt_snapshot(
    input: SystemPromptSnapshotInput<'_>,
) -> Result<(String, String), ExtensionError> {
    let SystemPromptSnapshotInput {
        extension_runner,
        prompt_provider,
        prompt_file_provider,
        session_id,
        working_dir,
        model_id,
        tools,
        extra_system_prompt,
        tool_prompt_metadata,
        include_agents_rules,
    } = input;

    let ext_data = collect_extension_prompt_data(
        extension_runner,
        session_id,
        working_dir,
        model_id,
        tools,
        tool_prompt_metadata,
    )
    .await?;

    let extra_instructions = normalize_extra_system_prompt(extra_system_prompt);
    let prompt_files = prompt_file_provider
        .load(working_dir, include_agents_rules)
        .await;

    let prompt_input = SystemPromptInput {
        working_dir: working_dir.to_string(),
        os: std::env::consts::OS.into(),
        shell: resolve_shell().name,
        gh_cli_available: astrcode_support::shell::is_gh_cli_available(),
        identity: prompt_files.identity,
        user_rules: prompt_files.user_rules,
        project_rules: prompt_files.project_rules,
        tools: tools.to_vec(),
        tool_prompt_metadata: ext_data.merged_tool_metadata,
        extension_blocks: ext_data.extension_blocks,
        extra_instructions,
    };

    let system_prompt = prompt_provider
        .assemble(prompt_input)
        .await
        .system_prompt
        .unwrap_or_default();
    let fingerprint = hex_fingerprint(system_prompt.as_bytes());
    Ok((system_prompt, fingerprint))
}
