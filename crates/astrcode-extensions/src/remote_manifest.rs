//! 远程扩展（IPC）共用的 manifest 构建与 HandlerResult 解析。

use astrcode_extension_sdk::{
    extension::{
        CompactContributions, CompactResult, ContinueAfterStopOptions, ContinueAfterStopResult,
        ExtensionCommandResult, ExtensionError, ExtensionEvent, ExtensionHttpResponse, HookMode,
        HookResult, PostToolUseResult, PreToolUseResult, PromptContributions, ProviderResult,
        SlashCommand,
    },
    s5r::{effects::HandlerResult, event_from_name, manifest::ManifestHook, mode_from_name},
    tool::{ExecutionMode, ToolDefinition, ToolOrigin, ToolResult},
};
use serde::Deserialize;
use serde_json::json;

use crate::extension_manifest::ExtensionRegistration;

pub fn validate_registration(reg: &ExtensionRegistration) -> Result<(), String> {
    if reg.extension_id.trim().is_empty() {
        return Err("extension id is empty".into());
    }
    for tool in &reg.tools {
        if !matches!(tool.mode.as_str(), "parallel" | "sequential") {
            return Err(format!(
                "unknown tool execution mode in manifest: {}",
                tool.mode
            ));
        }
    }
    for hook in &reg.hooks {
        let event = event_from_name(&hook.on)
            .ok_or_else(|| format!("unknown hook event in manifest: {}", hook.on))?;
        let mode = mode_from_name(&hook.mode)
            .ok_or_else(|| format!("unknown hook mode in manifest: {}", hook.mode))?;
        if s5r_unsupported_typed_hook(&event) {
            return Err(format!("{} is not supported by s5r manifest", hook.on));
        }
        if event == ExtensionEvent::ContinueAfterStop && mode != HookMode::Blocking {
            return Err(format!("{} is a blocking-only hook", hook.on));
        }
    }
    for entry in &reg.http_routes {
        entry.route.validate()?;
        if entry.handler_id.trim().is_empty() {
            return Err(format!(
                "HTTP route {} is missing handler_id",
                entry.route.path
            ));
        }
    }
    Ok(())
}

pub fn parse_http_response(resp: &HandlerResult) -> Result<ExtensionHttpResponse, ExtensionError> {
    if !resp.ok {
        return Err(ExtensionError::Internal(
            resp.error.clone().unwrap_or_default(),
        ));
    }
    if resp.effect_name() != "http_response" {
        return Err(ExtensionError::Internal(format!(
            "expected http_response effect, got {}",
            resp.effect_name()
        )));
    }
    serde_json::from_value(resp.data.clone().unwrap_or_default())
        .map_err(|error| ExtensionError::Internal(format!("parse HTTP response: {error}")))
}

pub fn build_tools(reg: &ExtensionRegistration) -> Vec<ToolDefinition> {
    reg.tools
        .iter()
        .map(|t| ToolDefinition {
            name: t.name.clone(),
            description: t.description.clone(),
            parameters: t.parameters.clone(),
            strict: t.strict,
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
            requires_idle: false,
            argument_completions: false,
            priority: 0,
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
    matches!(event, ExtensionEvent::UserMessageEnvelope)
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
            let outcome: ExtensionToolOutput = serde_json::from_value(raw)
                .map_err(|e| ExtensionError::Internal(format!("parse tool_outcome: {e}")))?;
            match outcome {
                ExtensionToolOutput::Text { content, is_error } => {
                    Ok(ToolResult::text(content, is_error, Default::default()))
                },
            }
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

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ExtensionToolOutput {
    Text { content: String, is_error: bool },
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

#[cfg(test)]
mod tests {
    use astrcode_extension_sdk::s5r::manifest::{ManifestHook, ManifestHookOptions, ManifestTool};

    use super::*;

    fn registration_with_hook(on: &str, mode: &str) -> ExtensionRegistration {
        ExtensionRegistration {
            extension_id: "test-extension".into(),
            capabilities: Vec::new(),
            tools: Vec::new(),
            commands: Vec::new(),
            hooks: vec![ManifestHook {
                on: on.into(),
                mode: mode.into(),
                options: ManifestHookOptions::default(),
            }],
            http_routes: Vec::new(),
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
    fn validate_registration_rejects_s5r_internal_typed_hook() {
        let reg = registration_with_hook("user_message_envelope", "blocking");

        let err = validate_registration(&reg).unwrap_err();

        assert!(err.contains("not supported by s5r manifest"));
    }

    #[test]
    fn validate_registration_rejects_unknown_hook_and_tool_modes() {
        let unknown_hook = registration_with_hook("typo_hook", "blocking");
        assert!(
            validate_registration(&unknown_hook)
                .unwrap_err()
                .contains("unknown hook event")
        );

        let mut unknown_mode = registration_with_hook("turn_end", "advisory");
        unknown_mode.tools.push(ManifestTool {
            name: "bad-tool".into(),
            description: String::new(),
            parameters: serde_json::json!({"type": "object"}),
            strict: false,
            mode: "concurrent-ish".into(),
        });
        assert!(
            validate_registration(&unknown_mode)
                .unwrap_err()
                .contains("unknown tool execution mode")
        );
    }
}
