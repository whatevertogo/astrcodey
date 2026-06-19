//! 远程扩展（IPC）共用的 manifest 构建与 HandlerResult 解析。

use std::collections::HashMap;

use astrcode_core::extension::{
    CompactContributions, CompactResult, ContinueAfterStopOptions, ContinueAfterStopResult,
    EXTENSION_TOOL_OUTCOME_KEY, ExtensionCommandResult, ExtensionError, ExtensionEvent,
    ExtensionEventDecl, ExtensionToolOutcome, HookMode, HookResult, PostToolUseResult,
    PreToolUseResult, PromptContributions, ProviderResult,
};
use astrcode_extension_sdk::{
    extension::SlashCommand,
    s5r::{effects::HandlerResult, event_from_name, mode_from_name},
    tool::{ExecutionMode, ToolDefinition, ToolOrigin, ToolResult, tool_metadata},
};
use serde_json::json;

use crate::extension_manifest::{ExtensionRegistration, manifest_types::ManifestHook};

pub fn validate_registration(reg: &ExtensionRegistration) -> Result<(), String> {
    if reg.extension_id.trim().is_empty() {
        return Err("extension id is empty".into());
    }
    for hook in &reg.hooks {
        if let Some(event) = event_from_name(&hook.on) {
            let mode = mode_from_name(&hook.mode)
                .ok_or_else(|| format!("unknown hook mode in manifest: {}", hook.mode))?;
            if s5r_unsupported_typed_hook(&event) {
                return Err(format!("{} is not supported by s5r manifest", hook.on));
            }
            if event == ExtensionEvent::ContinueAfterStop && mode != HookMode::Blocking {
                return Err(format!("{} is a blocking-only hook", hook.on));
            }
        }
    }
    Ok(())
}

pub fn build_tools(reg: &ExtensionRegistration) -> Vec<ToolDefinition> {
    reg.tools
        .iter()
        .map(|t| ToolDefinition {
            name: t.name.clone(),
            description: t.description.clone(),
            parameters: t.parameters.clone(),
            origin: ToolOrigin::Extension,
            execution_mode: if t.mode == "parallel" {
                ExecutionMode::Parallel
            } else {
                ExecutionMode::Sequential
            },
        })
        .collect()
}

pub fn build_commands(reg: &ExtensionRegistration) -> Vec<SlashCommand> {
    reg.commands
        .iter()
        .map(|c| SlashCommand {
            name: c.name.clone(),
            description: c.description.clone(),
            args_schema: None,
        })
        .collect()
}

pub fn build_subscriptions(
    reg: &ExtensionRegistration,
) -> Vec<(ExtensionEvent, HookMode, ContinueAfterStopOptions)> {
    reg.hooks
        .iter()
        .filter_map(|h: &ManifestHook| {
            let event = event_from_name(&h.on)?;
            if s5r_unsupported_typed_hook(&event) {
                return None;
            }
            let mode = mode_from_name(&h.mode)?;
            Some((
                event,
                mode,
                ContinueAfterStopOptions {
                    max_per_turn: h
                        .options
                        .max_per_turn
                        .unwrap_or(ContinueAfterStopOptions::default().max_per_turn),
                },
            ))
        })
        .collect()
}

fn s5r_unsupported_typed_hook(event: &ExtensionEvent) -> bool {
    matches!(
        event,
        ExtensionEvent::AfterToolResults | ExtensionEvent::UserMessageEnvelope
    )
}

pub fn handler_id(extension_id: &str, kind: &str, name: &str) -> String {
    format!("{extension_id}:{kind}:{name}")
}

pub fn parse_tool_result(resp: &HandlerResult) -> Result<ToolResult, ExtensionError> {
    if !resp.ok {
        let msg = resp.error.clone().unwrap_or_default();
        return Ok(ToolResult::text(msg, true, Default::default()));
    }
    match resp.effect_name() {
        "tool_outcome" => {
            let raw = resp
                .data_value("outcome")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let outcome: ExtensionToolOutcome = serde_json::from_value(raw)
                .map_err(|e| ExtensionError::Internal(format!("parse tool_outcome: {e}")))?;
            let outcome_json = serde_json::to_value(&outcome)
                .map_err(|e| ExtensionError::Internal(format!("serialize outcome: {e}")))?;
            Ok(ToolResult::text(
                String::new(),
                false,
                tool_metadata([(EXTENSION_TOOL_OUTCOME_KEY, outcome_json)]),
            ))
        },
        _ => {
            let content = resp
                .data_value("content")
                .and_then(|v| v.as_str())
                .map(ToString::to_string)
                .unwrap_or_default();
            Ok(ToolResult::text(content, false, Default::default()))
        },
    }
}

pub fn parse_command_result(
    resp: &HandlerResult,
) -> Result<ExtensionCommandResult, ExtensionError> {
    if !resp.ok {
        return Err(ExtensionError::Internal(
            resp.error.clone().unwrap_or_default(),
        ));
    }
    let data = resp.data.clone().unwrap_or(json!({}));
    serde_json::from_value(data)
        .map_err(|e| ExtensionError::Internal(format!("parse command result: {e}")))
}

pub fn parse_pre_tool_use_result(resp: &HandlerResult) -> Result<PreToolUseResult, ExtensionError> {
    if !resp.ok {
        return Ok(PreToolUseResult::Allow);
    }
    match resp.effect_name() {
        "block" => Ok(PreToolUseResult::Block {
            reason: resp.data_str("reason").to_string(),
        }),
        "modified_input" => {
            let tool_input = resp.data_value("tool_input").cloned().ok_or_else(|| {
                ExtensionError::Internal("effect=modified_input but data.tool_input missing".into())
            })?;
            Ok(PreToolUseResult::ModifyInput { tool_input })
        },
        _ => Ok(PreToolUseResult::Allow),
    }
}

pub fn parse_post_tool_use_result(
    resp: &HandlerResult,
) -> Result<PostToolUseResult, ExtensionError> {
    if !resp.ok {
        return Ok(PostToolUseResult::Allow);
    }
    match resp.effect_name() {
        "block" => Ok(PostToolUseResult::Block {
            reason: resp.data_str("reason").to_string(),
        }),
        "tool_outcome" => Ok(PostToolUseResult::ModifyResult {
            content: resp.data_str("content").to_string(),
        }),
        _ => Ok(PostToolUseResult::Allow),
    }
}

pub fn parse_provider_result(resp: &HandlerResult) -> Result<ProviderResult, ExtensionError> {
    if !resp.ok {
        return Ok(ProviderResult::Allow);
    }
    match resp.effect_name() {
        "block" => Ok(ProviderResult::Block {
            reason: resp.data_str("reason").to_string(),
        }),
        "replace_messages" => {
            let messages_val = resp.data_value("messages").cloned().ok_or_else(|| {
                ExtensionError::Internal("effect=replace_messages but data.messages missing".into())
            })?;
            Ok(ProviderResult::ReplaceMessages {
                messages: serde_json::from_value(messages_val)
                    .map_err(|e| ExtensionError::Internal(format!("parse messages: {e}")))?,
            })
        },
        "append_messages" => {
            let messages_val = resp.data_value("messages").cloned().ok_or_else(|| {
                ExtensionError::Internal("effect=append_messages but data.messages missing".into())
            })?;
            Ok(ProviderResult::AppendMessages {
                messages: serde_json::from_value(messages_val)
                    .map_err(|e| ExtensionError::Internal(format!("parse messages: {e}")))?,
            })
        },
        _ => Ok(ProviderResult::Allow),
    }
}

pub fn parse_continue_after_stop_result(
    resp: &HandlerResult,
) -> Result<ContinueAfterStopResult, ExtensionError> {
    if !resp.ok {
        return Ok(ContinueAfterStopResult::EndTurn);
    }
    match resp.effect_name() {
        "continue_one_step" => Ok(ContinueAfterStopResult::ContinueOneStep),
        _ => Ok(ContinueAfterStopResult::EndTurn),
    }
}

pub fn parse_prompt_build_result(
    resp: &HandlerResult,
) -> Result<PromptContributions, ExtensionError> {
    if !resp.ok || resp.effect_name() != "prompt_contributions" {
        return Ok(PromptContributions::default());
    }
    serde_json::from_value(resp.data.clone().unwrap_or_default())
        .map_err(|e| ExtensionError::Internal(format!("parse PromptContributions: {e}")))
}

pub fn parse_compact_result(resp: &HandlerResult) -> Result<CompactResult, ExtensionError> {
    if !resp.ok || resp.effect_name() != "compact_contributions" {
        return Ok(CompactResult::Allow);
    }
    let contributions: CompactContributions =
        serde_json::from_value(resp.data.clone().unwrap_or_default())
            .map_err(|e| ExtensionError::Internal(format!("parse CompactContributions: {e}")))?;
    Ok(CompactResult::Contributions(contributions))
}

pub fn parse_lifecycle_result(resp: &HandlerResult) -> Result<HookResult, ExtensionError> {
    if !resp.ok {
        return Ok(HookResult::Block {
            reason: resp.error.clone().unwrap_or_default(),
        });
    }
    match resp.effect_name() {
        "block" => Ok(HookResult::Block {
            reason: resp.data_str("reason").to_string(),
        }),
        _ => Ok(HookResult::Allow),
    }
}

pub fn event_decls_map(reg: &ExtensionRegistration) -> HashMap<String, ExtensionEventDecl> {
    crate::host_router::decls_to_map(&reg.extension_events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension_manifest::manifest_types::{ManifestHook, ManifestHookOptions};

    fn registration_with_hook(on: &str, mode: &str) -> ExtensionRegistration {
        ExtensionRegistration {
            extension_id: "test-extension".into(),
            version: "0.0.0".into(),
            capabilities: Vec::new(),
            tools: Vec::new(),
            commands: Vec::new(),
            hooks: vec![ManifestHook {
                on: on.into(),
                mode: mode.into(),
                options: ManifestHookOptions::default(),
            }],
            extension_events: Vec::new(),
        }
    }

    #[test]
    fn validate_registration_rejects_non_blocking_continue_after_stop() {
        let reg = registration_with_hook("continue_after_stop", "non_blocking");

        let err = validate_registration(&reg).unwrap_err();

        assert!(err.contains("blocking-only"));
    }

    #[test]
    fn validate_registration_rejects_s5r_internal_typed_hooks() {
        for hook in ["user_message_envelope", "after_tool_results"] {
            let reg = registration_with_hook(hook, "blocking");

            let err = validate_registration(&reg).unwrap_err();

            assert!(err.contains("not supported by s5r manifest"));
        }
    }
}
