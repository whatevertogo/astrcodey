//! 能力（工具）指南贡献者。
//!
//! 从 [`CapabilitySpec`] 中提取工具的 prompt 元数据，
//! 生成工具摘要块和详细指南块。
//!
//! # 设计原则
//!
//! - 外部 MCP / plugin 工具仅保留粗略摘要，不展开详细指南
//! - 内置工具默认进入稳定摘要，只有工具发现和协作类工具展开详细指南
//! - 只负责工具指南；运行时 system prompt 指令由独立 contributor 承接

use astrcode_core::CapabilitySpec;
use astrcode_runtime_contract::tool::ToolPromptMetadata;
use async_trait::async_trait;

use crate::{BlockKind, BlockSpec, PromptContext, PromptContribution, PromptContributor};

pub struct CapabilityPromptContributor;

#[async_trait]
impl PromptContributor for CapabilityPromptContributor {
    fn contributor_id(&self) -> &'static str {
        "capability-prompt"
    }

    fn cache_version(&self) -> u64 {
        7
    }

    fn cache_fingerprint(&self, ctx: &PromptContext) -> String {
        ctx.contributor_cache_fingerprint()
    }

    async fn contribute(&self, ctx: &PromptContext) -> PromptContribution {
        let mut blocks = Vec::new();

        // 带 prompt 元数据的工具（通常为 builtin/internal）。
        let tool_guides = collect_tool_guides(&ctx.capability_specs);
        let internal_guides = tool_guides
            .iter()
            .filter(|guide| !is_external_tool(&guide.spec))
            .cloned()
            .collect::<Vec<_>>();
        // 标记为 source:mcp 或 source:plugin 的外部工具，只保留摘要。
        let external_tools = collect_external_tools(&ctx.capability_specs);

        if !internal_guides.is_empty() || !external_tools.is_empty() {
            blocks.push(build_tool_summary_block(&internal_guides, &external_tools));
        }

        blocks.extend(
            internal_guides
                .iter()
                .filter(|guide| should_render_detailed_tool_guide(guide))
                .map(build_detailed_tool_block),
        );

        if !external_tools.is_empty() {
            blocks.push(build_tool_search_workflow_block());
        }

        PromptContribution {
            blocks,
            ..PromptContribution::default()
        }
    }
}

#[derive(Clone)]
struct ToolGuideEntry {
    spec: CapabilitySpec,
    prompt: ToolPromptMetadata,
}

fn collect_tool_guides(capability_specs: &[CapabilitySpec]) -> Vec<ToolGuideEntry> {
    let mut guides = capability_specs
        .iter()
        .filter(|spec| spec.kind.is_tool())
        .filter_map(|spec| {
            let prompt =
                spec.metadata.get("prompt").cloned().and_then(
                    |value| match serde_json::from_value::<ToolPromptMetadata>(value) {
                        Ok(prompt) => Some(prompt),
                        Err(error) => {
                            log::warn!(
                                "ignoring invalid prompt metadata for capability '{}': {}",
                                spec.name,
                                error
                            );
                            None
                        },
                    },
                )?;
            Some(ToolGuideEntry {
                spec: spec.clone(),
                prompt,
            })
        })
        .collect::<Vec<_>>();
    guides.sort_by(|left, right| {
        tool_summary_rank(left.spec.name.as_str())
            .cmp(&tool_summary_rank(right.spec.name.as_str()))
            .then_with(|| left.spec.name.as_str().cmp(right.spec.name.as_str()))
    });
    guides
}

/// 收集标记为 source:mcp 或 source:plugin 的外部工具。
///
/// 这些工具只保留名称和粗略用途摘要，不展开详细指南。
fn collect_external_tools(capability_specs: &[CapabilitySpec]) -> Vec<CapabilitySpec> {
    let mut tools: Vec<CapabilitySpec> = capability_specs
        .iter()
        .filter(|spec| spec.kind.is_tool())
        .filter(|spec| is_external_tool(spec))
        .cloned()
        .collect();
    tools.sort_by(|left, right| left.name.as_str().cmp(right.name.as_str()));
    tools.dedup_by(|left, right| left.name == right.name);
    tools
}

fn is_external_tool(spec: &CapabilitySpec) -> bool {
    spec.tags
        .iter()
        .any(|tag| tag == "source:mcp" || tag == "source:plugin")
}

fn tool_summary_rank(name: &str) -> u8 {
    match name {
        "readFile" => 0,
        "findFiles" => 1,
        "grep" => 2,
        "shell" => 3,
        "tool_search" => 4,
        "Skill" => 6,
        "apply_patch" => 90,
        "editFile" => 91,
        "writeFile" => 92,
        _ => 50,
    }
}

fn should_render_detailed_tool_guide(guide: &ToolGuideEntry) -> bool {
    is_agent_collaboration_tool(guide)
        || matches!(guide.spec.name.as_str(), "tool_search" | "Skill")
}

fn is_agent_collaboration_tool(guide: &ToolGuideEntry) -> bool {
    guide
        .prompt
        .prompt_tags
        .iter()
        .any(|tag| tag == "collaboration")
}

fn build_tool_summary_block(
    tool_guides: &[ToolGuideEntry],
    external_tools: &[CapabilitySpec],
) -> BlockSpec {
    let mut content = String::from(
        "Use the narrowest tool that can answer the request. Prefer read-only inspection before \
         mutation. All paths must stay inside the working directory. When a tool returns a \
         persisted-result reference for large output, keep the reference in context and inspect \
         it with `readFile` chunks instead of asking the tool to inline the whole result again. \
         Use `findFiles` for file names and paths, `grep` for content search, `shell` for \
         directory inspection or commands, `readFile` for known files, and \
         `editFile`/`writeFile`/`apply_patch` for file changes.",
    );

    if !tool_guides.is_empty() {
        content.push_str("\n\nBuiltin Tools");
        for guide in tool_guides
            .iter()
            .filter(|guide| !is_agent_collaboration_tool(guide))
        {
            content.push_str(&format!(
                "\n- `{}`: {}",
                guide.spec.name, guide.prompt.summary
            ));
        }

        let collaboration_guides = tool_guides
            .iter()
            .filter(|guide| is_agent_collaboration_tool(guide))
            .collect::<Vec<_>>();
        if !collaboration_guides.is_empty() {
            content.push_str(
                "\n\nAgent Collaboration Tools\n- Use these tools together to spawn, inspect, \
                 update, and close child agents. Keep the original `agentId` byte-for-byte across \
                 calls.",
            );
            for guide in collaboration_guides {
                content.push_str(&format!(
                    "\n- `{}`: {}",
                    guide.spec.name, guide.prompt.summary
                ));
            }
        }
    }

    if !external_tools.is_empty() {
        content.push_str("\n\nExternal MCP / Plugin Tools");
        for tool in external_tools {
            content.push_str(&format!("\n- `{}`: {}", tool.name, tool.description));
        }

        content.push_str(
            "\n\nWhen To Use `tool_search`\n- Builtin tools do not need discovery through \
             `tool_search`.\n- Use `tool_search` only when builtin tools are not enough and you \
             need the schema of an external MCP/plugin tool from its rough summary.\n- After \
             `tool_search` returns candidate tools and schemas, call the matching concrete tool \
             directly.",
        );
    }

    BlockSpec::system_text(
        "tool-summary",
        BlockKind::ToolGuide,
        "Tool Summary",
        content,
    )
    .with_tag("source:capability")
    .with_category("capabilities")
}

fn build_tool_search_workflow_block() -> BlockSpec {
    BlockSpec::system_text(
        "tool-search-workflow",
        BlockKind::ToolGuide,
        "Example Workflow",
        "1. Check whether builtin tools already solve the task.\n2. If an external tool is needed \
         or a visible `mcp__...` tool has unclear parameters, call `tool_search` first with part \
         of the tool name or the task purpose, for example `{ \"query\": \"webReader\" }` or `{ \
         \"query\": \"github repo structure\" }`.\n3. Read the returned `inputSchema` from \
         `tool_search` before making the external tool call.\n4. Pick the matching concrete tool \
         from the search results, such as `mcp__...`, and call it directly. Do not guess argument \
         names when schema is available.",
    )
    .with_tag("source:capability")
    .with_category("capabilities")
}

fn build_detailed_tool_block(guide: &ToolGuideEntry) -> BlockSpec {
    let mut sections = vec![guide.prompt.guide.clone()];
    if !guide.prompt.caveats.is_empty() {
        sections.push(format!(
            "Caveats:\n{}",
            guide
                .prompt
                .caveats
                .iter()
                .map(|caveat| format!("- {caveat}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if !guide.prompt.examples.is_empty() {
        sections.push(format!(
            "Examples:\n{}",
            guide
                .prompt
                .examples
                .iter()
                .map(|example| format!("- {example}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }

    let mut block = BlockSpec::system_text(
        format!("tool-guide-{}", guide.spec.name),
        BlockKind::ToolGuide,
        format!("Tool Guide: {}", guide.spec.name),
        sections.join("\n\n"),
    )
    .with_category("capabilities")
    .with_tag("source:capability")
    .with_tag(format!("capability:{}", guide.spec.name));
    if let Some(origin) = guide
        .spec
        .metadata
        .get("origin")
        .and_then(serde_json::Value::as_str)
    {
        block = block.with_origin(origin.to_string());
    }
    block
}

#[cfg(test)]
mod tests {
    use astrcode_core::{CapabilityKind, CapabilitySpec, test_support::TestEnvGuard};
    use astrcode_runtime_contract::tool::ToolPromptMetadata;
    use serde_json::json;

    use super::*;
    use crate::BlockContent;

    fn tool_spec(name: &str, always_include: bool) -> CapabilitySpec {
        CapabilitySpec::builder(name, CapabilityKind::Tool)
            .description(format!("spec for {name}"))
            .schema(json!({"type": "object"}), json!({"type": "string"}))
            .metadata(json!({
                "prompt": ToolPromptMetadata::new(
                    format!("{name} summary"),
                    format!("{name} detailed guide")
                )
                .caveat(format!("{name} caveat"))
                .example(format!("{name} example"))
                .always_include(always_include)
            }))
            .build()
            .expect("spec should build")
    }

    fn external_tool_spec(name: &str) -> CapabilitySpec {
        CapabilitySpec::builder(name, CapabilityKind::Tool)
            .description(format!("spec for {name}"))
            .schema(json!({"type": "object"}), json!({"type": "string"}))
            .tags(["source:mcp"])
            .build()
            .expect("spec should build")
    }

    fn collaboration_tool_spec(name: &str) -> CapabilitySpec {
        CapabilitySpec::builder(name, CapabilityKind::Tool)
            .description(format!("spec for {name}"))
            .schema(json!({"type": "object"}), json!({"type": "string"}))
            .metadata(json!({
                "prompt": ToolPromptMetadata::new(
                    format!("{name} summary"),
                    format!("{name} detailed guide")
                )
                .caveat(format!("{name} caveat"))
                .prompt_tag("collaboration")
            }))
            .build()
            .expect("spec should build")
    }

    fn context() -> PromptContext {
        PromptContext {
            working_dir: "/workspace/demo".to_string(),
            tool_names: vec!["shell".to_string(), "grep".to_string()],
            capability_specs: vec![tool_spec("shell", false), tool_spec("grep", false)],
            system_prompt_instructions: Vec::new(),
            agent_profiles: Vec::new(),
            skills: Vec::new(),
            step_index: 0,
            turn_index: 0,
            vars: Default::default(),
        }
    }

    #[tokio::test]
    async fn contributes_tool_summary_without_default_core_guides() {
        let contribution = CapabilityPromptContributor.contribute(&context()).await;

        assert!(
            contribution
                .blocks
                .iter()
                .any(|block| block.id == "tool-summary" && block.kind == BlockKind::ToolGuide)
        );
        assert!(
            !contribution
                .blocks
                .iter()
                .any(|block| block.id == "tool-guide-grep")
        );
    }

    #[tokio::test]
    async fn large_tool_surfaces_keep_core_tools_in_summary_only() {
        let _guard = TestEnvGuard::new();
        let mut ctx = context();
        ctx.capability_specs = vec![
            tool_spec("alpha", false),
            tool_spec("beta", false),
            tool_spec("gamma", false),
            tool_spec("delta", false),
            tool_spec("epsilon", false),
        ];

        let contribution = CapabilityPromptContributor.contribute(&ctx).await;

        for name in ["alpha", "beta", "gamma", "delta", "epsilon"] {
            assert!(
                !contribution
                    .blocks
                    .iter()
                    .any(|block| block.id == format!("tool-guide-{name}"))
            );
        }
    }

    #[tokio::test]
    async fn only_discovery_and_collaboration_tools_get_detailed_guides() {
        let _guard = TestEnvGuard::new();
        let mut ctx = context();
        ctx.capability_specs = vec![
            tool_spec("readFile", false),
            tool_spec("tool_search", false),
            tool_spec("upsertSessionPlan", false),
            tool_spec("Skill", false),
        ];

        let contribution = CapabilityPromptContributor.contribute(&ctx).await;

        assert!(
            !contribution
                .blocks
                .iter()
                .any(|block| block.id == "tool-guide-readFile")
        );
        assert!(
            !contribution
                .blocks
                .iter()
                .any(|block| block.id == "tool-guide-upsertSessionPlan")
        );
        assert!(
            contribution
                .blocks
                .iter()
                .any(|block| block.id == "tool-guide-tool_search")
        );
        assert!(
            contribution
                .blocks
                .iter()
                .any(|block| block.id == "tool-guide-Skill")
        );
    }

    #[tokio::test]
    async fn tool_summary_places_builtin_before_external_and_adds_workflow() {
        let _guard = TestEnvGuard::new();
        let mut ctx = context();
        ctx.capability_specs = vec![
            tool_spec("writeFile", false),
            tool_spec("readFile", false),
            external_tool_spec("mcp__demo__search"),
        ];

        let contribution = CapabilityPromptContributor.contribute(&ctx).await;
        let summary = contribution
            .blocks
            .iter()
            .find(|block| block.id == "tool-summary")
            .expect("summary block should exist");
        let content = match &summary.content {
            BlockContent::Text(content) => content,
            _ => panic!("expected text content"),
        };

        let builtin_index = content
            .find("Builtin Tools")
            .expect("builtin section should exist");
        let external_index = content
            .find("External MCP / Plugin Tools")
            .expect("external section should exist");
        let read_index = content
            .find("`readFile`")
            .expect("readFile should be listed");
        let write_index = content
            .find("`writeFile`")
            .expect("writeFile should be listed");
        let external_tool_index = content
            .find("`mcp__demo__search`")
            .expect("external tool should be listed");

        assert!(builtin_index < external_index);
        assert!(read_index < write_index);
        assert!(write_index < external_tool_index);
        assert!(content.contains("When To Use `tool_search`"));
        assert!(content.contains("persisted-result reference"));
        assert!(
            contribution
                .blocks
                .iter()
                .any(|block| block.id == "tool-search-workflow")
        );
        assert!(
            !contribution
                .blocks
                .iter()
                .any(|block| block.id == "tool-guide-mcp__demo__search")
        );
    }

    #[tokio::test]
    async fn collaboration_tools_stay_visible_on_large_tool_surfaces() {
        let _guard = TestEnvGuard::new();
        let mut ctx = context();
        ctx.capability_specs = vec![
            tool_spec("alpha", false),
            tool_spec("beta", false),
            tool_spec("gamma", false),
            tool_spec("delta", false),
            collaboration_tool_spec("spawn"),
            collaboration_tool_spec("send"),
        ];

        let contribution = CapabilityPromptContributor.contribute(&ctx).await;
        let summary = contribution
            .blocks
            .iter()
            .find(|block| block.id == "tool-summary")
            .expect("summary block should exist");
        let content = match &summary.content {
            BlockContent::Text(content) => content,
            _ => panic!("expected text content"),
        };

        assert!(content.contains("Agent Collaboration Tools"));
        assert!(content.contains("`spawn`"));
        assert!(content.contains("`send`"));
        assert!(
            contribution
                .blocks
                .iter()
                .any(|block| block.id == "tool-guide-spawn")
        );
        assert!(
            contribution
                .blocks
                .iter()
                .any(|block| block.id == "tool-guide-send")
        );
    }

    #[tokio::test]
    async fn omits_external_workflow_when_no_external_tools_exist() {
        let contribution = CapabilityPromptContributor.contribute(&context()).await;

        let summary = contribution
            .blocks
            .iter()
            .find(|block| block.id == "tool-summary")
            .expect("summary block should exist");
        let content = match &summary.content {
            BlockContent::Text(content) => content,
            _ => panic!("expected text content"),
        };

        assert!(!content.contains("External MCP / Plugin Tools"));
        assert!(!content.contains("When To Use `tool_search`"));
        assert!(
            !contribution
                .blocks
                .iter()
                .any(|block| block.id == "tool-search-workflow")
        );
    }
}
