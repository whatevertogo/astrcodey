//! Session 启动期资源构建：工具表快照与 system prompt 装配。
//!
//! 这两个动作以前在 server crate 的 `SessionManager` 内，但因为它们直接依赖
//! `Session` 自己的工具边界和事件日志（追加 `SystemPromptConfigured`），把它们搬到
//! session crate 后能让 Session 真正掌控自己的运行时。

use std::collections::HashMap;

use astrcode_core::{
    config::ModelSelection,
    extension::{ExtensionError, PromptBuildContext, SessionToolSelection},
    prompt::{
        ExtensionPromptBlock, ExtensionSection, PromptFileProvider, PromptProvider,
        SystemPromptInput,
    },
    tool::{ToolDefinition, ToolPromptMetadata},
};
use astrcode_extension_sdk::{
    runtime_ports::{PromptContributor, ToolCatalogCompleteness, ToolCatalogProvider},
    tool_pack::{ToolPack, ToolPackScope},
};
use astrcode_support::{hash::hex_fingerprint, shell::resolve_shell};

use crate::{ToolRegistry, session::normalize_extra_system_prompt};

pub(crate) struct BuiltToolRegistry {
    pub registry: ToolRegistry,
    pub completeness: ToolCatalogCompleteness,
}

/// 构建一个工作目录绑定的工具表快照。
///
/// Session 快照缓存未命中时调用；工具执行期间只读取构建出的快照。
///
/// `tool_selection` 用于 session 的工具裁剪：
/// - `None`：保留所有 builtin + extension 工具。
/// - `Some(All)`：保留全集，但排除 `except` 中的工具。
/// - `Some(Only)`：仅保留 `names` 中的工具。空名单表示明确禁用全部工具。
///
/// 过滤在表构建末尾一次完成，确保 LLM schema、prompt 渲染、运行时白名单三处
/// 都看到同一份工具集。
pub(crate) async fn build_tool_registry_snapshot(
    tool_catalog: &dyn ToolCatalogProvider,
    tool_packs: &[std::sync::Arc<dyn ToolPack>],
    working_dir: &str,
    tool_selection: Option<&SessionToolSelection>,
) -> Result<BuiltToolRegistry, ExtensionError> {
    let mut tool_registry = ToolRegistry::new();
    let scope = ToolPackScope { working_dir };

    for pack in tool_packs {
        for tool in pack.tools(&scope) {
            tool_registry.register(tool);
        }
    }

    // Extensions override host tool packs, and earlier registered extensions
    // keep precedence over later registered extensions with the same tool name.
    let catalog = tool_catalog.tool_catalog(working_dir).await?;
    for diagnostic in &catalog.diagnostics {
        tracing::warn!(
            working_dir,
            extension_id = diagnostic.extension_id,
            error = diagnostic.message,
            "extension tool catalog is partial"
        );
    }
    for tool in catalog.tools.into_iter().rev() {
        tool_registry.register(tool);
    }

    if let Some(selection) = tool_selection {
        tool_registry.apply_tool_selection(selection);
    }

    Ok(BuiltToolRegistry {
        registry: tool_registry,
        completeness: catalog.completeness,
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

/// 扩展动态贡献的收集结果。
struct ExtensionPromptData {
    pub extension_blocks: Vec<ExtensionPromptBlock>,
    pub merged_tool_metadata: HashMap<String, ToolPromptMetadata>,
}

/// 收集扩展的 prompt 贡献。
///
/// 纯数据收集函数，不组装 prompt。调用方可自行决定如何与稳定前缀组合。
async fn collect_extension_prompt_data(
    prompt_contributor: &dyn PromptContributor,
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

    Ok(ExtensionPromptData {
        extension_blocks,
        merged_tool_metadata: base_tool_prompt_metadata,
    })
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

    let ext_data = collect_extension_prompt_data(
        prompt_contributor,
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
