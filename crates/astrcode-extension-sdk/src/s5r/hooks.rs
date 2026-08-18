//! Typed wire contracts for S5R hook invocations.
//!
//! The input DTOs are the single source of truth for a hook event's `input` field: the host
//! serializes them in `crates/astrcode-extensions/src/s5r_ext/mod.rs`, and the typed handlers on
//! the worker side deserialize the same types; the result conversions map the SDK's hook result
//! enums onto the `HandlerResult` effect/data shapes accepted by the host's `s5r_handler`
//! parsers.

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

/// Wire input for the `pre_tool_use` and `tool_input_transform` hooks.
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

/// Wire input for the `post_tool_use` hook.
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

/// Wire input for the `before_provider_request` and `after_provider_response` hooks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderHookInput {
    pub request_id: String,
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
    pub messages: Vec<LlmMessage>,
}

/// Wire input for the `provider_contribution` hook; `acknowledge` is only delivered after
/// durable provider success.
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

/// Wire input for the `continue_after_stop` hook.
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

/// Wire input for the `prompt_build` hook.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptBuildHookInput {
    pub session_id: String,
    pub working_dir: String,
    pub model: ModelSelection,
}

/// Wire input for the `pre_compact` hook.
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

/// Wire input for the `post_compact` notification hook.
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

/// Wire input for generic lifecycle hooks (`session_start`, `turn_end`, etc.).
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
            // The host only reads `data.content`; structured fields such as `is_error` are
            // preserved by the host and cannot be overridden by the hook.
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

/// `PreCompactResult::Block` exists only in the in-process contract; S5R hosts do not accept
/// that effect, so it fails early at the worker boundary rather than emitting a result the host
/// would necessarily reject.
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

/// Wire mapping for `prompt_build` results: empty contributions collapse to `ok`, avoiding a
/// meaningless effect on the wire.
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

/// Wire mapping for `provider_contribution` prepare results; the `acknowledge` phase always
/// returns `ok`.
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

    // The input DTOs are the shared type between host serialization and worker
    // deserialization; the literal JSON here pins the wire format itself (field names/enum
    // tags), and `deny_unknown_fields` plus round-trip assertions surface shape drift on either
    // side at test time.
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
        // Fields defaulted during deserialization become materialized when serialized (the
        // shape the host emits is the DTO's serialized shape).
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

    // The expected values of the result mappings correspond one-to-one with the parse_*
    // functions in `astrcode-extensions/src/s5r_handler.rs`.
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
