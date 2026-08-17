//! S5R hook 调用的类型化 wire 契约。
//!
//! 输入 DTO 是 hook 事件 `input` 字段的唯一数据源:宿主在
//! `crates/astrcode-extensions/src/s5r_ext/mod.rs` 序列化它们,worker 侧类型化 handler
//! 反序列化同一类型;结果转换把 SDK 的 hook 结果枚举映射为宿主 `s5r_handler` 解析器
//! 接受的 `HandlerResult` effect/data 形状。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    config::ModelSelection,
    extension::{
        CompactTrigger, ContinueAfterStopResult, HookResult, PostToolUseResult, PreCompactResult,
        PreToolUseResult, PromptContributions, ProviderResult, ToolInputTransformResult,
    },
    llm::LlmMessage,
    s5r::{ErrorPayload, HandlerEffect, HandlerResult, ProviderContributionData},
    tool::{ToolDefinition, ToolResult},
    wire::WireErrorCode,
};

/// `pre_tool_use` 与 `tool_input_transform` hook 的 wire 输入。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolUseHookInput {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub tool_call_id: String,
    pub tool_name: String,
    pub tool_input: Value,
    pub available_tools: Vec<ToolDefinition>,
}

/// `post_tool_use` hook 的 wire 输入。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostToolUseHookInput {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub tool_call_id: String,
    pub tool_name: String,
    pub tool_input: Value,
    pub tool_result: ToolResult,
    pub is_error: bool,
}

/// `before_provider_request` 与 `after_provider_response` hook 的 wire 输入。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderHookInput {
    pub request_id: String,
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub messages: Vec<LlmMessage>,
}

/// `provider_contribution` hook 的 wire 输入;`acknowledge` 只在 durable provider 成功后投递。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderContributionHookInput {
    Prepare {
        request_id: String,
        session_id: String,
        working_dir: String,
        model: ModelSelection,
        messages: Vec<LlmMessage>,
    },
    Acknowledge {
        request_id: String,
        contribution_id: String,
        session_id: String,
        working_dir: String,
        model: ModelSelection,
    },
}

/// `continue_after_stop` hook 的 wire 输入。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinueAfterStopHookInput {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub assistant_text: String,
    pub finish_reason: String,
    pub continuations_this_turn: u32,
}

/// `prompt_build` hook 的 wire 输入。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptBuildHookInput {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
}

/// `pre_compact` hook 的 wire 输入。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreCompactHookInput {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub trigger: CompactTrigger,
    pub message_count: usize,
    pub source_messages: Vec<LlmMessage>,
    pub retained_file_limit: usize,
}

/// `post_compact` 通知 hook 的 wire 输入。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostCompactHookInput {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub trigger: CompactTrigger,
    pub message_count: usize,
    pub pre_tokens: usize,
    pub post_tokens: usize,
    pub summary: String,
}

/// 通用 lifecycle hook(`session_start`、`turn_end` 等)的 wire 输入。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleHookInput {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub mid_turn_user_messages_synced: u32,
}

impl From<HookResult> for HandlerResult {
    fn from(result: HookResult) -> Self {
        match result {
            HookResult::Allow => Self::ok(),
            HookResult::Block { reason } => {
                Self::effect(HandlerEffect::Block, json!({ "reason": reason }))
            },
        }
    }
}

impl From<PreToolUseResult> for HandlerResult {
    fn from(result: PreToolUseResult) -> Self {
        match result {
            PreToolUseResult::Allow => Self::ok(),
            PreToolUseResult::Block { reason } => {
                Self::effect(HandlerEffect::Block, json!({ "reason": reason }))
            },
            PreToolUseResult::Ask { prompt, rule_key } => Self::effect(
                HandlerEffect::Ask,
                json!({ "prompt": prompt, "rule_key": rule_key }),
            ),
        }
    }
}

impl From<ToolInputTransformResult> for HandlerResult {
    fn from(result: ToolInputTransformResult) -> Self {
        match result {
            ToolInputTransformResult::Unchanged => Self::ok(),
            ToolInputTransformResult::Replace { tool_input } => Self::effect(
                HandlerEffect::ReplaceToolInput,
                json!({ "tool_input": tool_input }),
            ),
        }
    }
}

impl From<PostToolUseResult> for HandlerResult {
    fn from(result: PostToolUseResult) -> Self {
        match result {
            PostToolUseResult::Allow => Self::ok(),
            PostToolUseResult::Block { reason } => {
                Self::effect(HandlerEffect::Block, json!({ "reason": reason }))
            },
            // 宿主只读取 `data.content`;`is_error` 等结构化字段由宿主保留,不能由 hook 覆盖。
            PostToolUseResult::ModifyResult { content } => {
                Self::effect(HandlerEffect::ToolOutcome, json!({ "content": content }))
            },
        }
    }
}

impl From<ProviderResult> for HandlerResult {
    fn from(result: ProviderResult) -> Self {
        match result {
            ProviderResult::Allow => Self::ok(),
            ProviderResult::Block { reason } => {
                Self::effect(HandlerEffect::Block, json!({ "reason": reason }))
            },
            ProviderResult::ReplaceMessages { messages } => Self::effect(
                HandlerEffect::ReplaceMessages,
                json!({ "messages": messages }),
            ),
            ProviderResult::AppendMessages { messages } => Self::effect(
                HandlerEffect::AppendMessages,
                json!({ "messages": messages }),
            ),
        }
    }
}

impl From<ContinueAfterStopResult> for HandlerResult {
    fn from(result: ContinueAfterStopResult) -> Self {
        match result {
            ContinueAfterStopResult::EndTurn => Self::ok(),
            ContinueAfterStopResult::ContinueOneStep => {
                Self::effect(HandlerEffect::ContinueOneStep, Value::Null)
            },
        }
    }
}

/// `PreCompactResult::Block` 只存在于 in-process 契约;S5R 宿主不接受该 effect,
/// 因此在 worker 边界提前失败而不是发出宿主必然拒绝的结果。
impl TryFrom<PreCompactResult> for HandlerResult {
    type Error = ErrorPayload;

    fn try_from(result: PreCompactResult) -> Result<Self, Self::Error> {
        match result {
            PreCompactResult::Allow => Ok(Self::ok()),
            PreCompactResult::Contributions(contributions) => {
                let data = serde_json::to_value(contributions).map_err(|error| {
                    ErrorPayload::new(
                        WireErrorCode::SerializationFailed,
                        format!("serialize compact contributions: {error}"),
                    )
                })?;
                Ok(Self::effect(HandlerEffect::CompactContributions, data))
            },
            PreCompactResult::Block { .. } => Err(ErrorPayload::new(
                WireErrorCode::Unsupported,
                "pre_compact block is not supported over S5R",
            )),
        }
    }
}

/// `prompt_build` 结果的 wire 映射:空贡献折叠为 `ok`,避免发送无语义的 effect。
pub fn prompt_contributions_to_wire(
    contributions: PromptContributions,
) -> Result<HandlerResult, ErrorPayload> {
    let PromptContributions {
        system_prompts,
        additional_instructions,
        skills,
        agents,
    } = &contributions;
    if system_prompts.is_empty()
        && additional_instructions.is_empty()
        && skills.is_empty()
        && agents.is_empty()
    {
        return Ok(HandlerResult::ok());
    }
    let data = serde_json::to_value(contributions).map_err(|error| {
        ErrorPayload::new(
            WireErrorCode::SerializationFailed,
            format!("serialize prompt contributions: {error}"),
        )
    })?;
    Ok(HandlerResult::effect(
        HandlerEffect::PromptContributions,
        data,
    ))
}

/// `provider_contribution` prepare 结果的 wire 映射;`acknowledge` 阶段固定返回 `ok`。
pub fn provider_contribution_to_wire(
    contribution: Option<ProviderContributionData>,
) -> Result<HandlerResult, ErrorPayload> {
    match contribution {
        None => Ok(HandlerResult::ok()),
        Some(data) => {
            let data = serde_json::to_value(data).map_err(|error| {
                ErrorPayload::new(
                    WireErrorCode::SerializationFailed,
                    format!("serialize provider contribution: {error}"),
                )
            })?;
            Ok(HandlerResult::effect(
                HandlerEffect::ProviderContribution,
                data,
            ))
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn model() -> Value {
        json!({ "profile_name": "default", "model": "model-x", "provider_kind": "openai" })
    }

    // 输入 DTO 是宿主序列化与 worker 反序列化的共同类型;这里的字面 JSON 钉住线格式本身
    // (字段名/枚举 tag),`deny_unknown_fields` + 回环断言使任一侧形状漂移都在测试期暴露。
    #[test]
    fn hook_inputs_mirror_host_payloads() {
        let mut tool_use_json = json!({
            "session_id": "s-1",
            "working_dir": "/workspace",
            "model": model(),
            "tool_call_id": "call-1",
            "tool_name": "read",
            "tool_input": { "path": "a.rs" },
            "available_tools": [{
                "name": "read",
                "description": "read a file",
                "parameters": { "type": "object" },
                "origin": "bundled"
            }]
        });
        let tool_use: ToolUseHookInput = serde_json::from_value(tool_use_json.clone()).unwrap();
        assert_eq!(tool_use.tool_name, "read");
        assert_eq!(tool_use.available_tools.len(), 1);
        // 反序列化接受的缺省字段在序列化时会物化(宿主发出的形状即 DTO 序列化形状)。
        tool_use_json["available_tools"][0]["strict"] = json!(false);
        assert_eq!(serde_json::to_value(&tool_use).unwrap(), tool_use_json);

        let post_tool_use: PostToolUseHookInput = serde_json::from_value(json!({
            "session_id": "s-1",
            "working_dir": "/workspace",
            "model": model(),
            "tool_call_id": "call-1",
            "tool_name": "read",
            "tool_input": {},
            "tool_result": { "content": "ok", "is_error": false, "metadata": {} },
            "is_error": false
        }))
        .unwrap();
        assert_eq!(post_tool_use.tool_result.content, "ok");

        let provider: ProviderHookInput = serde_json::from_value(json!({
            "request_id": "req-1",
            "session_id": "s-1",
            "working_dir": "/workspace",
            "model": model(),
            "messages": [{ "role": "user", "content": [{ "type": "text", "text": "hi" }] }]
        }))
        .unwrap();
        assert_eq!(provider.messages.len(), 1);

        let prepare: ProviderContributionHookInput = serde_json::from_value(json!({
            "phase": "prepare",
            "request_id": "req-1",
            "session_id": "s-1",
            "working_dir": "/workspace",
            "model": model(),
            "messages": []
        }))
        .unwrap();
        assert!(matches!(
            prepare,
            ProviderContributionHookInput::Prepare { .. }
        ));
        assert_eq!(
            serde_json::to_value(&prepare).unwrap(),
            json!({
                "phase": "prepare",
                "request_id": "req-1",
                "session_id": "s-1",
                "working_dir": "/workspace",
                "model": model(),
                "messages": []
            })
        );

        let acknowledge: ProviderContributionHookInput = serde_json::from_value(json!({
            "phase": "acknowledge",
            "request_id": "req-1",
            "contribution_id": "c-1",
            "session_id": "s-1",
            "working_dir": "/workspace",
            "model": model()
        }))
        .unwrap();
        assert!(matches!(
            acknowledge,
            ProviderContributionHookInput::Acknowledge { .. }
        ));

        let continue_after_stop: ContinueAfterStopHookInput = serde_json::from_value(json!({
            "session_id": "s-1",
            "working_dir": "/workspace",
            "model": model(),
            "assistant_text": "done",
            "finish_reason": "stop",
            "continuations_this_turn": 0
        }))
        .unwrap();
        assert_eq!(continue_after_stop.assistant_text, "done");

        let prompt_build: PromptBuildHookInput = serde_json::from_value(json!({
            "session_id": "s-1",
            "working_dir": "/workspace",
            "model": model()
        }))
        .unwrap();
        assert_eq!(prompt_build.session_id, "s-1");

        let pre_compact: PreCompactHookInput = serde_json::from_value(json!({
            "session_id": "s-1",
            "working_dir": "/workspace",
            "model": model(),
            "trigger": "manual_command",
            "message_count": 1,
            "source_messages": [{ "role": "user", "content": [{ "type": "text", "text": "hi" }] }],
            "retained_file_limit": 3
        }))
        .unwrap();
        assert_eq!(pre_compact.trigger, CompactTrigger::ManualCommand);

        let post_compact: PostCompactHookInput = serde_json::from_value(json!({
            "session_id": "s-1",
            "working_dir": "/workspace",
            "model": model(),
            "trigger": "auto_threshold",
            "message_count": 10,
            "pre_tokens": 100,
            "post_tokens": 50,
            "summary": "short"
        }))
        .unwrap();
        assert_eq!(post_compact.post_tokens, 50);
    }

    // 结果映射的期望值与 `astrcode-extensions/src/s5r_handler.rs` 各 parse_* 函数一一对应。
    #[test]
    fn hook_results_map_to_host_parseable_effects() {
        assert_eq!(
            HandlerResult::from(HookResult::Allow).effect,
            HandlerEffect::Ok
        );
        let block = HandlerResult::from(HookResult::Block {
            reason: "no".into(),
        });
        assert_eq!(block.effect, HandlerEffect::Block);
        assert_eq!(block.data["reason"], "no");

        let ask = HandlerResult::from(PreToolUseResult::Ask {
            prompt: "approve".into(),
            rule_key: Some("dangerous".into()),
        });
        assert_eq!(ask.effect, HandlerEffect::Ask);
        assert_eq!(ask.data["prompt"], "approve");
        assert_eq!(ask.data["rule_key"], "dangerous");

        let replace = HandlerResult::from(ToolInputTransformResult::Replace {
            tool_input: json!({ "canonical": true }),
        });
        assert_eq!(replace.effect, HandlerEffect::ReplaceToolInput);
        assert_eq!(replace.data["tool_input"], json!({ "canonical": true }));

        let modify = HandlerResult::from(PostToolUseResult::ModifyResult {
            content: "redacted".into(),
        });
        assert_eq!(modify.effect, HandlerEffect::ToolOutcome);
        assert_eq!(modify.data["content"], "redacted");
        assert!(modify.data.get("is_error").is_none());

        let append = HandlerResult::from(ProviderResult::AppendMessages {
            messages: vec![LlmMessage::user("extra")],
        });
        assert_eq!(append.effect, HandlerEffect::AppendMessages);
        assert_eq!(append.data["messages"].as_array().unwrap().len(), 1);

        assert_eq!(
            HandlerResult::from(ContinueAfterStopResult::EndTurn).effect,
            HandlerEffect::Ok
        );
        assert_eq!(
            HandlerResult::from(ContinueAfterStopResult::ContinueOneStep).effect,
            HandlerEffect::ContinueOneStep
        );

        assert_eq!(
            HandlerResult::try_from(PreCompactResult::Allow)
                .unwrap()
                .effect,
            HandlerEffect::Ok
        );
        let contributions = HandlerResult::try_from(PreCompactResult::Contributions(
            crate::extension::CompactContributions {
                instructions: vec!["preserve the plan".into()],
                retained_context: Vec::new(),
            },
        ))
        .unwrap();
        assert_eq!(contributions.effect, HandlerEffect::CompactContributions);
        assert_eq!(
            contributions.data["instructions"],
            json!(["preserve the plan"])
        );
        let blocked = HandlerResult::try_from(PreCompactResult::Block {
            reason: "no".into(),
        });
        assert_eq!(
            blocked.unwrap_err().code_enum(),
            Some(WireErrorCode::Unsupported)
        );

        assert_eq!(
            prompt_contributions_to_wire(PromptContributions::default())
                .unwrap()
                .effect,
            HandlerEffect::Ok
        );
        let mut prompts = PromptContributions::default();
        prompts.system_prompts.push("be terse".into());
        let prompts = prompt_contributions_to_wire(prompts).unwrap();
        assert_eq!(prompts.effect, HandlerEffect::PromptContributions);
        assert_eq!(prompts.data["system_prompts"], json!(["be terse"]));

        assert_eq!(
            provider_contribution_to_wire(None).unwrap().effect,
            HandlerEffect::Ok
        );
        let contribution = provider_contribution_to_wire(Some(ProviderContributionData {
            contribution_id: "pending-1".into(),
            effect: crate::s5r::ProviderContributionEffect::Unchanged {},
        }))
        .unwrap();
        assert_eq!(contribution.effect, HandlerEffect::ProviderContribution);
        assert_eq!(contribution.data["contribution_id"], "pending-1");
        assert_eq!(
            contribution.data["effect"],
            json!({ "message_effect": "unchanged" })
        );
    }
}
