//! S5R handler identity construction and wire-result translation.

use astrcode_extension_sdk::{
    extension::{
        CommandCompletions, CompactContributions, ContinueAfterStopResult, ExtensionCommandResult,
        ExtensionError, ExtensionHttpResponse, HookResult, PostToolUseResult, PreCompactResult,
        PreToolUseResult, PreparedProviderContribution, PreparedProviderEffect,
        PromptContributions, ProviderContributionId, ProviderResult, ToolInputTransformResult,
    },
    tool::ToolResult,
    wire::{HandlerEffect, HandlerId, HandlerKind, HandlerResult, ToolOutcome},
};
pub(crate) fn parse_http_response(
    resp: &HandlerResult,
) -> Result<ExtensionHttpResponse, ExtensionError> {
    if resp.effect != HandlerEffect::HttpResponse {
        return Err(ExtensionError::Internal(format!(
            "expected http_response effect, got {:?}",
            resp.effect
        )));
    }
    serde_json::from_value(resp.data.clone())
        .map_err(|error| ExtensionError::Internal(format!("parse HTTP response: {error}")))
}

pub(crate) fn parse_provider_contribution(
    resp: &HandlerResult,
) -> Result<Option<PreparedProviderContribution>, ExtensionError> {
    if resp.effect == HandlerEffect::Ok {
        return Ok(None);
    }
    if resp.effect != HandlerEffect::ProviderContribution {
        return Err(unexpected_effect("provider_contribution", resp.effect));
    }
    let data = serde_json::from_value::<astrcode_extension_sdk::wire::ProviderContributionData>(
        resp.data.clone(),
    )
    .map_err(|error| ExtensionError::Internal(format!("parse provider contribution: {error}")))?;
    if data.contribution_id.trim().is_empty() {
        return Err(ExtensionError::Internal(
            "provider contribution id cannot be empty".into(),
        ));
    }
    let effect = match data.effect {
        astrcode_extension_sdk::wire::ProviderContributionEffect::Unchanged {} => {
            PreparedProviderEffect::Unchanged
        },
        astrcode_extension_sdk::wire::ProviderContributionEffect::ReplaceMessages { messages } => {
            PreparedProviderEffect::ReplaceMessages(messages)
        },
        astrcode_extension_sdk::wire::ProviderContributionEffect::AppendMessages { messages } => {
            PreparedProviderEffect::AppendMessages(messages)
        },
    };
    Ok(Some(PreparedProviderContribution::new(
        ProviderContributionId::new(data.contribution_id),
        effect,
    )))
}

pub(crate) fn handler_id(
    extension_id: &str,
    kind: HandlerKind,
    name: &str,
) -> Result<HandlerId, ExtensionError> {
    HandlerId::new(extension_id, kind, name).map_err(ExtensionError::Internal)
}

pub(crate) fn parse_tool_result(resp: &HandlerResult) -> Result<ToolResult, ExtensionError> {
    match resp.effect {
        HandlerEffect::ToolOutcome => {
            let outcome: ToolOutcome = serde_json::from_value(resp.data.clone())
                .map_err(|e| ExtensionError::Internal(format!("parse tool_outcome: {e}")))?;
            Ok(ToolResult::text(
                outcome.content,
                outcome.is_error,
                outcome.metadata,
            ))
        },
        effect => Err(unexpected_effect("tool", effect)),
    }
}

pub(crate) fn parse_command_result(
    resp: &HandlerResult,
) -> Result<ExtensionCommandResult, ExtensionError> {
    if resp.effect != HandlerEffect::Ok {
        return Err(unexpected_effect("command", resp.effect));
    }
    serde_json::from_value(resp.data.clone())
        .map_err(|e| ExtensionError::Internal(format!("parse command result: {e}")))
}

pub(crate) fn parse_command_completions(
    resp: &HandlerResult,
) -> Result<CommandCompletions, ExtensionError> {
    if resp.effect != HandlerEffect::Ok {
        return Err(unexpected_effect("command_complete", resp.effect));
    }
    serde_json::from_value(resp.data.clone())
        .map_err(|error| ExtensionError::Internal(format!("parse command completions: {error}")))
}

pub(crate) fn parse_pre_tool_use_result(
    resp: &HandlerResult,
) -> Result<PreToolUseResult, ExtensionError> {
    match resp.effect {
        HandlerEffect::Ok => Ok(PreToolUseResult::Allow),
        HandlerEffect::Block => Ok(PreToolUseResult::Block {
            reason: required_data_string(resp, "reason")?,
        }),
        HandlerEffect::Ask => Ok(PreToolUseResult::Ask {
            prompt: required_data_string(resp, "prompt")?,
            rule_key: optional_data_string(resp, "rule_key")?,
        }),
        effect => Err(unexpected_effect("pre_tool_use", effect)),
    }
}

pub(crate) fn parse_tool_input_transform_result(
    resp: &HandlerResult,
) -> Result<ToolInputTransformResult, ExtensionError> {
    match resp.effect {
        HandlerEffect::Ok => Ok(ToolInputTransformResult::Unchanged),
        HandlerEffect::ReplaceToolInput => Ok(ToolInputTransformResult::Replace {
            tool_input: required_data_value(resp, "tool_input")?,
        }),
        effect => Err(unexpected_effect("tool_input_transform", effect)),
    }
}

pub(crate) fn parse_post_tool_use_result(
    resp: &HandlerResult,
) -> Result<PostToolUseResult, ExtensionError> {
    match resp.effect {
        HandlerEffect::Ok => Ok(PostToolUseResult::Allow),
        HandlerEffect::Block => Ok(PostToolUseResult::Block {
            reason: required_data_string(resp, "reason")?,
        }),
        HandlerEffect::ToolOutcome => Ok(PostToolUseResult::ModifyResult {
            content: required_data_string(resp, "content")?,
        }),
        effect => Err(unexpected_effect("post_tool_use", effect)),
    }
}

pub(crate) fn parse_provider_result(
    resp: &HandlerResult,
) -> Result<ProviderResult, ExtensionError> {
    match resp.effect {
        HandlerEffect::Ok => Ok(ProviderResult::Allow),
        HandlerEffect::Block => Ok(ProviderResult::Block {
            reason: required_data_string(resp, "reason")?,
        }),
        HandlerEffect::ReplaceMessages => {
            let messages_val = required_data_value(resp, "messages")?;
            Ok(ProviderResult::ReplaceMessages {
                messages: serde_json::from_value(messages_val)
                    .map_err(|e| ExtensionError::Internal(format!("parse messages: {e}")))?,
            })
        },
        HandlerEffect::AppendMessages => {
            let messages_val = required_data_value(resp, "messages")?;
            Ok(ProviderResult::AppendMessages {
                messages: serde_json::from_value(messages_val)
                    .map_err(|e| ExtensionError::Internal(format!("parse messages: {e}")))?,
            })
        },
        effect => Err(unexpected_effect("provider", effect)),
    }
}

pub(crate) fn parse_continue_after_stop_result(
    resp: &HandlerResult,
) -> Result<ContinueAfterStopResult, ExtensionError> {
    match resp.effect {
        HandlerEffect::ContinueOneStep => Ok(ContinueAfterStopResult::ContinueOneStep),
        HandlerEffect::Ok => Ok(ContinueAfterStopResult::EndTurn),
        effect => Err(unexpected_effect("continue_after_stop", effect)),
    }
}

pub(crate) fn parse_prompt_build_result(
    resp: &HandlerResult,
) -> Result<PromptContributions, ExtensionError> {
    match resp.effect {
        HandlerEffect::Ok => return Ok(PromptContributions::default()),
        HandlerEffect::PromptContributions => {},
        effect => return Err(unexpected_effect("prompt_build", effect)),
    }
    serde_json::from_value(resp.data.clone())
        .map_err(|e| ExtensionError::Internal(format!("parse PromptContributions: {e}")))
}

pub(crate) fn parse_pre_compact_result(
    resp: &HandlerResult,
) -> Result<PreCompactResult, ExtensionError> {
    match resp.effect {
        HandlerEffect::Ok if resp.data.is_null() => return Ok(PreCompactResult::Allow),
        HandlerEffect::Ok => {
            return Err(ExtensionError::Internal(
                "pre_compact allow result must return no data".into(),
            ));
        },
        HandlerEffect::CompactContributions => {},
        effect => return Err(unexpected_effect("pre_compact", effect)),
    }
    let contributions: CompactContributions = serde_json::from_value(resp.data.clone())
        .map_err(|e| ExtensionError::Internal(format!("parse CompactContributions: {e}")))?;
    Ok(PreCompactResult::Contributions(contributions))
}

pub(crate) fn parse_post_compact_result(resp: &HandlerResult) -> Result<(), ExtensionError> {
    if resp.effect != HandlerEffect::Ok {
        return Err(unexpected_effect("post_compact", resp.effect));
    }
    if !resp.data.is_null() {
        return Err(ExtensionError::Internal(
            "post_compact notification must return no data".into(),
        ));
    }
    Ok(())
}

pub(crate) fn parse_lifecycle_result(resp: &HandlerResult) -> Result<HookResult, ExtensionError> {
    match resp.effect {
        HandlerEffect::Block => Ok(HookResult::Block {
            reason: required_data_string(resp, "reason")?,
        }),
        HandlerEffect::Ok => Ok(HookResult::Allow),
        effect => Err(unexpected_effect("lifecycle", effect)),
    }
}

fn unexpected_effect(handler: &str, effect: HandlerEffect) -> ExtensionError {
    ExtensionError::Internal(format!(
        "unexpected {effect:?} effect from {handler} handler"
    ))
}

fn required_data_value(
    result: &HandlerResult,
    field: &str,
) -> Result<serde_json::Value, ExtensionError> {
    result.data.get(field).cloned().ok_or_else(|| {
        ExtensionError::Internal(format!("effect={:?} requires data.{field}", result.effect))
    })
}

fn required_data_string(result: &HandlerResult, field: &str) -> Result<String, ExtensionError> {
    required_data_value(result, field)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| {
            ExtensionError::Internal(format!(
                "effect={:?} requires string data.{field}",
                result.effect
            ))
        })
}

fn optional_data_string(
    result: &HandlerResult,
    field: &str,
) -> Result<Option<String>, ExtensionError> {
    match result.data.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ExtensionError::Internal(format!(
            "effect={:?} requires optional string data.{field}",
            result.effect
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handler_results_keep_transform_and_admission_effects_disjoint_and_strict() {
        let missing_reason = HandlerResult::effect(HandlerEffect::Block, serde_json::json!({}));
        let invalid_reason =
            HandlerResult::effect(HandlerEffect::Block, serde_json::json!({"reason": false}));
        let missing_messages =
            HandlerResult::effect(HandlerEffect::AppendMessages, serde_json::json!({}));
        let ask = HandlerResult::effect(
            HandlerEffect::Ask,
            serde_json::json!({"prompt": "approve", "rule_key": "dangerous"}),
        );
        let replace = HandlerResult::effect(
            HandlerEffect::ReplaceToolInput,
            serde_json::json!({"tool_input": {"canonical": true}}),
        );
        let contribution = HandlerResult::effect(
            HandlerEffect::ProviderContribution,
            serde_json::json!({
                "contribution_id": "pending-1",
                "effect": {"message_effect": "unchanged"}
            }),
        );
        let invalid_contribution = HandlerResult::effect(
            HandlerEffect::ProviderContribution,
            serde_json::json!({
                "settlement": "transient",
                "effect": {"message_effect": "unchanged"}
            }),
        );
        let compact = HandlerResult::effect(
            HandlerEffect::CompactContributions,
            serde_json::json!({
                "instructions": ["preserve the plan"],
                "retained_context": [
                    {"kind": "note", "title": "Plan", "body": "typed boundary"},
                    {"kind": "file", "path": "src/lib.rs", "content": "fresh"}
                ]
            }),
        );
        let incomplete_compact = HandlerResult::effect(
            HandlerEffect::CompactContributions,
            serde_json::json!({"instructions": []}),
        );
        let post_with_data =
            HandlerResult::effect(HandlerEffect::Ok, serde_json::json!({"ignored": true}));

        assert!(parse_pre_tool_use_result(&missing_reason).is_err());
        assert!(parse_lifecycle_result(&invalid_reason).is_err());
        assert!(parse_provider_result(&missing_messages).is_err());
        assert!(parse_tool_result(&HandlerResult::ok()).is_err());
        assert!(matches!(
            parse_pre_tool_use_result(&ask),
            Ok(PreToolUseResult::Ask { prompt, rule_key })
                if prompt == "approve" && rule_key.as_deref() == Some("dangerous")
        ));
        assert!(matches!(
            parse_tool_input_transform_result(&replace),
            Ok(ToolInputTransformResult::Replace { tool_input })
                if tool_input == serde_json::json!({"canonical": true})
        ));
        assert!(parse_pre_tool_use_result(&replace).is_err());
        assert!(parse_tool_input_transform_result(&ask).is_err());
        let (contribution_id, effect) = parse_provider_contribution(&contribution)
            .unwrap()
            .unwrap()
            .into_parts();
        assert_eq!(contribution_id.as_str(), "pending-1");
        assert!(matches!(effect, PreparedProviderEffect::Unchanged));
        assert!(parse_provider_contribution(&invalid_contribution).is_err());
        assert!(matches!(
            parse_pre_compact_result(&compact),
            Ok(PreCompactResult::Contributions(contributions))
                if contributions.instructions == ["preserve the plan"]
                    && contributions.retained_context.len() == 2
        ));
        assert!(parse_pre_compact_result(&incomplete_compact).is_err());
        assert!(parse_pre_compact_result(&post_with_data).is_err());
        assert!(parse_post_compact_result(&HandlerResult::ok()).is_ok());
        assert!(parse_post_compact_result(&post_with_data).is_err());
        assert!(parse_post_compact_result(&compact).is_err());
    }
}
