//! Session 启动期资源构建：工具表快照与 system prompt 装配。
//!
//! 这两个动作以前在 server crate 的 `SessionManager` 内，但因为它们直接依赖
//! `Session` 自己的 runtime（写回 tool_registry）和事件日志（追加 `SystemPromptConfigured`），
//! 把它们搬到 session crate 后能让 Session 真正掌控自己的运行时。

use std::collections::HashMap;

use astrcode_context::prompt_engine::{PromptEngine, PromptFiles};
use astrcode_core::{
    config::ModelSelection,
    extension::{ExtensionError, PromptBuildContext},
    prompt::{ExtensionPromptBlock, ExtensionSection, PromptProvider, SystemPromptInput},
    tool::{ToolDefinition, ToolPromptMetadata},
};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_support::{hash::hex_fingerprint, shell::resolve_shell};
use astrcode_tools::registry::{ToolRegistry, builtin_tools};

/// 构建一个工作目录绑定的工具表快照。
///
/// 每次新建/恢复 session 时调用一次；工具执行期间只读取这份快照，
/// 不再维护运行中的动态工具层。
pub async fn build_tool_registry_snapshot(
    extension_runner: &ExtensionRunner,
    working_dir: &str,
    timeout_secs: u64,
) -> ToolRegistry {
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

    tool_registry
}

pub struct SystemPromptSnapshotInput<'a> {
    pub extension_runner: &'a ExtensionRunner,
    pub session_id: &'a str,
    pub working_dir: &'a str,
    pub model_id: &'a str,
    pub tools: &'a [ToolDefinition],
    pub extra_system_prompt: Option<&'a str>,
    pub tool_prompt_metadata: HashMap<String, ToolPromptMetadata>,
    pub prompt_files: PromptFiles,
}

/// 构建 system prompt 文本与指纹。
///
/// 调用方决定是否要把结果写成 `SystemPromptConfigured` 事件。
pub async fn build_system_prompt_snapshot(
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
    let extra_instructions = extra_system_prompt.and_then(|s| {
        let trimmed = s.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    });

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

    let system_prompt = PromptEngine::new()
        .assemble(input)
        .await
        .system_prompt
        .unwrap_or_default();
    let fingerprint = hex_fingerprint(system_prompt.as_bytes());
    Ok((system_prompt, fingerprint))
}
