//! Anthropic Messages JSON request contract: endpoint resolution, message/tool conversion,
//! cache breakpoints, and count-token body shape. SSE event handling lives in [`super::parser`]
//! and byte-stream transport in [`super::transport`].

use std::sync::Arc;

use astrcode_core::{
    llm::{
        LlmContent, LlmMessage, LlmRole,
        thinking::{ThinkingCapability, ThinkingConfig, ThinkingWireMapping},
    },
    tool::ToolDefinition,
};

use crate::tool_result_wire::anthropic_tool_result_content;

/// Anthropic Messages API 版本请求头。流式与 count_tokens 两条路径都需要。
pub(crate) const ANTHROPIC_API_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone, Copy)]
pub(crate) struct AnthropicRequestConfig<'a> {
    pub model_id: &'a str,
    pub max_output_tokens: usize,
    pub supports_strict_tool_use: bool,
    pub thinking: &'a ThinkingConfig,
    pub thinking_capability: Option<&'a ThinkingCapability>,
}

pub(crate) fn endpoint_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/messages") {
        base.to_string()
    } else if is_versioned_path(base) {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

pub(crate) fn count_tokens_endpoint(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/messages/count_tokens") {
        return base.to_string();
    }
    let endpoint = endpoint_url(base_url);
    format!("{endpoint}/count_tokens")
}

pub(crate) fn build_request_body(
    config: AnthropicRequestConfig<'_>,
    messages: &[Arc<LlmMessage>],
    tools: &[ToolDefinition],
    stream: bool,
) -> Result<serde_json::Value, astrcode_core::llm::LlmError> {
    let (system, api_messages) = convert_messages(messages);
    let mut request_body = serde_json::json!({
        "model": config.model_id,
        "messages": api_messages,
        "max_tokens": config.max_output_tokens,
    });
    if stream {
        request_body["stream"] = serde_json::json!(true);
    }
    if let Some(sys) = system {
        request_body["system"] = sys;
    }
    if !tools.is_empty() {
        request_body["tools"] = convert_tools(tools, config.supports_strict_tool_use);
    }
    apply_anthropic_thinking(config, &mut request_body)?;
    Ok(request_body)
}

/// Apply Anthropic thinking fields based on capability mapping.
fn apply_anthropic_thinking(
    config: AnthropicRequestConfig<'_>,
    body: &mut serde_json::Value,
) -> Result<(), astrcode_core::llm::LlmError> {
    let Some(cap) = config.thinking_capability else {
        return Ok(());
    };
    match cap.wire_mapping {
        ThinkingWireMapping::AnthropicAdaptive => {
            if config.thinking.enabled
                && let Some(ref effort) = config.thinking.effort
            {
                body["thinking"] = serde_json::json!({"type": "adaptive"});
                body["output_config"] = serde_json::json!({
                    "effort": effort
                });
            }
            // Disabled → omit thinking field entirely
        },
        ThinkingWireMapping::AnthropicBudget if config.thinking.enabled => {
            let budget = config.thinking.budget_tokens.ok_or_else(|| {
                astrcode_core::llm::LlmError::Unsupported {
                    message: "budget_tokens is required when AnthropicBudget thinking is enabled"
                        .into(),
                }
            })?;
            if budget as usize >= config.max_output_tokens {
                return Err(astrcode_core::llm::LlmError::Unsupported {
                    message: format!(
                        "budget_tokens ({}) must be less than max_output_tokens ({})",
                        budget, config.max_output_tokens
                    ),
                });
            }
            body["thinking"] = serde_json::json!({
                "type": "enabled",
                "budget_tokens": budget
            });
        },
        _ => {
            // Not an Anthropic mapping → no change
        },
    }
    Ok(())
}

pub(crate) fn build_count_tokens_body(
    config: AnthropicRequestConfig<'_>,
    messages: &[Arc<LlmMessage>],
    tools: &[ToolDefinition],
) -> serde_json::Value {
    build_request_without_thinking(config, messages, tools)
}

/// Build request body without any thinking fields (used for count-tokens).
fn build_request_without_thinking(
    config: AnthropicRequestConfig<'_>,
    messages: &[Arc<LlmMessage>],
    tools: &[ToolDefinition],
) -> serde_json::Value {
    let (system, api_messages) = convert_messages(messages);
    let mut request_body = serde_json::json!({
        "model": config.model_id,
        "messages": api_messages,
    });
    if let Some(sys) = system {
        request_body["system"] = sys;
    }
    if !tools.is_empty() {
        request_body["tools"] = convert_tools(tools, config.supports_strict_tool_use);
    }
    request_body
}

fn convert_messages(
    messages: &[Arc<LlmMessage>],
) -> (Option<serde_json::Value>, Vec<serde_json::Value>) {
    let mut system_blocks: Vec<serde_json::Value> = Vec::new();
    let mut api_messages: Vec<serde_json::Value> = Vec::new();

    for msg in messages {
        match msg.role {
            LlmRole::System => {
                let text = msg.joined_text("\n");
                if !text.is_empty() {
                    system_blocks.push(serde_json::json!({
                        "type": "text",
                        "text": text,
                        "cache_control": {"type": "ephemeral"}
                    }));
                }
            },
            LlmRole::User => {
                api_messages.push(AnthropicMapper::map_user(msg));
            },
            LlmRole::Assistant => {
                api_messages.push(AnthropicMapper::map_assistant(msg));
            },
            LlmRole::Tool => {
                let block = convert_tool_result_block(msg);
                if let Some(last) = api_messages.last_mut()
                    && last["role"] == "user"
                    && has_only_tool_results(last)
                {
                    if let Some(content) = last.get_mut("content").and_then(|c| c.as_array_mut()) {
                        content.push(block);
                        continue;
                    }
                    tracing::warn!(
                        "tool result merge: last user message content is not an array, creating \
                         new block"
                    );
                }
                api_messages.push(serde_json::json!({
                    "role": "user",
                    "content": [block]
                }));
            },
        }
    }

    mark_history_cache_breakpoint(&mut api_messages);

    let system = if system_blocks.is_empty() {
        None
    } else {
        Some(serde_json::Value::Array(system_blocks))
    };

    (system, api_messages)
}

struct AnthropicMapper;

impl AnthropicMapper {
    fn map_user(message: &LlmMessage) -> serde_json::Value {
        let mut content = message
            .content
            .iter()
            .filter_map(|item| match item {
                LlmContent::Text { text } => Some(Self::text(text)),
                LlmContent::Image {
                    base64, media_type, ..
                } => Some(Self::image(base64, media_type)),
                LlmContent::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                } => Some(Self::tool_result(tool_call_id, content, *is_error)),
                _ => None,
            })
            .collect::<Vec<_>>();
        if content.is_empty() {
            content.push(Self::empty());
        }
        serde_json::json!({"role": "user", "content": content})
    }

    fn map_assistant(message: &LlmMessage) -> serde_json::Value {
        let mut content = message
            .content
            .iter()
            .filter_map(|item| match item {
                LlmContent::Text { text } => Some(Self::text(text)),
                LlmContent::ToolCall {
                    call_id,
                    name,
                    arguments,
                    raw_arguments,
                } => Some(Self::tool_call(
                    call_id,
                    name,
                    arguments,
                    raw_arguments.as_deref(),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        if content.is_empty() {
            content.push(Self::empty());
        }
        serde_json::json!({"role": "assistant", "content": content})
    }

    fn text(text: &str) -> serde_json::Value {
        serde_json::json!({"type": "text", "text": text})
    }

    fn image(base64: &str, media_type: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "image",
            "source": {"type": "base64", "data": base64, "media_type": media_type}
        })
    }

    fn tool_call(
        call_id: &str,
        name: &str,
        arguments: &serde_json::Value,
        raw_arguments: Option<&str>,
    ) -> serde_json::Value {
        // Anthropic requires `input` to be JSON. Keep malformed provider text
        // in the durable model, but replay a protocol-valid placeholder next
        // to the paired error result.
        let input = raw_arguments.map_or_else(|| arguments.clone(), |_| serde_json::json!({}));
        serde_json::json!({
            "type": "tool_use",
            "id": call_id,
            "name": name,
            "input": input
        })
    }

    fn tool_result(id: &str, content: &str, is_error: bool) -> serde_json::Value {
        serde_json::json!({
            "type": "tool_result",
            "tool_use_id": id,
            "content": content,
            "is_error": is_error,
        })
    }

    fn empty() -> serde_json::Value {
        serde_json::json!({"type": "text", "text": ""})
    }
}

fn convert_tool_result_block(msg: &LlmMessage) -> serde_json::Value {
    for content in &msg.content {
        if let LlmContent::ToolResult {
            tool_call_id,
            content,
            is_error,
        } = content
        {
            return serde_json::json!({
                "type": "tool_result",
                "tool_use_id": tool_call_id,
                "content": anthropic_tool_result_content(content),
                "is_error": is_error,
            });
        }
    }
    serde_json::json!({"type": "tool_result", "tool_use_id": "", "content": "", "is_error": false})
}

fn has_only_tool_results(msg: &serde_json::Value) -> bool {
    let Some(content) = msg.get("content").and_then(|v| v.as_array()) else {
        return false;
    };
    !content.is_empty() && content.iter().all(|b| b["type"] == "tool_result")
}

fn convert_tools(tools: &[ToolDefinition], supports_strict_tool_use: bool) -> serde_json::Value {
    let mut converted: Vec<serde_json::Value> = tools
        .iter()
        .map(|t| {
            let mut converted = serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            });
            if supports_strict_tool_use && t.strict {
                converted["strict"] = serde_json::json!(true);
            }
            converted
        })
        .collect();
    if let Some(last) = converted.last_mut() {
        last["cache_control"] = serde_json::json!({"type": "ephemeral"});
    }
    serde_json::Value::Array(converted)
}

fn is_versioned_path(url: &str) -> bool {
    url.rsplit('/').next().is_some_and(|seg| {
        seg.starts_with('v') && seg.len() > 1 && seg[1..].chars().all(|c| c.is_ascii_digit())
    })
}

fn mark_history_cache_breakpoint(api_messages: &mut [serde_json::Value]) {
    let Some(last_msg) = api_messages.last_mut() else {
        return;
    };
    let Some(content) = last_msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
        return;
    };
    let Some(last_block) = content.last_mut() else {
        return;
    };
    if let Some(obj) = last_block.as_object_mut() {
        obj.insert(
            "cache_control".into(),
            serde_json::json!({"type": "ephemeral"}),
        );
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        llm::{LlmContent, LlmMessage, LlmRole, thinking::ThinkingConfig},
        tool::{ExecutionMode, ToolDefinition, ToolOrigin},
    };

    use super::*;

    #[test]
    fn user_message_converts_text() {
        let msg = Arc::new(LlmMessage::user("hello"));
        let json = AnthropicMapper::map_user(&msg);
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"][0]["type"], "text");
        assert_eq!(json["content"][0]["text"], "hello");
    }

    #[test]
    fn assistant_message_converts_tool_call() {
        let msg = LlmMessage {
            role: LlmRole::Assistant,
            content: vec![
                LlmContent::ToolCall {
                    call_id: "call_1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "foo.rs"}),
                    raw_arguments: None,
                },
                LlmContent::ToolCall {
                    call_id: "call_bad".into(),
                    name: "read".into(),
                    arguments: serde_json::Value::String(r#"{"path":"#.into()),
                    raw_arguments: Some(r#"{"path":"#.into()),
                },
            ],
            name: None,
            reasoning_content: None,
        };
        let json = AnthropicMapper::map_assistant(&msg);
        let block = &json["content"][0];
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["id"], "call_1");
        assert_eq!(block["name"], "read");
        assert_eq!(block["input"]["path"], "foo.rs");
        assert_eq!(json["content"][1]["input"], serde_json::json!({}));
    }

    #[test]
    fn tool_results_merge_into_same_user_message() {
        let messages = vec![
            Arc::new(LlmMessage::assistant("I'll check")),
            Arc::new(LlmMessage::tool("read", "call_1", "file content", false)),
            Arc::new(LlmMessage::tool("grep", "call_2", "match found", false)),
        ];
        let (_system, api_messages) = convert_messages(&messages);

        assert_eq!(api_messages.len(), 2);
        let content = api_messages[1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["tool_use_id"], "call_1");
        assert_eq!(content[1]["tool_use_id"], "call_2");
    }

    #[test]
    fn endpoint_appends_messages() {
        assert_eq!(
            endpoint_url("https://api.anthropic.com/v1"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn endpoint_auto_adds_v1_for_bare_base() {
        assert_eq!(
            endpoint_url("https://open.bigmodel.cn/api/anthropic"),
            "https://open.bigmodel.cn/api/anthropic/v1/messages"
        );
    }

    #[test]
    fn endpoint_preserves_full_messages_url() {
        assert_eq!(
            endpoint_url("https://custom.proxy/messages"),
            "https://custom.proxy/messages"
        );
    }

    #[test]
    fn count_tokens_request_reuses_messages_system_and_tools() {
        let tools = vec![ToolDefinition {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: serde_json::json!({"type": "object"}),
            strict: false,
            origin: ToolOrigin::Bundled,
            execution_mode: ExecutionMode::Parallel,
            timeout_ms: None,
        }];
        let config = AnthropicRequestConfig {
            model_id: "claude-test",
            max_output_tokens: 1024,
            supports_strict_tool_use: false,
            thinking: &ThinkingConfig::default(),
            thinking_capability: None,
        };
        let body = build_count_tokens_body(
            config,
            &[
                Arc::new(LlmMessage::system("s")),
                Arc::new(LlmMessage::user("hi")),
            ],
            &tools,
        );

        assert_eq!(
            count_tokens_endpoint("https://api.anthropic.com/v1"),
            "https://api.anthropic.com/v1/messages/count_tokens"
        );
        assert_eq!(body["model"], "claude-test");
        assert_eq!(body["system"][0]["text"], "s");
        assert!(body["messages"].is_array());
        assert!(body["tools"].is_array());
        assert!(body.get("stream").is_none());
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn serializes_anthropic_strict_only_when_declared_and_supported() {
        for (strict, supported, expected) in [
            (true, true, Some(true)),
            (true, false, None),
            (false, true, None),
        ] {
            let serialized = convert_tools(
                &[ToolDefinition {
                    name: "lookup".into(),
                    description: String::new(),
                    parameters: serde_json::json!({"type": "object"}),
                    strict,
                    origin: ToolOrigin::Bundled,
                    execution_mode: ExecutionMode::Parallel,
                    timeout_ms: None,
                }],
                supported,
            );

            assert_eq!(
                serialized
                    .pointer("/0/strict")
                    .and_then(serde_json::Value::as_bool),
                expected
            );
        }
    }

    #[test]
    fn count_tokens_endpoint_preserves_full_count_tokens_url() {
        assert_eq!(
            count_tokens_endpoint("https://custom.proxy/v1/messages/count_tokens"),
            "https://custom.proxy/v1/messages/count_tokens"
        );
    }

    #[test]
    fn convert_messages_extracts_system() {
        let messages = vec![
            Arc::new(LlmMessage::system("You are helpful")),
            Arc::new(LlmMessage::user("hello")),
        ];
        let (system, api_messages) = convert_messages(&messages);
        let sys = system.expect("system should be present");
        assert_eq!(sys[0]["text"], "You are helpful");
        assert_eq!(api_messages.len(), 1);
        assert_eq!(api_messages[0]["role"], "user");
    }

    // ─── Thinking wire mapping tests ─────────────────────────────────

    #[test]
    fn anthropic_adaptive_thinking_emits_type_adaptive_and_effort() {
        use astrcode_core::llm::thinking::{ThinkingCapability, ThinkingWireMapping};
        let thinking_capability = ThinkingCapability {
            wire_mapping: ThinkingWireMapping::AnthropicAdaptive,
            allowed_effort: Some(vec!["high".into()]),
            budget_min: None,
            budget_max: None,
            can_disable: true,
        };
        let config = AnthropicRequestConfig {
            model_id: "claude-opus-4-6",
            max_output_tokens: 8192,
            supports_strict_tool_use: false,
            thinking: &ThinkingConfig {
                enabled: true,
                effort: Some("high".into()),
                budget_tokens: None,
            },
            thinking_capability: Some(&thinking_capability),
        };
        let body = build_request_body(config, &[], &[], false).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
    }

    #[test]
    fn anthropic_budget_thinking_emits_type_enabled_and_budget_tokens() {
        use astrcode_core::llm::thinking::{ThinkingCapability, ThinkingWireMapping};
        let thinking_capability = ThinkingCapability {
            wire_mapping: ThinkingWireMapping::AnthropicBudget,
            allowed_effort: Some(vec![]),
            budget_min: Some(1024),
            budget_max: Some(64000),
            can_disable: true,
        };
        let config = AnthropicRequestConfig {
            model_id: "claude-sonnet-4-6",
            max_output_tokens: 8192,
            supports_strict_tool_use: false,
            thinking: &ThinkingConfig {
                enabled: true,
                effort: None,
                budget_tokens: Some(4096),
            },
            thinking_capability: Some(&thinking_capability),
        };
        let body = build_request_body(config, &[], &[], false).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 4096);
    }

    #[test]
    fn anthropic_budget_rejects_when_budget_equals_or_exceeds_max_output_tokens() {
        use astrcode_core::llm::thinking::{ThinkingCapability, ThinkingWireMapping};
        let thinking_capability = ThinkingCapability {
            wire_mapping: ThinkingWireMapping::AnthropicBudget,
            allowed_effort: Some(vec![]),
            budget_min: Some(1024),
            budget_max: Some(64000),
            can_disable: true,
        };
        let config = AnthropicRequestConfig {
            model_id: "claude-sonnet-4-6",
            max_output_tokens: 4096,
            supports_strict_tool_use: false,
            thinking: &ThinkingConfig {
                enabled: true,
                effort: None,
                budget_tokens: Some(4096),
            },
            thinking_capability: Some(&thinking_capability),
        };
        let result = build_request_body(config, &[], &[], false);
        assert!(
            result.is_err(),
            "budget >= max_output_tokens should be rejected"
        );
    }

    #[test]
    fn anthropic_thinking_omitted_when_disabled() {
        use astrcode_core::llm::thinking::{ThinkingCapability, ThinkingWireMapping};
        let thinking_capability = ThinkingCapability {
            wire_mapping: ThinkingWireMapping::AnthropicAdaptive,
            allowed_effort: Some(vec!["high".into()]),
            budget_min: None,
            budget_max: None,
            can_disable: true,
        };
        let config = AnthropicRequestConfig {
            model_id: "claude-opus-4-6",
            max_output_tokens: 8192,
            supports_strict_tool_use: false,
            thinking: &ThinkingConfig {
                enabled: false,
                effort: None,
                budget_tokens: None,
            },
            thinking_capability: Some(&thinking_capability),
        };
        let body = build_request_body(config, &[], &[], false).unwrap();
        assert!(
            body.get("thinking").is_none(),
            "thinking should be omitted when disabled"
        );
    }

    #[test]
    fn anthropic_count_tokens_omits_thinking() {
        use astrcode_core::llm::thinking::{ThinkingCapability, ThinkingWireMapping};
        let thinking_capability = ThinkingCapability {
            wire_mapping: ThinkingWireMapping::AnthropicAdaptive,
            allowed_effort: Some(vec!["high".into()]),
            budget_min: None,
            budget_max: None,
            can_disable: true,
        };
        let config = AnthropicRequestConfig {
            model_id: "claude-opus-4-6",
            max_output_tokens: 8192,
            supports_strict_tool_use: false,
            thinking: &ThinkingConfig {
                enabled: true,
                effort: Some("high".into()),
                budget_tokens: None,
            },
            thinking_capability: Some(&thinking_capability),
        };
        let body = build_count_tokens_body(config, &[], &[]);
        assert!(
            body.get("thinking").is_none(),
            "count tokens body should omit thinking"
        );
        assert!(
            body.get("max_tokens").is_none(),
            "count tokens body should omit max_tokens"
        );
    }
}
