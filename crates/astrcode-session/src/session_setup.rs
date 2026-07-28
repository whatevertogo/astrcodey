//! Session 启动期资源构建：工具表快照与 system prompt 装配。
//!
//! 这两个动作以前在 server crate 的 `SessionManager` 内，但因为它们直接依赖
//! `Session` 自己的工具边界和事件日志（追加 `SystemPromptConfigured`），把它们搬到
//! session crate 后能让 Session 真正掌控自己的运行时。

use std::collections::HashMap;

use astrcode_core::{
    config::ModelSelection,
    prompt::{
        ExtensionPromptBlock, ExtensionSection, PromptFileProvider, PromptProvider,
        SystemPromptInput,
    },
    tool::{ToolDefinition, ToolPromptMetadata},
};
use astrcode_extension_sdk::{
    extension::{ExtensionError, PromptBuildContext},
    runtime_ports::{
        PromptContributor, ToolCatalogCompleteness, ToolCatalogProvider, ToolCatalogScope,
    },
};
use astrcode_support::{hash::hex_fingerprint, shell::resolve_shell};

use crate::{ToolRegistry, session::normalize_extra_system_prompt};

pub(crate) struct BuiltBaseToolRegistry {
    pub registry: ToolRegistry,
    pub completeness: ToolCatalogCompleteness,
    pub revision: u64,
}

/// 构建一个工作目录绑定的工具表快照。
///
/// Session 快照缓存未命中时调用；工具执行期间只读取构建出的快照。
///
/// 返回未应用 session 工具边界的完整工具表。Session 在此快照之上派生筛选后的
/// 不可变 registry，使工具边界变化不必重新执行动态工具发现。
pub(crate) async fn build_base_tool_registry(
    tool_catalog: &dyn ToolCatalogProvider,
    scope: &ToolCatalogScope,
) -> Result<BuiltBaseToolRegistry, ExtensionError> {
    let mut tool_registry = ToolRegistry::new();
    let catalog = tool_catalog.tool_catalog(scope).await?;
    for diagnostic in &catalog.diagnostics {
        tracing::warn!(
            working_dir = scope.working_dir,
            source = diagnostic.source,
            message = diagnostic.message,
            "tool catalog diagnostic"
        );
    }
    // Catalog tools are ordered from highest to lowest priority. Registering in
    // reverse preserves the first provider's value on duplicate names.
    for tool in catalog.tools.into_iter().rev() {
        tool_registry.register(tool);
    }

    Ok(BuiltBaseToolRegistry {
        registry: tool_registry,
        completeness: catalog.completeness,
        revision: catalog.revision,
    })
}

pub(crate) struct SystemPromptSnapshotInput<'a> {
    pub prompt_contributor: &'a dyn PromptContributor,
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

/// 收集扩展的 prompt 贡献。
///
/// 纯数据收集函数，不组装 prompt。调用方可自行决定如何与稳定前缀组合。
async fn collect_extension_prompt_blocks(
    prompt_contributor: &dyn PromptContributor,
    session_id: &str,
    working_dir: &str,
    model_id: &str,
    tools: &[ToolDefinition],
) -> Result<Vec<ExtensionPromptBlock>, ExtensionError> {
    let prompt_ctx = PromptBuildContext {
        session_id: session_id.to_string(),
        working_dir: working_dir.to_string(),
        model: ModelSelection::simple(model_id),
        tools: tools.to_vec(),
    };
    let contributions = prompt_contributor
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

    Ok(extension_blocks)
}

/// 构建 system prompt 文本与指纹。
///
/// 调用方决定是否要把结果写成 `SystemPromptConfigured` 事件。
pub(crate) async fn build_system_prompt_snapshot(
    input: SystemPromptSnapshotInput<'_>,
) -> Result<(String, String), ExtensionError> {
    let SystemPromptSnapshotInput {
        prompt_contributor,
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

    let extension_blocks = collect_extension_prompt_blocks(
        prompt_contributor,
        session_id,
        working_dir,
        model_id,
        tools,
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
        tool_prompt_metadata,
        extension_blocks,
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
