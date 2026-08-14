//! astrcode-extension-agent-tools — 子 Agent 委派与协作。
//!
//! 注册的工具：
//! - `agent`: 派生子 Agent 执行委派任务

mod agent;

use std::{collections::HashMap, sync::Arc};

use astrcode_extension_sdk::{
    builder::{ExtensionToolDefinition, manifest},
    discovery::DiscoveryCache,
    extension::{
        CompactContributions, CompactRetainedContext, Extension, ExtensionCall,
        ExtensionCapability, ExtensionError, ExtensionManifest, PreCompactContext,
        PreCompactHandler, PreCompactResult, PromptBuildContext, PromptBuildHandler,
        PromptContributions, Registrar, ToolContext, ToolHandler, ToolPlanContext,
    },
    llm::{LlmContent, LlmMessage, LlmRole},
    session::{
        HostCreateSessionRequest, HostRecycleSessionRequest, HostSubmitTurnOutput,
        HostSubmitTurnRequest,
    },
    tool::{
        ExecutionMode, HostResource, ToolDefinition, ToolOrigin, ToolPlan, ToolPromptMetadata,
        ToolPromptTag, ToolResult, tool_metadata,
    },
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
    fn manifest(&self) -> ExtensionManifest {
        manifest("astrcode-agent-tools")
            .version(env!("CARGO_PKG_VERSION"))
            .description(env!("CARGO_PKG_DESCRIPTION"))
            .capability(ExtensionCapability::SessionControl)
            .capability(ExtensionCapability::SessionHistory)
            .capability(ExtensionCapability::SmallModel)
            .build()
    }

    fn register(&self, reg: &mut Registrar) {
        let shared = Arc::new(AgentShared::new());
        reg.tool(
            ExtensionToolDefinition::from_definition(agent_tool_definition())
                .with_prompt(agent_tool_prompt()),
            Arc::new(AgentToolHandler {
                shared: shared.clone(),
            }),
        );
        reg.on_prompt_build(0, Arc::new(AgentPromptBuildHandler { shared }));
        reg.on_pre_compact(0, Arc::new(AgentPreCompactHandler));
    }
}

// ─── Agent 发现缓存 ────────────────────────────────────────────────────

/// Agent 发现结果缓存，按 working_dir 缓存。
struct AgentShared {
    cache: DiscoveryCache<Vec<agent::AgentConfig>>,
}

impl AgentShared {
    fn new() -> Self {
        Self {
            cache: DiscoveryCache::new(),
        }
    }

    fn get_or_discover(&self, working_dir: Option<&str>) -> Vec<agent::AgentConfig> {
        self.cache.get_or_discover(working_dir.unwrap_or(""), || {
            agent::discover_agents(working_dir)
        })
    }
}

// ─── agent 工具 ────────────────────────────────────────────────────────
//
// 定义 → 参数 → 构建逻辑 → 渲染 → Handler，自上而下阅读即可理解完整流程。

const AGENT_TOOL_DESCRIPTION: &str =
    "Launch a specialized subagent. Types: [Agents].\n\nWhen NOT to use:\n- Simple or needle \
     tasks; known path → `read`; symbol/pattern → `grep`/`glob`; few direct tool calls \
     enough\n\nTips:\n- One focused subtask per call; pass the objective, scope, constraints, \
     acceptance criteria, and known anchors — do not copy the parent transcript\n- Multiple \
     agents can run concurrently; call `agent` multiple times in one turn for parallel \
     execution\n- `waitForResult` (default true): when false, the agent runs in the background \
     and completion arrives as `<background-agent-notification>` with `<output>` in a later turn \
     (do not poll or re-run the task)";

const AGENT_TOOL_PARAMETERS: &str = r#"{"type":"object","properties":{"description":{"type":"string","description":"3-5 word task summary."},"prompt":{"type":"string","description":"Focused task packet: objective, scope, constraints, acceptance criteria, and known file/symbol anchors. Omit parent transcript and already-visible generic instructions."},"subagentType":{"type":"string","description":"Agent name from [Agents] section."},"waitForResult":{"type":"boolean","default":true,"description":"true: block until done. false: run in background, continue immediately."}},"required":["prompt","description"]}"#;
const AGENT_TOOL_NAME: &str = "agent";

fn agent_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: AGENT_TOOL_NAME.into(),
        description: AGENT_TOOL_DESCRIPTION.into(),
        parameters: serde_json::from_str(AGENT_TOOL_PARAMETERS)
            .unwrap_or_else(|_| json!({ "type": "object", "properties": {} })),
        strict: true,
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
    true
}

struct AgentToolHandler {
    shared: Arc<AgentShared>,
}

#[async_trait::async_trait]
impl ToolHandler for AgentToolHandler {
    async fn plan(&self, _ctx: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        Ok(ToolPlan::host(HostResource::Session))
    }

    async fn execute(
        &self,
        ctx: ToolContext,
    ) -> Result<astrcode_extension_sdk::tool::ToolExecutionResult, ExtensionError> {
        if ctx.tool_name() != AGENT_TOOL_NAME {
            return Err(ExtensionError::NotFound(ctx.tool_name().into()));
        }

        let working_dir = ctx.working_dir().to_string_lossy();
        let agents = self.shared.get_or_discover(Some(&working_dir));
        let args: AgentArgs = ctx.arguments()?;

        let matched = match args.subagent_type.as_deref() {
            None => {
                return Err(ExtensionError::Internal(format!(
                    "subagentType is required.\n\n{}",
                    format_agents_for_model(&agents)
                )));
            },
            Some("") => agents
                .first()
                .ok_or_else(|| ExtensionError::Internal("no agents configured".into()))?,
            Some(name) => agents
                .iter()
                .find(|a| a.name == name || a.id == name)
                .ok_or_else(|| {
                    ExtensionError::Internal(format!(
                        "agent '{name}' not found.\n\n{}",
                        format_agents_for_model(&agents)
                    ))
                })?,
        };

        let model_preference = resolve_child_small_model(ctx.small_model_id())?;
        let model_label = model_preference.clone();

        let session_control = ctx.host().session_control()?;

        // 1. 创建子会话
        let handle = session_control
            .create_child(HostCreateSessionRequest {
                name: matched.name.clone(),
                system_prompt: Some(enhance_agent_prompt(&matched.body, &working_dir)),
                model_preference: Some(model_preference),
                tool_selection: matched
                    .tool_selection
                    .clone()
                    .map(astrcode_extension_sdk::session::tool_selection_to_dto),
                ephemeral: true,
            })
            .await?;

        // 2. 提交 turn
        let submit = session_control
            .submit_turn(HostSubmitTurnRequest {
                target_session_id: handle.session_id.clone(),
                user_prompt: args.prompt,
                wait_for_result: args.wait_for_result,
                notify_parent_on_complete: if args.wait_for_result {
                    None
                } else {
                    Some(format!(
                        "Background agent \"{}\" completed",
                        args.description.trim()
                    ))
                },
                recycle_on_complete: !args.wait_for_result,
            })
            .await;
        if let Err(ref e) = submit
            && let Err(recycle_err) = session_control
                .recycle(HostRecycleSessionRequest::new(&handle.session_id))
                .await
        {
            tracing::warn!(
                child_session_id = %handle.session_id,
                error = %recycle_err,
                "failed to recycle child session after submit_turn error: {e}"
            );
        }
        let result = submit?;

        // 3. 构造 ToolResult
        let mut metadata = tool_metadata([
            ("child_session_id", serde_json::json!(handle.session_id)),
            ("agent_name", serde_json::json!(matched.name)),
            ("model", serde_json::json!(model_label)),
            ("wait_for_result", serde_json::json!(args.wait_for_result)),
        ]);

        match result {
            HostSubmitTurnOutput::Completed { content } => {
                // 同步路径：turn 完成后回收 ephemeral 子 session
                if let Err(e) = session_control
                    .recycle(HostRecycleSessionRequest::new(&handle.session_id))
                    .await
                {
                    tracing::warn!(
                        child_session_id = %handle.session_id,
                        error = %e,
                        "failed to recycle ephemeral child session"
                    );
                }
                Ok(ToolResult {
                    content,
                    is_error: false,
                    error: None,
                    metadata,
                    duration_ms: None,
                }
                .into())
            },
            HostSubmitTurnOutput::Backgrounded {
                task_id,
                session_id,
            } => {
                metadata.insert("backgrounded".into(), serde_json::json!(true));
                metadata.insert("task_id".into(), serde_json::json!(task_id));
                Ok(ToolResult {
                    content: format!(
                        "task_id: {task_id}\nstatus: running\nchild_session_id: \
                         {session_id}\nautomatic_notification: true\n\ndescription: \
                         {}\n\nnext_step: Completion arrives automatically in a later turn as \
                         `<background-agent-notification>` with `<output>` — do not poll or \
                         re-run the task.",
                        args.description.trim()
                    ),
                    is_error: false,
                    error: None,
                    metadata,
                    duration_ms: None,
                }
                .into())
            },
        }
    }
}


// ─── Prompt 贡献 ──────────────────────────────────────────────────────

struct AgentPromptBuildHandler {
    shared: Arc<AgentShared>,
}

struct AgentPreCompactHandler;

#[async_trait::async_trait]
impl PreCompactHandler for AgentPreCompactHandler {
    async fn handle(&self, ctx: PreCompactContext) -> Result<PreCompactResult, ExtensionError> {
        let Some(body) = agent_status(ctx.source_messages()) else {
            return Ok(PreCompactResult::Allow);
        };
        Ok(PreCompactResult::Contributions(CompactContributions {
            instructions: Vec::new(),
            retained_context: vec![CompactRetainedContext::Note {
                title: "Agent Task Status".into(),
                body,
            }],
        }))
    }
}

fn agent_status(messages: &[LlmMessage]) -> Option<String> {
    let mut descriptions = HashMap::new();
    let mut entries = Vec::new();

    for message in messages {
        match message.role {
            LlmRole::Assistant => {
                for content in &message.content {
                    let LlmContent::ToolCall {
                        call_id,
                        name,
                        arguments,
                        ..
                    } = content
                    else {
                        continue;
                    };
                    if name != AGENT_TOOL_NAME {
                        continue;
                    }
                    let description = arguments
                        .get("description")
                        .and_then(serde_json::Value::as_str)
                        .or_else(|| {
                            arguments
                                .get("subagentType")
                                .and_then(serde_json::Value::as_str)
                        })
                        .unwrap_or("agent task");
                    descriptions.insert(call_id.as_str(), description);
                }
            },
            LlmRole::Tool if message.name.as_deref() == Some(AGENT_TOOL_NAME) => {
                for content in &message.content {
                    let LlmContent::ToolResult {
                        tool_call_id,
                        content,
                        is_error,
                    } = content
                    else {
                        continue;
                    };
                    let description = descriptions
                        .get(tool_call_id.as_str())
                        .copied()
                        .unwrap_or("agent task");
                    let status = if *is_error {
                        "failed"
                    } else if content.lines().any(|line| line.trim() == "status: running") {
                        "running"
                    } else {
                        "completed"
                    };
                    let mut entry = format!("- {description}: {status}");
                    let excerpt = truncate_agent_result(content, 1_200);
                    if !excerpt.is_empty() {
                        entry.push('\n');
                        entry.push_str(&excerpt);
                    }
                    entries.push(entry);
                }
            },
            _ => {},
        }
    }

    let start = entries.len().saturating_sub(5);
    (!entries.is_empty()).then(|| entries[start..].join("\n\n"))
}

fn truncate_agent_result(content: &str, max_chars: usize) -> String {
    if content.chars().count() <= max_chars {
        return content.to_string();
    }
    let mut excerpt = content.chars().take(max_chars).collect::<String>();
    excerpt.push_str("\n\n[... agent result truncated]");
    excerpt
}

#[async_trait::async_trait]
impl PromptBuildHandler for AgentPromptBuildHandler {
    async fn handle(&self, ctx: PromptBuildContext) -> Result<PromptContributions, ExtensionError> {
        let working_dir = ctx.working_dir().to_string_lossy();
        let agents = self.shared.get_or_discover(Some(&working_dir));
        Ok(PromptContributions {
            agents: vec![format_agents_for_model(&agents)],
            ..Default::default()
        })
    }
}

fn agent_tool_prompt() -> ToolPromptMetadata {
    ToolPromptMetadata::new(
        "Scale delegation to the work instead of forcing a fixed workflow:\n- Quick lookup or \
         edit needing only a few direct tool calls → work directly\n- One clear, non-trivial \
         subtask that benefits from isolation → use the matching single agent\n- Multiple \
         independent subtasks → use matching agents in parallel\n- Dependent subtasks → sequence \
         only the agents actually needed\n\nUse `explore` for missing codebase facts, `execute` \
         for a self-contained implementation after the main agent has decided the design and \
         acceptance criteria, and `reviewer` for independent verification when the change's risk \
         or scope warrants it. Agent types may be used independently or combined. The main agent \
         retains product, architecture, protocol, dependency, and scope decisions.",
    )
    .example(
        "Planned cross-module auth change with a known design → split independent, \
         non-overlapping implementation slices across `execute` agents; use `reviewer` after \
         implementation because the change is security-sensitive. For a small equivalent change, \
         work directly.",
    )
    .caveat("Unknown `subagentType` → pick from [Agents].")
    .caveat("Don't parallel `execute` on overlapping files.")
    .prompt_tag(ToolPromptTag::Collaboration)
}

// ─── 共享工具函数 ──────────────────────────────────────────────────────

/// 子 session 固定使用配置的小模型（`activeSmallModel` / effective `small_llm`）。
///
/// agent 文件中的 `model` 字段暂不生效；后续若支持按 agent 选模型再扩展此处。
fn resolve_child_small_model(small_model_id: Option<&str>) -> Result<String, ExtensionError> {
    small_model_id.map(str::to_owned).ok_or_else(|| {
        ExtensionError::Internal(
            "子 Agent 需要已配置的小模型（activeSmallProfile + activeSmallModel）。请在设置中配置 \
             Small LLM 后重试。"
                .into(),
        )
    })
}

/// 为子 agent 的 body 追加共享增强内容：环境信息 + 行为规范。
fn enhance_agent_prompt(agent_body: &str, working_dir: &str) -> String {
    let os = std::env::consts::OS;
    let shell = astrcode_extension_sdk::shell::resolve_shell().name;
    format!(
        "{}\n\n---\n\nHandoff contract:\n- Return a decision packet, not a work diary. Put the \
         conclusion first and keep the whole response within about 600 tokens unless correctness \
         requires more.\n- Include only task-relevant conclusions, supporting evidence, completed \
         work, validation status, and unresolved risks.\n- Omit the repeated task, routine \
         searches, generic praise, and code excerpts unless exact text is necessary evidence.\n- \
         If blocked or uncertain, say why and name the smallest missing input. Never trade \
         correctness for brevity.\n\nRuntime notes:\n- Bash calls reset cwd; use absolute \
         paths.\n- Relevant file references must be absolute. Avoid emojis and do not use a colon \
         before tool calls.\n\nEnvironment: working directory is {working_dir}, OS is {os}, shell \
         is {shell}.",
        agent_body.trim(),
    )
}
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

// ─── 测试 ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
    fn agent_args_reject_missing_prompt() {
        let input = json!({ "description": "test" });
        let result = serde_json::from_value::<AgentArgs>(input);
        assert!(result.is_err());
    }

    #[test]
    fn agent_catalog_and_prompt_metadata_keep_distinct_delegation_responsibilities() {
        let configured = vec![agent::AgentConfig {
            id: "code-reviewer".into(),
            name: "code-reviewer".into(),
            description: "Use for behavior-focused code review".into(),
            body: "Review carefully.".into(),
            tool_selection: None,
        }];
        let rendered = format_agents_for_model(&configured);
        for expected in [
            "Available agents",
            "code-reviewer",
            "Use for behavior-focused code review",
            "subagentType",
        ] {
            assert!(rendered.contains(expected));
        }
        assert_eq!(format_agents_for_model(&[]), "No agents configured.");

        let agents = agent::builtin_agents();
        let by_id = |id: &str| agents.iter().find(|agent| agent.id == id).unwrap();
        assert!(
            by_id("explore")
                .description
                .contains("before the main agent makes a design")
        );
        assert!(
            by_id("execute")
                .description
                .contains("concrete plan defined by the main agent")
        );
        assert!(
            by_id("reviewer")
                .description
                .contains("after implementation")
        );
        assert!(
            by_id("explore")
                .body
                .contains("complete the delegated investigation")
        );
        assert!(
            by_id("explore")
                .body
                .contains("report the concrete blocker")
        );

        let metadata = agent_tool_prompt();
        assert!(
            metadata
                .guide
                .contains("instead of forcing a fixed workflow")
        );
        assert!(metadata.guide.contains("used independently or combined"));
        assert_eq!(metadata.examples.len(), 1);
        assert!(metadata.examples[0].contains("work directly"));

        let enhanced = enhance_agent_prompt("Role guidance.", "/workspace");
        assert!(enhanced.contains("Return a decision packet, not a work diary"));
        assert!(enhanced.contains("within about 600 tokens"));
        assert!(enhanced.contains("Never trade correctness for brevity"));
        assert!(enhanced.contains("working directory is /workspace"));

        let status = agent_status(&[
            LlmMessage {
                role: LlmRole::Assistant,
                content: vec![LlmContent::ToolCall {
                    call_id: "running-agent".into(),
                    name: AGENT_TOOL_NAME.into(),
                    arguments: json!({"description": "inspect compact flow"}),
                    raw_arguments: None,
                }],
                name: None,
                reasoning_content: None,
            },
            LlmMessage::tool(
                AGENT_TOOL_NAME,
                "running-agent",
                "task_id: task-1\nstatus: running",
                false,
            ),
            LlmMessage::tool(AGENT_TOOL_NAME, "unknown-agent", "worker failed", true),
        ])
        .expect("agent results contribute compact status");
        assert!(status.contains("inspect compact flow: running"));
        assert!(status.contains("agent task: failed"));
    }

    #[test]
    fn resolve_child_small_model_always_uses_configured_small_llm() {
        assert_eq!(resolve_child_small_model(Some("haiku")).unwrap(), "haiku");
    }

    #[test]
    fn resolve_child_small_model_errors_when_missing() {
        assert!(resolve_child_small_model(None).is_err());
    }
}
