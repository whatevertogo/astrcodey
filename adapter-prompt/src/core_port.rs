//! 桥接 `adapter-prompt` 的分层 prompt builder 与 `runtime-contract::prompt::PromptProvider`。
//!
//! `runtime-contract::prompt::PromptProvider` 是 runtime 消费的 prompt 组装端口，
//! 本模块将其适配到 `LayeredPromptBuilder` 的完整 prompt 构建能力上。

use astrcode_core::{Result, policy::SystemPromptLayer};
use astrcode_runtime_contract::prompt::{
    PromptAgentProfileSummary as HostPromptAgentProfileSummary, PromptBuildCacheMetrics,
    PromptBuildOutput, PromptBuildRequest, PromptCacheGlobalStrategy, PromptCacheHints,
    PromptProvider, PromptSkillSummary as HostPromptSkillSummary, SystemPromptBlock,
};
use async_trait::async_trait;
use serde_json::Value;

use crate::{
    PromptAgentProfileSummary, PromptContext, PromptSkillSummary,
    diagnostics::DiagnosticReason,
    layered_builder::{LayeredPromptBuilder, default_layered_prompt_builder},
};

/// 基于 `LayeredPromptBuilder` 的 `PromptProvider` 实现。
///
/// 将 `runtime-contract::prompt::PromptBuildRequest` 转为 `PromptContext`，
/// 调用分层 builder 后将 `PromptPlan` 渲染为 system prompt。
pub struct ComposerPromptProvider {
    builder: LayeredPromptBuilder,
}

impl ComposerPromptProvider {
    pub fn new(builder: LayeredPromptBuilder) -> Self {
        Self { builder }
    }

    /// 使用默认贡献者创建。
    pub fn with_defaults() -> Self {
        Self {
            builder: default_layered_prompt_builder(),
        }
    }
}

impl std::fmt::Debug for ComposerPromptProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComposerPromptProvider").finish()
    }
}

#[async_trait]
impl PromptProvider for ComposerPromptProvider {
    async fn build_prompt(&self, request: PromptBuildRequest) -> Result<PromptBuildOutput> {
        let vars = build_prompt_vars(&request);
        let ctx = PromptContext {
            working_dir: request.working_dir.to_string_lossy().to_string(),
            tool_names: request
                .capabilities
                .iter()
                .filter(|capability| capability.kind.is_tool())
                .map(|capability| capability.name.to_string())
                .collect(),
            capability_specs: request.capabilities,
            system_prompt_instructions: request.system_prompt_instructions,
            agent_profiles: request
                .agent_profiles
                .into_iter()
                .map(convert_agent_profile)
                .collect(),
            skills: request.skills.into_iter().map(convert_skill).collect(),
            step_index: request.step_index,
            turn_index: request.turn_index,
            vars,
        };

        let output = self
            .builder
            .build(&ctx)
            .await
            .map_err(|e| astrcode_core::AstrError::Internal(e.to_string()))?;

        let system_prompt = output.plan.render_system().unwrap_or_default();
        let mut prompt_cache_hints = output.cache_hints.clone();
        prompt_cache_hints.global_cache_strategy = select_global_cache_strategy(&ctx.tool_names);
        let system_prompt_blocks = build_system_prompt_blocks(&output.plan);

        Ok(PromptBuildOutput {
            system_prompt,
            system_prompt_blocks,
            prompt_cache_hints: prompt_cache_hints.clone(),
            cache_metrics: summarize_prompt_cache_metrics(&output),
            metadata: build_output_metadata(
                &request.profile,
                request.step_index,
                request.turn_index,
                &output,
                prompt_cache_hints,
            ),
        })
    }
}

fn convert_agent_profile(summary: HostPromptAgentProfileSummary) -> PromptAgentProfileSummary {
    PromptAgentProfileSummary::new(summary.id, summary.description)
}

fn convert_skill(summary: HostPromptSkillSummary) -> PromptSkillSummary {
    PromptSkillSummary::new(summary.id, summary.description)
}

fn build_prompt_vars(request: &PromptBuildRequest) -> std::collections::HashMap<String, String> {
    let mut vars = std::collections::HashMap::new();
    if let Some(session_id) = &request.session_id {
        vars.insert("session.id".to_string(), session_id.to_string());
    }
    if let Some(turn_id) = &request.turn_id {
        vars.insert("turn.id".to_string(), turn_id.to_string());
    }
    vars.insert("profile.name".to_string(), request.profile.clone());
    insert_json_string(&mut vars, "profile.context", &request.profile_context);
    insert_json_string(&mut vars, "request.metadata", &request.metadata);
    if let Some(config_version) = request
        .metadata
        .get("configVersion")
        .and_then(Value::as_str)
    {
        vars.insert("config.version".to_string(), config_version.to_string());
    }
    if let Some(user_message) = request
        .metadata
        .get("latestUserMessage")
        .and_then(Value::as_str)
    {
        vars.insert("turn.user_message".to_string(), user_message.to_string());
    }
    if let Some(max_depth) = request
        .metadata
        .get("agentMaxSubrunDepth")
        .and_then(Value::as_u64)
    {
        vars.insert("agent.max_subrun_depth".to_string(), max_depth.to_string());
    }
    if let Some(max_spawn_per_turn) = request
        .metadata
        .get("agentMaxSpawnPerTurn")
        .and_then(Value::as_u64)
    {
        vars.insert(
            "agent.max_spawn_per_turn".to_string(),
            max_spawn_per_turn.to_string(),
        );
    }
    vars
}

fn summarize_prompt_cache_metrics(output: &crate::PromptBuildOutput) -> PromptBuildCacheMetrics {
    let mut metrics = PromptBuildCacheMetrics::default();
    for diagnostic in &output.diagnostics.items {
        match &diagnostic.reason {
            DiagnosticReason::CacheReuseHit { .. } => {
                metrics.reuse_hits = metrics.reuse_hits.saturating_add(1);
            },
            DiagnosticReason::CacheReuseMiss { .. } => {
                metrics.reuse_misses = metrics.reuse_misses.saturating_add(1);
            },
            _ => {},
        }
    }
    metrics.unchanged_layers = output.cache_hints.unchanged_layers.clone();
    metrics
}

fn build_output_metadata(
    profile: &str,
    step_index: usize,
    turn_index: usize,
    output: &crate::PromptBuildOutput,
    prompt_cache_hints: PromptCacheHints,
) -> Value {
    serde_json::json!({
        "extra_tools_count": output.plan.extra_tools.len(),
        "diagnostics_count": output.diagnostics.items.len(),
        "profile": profile,
        "step_index": step_index,
        "turn_index": turn_index,
        "promptCacheHints": prompt_cache_hints,
        "promptSources": output.plan.source_metadata(),
    })
}

fn build_system_prompt_blocks(plan: &crate::PromptPlan) -> Vec<SystemPromptBlock> {
    let ordered = plan.ordered_system_blocks();
    let mut last_cacheable_index = std::collections::HashMap::<SystemPromptLayer, usize>::new();
    for (index, block) in ordered.iter().enumerate() {
        if cacheable_prompt_layer(block.layer) {
            last_cacheable_index.insert(block.layer, index);
        }
    }
    ordered
        .into_iter()
        .enumerate()
        .map(|(index, block)| SystemPromptBlock {
            title: block.title.clone(),
            content: block.content.clone(),
            cache_boundary: last_cacheable_index
                .get(&block.layer)
                .is_some_and(|candidate| *candidate == index),
            layer: block.layer,
        })
        .collect()
}

fn cacheable_prompt_layer(layer: SystemPromptLayer) -> bool {
    matches!(
        layer,
        SystemPromptLayer::Stable | SystemPromptLayer::SemiStable | SystemPromptLayer::Inherited
    )
}

fn select_global_cache_strategy(tool_names: &[String]) -> PromptCacheGlobalStrategy {
    // Why:
    // - MCP 工具集合按用户/环境动态变化，比内建工具更容易让全局前缀抖动
    // - 一旦检测到这类动态工具，就把“全局断点”预算让给 tools，system 只保留更稳定的层边界
    if tool_names.iter().any(|name| name.starts_with("mcp__")) {
        PromptCacheGlobalStrategy::ToolBased
    } else {
        PromptCacheGlobalStrategy::SystemPrompt
    }
}

fn insert_json_string(
    vars: &mut std::collections::HashMap<String, String>,
    key: &str,
    value: &Value,
) {
    if value.is_null() {
        return;
    }
    let rendered = if let Some(text) = value.as_str() {
        text.to_string()
    } else {
        value.to_string()
    };
    vars.insert(key.to_string(), rendered);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use astrcode_core::{CapabilityKind, CapabilitySpec};
    use astrcode_runtime_contract::prompt::{PromptBuildRequest, PromptCacheGlobalStrategy};

    use super::{build_output_metadata, build_prompt_vars, select_global_cache_strategy};
    use crate::{BlockKind, PromptBlock, PromptDiagnostics, PromptPlan, block::BlockMetadata};

    #[test]
    fn build_prompt_vars_exposes_agent_max_subrun_depth() {
        let request = PromptBuildRequest {
            session_id: None,
            turn_id: None,
            working_dir: PathBuf::from("/workspace/demo"),
            profile: "default".to_string(),
            step_index: 0,
            turn_index: 0,
            profile_context: serde_json::Value::Null,
            capabilities: Vec::new(),
            skills: Vec::new(),
            agent_profiles: Vec::new(),
            system_prompt_instructions: Vec::new(),
            metadata: serde_json::json!({
                "agentMaxSubrunDepth": 3u64,
            }),
        };

        let vars = build_prompt_vars(&request);

        assert_eq!(
            vars.get("agent.max_subrun_depth").map(String::as_str),
            Some("3")
        );
    }

    #[test]
    fn build_prompt_vars_exposes_agent_max_spawn_per_turn() {
        let request = PromptBuildRequest {
            session_id: None,
            turn_id: None,
            working_dir: PathBuf::from("/workspace/demo"),
            profile: "default".to_string(),
            step_index: 0,
            turn_index: 0,
            profile_context: serde_json::Value::Null,
            capabilities: Vec::new(),
            skills: Vec::new(),
            agent_profiles: Vec::new(),
            system_prompt_instructions: Vec::new(),
            metadata: serde_json::json!({
                "agentMaxSpawnPerTurn": 2u64,
            }),
        };

        let vars = build_prompt_vars(&request);

        assert_eq!(
            vars.get("agent.max_spawn_per_turn").map(String::as_str),
            Some("2")
        );
    }

    #[test]
    fn build_output_metadata_includes_prompt_source_projection() {
        let request = PromptBuildRequest {
            session_id: None,
            turn_id: None,
            working_dir: PathBuf::from("/workspace/demo"),
            profile: "default".to_string(),
            step_index: 1,
            turn_index: 2,
            profile_context: serde_json::Value::Null,
            capabilities: Vec::new(),
            skills: Vec::new(),
            agent_profiles: Vec::new(),
            system_prompt_instructions: Vec::new(),
            metadata: serde_json::Value::Null,
        };
        let output = crate::PromptBuildOutput {
            plan: PromptPlan {
                system_blocks: vec![
                    PromptBlock::new(
                        "child.execution.contract",
                        BlockKind::ExtensionInstruction,
                        "Child Execution Contract",
                        "contract",
                        585,
                        BlockMetadata {
                            tags: vec!["source:builtin".into()],
                            category: Some("extensions".into()),
                            origin: Some("child-contract:fresh".to_string()),
                        },
                        0,
                    )
                    .with_layer(crate::PromptLayer::Inherited),
                ],
                ..PromptPlan::default()
            },
            diagnostics: PromptDiagnostics::default(),
            cache_hints: Default::default(),
        };

        let metadata = build_output_metadata(
            &request.profile,
            request.step_index,
            request.turn_index,
            &output,
            Default::default(),
        );

        assert_eq!(
            metadata["promptSources"][0]["blockId"],
            "child.execution.contract"
        );
        assert_eq!(metadata["promptSources"][0]["source"], "builtin");
        assert_eq!(
            metadata["promptSources"][0]["origin"],
            "child-contract:fresh"
        );
    }

    #[test]
    fn select_global_cache_strategy_prefers_tool_based_when_mcp_tools_exist() {
        let request = PromptBuildRequest {
            session_id: None,
            turn_id: None,
            working_dir: PathBuf::from("/workspace/demo"),
            profile: "default".to_string(),
            step_index: 0,
            turn_index: 0,
            profile_context: serde_json::Value::Null,
            capabilities: vec![
                CapabilitySpec::builder("read_file", CapabilityKind::tool())
                    .description("read")
                    .input_schema(serde_json::json!({ "type": "object" }))
                    .output_schema(serde_json::json!({ "type": "object" }))
                    .build()
                    .expect("builtin capability should build"),
                CapabilitySpec::builder("mcp__demo__search", CapabilityKind::tool())
                    .description("search")
                    .input_schema(serde_json::json!({ "type": "object" }))
                    .output_schema(serde_json::json!({ "type": "object" }))
                    .build()
                    .expect("mcp capability should build"),
            ],
            skills: Vec::new(),
            agent_profiles: Vec::new(),
            system_prompt_instructions: Vec::new(),
            metadata: serde_json::Value::Null,
        };
        let tool_names = request
            .capabilities
            .iter()
            .filter(|capability| capability.kind.is_tool())
            .map(|capability| capability.name.to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            select_global_cache_strategy(&tool_names),
            PromptCacheGlobalStrategy::ToolBased
        );
    }
}
