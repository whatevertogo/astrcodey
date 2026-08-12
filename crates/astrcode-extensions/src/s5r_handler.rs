//! S5R handler identity construction and wire-result translation.

use astrcode_extension_contract::{
    HandlerEffect, HandlerId, HandlerKind, HandlerResult, ToolOutcome,
};
use astrcode_extension_sdk::{
    extension::{
        CompactContributions, CompactResult, ContinueAfterStopResult, ExtensionCommandResult,
        ExtensionError, ExtensionHttpResponse, HookResult, PostToolUseResult, PreToolUseResult,
        PromptContributions, ProviderResult,
    },
    tool::ToolResult,
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
                Default::default(),
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

pub(crate) fn parse_pre_tool_use_result(
    resp: &HandlerResult,
) -> Result<PreToolUseResult, ExtensionError> {
    match resp.effect {
        HandlerEffect::Ok => Ok(PreToolUseResult::Allow),
        HandlerEffect::Block => Ok(PreToolUseResult::Block {
            reason: required_data_string(resp, "reason")?,
        }),
        HandlerEffect::ModifiedInput => {
            let tool_input = required_data_value(resp, "tool_input")?;
            Ok(PreToolUseResult::ModifyInput { tool_input })
        },
        effect => Err(unexpected_effect("pre_tool_use", effect)),
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

pub(crate) fn parse_compact_result(resp: &HandlerResult) -> Result<CompactResult, ExtensionError> {
    match resp.effect {
        HandlerEffect::Ok => return Ok(CompactResult::Allow),
        HandlerEffect::CompactContributions => {},
        effect => return Err(unexpected_effect("compact", effect)),
    }
    let contributions: CompactContributions = serde_json::from_value(resp.data.clone())
        .map_err(|e| ExtensionError::Internal(format!("parse CompactContributions: {e}")))?;
    Ok(CompactResult::Contributions(contributions))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handler_results_reject_missing_mistyped_and_mismatched_payloads() {
        let missing_reason = HandlerResult::effect(HandlerEffect::Block, serde_json::json!({}));
        let invalid_reason =
            HandlerResult::effect(HandlerEffect::Block, serde_json::json!({"reason": false}));
        let missing_messages =
            HandlerResult::effect(HandlerEffect::AppendMessages, serde_json::json!({}));

        assert!(parse_pre_tool_use_result(&missing_reason).is_err());
        assert!(parse_lifecycle_result(&invalid_reason).is_err());
        assert!(parse_provider_result(&missing_messages).is_err());
        assert!(parse_tool_result(&HandlerResult::ok()).is_err());
    }
}
