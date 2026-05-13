//! astrcode-extension-agent-tools — 子 Agent 委派与协作。
//!
//! 注册的工具：
//! - `agent`: 派生子 Agent 执行委派任务
//! - `send`: 向已运行的子 Agent 发送消息并等待回复

mod agent;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use astrcode_core::{
    extension::{
        Extension, ExtensionError, ExtensionToolOutcome, PromptBuildContext, PromptBuildHandler,
        PromptContributions, Registrar, ToolHandler,
    },
    render::{RenderKeyValue, RenderSpec, RenderTone, UI_RENDER_METADATA_KEY},
    tool::{
        AgentSessionControl, ExecutionMode, ToolDefinition, ToolOrigin, ToolResult, tool_metadata,
    },
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

    fn register(&self, reg: &mut Registrar) {
        let shared = Arc::new(AgentShared::new());
        reg.tool(
            agent_tool_definition(),
            Arc::new(AgentToolHandler {
                shared: shared.clone(),
            }),
        );
        reg.tool(send_tool_definition(), Arc::new(SendToolHandler));
        reg.tool_metadata(agent_tool_metadata());
        reg.on_prompt_build(
            0,
            Arc::new(AgentPromptBuildHandler {
                shared: shared.clone(),
            }),
        );
    }
}

// ─── Shared Cache ───────────────────────────────────────────────────────

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
        if let Some(agents) = self.cache.lock().unwrap().get(key) {
            return agents.clone();
        }
        let agents = agent::discover_agents(working_dir);
        self.cache
            .lock()
            .unwrap()
            .entry(key.to_string())
            .or_insert_with(|| agents.clone());
        agents
    }
}

// ─── agent 工具 ────────────────────────────────────────────────────────

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
                ("extension_tool_outcome", outcome_json),
                (UI_RENDER_METADATA_KEY, render_json),
            ]),
        ))
    }
}

// ─── send 工具 ─────────────────────────────────────────────────────────

struct SendToolHandler;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendArgs {
    /// 目标 Agent 的 session ID 或 agent name。
    agent_id: String,
    /// 发送给子 Agent 的消息。
    message: String,
}

#[async_trait::async_trait]
impl ToolHandler for SendToolHandler {
    async fn execute(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        _working_dir: &str,
        ctx: &astrcode_core::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        if tool_name != "send" {
            return Err(ExtensionError::NotFound(tool_name.into()));
        }

        let args: SendArgs = serde_json::from_value(arguments)
            .map_err(|e| ExtensionError::Internal(format!("invalid send args: {e}")))?;

        let port = ctx
            .capabilities
            .agent_session_control
            .as_ref()
            .ok_or_else(|| ExtensionError::Internal("agent session control not available".into()))?;

        // 尝试把 agent_id 解析为 agent_name → session_id
        let child_session_id =
            resolve_child_session_id(port, &args.agent_id, ctx.session_id.as_str())
                .await
                .unwrap_or_else(|| args.agent_id.clone());

        let result = port
            .send_and_wait(&child_session_id, args.message.clone())
            .await
            .map_err(|e| ExtensionError::Internal(format!("send_and_wait: {e}")))?;

        let (content, is_error) = match result {
            astrcode_core::tool::TurnResult::Completed { output } => {
                if output.is_empty() {
                    ("(agent completed with no output)".into(), false)
                } else {
                    (output, false)
                }
            },
            astrcode_core::tool::TurnResult::Failed { error } => (error, true),
            astrcode_core::tool::TurnResult::Aborted => {
                ("agent was aborted".into(), true)
            },
        };

        let render = RenderSpec::Box {
            title: None,
            tone: if is_error {
                RenderTone::Error
            } else {
                RenderTone::Default
            },
            children: vec![
                RenderSpec::KeyValue {
                    entries: vec![RenderKeyValue {
                        key: "send".into(),
                        value: format!("→ {child_session_id}"),
                        tone: RenderTone::Muted,
                    }],
                    tone: RenderTone::Default,
                },
                RenderSpec::Text {
                    text: compact_inline(&content, 500),
                    tone: if is_error {
                        RenderTone::Error
                    } else {
                        RenderTone::Default
                    },
                },
            ],
        };
        let render_json = serde_json::to_value(&render)
            .map_err(|e| ExtensionError::Internal(format!("serialize render: {e}")))?;

        Ok(ToolResult::text(
            content,
            is_error,
            tool_metadata([(UI_RENDER_METADATA_KEY, render_json)]),
        ))
    }
}

/// 把 agent_id（可能是 name 或 session_id）解析为 session_id。
///
/// 先查 list_children 匹配 agent_name；若未匹配，直接当作 session_id 使用。
async fn resolve_child_session_id(
    port: &Arc<dyn AgentSessionControl>,
    agent_id: &str,
    parent_session_id: &str,
) -> Option<String> {
    let children = port.list_children(parent_session_id).await.ok()?;
    children
        .iter()
        .find(|c| c.agent_name == agent_id)
        .map(|c| c.session_id.clone())
}

// ─── Prompt Build ──────────────────────────────────────────────────────

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

// ─── Prompt Metadata ───────────────────────────────────────────────────

fn agent_tool_metadata()
-> std::collections::HashMap<String, astrcode_core::tool::ToolPromptMetadata> {
    let mut map = std::collections::HashMap::new();
    map.insert(
        "agent".to_string(),
        astrcode_core::tool::ToolPromptMetadata::new(
            "Use `agent` to delegate isolated tasks to specialized subagents. By default, the \
             agent runs synchronously and blocks until completion — use this when your next step \
             depends on the result. Set waitForResult to false to run the agent in the background \
             and continue working. Background agent results arrive as a notification in the next \
             turn.",
        )
        .caveat(
            "For simple file reads or targeted searches, use Read/Grep directly instead of \
             spawning an agent. When running agents in the background (waitForResult: false), \
             avoid duplicating their work — work on non-overlapping tasks. Background agents are \
             automatically cancelled if the session ends.",
        )
        .prompt_tag("collaboration"),
    );
    map.insert(
        "send".to_string(),
        astrcode_core::tool::ToolPromptMetadata::new(
            "Send a message to an already-running agent and wait for its response. Use this to \
             continue a conversation with an agent that was started with waitForResult: false, \
             or to give additional instructions to a running agent.",
        )
        .caveat(
            "The agentId can be either the agent's name (e.g. 'Code Reviewer') or its session \
             ID. Sending blocks until the agent responds — for long tasks, consider spawning a \
             new background agent instead.",
        )
        .prompt_tag("collaboration"),
    );
    map
}

// ─── agent 工具实现 ────────────────────────────────────────────────────

/// LLM tool call 参数类型。
///
/// JSON schema 定义了 LLM 的调用契约，因此 `rename_all = "camelCase"` 是合理的。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentArgs {
    prompt: String,
    description: String,
    subagent_type: Option<String>,
    /// 是否同步阻塞等待子 agent 完成。默认 `true`。
    #[serde(default = "default_wait_for_result")]
    wait_for_result: bool,
}

const fn default_wait_for_result() -> bool {
    true
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

    let matched = match args.subagent_type.as_deref() {
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
        render: agent_run_render_spec(&args, matched),
        outcome: ExtensionToolOutcome::RunSession {
            name: matched.name.clone(),
            system_prompt: matched.body.clone(),
            user_prompt: args.prompt,
            model_preference: matched.model.clone(),
            wait_for_result: args.wait_for_result,
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

fn compact_inline(text: &str, max_chars: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }

    let mut preview = compact.chars().take(max_chars).collect::<String>();
    preview.push('…');
    preview
}

// ─── 工具定义 ─────────────────────────────────────────────────────────

const AGENT_TOOL_DESCRIPTION: &str =
    "Launch a specialized subagent for one narrow, delegated task. By default, blocks until the \
     agent completes and returns its result. Set waitForResult to false to run the agent in the \
     background — you can continue working and the result will be available in the next turn. See \
     the [Agents] section in the system prompt for available agent types.";

const AGENT_TOOL_PARAMETERS: &str = r#"{"type":"object","properties":{"description":{"type":"string","description":"Short 3-5 word description of the task"},"prompt":{"type":"string","description":"Task for the subagent"},"subagentType":{"type":"string","description":"Agent name from agents/ directory"},"waitForResult":{"type":"boolean","default":true,"description":"If true (default), block until the agent completes. If false, run in the background and return immediately."}},"required":["prompt","description"]}"#;

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

const SEND_TOOL_DESCRIPTION: &str =
    "Send a message to a running agent and wait for its response. Use this to continue a \
     conversation with an agent that was started with waitForResult: false, or to give \
     additional instructions to a running agent.";

const SEND_TOOL_PARAMETERS: &str = r#"{"type":"object","properties":{"agentId":{"type":"string","description":"The agent's name or session ID to send the message to"},"message":{"type":"string","description":"The message to send to the agent"}},"required":["agentId","message"]}"#;

fn send_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "send".into(),
        description: SEND_TOOL_DESCRIPTION.into(),
        parameters: serde_json::from_str(SEND_TOOL_PARAMETERS)
            .unwrap_or_else(|_| json!({ "type": "object", "properties": {} })),
        origin: ToolOrigin::Bundled,
        execution_mode: ExecutionMode::Sequential,
    }
}

/// 将 Agent 列表格式化为模型可读的文本。
///
/// 格式设计原则：
/// - 清晰的标题和格式，让 LLM 知道这是什么
/// - 包含足够的信息让 LLM 做出选择
/// - 简洁但自包含
fn format_agents_for_model(agents: &[agent::AgentConfig]) -> String {
    if agents.is_empty() {
        return String::from("No agents configured.");
    }

    let mut lines = Vec::with_capacity(agents.len() + 2);
    lines.push(String::from(
        "Available agents (use the name before the colon as subagentType):",
    ));
    for agent in agents {
        lines.push(format!("- {}: {}", agent.name, agent.description));
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
            serde_json::json!(true)
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
        assert!(args.wait_for_result);
    }

    #[test]
    fn agent_args_async_mode() {
        let input = json!({
            "prompt": "find the bug",
            "description": "bug hunt",
            "waitForResult": false
        });
        let args: AgentArgs = serde_json::from_value(input).unwrap();
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
            "prompt": "search", "description": "test", "waitForResult": true
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
            "prompt": "search", "description": "test", "waitForResult": false
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
    fn send_args_deserialize() {
        let input = json!({
            "agentId": "code-reviewer",
            "message": "check this file"
        });
        let args: SendArgs = serde_json::from_value(input).unwrap();
        assert_eq!(args.agent_id, "code-reviewer");
        assert_eq!(args.message, "check this file");
    }

    #[test]
    fn send_args_reject_missing_fields() {
        let no_message = json!({ "agentId": "x" });
        assert!(serde_json::from_value::<SendArgs>(no_message).is_err());

        let no_id = json!({ "message": "hi" });
        assert!(serde_json::from_value::<SendArgs>(no_id).is_err());
    }

    #[test]
    fn send_tool_definition_has_required_fields() {
        let send_def = send_tool_definition();
        assert_eq!(send_def.name, "send");
        assert_eq!(send_def.origin, ToolOrigin::Bundled);
        assert_eq!(send_def.execution_mode, ExecutionMode::Sequential);
    }
}
