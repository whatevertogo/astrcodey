//! astrcode-extension-agent-tools — 子 Agent 委派与协作。
//!
//! 注册的工具：
//! - `agent`: 派生子 Agent 执行委派任务

mod agent;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use astrcode_core::{
    extension::{
        ChildToolPolicy, EXTENSION_TOOL_OUTCOME_KEY, Extension, ExtensionError,
        ExtensionToolOutcome, PromptBuildContext, PromptBuildHandler, PromptContributions,
        Registrar, ToolHandler,
    },
    render::{RenderKeyValue, RenderSpec, RenderTone, UI_RENDER_METADATA_KEY},
    tool::{ExecutionMode, ToolDefinition, ToolOrigin, ToolResult, tool_metadata},
};
use serde::Deserialize;
use serde_json::json;

// ─── 扩展入口 ──────────────────────────────────────────────────────────

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

    fn register(&self, reg: &mut Registrar) {
        let shared = Arc::new(AgentShared::new());
        reg.tool(
            agent_tool_definition(),
            Arc::new(AgentToolHandler {
                shared: shared.clone(),
            }),
        );
        reg.tool_metadata(agent_tool_metadata());
        reg.on_prompt_build(
            0,
            Arc::new(AgentPromptBuildHandler {
                shared: shared.clone(),
            }),
        );
    }
}

// ─── Agent 发现缓存 ────────────────────────────────────────────────────

/// Agent 发现结果缓存，按 working_dir 缓存。
struct AgentShared {
    cache: Mutex<HashMap<String, Vec<agent::AgentConfig>>>,
}

impl AgentShared {
    fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn get_or_discover(&self, working_dir: Option<&str>) -> Vec<agent::AgentConfig> {
        let key = working_dir.unwrap_or("");
        if let Some(agents) = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
        {
            return agents.clone();
        }
        let agents = agent::discover_agents(working_dir);
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.to_string())
            .or_insert_with(|| agents.clone());
        agents
    }
}

// ─── agent 工具 ────────────────────────────────────────────────────────
//
// 定义 → 参数 → 构建逻辑 → 渲染 → Handler，自上而下阅读即可理解完整流程。

const AGENT_TOOL_DESCRIPTION: &str =
    "Launch a specialized subagent for one narrow, delegated task. Agents run in the background \
     by default — you can continue working and results arrive in the next turn. Set waitForResult \
     to true only when your next step depends on the agent's output. You may launch multiple \
     agents in a single response to parallelize independent tasks. See the [Agents] section in \
     the system prompt for available agent types.";

const AGENT_TOOL_PARAMETERS: &str = r#"{"type":"object","properties":{"description":{"type":"string","description":"Short 3-5 word description of the task"},"prompt":{"type":"string","description":"Task for the subagent"},"subagentType":{"type":"string","description":"Agent name from agents/ directory"},"waitForResult":{"type":"boolean","default":false,"description":"If true, block until the agent completes. If false (default), run in the background and return immediately."}},"required":["prompt","description"]}"#;

fn agent_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "agent".into(),
        description: AGENT_TOOL_DESCRIPTION.into(),
        parameters: serde_json::from_str(AGENT_TOOL_PARAMETERS)
            .unwrap_or_else(|_| json!({ "type": "object", "properties": {} })),
        origin: ToolOrigin::Bundled,
        execution_mode: ExecutionMode::Parallel,
    }
}

/// LLM tool call 参数类型。
///
/// JSON schema 定义了 LLM 的调用契约，因此 `rename_all = "camelCase"` 是合理的。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentArgs {
    prompt: String,
    description: String,
    subagent_type: Option<String>,
    #[serde(default = "default_wait_for_result")]
    wait_for_result: bool,
}

const fn default_wait_for_result() -> bool {
    false
}

#[derive(Debug)]
struct AgentRun {
    outcome: ExtensionToolOutcome,
    render: RenderSpec,
}

/// 解析 LLM 调用参数，匹配 Agent 配置，返回声明式 RunSession 结果和 UI 渲染。
fn build_agent_run(
    input: &serde_json::Value,
    agents: &[agent::AgentConfig],
) -> Result<AgentRun, String> {
    let args: AgentArgs =
        serde_json::from_value(input.clone()).map_err(|e| format!("invalid agent args: {e}"))?;

    let matched = match args.subagent_type.as_deref() {
        // 缺少 subagentType 是调用错误，告知 LLM 可用列表。
        None => {
            return Err(format!(
                "subagentType is required.\n\n{}",
                format_agents_for_model(agents)
            ));
        },
        // 空字符串回退到第一个 agent（向后兼容旧调用模式）。
        Some("") => agents.first().ok_or("no agents configured")?,
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
        render: agent_run_render_spec(&args, matched),
        outcome: ExtensionToolOutcome::RunSession {
            name: matched.name.clone(),
            system_prompt: matched.body.clone(),
            user_prompt: args.prompt,
            model_preference: matched.model.clone(),
            wait_for_result: args.wait_for_result,
            // 子 agent 不再持有 agent 工具，避免递归生成 agent 形成无界扩散。
            // max_depth 配置项是兜底；这条 policy 是声明式护栏，让递归在工具表层就不可能发生。
            tool_policy: Some(ChildToolPolicy::Deny {
                tools: vec!["agent".into()],
            }),
        },
    })
}

fn agent_run_render_spec(args: &AgentArgs, agent: &agent::AgentConfig) -> RenderSpec {
    let model = agent.model.as_deref().unwrap_or("inherit/default");
    let mode_label = if args.wait_for_result {
        "sync"
    } else {
        "async"
    };

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
                    RenderKeyValue {
                        key: "mode".into(),
                        value: mode_label.into(),
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

struct AgentToolHandler {
    shared: Arc<AgentShared>,
}

#[async_trait::async_trait]
impl ToolHandler for AgentToolHandler {
    async fn execute(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        working_dir: &str,
        _ctx: &astrcode_core::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        if tool_name != "agent" {
            return Err(ExtensionError::NotFound(tool_name.into()));
        }

        let agents = self.shared.get_or_discover(Some(working_dir));
        let run = build_agent_run(&arguments, &agents).map_err(ExtensionError::Internal)?;
        let outcome_json = serde_json::to_value(&run.outcome)
            .map_err(|e| ExtensionError::Internal(format!("serialize agent outcome: {e}")))?;
        let render_json = serde_json::to_value(&run.render)
            .map_err(|e| ExtensionError::Internal(format!("serialize agent render: {e}")))?;

        Ok(ToolResult::text(
            String::new(),
            false,
            tool_metadata([
                (EXTENSION_TOOL_OUTCOME_KEY, outcome_json),
                (UI_RENDER_METADATA_KEY, render_json),
            ]),
        ))
    }
}


// ─── Prompt 贡献 ──────────────────────────────────────────────────────

struct AgentPromptBuildHandler {
    shared: Arc<AgentShared>,
}

#[async_trait::async_trait]
impl PromptBuildHandler for AgentPromptBuildHandler {
    async fn handle(&self, ctx: PromptBuildContext) -> Result<PromptContributions, ExtensionError> {
        let agents = self.shared.get_or_discover(Some(&ctx.working_dir));
        Ok(PromptContributions {
            agents: vec![format_agents_for_model(&agents)],
            ..Default::default()
        })
    }
}

fn agent_tool_metadata()
-> std::collections::HashMap<String, astrcode_core::tool::ToolPromptMetadata> {
    let mut map = std::collections::HashMap::new();
    map.insert(
        "agent".to_string(),
        astrcode_core::tool::ToolPromptMetadata::new(
            "Use `agent` to delegate isolated tasks to specialized subagents. Agents run in the \
             background by default — launch them and continue working; results arrive in the next \
             turn. Prefer launching multiple agents in a single response to parallelize \
             independent tasks (e.g. investigate bug A while agent B searches for related code). \
             Only set waitForResult to true when your very next step depends on the agent's \
             output.",
        )
        .caveat(
            "For simple file reads or targeted searches, use Read/Grep directly instead of \
             spawning an agent. When launching multiple background agents, ensure their tasks are \
             non-overlapping to avoid duplicated work. Background agents are automatically \
             cancelled if the session ends.",
        )
        .prompt_tag("collaboration"),
    );
    map
}

// ─── 共享工具函数 ──────────────────────────────────────────────────────

/// 将 Agent 列表格式化为模型可读的文本，供 system prompt 和错误消息使用。
fn format_agents_for_model(agents: &[agent::AgentConfig]) -> String {
    if agents.is_empty() {
        return String::from("No agents configured.");
    }

    let mut lines = Vec::with_capacity(agents.len() + 1);
    lines.push(String::from(
        "Available agents (use the name before the colon as subagentType):",
    ));
    for agent in agents {
        lines.push(format!("- {}: {}", agent.name, agent.description));
    }
    lines.join("\n")
}

/// 截断文本用于内联显示。
fn compact_inline(text: &str, max_chars: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }

    let mut preview = compact.chars().take(max_chars).collect::<String>();
    preview.push('…');
    preview
}

// ─── 测试 ──────────────────────────────────────────────────────────────

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

        assert!(output.contains("Available agents"));
        assert!(output.contains("code-reviewer"));
        assert!(output.contains("Use for behavior-focused code review"));
        assert!(output.contains("subagentType"));
    }

    #[test]
    fn formats_empty_agent_list() {
        assert_eq!(format_agents_for_model(&[]), "No agents configured.");
    }

    #[test]
    fn agent_tool_schema_has_wait_for_result() {
        let definition = agent_tool_definition();
        let properties = definition.parameters["properties"]
            .as_object()
            .expect("tool schema properties");

        assert!(properties.contains_key("waitForResult"));
        assert_eq!(
            properties["waitForResult"]["default"],
            serde_json::json!(false)
        );
        assert!(!properties.contains_key("mode"));
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
        assert!(!args.wait_for_result);
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

    #[test]
    fn build_agent_run_passes_wait_for_result() {
        let agents = vec![agent::AgentConfig {
            id: String::from("explore"),
            name: String::from("explore"),
            description: String::from("explore code"),
            model: None,
            body: String::from("Explore."),
        }];

        let sync_input = json!({
            "prompt": "search", "description": "test", "subagentType": "explore", "waitForResult": true
        });
        let run = build_agent_run(&sync_input, &agents).unwrap();
        match run.outcome {
            ExtensionToolOutcome::RunSession {
                wait_for_result, ..
            } => {
                assert!(wait_for_result);
            },
            _ => panic!("expected RunSession"),
        }

        let async_input = json!({
            "prompt": "search", "description": "test", "subagentType": "explore", "waitForResult": false
        });
        let run = build_agent_run(&async_input, &agents).unwrap();
        match run.outcome {
            ExtensionToolOutcome::RunSession {
                wait_for_result, ..
            } => {
                assert!(!wait_for_result);
            },
            _ => panic!("expected RunSession"),
        }
    }

    #[test]
    fn build_agent_run_rejects_missing_subagent_type() {
        let agents = vec![agent::AgentConfig {
            id: String::from("explore"),
            name: String::from("explore"),
            description: String::from("explore code"),
            model: None,
            body: String::from("Explore."),
        }];

        let input = json!({ "prompt": "search", "description": "test" });
        let result = build_agent_run(&input, &agents);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("subagentType is required"),
            "error should mention subagentType: {err}"
        );
    }
}
