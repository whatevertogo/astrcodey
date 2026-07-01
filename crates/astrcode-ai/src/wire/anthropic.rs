//! Anthropic Messages wire request construction.
//!
//! This module owns the Anthropic Messages JSON contract: endpoint resolution, message/tool
//! conversion, cache breakpoints, and count-token body shape. Transport and SSE events stay in the
//! provider wrapper.

use astrcode_core::{
    llm::{LlmContent, LlmMessage, LlmRole},
    tool::ToolDefinition,
};

use crate::{serialization::ContentMapper, tool_result_wire::anthropic_tool_result_content};

#[derive(Debug, Clone, Copy)]
pub(crate) struct AnthropicRequestConfig<'a> {
    pub model_id: &'a str,
    pub max_output_tokens: usize,
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
    messages: &[LlmMessage],
    tools: &[ToolDefinition],
    stream: bool,
) -> serde_json::Value {
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
        request_body["tools"] = convert_tools(tools);
    }
    request_body
}

pub(crate) fn build_count_tokens_body(
    config: AnthropicRequestConfig<'_>,
    messages: &[LlmMessage],
    tools: &[ToolDefinition],
) -> serde_json::Value {
    let mut body = build_request_body(config, messages, tools, false);
    if let Some(obj) = body.as_object_mut() {
        obj.remove("max_tokens");
    }
    body
}

fn convert_messages(
    messages: &[LlmMessage],
) -> (Option<serde_json::Value>, Vec<serde_json::Value>) {
    let mut system_blocks: Vec<serde_json::Value> = Vec::new();
    let mut api_messages: Vec<serde_json::Value> = Vec::new();

    for msg in messages {
        match msg.role {
            LlmRole::System => {
                let text = msg
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        LlmContent::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
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
                if let Some(last) = api_messages.last_mut() {
                    if last["role"] == "user" && has_only_tool_results(last) {
                        if let Some(content) =
                            last.get_mut("content").and_then(|c| c.as_array_mut())
                        {
                            content.push(block);
                            continue;
                        }
                        tracing::warn!(
                            "tool result merge: last user message content is not an array, \
                             creating new block"
                        );
                    }
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

impl ContentMapper for AnthropicMapper {
    fn text(text: &str) -> serde_json::Value {
        serde_json::json!({"type": "text", "text": text})
    }

    fn image(base64: &str, media_type: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "image",
            "source": {"type": "base64", "data": base64, "media_type": media_type}
        })
    }

    fn tool_call(call_id: &str, name: &str, arguments: &serde_json::Value) -> serde_json::Value {
        let args_str = match arguments {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        serde_json::json!({
            "type": "tool_use",
            "id": call_id,
            "name": name,
            "input": serde_json::from_str::<serde_json::Value>(&args_str)
                .unwrap_or(serde_json::json!({}))
        })
    }

    fn tool_result(id: &str, content: &str, is_error: bool) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "tool_result",
            "tool_use_id": id,
            "content": content,
            "is_error": is_error,
        }))
    }

    fn empty() -> serde_json::Value {
        serde_json::json!({"type": "text", "text": ""})
    }

    fn wrap_user(parts: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({"role": "user", "content": parts})
    }

    fn wrap_assistant(parts: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({"role": "assistant", "content": parts})
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

fn convert_tools(tools: &[ToolDefinition]) -> serde_json::Value {
    let mut converted: Vec<serde_json::Value> = tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            })
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
        llm::{LlmContent, LlmMessage, LlmRole},
        tool::{ExecutionMode, ToolDefinition, ToolOrigin},
    };

    use super::*;

    #[test]
    fn user_message_converts_text() {
        let msg = LlmMessage::user("hello");
        let json = AnthropicMapper::map_user(&msg);
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"][0]["type"], "text");
        assert_eq!(json["content"][0]["text"], "hello");
    }

    #[test]
    fn assistant_message_converts_tool_call() {
        let msg = LlmMessage {
            role: LlmRole::Assistant,
            content: vec![LlmContent::ToolCall {
                call_id: "call_1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "foo.rs"}),
            }],
            name: None,
            reasoning_content: None,
        };
        let json = AnthropicMapper::map_assistant(&msg);
        let block = &json["content"][0];
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["id"], "call_1");
        assert_eq!(block["name"], "read");
        assert_eq!(block["input"]["path"], "foo.rs");
    }

    #[test]
    fn tool_results_merge_into_same_user_message() {
        let messages = vec![
            LlmMessage::assistant("I'll check"),
            LlmMessage::tool("read", "call_1", "file content", false),
            LlmMessage::tool("grep", "call_2", "match found", false),
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
            origin: ToolOrigin::Builtin,
            execution_mode: ExecutionMode::Parallel,
        }];
        let config = AnthropicRequestConfig {
            model_id: "claude-test",
            max_output_tokens: 1024,
        };
        let body = build_count_tokens_body(
            config,
            &[LlmMessage::system("s"), LlmMessage::user("hi")],
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
    fn count_tokens_endpoint_preserves_full_count_tokens_url() {
        assert_eq!(
            count_tokens_endpoint("https://custom.proxy/v1/messages/count_tokens"),
            "https://custom.proxy/v1/messages/count_tokens"
        );
    }

    #[test]
    fn convert_messages_extracts_system() {
        let messages = vec![
            LlmMessage::system("You are helpful"),
            LlmMessage::user("hello"),
        ];
        let (system, api_messages) = convert_messages(&messages);
        let sys = system.expect("system should be present");
        assert_eq!(sys[0]["text"], "You are helpful");
        assert_eq!(api_messages.len(), 1);
        assert_eq!(api_messages[0]["role"], "user");
    }
}
