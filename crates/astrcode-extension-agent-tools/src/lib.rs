//! astrcode-extension-agent-tools — 子 Agent 委派与协作。
//!
//! 注册的工具：
//! - `agent`: 派生子 Agent 执行委派任务

mod agent;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use astrcode_extension_sdk::{
    extension::{
        ChildToolPolicy, Extension, ExtensionCapability, ExtensionError, PromptBuildContext,
        PromptBuildHandler, PromptContributions, Registrar, ToolHandler,
    },
    render::{RenderKeyValue, RenderSpec, RenderTone, UI_RENDER_METADATA_KEY},
    tool::{
        CreateSessionRequest, ExecutionMode, SubmitTurnRequest, ToolDefinition, ToolOrigin,
        ToolResult, tool_metadata,
    },
};
use astrcode_support::text::compact_inline;
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

    fn capabilities(&self) -> &[ExtensionCapability] {
        &[
            ExtensionCapability::SessionControl,
            ExtensionCapability::SmallModel,
        ]
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
    "Launch a specialized subagent. Types: [Agents].\n\nWhen NOT to use:\n- Simple tasks you can \
     finish quickly in the current mode\n- Known file path → `read`\n- Known symbol, class, or \
     pattern → `grep`/`glob`\n- Needle queries or anything doable in a few direct tool \
     calls\n\nTips:\n- Often useful for multi-step work that fits an [Agents] type (explore / \
     reviewer / execute)\n- Parallel or serial calls are your choice; split prompts by concern \
     when running several\n- Give each subagent a self-contained prompt (goal, scope, expected \
     output)\n- `waitForResult=false` runs in background and notifies when done";

const AGENT_TOOL_PARAMETERS: &str = r#"{"type":"object","properties":{"description":{"type":"string","description":"3-5 word task summary."},"prompt":{"type":"string","description":"Full task description for the subagent, with all context it needs."},"subagentType":{"type":"string","description":"Agent name from [Agents] section."},"waitForResult":{"type":"boolean","default":true,"description":"true: block until done. false: run in background, continue immediately."}},"required":["prompt","description"]}"#;

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
    true
}

fn agent_run_render_spec(args: &AgentArgs, agent_name: &str, resolved_model: &str) -> RenderSpec {
    let model = resolved_model;
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
                        value: agent_name.into(),
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
        ctx: &astrcode_extension_sdk::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        if tool_name != "agent" {
            return Err(ExtensionError::NotFound(tool_name.into()));
        }

        let agents = self.shared.get_or_discover(Some(working_dir));
        let args: AgentArgs = serde_json::from_value(arguments.clone())
            .map_err(|e| ExtensionError::Internal(format!("invalid agent args: {e}")))?;

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

        let model_preference = resolve_child_small_model(&ctx.capabilities)?;
        let model_label = model_preference.as_str();

        // 构造 UI 渲染元数据
        let render = agent_run_render_spec(&args, &matched.name, model_label);
        let render_json = serde_json::to_value(&render)
            .map_err(|e| ExtensionError::Internal(format!("serialize render: {e}")))?;

        // 获取 session_ops
        let session_ops =
            ctx.capabilities.session_ops.as_ref().ok_or_else(|| {
                ExtensionError::Internal("session operations not available".into())
            })?;

        // 1. 创建子会话
        let handle = session_ops
            .create_session(
                ctx.session_id.as_str(),
                CreateSessionRequest {
                    name: matched.name.clone(),
                    working_dir: None,
                    system_prompt: Some(enhance_agent_prompt(&matched.body, working_dir)),
                    model_preference: Some(model_preference),
                    // TODO： A BETTER policy 设计
                    tool_policy: Some(ChildToolPolicy::Deny {
                        tools: vec!["agent".into()],
                    }),
                    source_extension: Some("astrcode-agent-tools".into()),
                    ephemeral: true,
                    tool_call_id: ctx.tool_call_id.clone().unwrap_or_default(),
                },
            )
            .await
            .map_err(|e| ExtensionError::Internal(format!("create_session: {e}")))?;

        // 2. 提交 turn
        let submit = session_ops
            .submit_turn(
                ctx.session_id.as_str(),
                SubmitTurnRequest {
                    target_session_id: handle.session_id.clone(),
                    user_prompt: args.prompt,
                    wait_for_result: args.wait_for_result,
                    notify_parent_on_complete: if args.wait_for_result {
                        None
                    } else {
                        Some(
                            "[A background agent task has completed. Review the tool result above \
                             and present the findings to the user.]"
                                .into(),
                        )
                    },
                    recycle_on_complete: !args.wait_for_result,
                    tool_call_id: ctx.tool_call_id.clone(),
                },
            )
            .await;
        if let Err(ref e) = submit {
            if let Err(recycle_err) = session_ops
                .recycle_session(ctx.session_id.as_str(), &handle.session_id)
                .await
            {
                tracing::warn!(
                    child_session_id = %handle.session_id,
                    error = %recycle_err,
                    "failed to recycle child session after submit_turn error: {e}"
                );
            }
        }
        let result = submit.map_err(|e| ExtensionError::Internal(format!("submit_turn: {e}")))?;

        // 3. 构造 ToolResult
        let mut metadata = tool_metadata([
            (UI_RENDER_METADATA_KEY, render_json),
            ("child_session_id", serde_json::json!(handle.session_id)),
        ]);

        match result {
            astrcode_extension_sdk::tool::SubmitTurnResult::Completed { content } => {
                // 同步路径：turn 完成后回收 ephemeral 子 session
                if let Err(e) = session_ops
                    .recycle_session(ctx.session_id.as_str(), &handle.session_id)
                    .await
                {
                    tracing::warn!(
                        child_session_id = %handle.session_id,
                        error = %e,
                        "failed to recycle ephemeral child session"
                    );
                }
                Ok(ToolResult {
                    call_id: String::new(),
                    content,
                    is_error: false,
                    error: None,
                    metadata,
                    duration_ms: None,
                })
            },
            astrcode_extension_sdk::tool::SubmitTurnResult::Backgrounded {
                task_id,
                session_id,
            } => {
                // 异步路径：后台完成后由 notify_parent_on_complete 通知父 agent。
                // 回收留给后续调用或自动清理。
                metadata.insert("backgrounded".into(), serde_json::json!(true));
                metadata.insert("task_id".into(), serde_json::json!(task_id));
                Ok(ToolResult {
                    call_id: String::new(),
                    content: format!(
                        "异步 agent 已启动。完成后结果将在下一轮对话中可用。\nsession: \
                         {session_id}"
                    ),
                    is_error: false,
                    error: None,
                    metadata,
                    duration_ms: None,
                })
            },
        }
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
-> std::collections::HashMap<String, astrcode_extension_sdk::tool::ToolPromptMetadata> {
    let mut map = std::collections::HashMap::new();
    map.insert(
        "agent".to_string(),
        astrcode_extension_sdk::tool::ToolPromptMetadata::new(String::new())
            .example("Needle query (\"where is X defined?\") → `grep`/`glob`, not `agent`.")
            .example(
                "Multi-area exploration → `subagentType=explore`; parallel or serial as you see \
                 fit.",
            )
            .caveat("Don't duplicate work you delegate — stop the same searches yourself.")
            .caveat("Unknown `subagentType` → pick from [Agents]; do not invent agent names.")
            .prompt_tag(astrcode_extension_sdk::tool::ToolPromptTag::Collaboration),
    );
    map
}

// ─── 共享工具函数 ──────────────────────────────────────────────────────

/// 子 session 固定使用配置的小模型（`activeSmallModel` / effective `small_llm`）。
///
/// agent 文件中的 `model` 字段暂不生效；后续若支持按 agent 选模型再扩展此处。
fn resolve_child_small_model(
    caps: &astrcode_extension_sdk::tool::ToolCapabilities,
) -> Result<String, ExtensionError> {
    caps.llm_models
        .small
        .clone()
        .or_else(|| caps.small_model_id.clone())
        .ok_or_else(|| {
            ExtensionError::Internal(
                "子 Agent 需要已配置的小模型（activeSmallProfile + \
                 activeSmallModel）。请在设置中配置 Small LLM 后重试。"
                    .into(),
            )
        })
}

/// 为子 agent 的 body 追加共享增强内容：环境信息 + 行为规范。
fn enhance_agent_prompt(agent_body: &str, working_dir: &str) -> String {
    let os = std::env::consts::OS;
    let shell = astrcode_support::shell::resolve_shell().name;
    format!(
        "{}\n\n---\n\nNotes:\n- Agent threads always have their cwd reset between bash calls; \
         please only use absolute file paths.\n- In your final response, share file paths (always \
         absolute, never relative) that are relevant to the task. Include code snippets only when \
         the exact text is load-bearing.\n- For clear communication with the user, avoid using \
         emojis.\n- Do not use a colon before tool calls.\n\nEnvironment: working directory is \
         {working_dir}, OS is {os}, shell is {shell}.",
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
    fn formats_agent_metadata_for_model_selection() {
        let agents = vec![agent::AgentConfig {
            id: String::from("code-reviewer"),
            name: String::from("code-reviewer"),
            description: String::from("Use for behavior-focused code review"),
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

        assert!(definition.description.contains("When NOT to use"));
        assert!(definition.description.contains("Tips"));
        assert!(!definition.description.contains("When to use"));
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
    fn resolve_child_small_model_always_uses_configured_small_llm() {
        let caps = astrcode_extension_sdk::tool::ToolCapabilities {
            small_model_id: Some("haiku".into()),
            ..Default::default()
        };
        assert_eq!(resolve_child_small_model(&caps).unwrap(), "haiku");
    }

    #[test]
    fn resolve_child_small_model_errors_when_missing() {
        let caps = astrcode_extension_sdk::tool::ToolCapabilities::default();
        assert!(resolve_child_small_model(&caps).is_err());
    }
}
