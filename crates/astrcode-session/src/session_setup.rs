//! Session 启动期资源构建：工具表快照与 system prompt 装配。
//!
//! 这两个动作以前在 server crate 的 `SessionManager` 内，但因为它们直接依赖
//! `Session` 自己的工具边界和事件日志（追加 `SystemPromptConfigured`），把它们搬到
//! session crate 后能让 Session 真正掌控自己的运行时。

use std::{collections::HashMap, sync::OnceLock};

use astrcode_context::prompt_engine::{
    ExtensionPromptBlock, ExtensionSection, PromptEngine, SystemPromptInput, load_prompt_files,
};
use astrcode_core::{
    config::ModelSelection,
    tool::{ToolDefinition, ToolPromptMetadata},
};
use astrcode_extension_sdk::{
    extension::{ExtensionError, PromptBuildContext},
    runtime_ports::{
        PromptContributor, ToolCatalogCompleteness, ToolCatalogProvider, ToolCatalogScope,
    },
    shell::resolve_shell,
};

use crate::ToolRegistry;

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
    for tool in catalog.tools {
        // `ToolRegistryError` 只有工具名，不携带扩展来源（catalog 是合并后的基础表，
        // 此处无法填 ToolConflict/InvalidRegistration 所需的 extension_id），且调用方
        // 通过 `ExtensionError` 传播——压平为 Internal 是有意的边界转换。
        tool_registry
            .register(tool)
            .map_err(|error| ExtensionError::Internal(error.to_string()))?;
    }

    Ok(BuiltBaseToolRegistry {
        registry: tool_registry,
        completeness: catalog.completeness,
        revision: catalog.revision,
    })
}

pub(crate) struct SystemPromptSnapshotInput<'a> {
    pub prompt_contributor: &'a dyn PromptContributor,
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
/// 参数与 [`SystemPromptSnapshotInput`] 的前 5 个字段完全重叠，直接收整个
/// 输入结构体，避免两处字段列表各自演变导致不一致。
async fn collect_extension_prompt_blocks(
    input: &SystemPromptSnapshotInput<'_>,
) -> Result<Vec<ExtensionPromptBlock>, ExtensionError> {
    let prompt_ctx = PromptBuildContext {
        session_id: input.session_id.to_string(),
        working_dir: input.working_dir.to_string(),
        model: ModelSelection::simple(input.model_id),
        tools: input.tools.to_vec(),
    };
    let contributions = input
        .prompt_contributor
        .collect_prompt_contributions(prompt_ctx)
        .await?;

    let mut extension_blocks = Vec::new();
    let sections = [
        (
            contributions.system_prompts,
            ExtensionSection::PlatformInstructions,
        ),
        (
            contributions.additional_instructions,
            ExtensionSection::AdditionalInstructions,
        ),
        (contributions.skills, ExtensionSection::Skills),
        (contributions.agents, ExtensionSection::Agents),
    ];
    for (contents, section) in sections {
        extension_blocks.extend(
            contents
                .into_iter()
                .map(|content| ExtensionPromptBlock { section, content }),
        );
    }

    Ok(extension_blocks)
}

/// 构建 system prompt 文本与指纹。
///
/// 调用方决定是否要把结果写成 `SystemPromptConfigured` 事件。
pub(crate) async fn build_system_prompt_snapshot(
    input: SystemPromptSnapshotInput<'_>,
) -> Result<(String, String), ExtensionError> {
    let extension_blocks = collect_extension_prompt_blocks(&input).await?;

    let SystemPromptSnapshotInput {
        working_dir,
        tools,
        extra_system_prompt,
        tool_prompt_metadata,
        include_agents_rules,
        ..
    } = input;

    let extra_instructions = extra_system_prompt.map(str::to_owned);
    let prompt_files = load_prompt_files(working_dir, include_agents_rules).await;

    let prompt_input = SystemPromptInput {
        working_dir: working_dir.to_string(),
        os: std::env::consts::OS.into(),
        shell: resolve_shell().name,
        gh_cli_available: is_gh_cli_available(),
        identity: prompt_files.identity,
        user_rules: prompt_files.user_rules,
        project_rules: prompt_files.project_rules,
        tools,
        tool_prompt_metadata,
        extension_blocks,
        extra_instructions,
    };

    let system_prompt = PromptEngine.assemble(&prompt_input);
    let fingerprint = system_prompt_fingerprint(&system_prompt);
    Ok((system_prompt, fingerprint))
}

/// 手写 FNV-1a（64 位）而非 `std::collections::hash_map::DefaultHasher`：指纹会被
/// 持久化为 `SystemPromptConfigured` 事件并在后续进程/构建中比较，必须跨进程、
/// 跨构建稳定；`DefaultHasher` 每次进程启动都随机化 seed，结果不可复现。
fn system_prompt_fingerprint(system_prompt: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for &byte in system_prompt.as_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn is_gh_cli_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let names: &[&str] = if cfg!(windows) {
            &["gh.exe", "gh"]
        } else {
            &["gh"]
        };
        std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path)
                .any(|dir| names.iter().any(|name| dir.join(name).is_file()))
        })
    })
}
