//! astrcode-extension-agent-tools — 子 Agent 委派。
//!
//! 注册的工具：
//! - `agent`: 派生子 Agent 执行委派任务

mod agent;

use std::{collections::BTreeMap, sync::Arc};

use agent::AgentConfig;
use astrcode_core::{
    extension::{
        Extension, ExtensionContext, ExtensionError, ExtensionEvent, ExtensionToolOutcome,
        HookEffect, HookMode, HookSubscription, PromptContributions,
    },
    render::{RenderKeyValue, RenderSpec, RenderTone, UI_RENDER_METADATA_KEY},
    tool::{ToolDefinition, ToolOrigin, ToolResult},
};

// ─── 内置扩展入口 ─────────────────────────────────────────────────────

/// 返回内置 Agent 工具扩展。
pub fn extension() -> Arc<dyn Extension> {
    Arc::new(AgentToolsExtension)
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
        let render_json = serde_json::to_value(run.render)
            .map_err(|e| ExtensionError::Internal(format!("serialize agent render: {e}")))?;
        let mut metadata = BTreeMap::new();
        metadata.insert("extension_tool_outcome".into(), outcome_json);
        metadata.insert(UI_RENDER_METADATA_KEY.into(), render_json);
        Ok(ToolResult {
            call_id: String::new(),
            content: String::new(),
            is_error: false,
            error: None,
            metadata,
            duration_ms: None,
        })
    }
}

// ─── 工具实现 ────────────────────────────────────────────────────────

struct AgentRun {
    outcome: ExtensionToolOutcome,
    render: RenderSpec,
}

/// 解析输入 + 可用 Agent 列表，返回声明式 RunSession 结果和 UI 渲染提示。
fn build_agent_run(input: &serde_json::Value, agents: &[AgentConfig]) -> Result<AgentRun, String> {
    let prompt = input["prompt"].as_str().ok_or("prompt required")?;
    let agent_name = input["subagent_type"].as_str().unwrap_or("");
    let mode = input["mode"].as_str().unwrap_or("single");

    match mode {
        "chain" => Err(
            "chain mode is not yet supported — use single mode or list each agent step manually"
                .into(),
        ),
        _ => {
            let agent = if agent_name.is_empty() {
                agents.first().ok_or("no agents configured")?
            } else {
                agents
                    .iter()
                    .find(|a| a.name == agent_name)
                    .ok_or_else(|| {
                        format!(
                            "agent '{agent_name}' not found.\n\n{}",
                            format_agents_for_model(agents)
                        )
                    })?
            };

            Ok(AgentRun {
                render: agent_run_render_spec(input, agent, prompt),
                outcome: ExtensionToolOutcome::RunSession {
                    name: agent.name.clone(),
                    system_prompt: agent.body.clone(),
                    user_prompt: prompt.to_string(),
                    allowed_tools: agent.tools.clone(),
                    model_preference: agent.model.clone(),
                },
            })
        },
    }
}

fn agent_run_render_spec(
    input: &serde_json::Value,
    agent: &AgentConfig,
    prompt: &str,
) -> RenderSpec {
    let description = input["description"]
        .as_str()
        .unwrap_or(agent.description.as_str());
    let tools = if agent.tools.is_empty() {
        "inherit/default".into()
    } else {
        agent.tools.join(", ")
    };
    let model = agent.model.as_deref().unwrap_or("inherit/default");

    RenderSpec::Box {
        title: None,
        tone: RenderTone::Default,
        children: vec![
            RenderSpec::KeyValue {
                entries: vec![
                    RenderKeyValue {
                        key: "task".into(),
                        value: description.into(),
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
                    RenderKeyValue {
                        key: "tools".into(),
                        value: tools,
                        tone: RenderTone::Muted,
                    },
                ],
                tone: RenderTone::Default,
            },
            RenderSpec::Text {
                text: format!("prompt: {}", compact_inline(prompt, 180)),
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

const AGENT_TOOL_PARAMETERS: &str = r#"{"type":"object","properties":{"description":{"type":"string","description":"Short 3-5 word description"},"prompt":{"type":"string","description":"Task for the subagent"},"subagent_type":{"type":"string","description":"Agent name from agents/ directory"},"mode":{"type":"string","enum":["single"],"default":"single"}},"required":["prompt","description"]}"#;

fn agent_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "agent".into(),
        description: AGENT_TOOL_DESCRIPTION.into(),
        parameters: serde_json::from_str(AGENT_TOOL_PARAMETERS).unwrap_or_else(|_| {
            serde_json::json!({
                "type": "object",
                "properties": {},
            })
        }),
        origin: ToolOrigin::Bundled,
    }
}

/// 将 Agent 列表格式化为模型可读的文本。
fn format_agents_for_model(agents: &[AgentConfig]) -> String {
    if agents.is_empty() {
        return String::from("No agents configured.");
    }

    let mut lines = Vec::with_capacity(agents.len() + 1);
    lines.push(String::from("Available agents:"));
    for agent in agents {
        let tools = if agent.tools.is_empty() {
            String::from("inherit/default")
        } else {
            agent.tools.join(", ")
        };
        let model = agent.model.as_deref().unwrap_or("inherit/default");
        lines.push(format!(
            "- {}: {} (tools: {}; model: {})",
            agent.name, agent.description, tools, model
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
            tools: vec![String::from("read"), String::from("grep")],
            model: Some(String::from("opus")),
            body: String::from("Review carefully."),
        }];

        let output = format_agents_for_model(&agents);

        assert!(output.contains("code-reviewer"));
        assert!(output.contains("Use for behavior-focused code review"));
        assert!(output.contains("tools: read, grep"));
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
}
