//! astrcode-extension-agent-tools — 子 Agent 委派。
//!
//! 注册的工具：
//! - `agent`: 派生子 Agent 执行委派任务

mod agent;

use astrcode_core::{
    extension::{
        Extension, ExtensionContext, ExtensionError, ExtensionEvent, ExtensionToolOutcome,
        HookEffect, HookMode, HookSubscription, PromptContributions,
    },
    render::{RenderKeyValue, RenderSpec, RenderTone, UI_RENDER_METADATA_KEY},
    tool::{ExecutionMode, ToolDefinition, ToolOrigin, ToolResult, tool_metadata},
};
use serde::Deserialize;
use serde_json::json;

// ─── 内置扩展入口 ─────────────────────────────────────────────────────

/// 返回内置 Agent 工具扩展。
pub fn extension() -> std::sync::Arc<dyn Extension> {
    std::sync::Arc::new(AgentToolsExtension)
}

struct AgentToolsExtension;

#[async_trait::async_trait]
impl Extension for AgentToolsExtension {
    fn id(&self) -> &str {
        "astrcode-agent-tools"
    }

    fn hook_subscriptions(&self) -> Vec<HookSubscription> {
        vec![HookSubscription {
            event: ExtensionEvent::PromptBuild,
            mode: HookMode::Blocking,
            priority: 0,
        }]
    }

    async fn on_event(
        &self,
        event: ExtensionEvent,
        ctx: &dyn ExtensionContext,
    ) -> Result<HookEffect, ExtensionError> {
        match event {
            ExtensionEvent::PromptBuild => {
                let agents = agent::discover_agents(Some(ctx.working_dir()));
                Ok(HookEffect::PromptContributions(PromptContributions {
                    agents: vec![format_agents_for_model(&agents)],
                    ..Default::default()
                }))
            },
            _ => Ok(HookEffect::Allow),
        }
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![agent_tool_definition()]
    }

    async fn execute_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        working_dir: &str,
        _ctx: &astrcode_core::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        if tool_name != "agent" {
            return Err(ExtensionError::NotFound(tool_name.into()));
        }

        let agents = agent::discover_agents(Some(working_dir));
        let run = build_agent_run(&arguments, &agents).map_err(ExtensionError::Internal)?;
        let outcome_json = serde_json::to_value(&run.outcome)
            .map_err(|e| ExtensionError::Internal(format!("serialize agent outcome: {e}")))?;
        let render_json = serde_json::to_value(&run.render)
            .map_err(|e| ExtensionError::Internal(format!("serialize agent render: {e}")))?;

        Ok(ToolResult::text(
            String::new(),
            false,
            tool_metadata([
                ("extension_tool_outcome", outcome_json),
                (UI_RENDER_METADATA_KEY, render_json),
            ]),
        ))
    }

    fn tool_prompt_metadata(
        &self,
    ) -> std::collections::HashMap<String, astrcode_core::tool::ToolPromptMetadata> {
        let mut map = std::collections::HashMap::new();
        map.insert(
            "agent".to_string(),
            astrcode_core::tool::ToolPromptMetadata::new(
                "Use `agent` for isolated responsibilities that benefit from parallel execution \
                 or context separation. Give the child one narrow task.",
            )
            .caveat(
                "If your next step depends on the result, doing it yourself is usually faster; \
                 only spawn when parallel or isolation value is clear.",
            )
            .prompt_tag("collaboration"),
        );
        map
    }
}

// ─── 工具实现 ────────────────────────────────────────────────────────

/// LLM tool call 参数类型。
///
/// JSON schema 定义了 LLM 的调用契约，因此 `rename_all = "camelCase"` 是合理的。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentArgs {
    prompt: String,
    description: String,
    subagent_type: Option<String>,
    #[serde(default)]
    mode: AgentMode,
}

#[derive(Debug, Default, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum AgentMode {
    #[default]
    Single,
    Chain,
}

struct AgentRun {
    outcome: ExtensionToolOutcome,
    render: RenderSpec,
}

/// 解析输入 + 可用 Agent 列表，返回声明式 RunSession 结果和 UI 渲染提示。
fn build_agent_run(
    input: &serde_json::Value,
    agents: &[agent::AgentConfig],
) -> Result<AgentRun, String> {
    let args: AgentArgs =
        serde_json::from_value(input.clone()).map_err(|e| format!("invalid agent args: {e}"))?;

    match args.mode {
        AgentMode::Chain => Err(
            "chain mode is not yet supported — use single mode or list each agent step manually"
                .into(),
        ),
        AgentMode::Single => {
            let agent = match args.subagent_type.as_deref() {
                None | Some("") => agents.first().ok_or("no agents configured")?,
                Some(name) => agents
                    .iter()
                    .find(|a| a.name == name || a.id == name)
                    .ok_or_else(|| {
                        format!(
                            "agent '{name}' not found.\n\n{}",
                            format_agents_for_model(agents)
                        )
                    })?,
            };

            Ok(AgentRun {
                render: agent_run_render_spec(&args, agent),
                outcome: ExtensionToolOutcome::RunSession {
                    name: agent.name.clone(),
                    system_prompt: agent.body.clone(),
                    user_prompt: args.prompt,
                    model_preference: agent.model.clone(),
                },
            })
        },
    }
}

fn agent_run_render_spec(args: &AgentArgs, agent: &agent::AgentConfig) -> RenderSpec {
    let model = agent.model.as_deref().unwrap_or("inherit/default");

    RenderSpec::Box {
        title: None,
        tone: RenderTone::Default,
        children: vec![
            RenderSpec::KeyValue {
                entries: vec![
                    RenderKeyValue {
                        key: "task".into(),
                        value: args.description.clone(),
                        tone: RenderTone::Accent,
                    },
                    RenderKeyValue {
                        key: "agent".into(),
                        value: agent.name.clone(),
                        tone: RenderTone::Accent,
                    },
                    RenderKeyValue {
                        key: "model".into(),
                        value: model.into(),
                        tone: RenderTone::Muted,
                    },
                ],
                tone: RenderTone::Default,
            },
            RenderSpec::Text {
                text: format!("prompt: {}", compact_inline(&args.prompt, 180)),
                tone: RenderTone::Muted,
            },
        ],
    }
}

fn compact_inline(text: &str, max_chars: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }

    let mut preview = compact.chars().take(max_chars).collect::<String>();
    preview.push('…');
    preview
}

const AGENT_TOOL_DESCRIPTION: &str =
    "Spawn a subagent to handle one delegated task. Choose subagent_type from the Agents section.";

const AGENT_TOOL_PARAMETERS: &str = r#"{"type":"object","properties":{"description":{"type":"string","description":"Short 3-5 word description"},"prompt":{"type":"string","description":"Task for the subagent"},"subagentType":{"type":"string","description":"Agent name from agents/ directory"},"mode":{"type":"string","enum":["single"],"default":"single"}},"required":["prompt","description"]}"#;

fn agent_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "agent".into(),
        description: AGENT_TOOL_DESCRIPTION.into(),
        parameters: serde_json::from_str(AGENT_TOOL_PARAMETERS)
            .unwrap_or_else(|_| json!({ "type": "object", "properties": {} })),
        origin: ToolOrigin::Bundled,
        execution_mode: ExecutionMode::Sequential,
    }
}

/// 将 Agent 列表格式化为模型可读的文本。
fn format_agents_for_model(agents: &[agent::AgentConfig]) -> String {
    if agents.is_empty() {
        return String::from("No agents configured.");
    }

    let mut lines = Vec::with_capacity(agents.len() + 1);
    lines.push(String::from("Available agents:"));
    for agent in agents {
        let model = agent.model.as_deref().unwrap_or("inherit/default");
        lines.push(format!(
            "- {}: {} (model: {})",
            agent.name, agent.description, model
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_agent_metadata_for_model_selection() {
        let agents = vec![agent::AgentConfig {
            id: String::from("code-reviewer"),
            name: String::from("code-reviewer"),
            description: String::from("Use for behavior-focused code review"),
            model: Some(String::from("opus")),
            body: String::from("Review carefully."),
        }];

        let output = format_agents_for_model(&agents);

        assert!(output.contains("code-reviewer"));
        assert!(output.contains("Use for behavior-focused code review"));
        assert!(output.contains("model: opus"));
    }

    #[test]
    fn formats_empty_agent_list() {
        assert_eq!(format_agents_for_model(&[]), "No agents configured.");
    }

    #[test]
    fn agent_tool_schema_exposes_only_supported_modes() {
        let definition = agent_tool_definition();
        let properties = definition.parameters["properties"]
            .as_object()
            .expect("tool schema properties");

        let modes: Vec<&str> = properties["mode"]["enum"]
            .as_array()
            .expect("mode enum")
            .iter()
            .map(|value| value.as_str().expect("mode value"))
            .collect();

        assert_eq!(modes, vec!["single"]);
        assert!(!properties.contains_key("chain"));
    }

    #[test]
    fn agent_args_deserialize_camel_case() {
        let input = json!({
            "prompt": "find the bug",
            "description": "bug hunt",
            "subagentType": "explore"
        });
        let args: AgentArgs = serde_json::from_value(input).unwrap();
        assert_eq!(args.prompt, "find the bug");
        assert_eq!(args.description, "bug hunt");
        assert_eq!(args.subagent_type.as_deref(), Some("explore"));
        assert!(matches!(args.mode, AgentMode::Single));
    }

    #[test]
    fn agent_args_reject_missing_prompt() {
        let input = json!({ "description": "test" });
        let result = serde_json::from_value::<AgentArgs>(input);
        assert!(result.is_err());
    }

    #[test]
    fn build_agent_run_matches_by_id_or_name() {
        let agents = vec![agent::AgentConfig {
            id: String::from("code-reviewer"),
            name: String::from("Code Reviewer"),
            description: String::from("review code"),
            model: None,
            body: String::from("Review."),
        }];

        let by_name =
            json!({ "prompt": "review", "description": "test", "subagentType": "Code Reviewer" });
        assert!(build_agent_run(&by_name, &agents).is_ok());

        let by_id =
            json!({ "prompt": "review", "description": "test", "subagentType": "code-reviewer" });
        assert!(build_agent_run(&by_id, &agents).is_ok());

        let by_unknown =
            json!({ "prompt": "review", "description": "test", "subagentType": "unknown" });
        assert!(build_agent_run(&by_unknown, &agents).is_err());
    }
}
