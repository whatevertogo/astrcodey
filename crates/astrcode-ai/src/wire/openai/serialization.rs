//! 消息与工具的 JSON 序列化。
//!
//! 将 crate-internal 的 [`LlmMessage`] / [`LlmContent`] / [`ToolDefinition`]
//! 转换为 OpenAI Chat Completions 和 Responses API 所需的 JSON 结构。

use astrcode_core::{
    config::OpenAiApiMode,
    llm::{LlmContent, LlmMessage, LlmRole, PromptCacheRetention},
    tool::ToolDefinition,
};

use crate::tool_result_wire::{
    openai_chat_tool_result_content, openai_responses_tool_result_output,
};

// ─── 工具序列化 ────────────────────────────────────────────────────────

pub(crate) fn tools_to_json(
    tools: &[ToolDefinition],
    supports_strict_tool_use: bool,
) -> serde_json::Value {
    serde_json::Value::Array(
        tools
            .iter()
            .map(|t| {
                let mut function = serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                });
                if supports_strict_tool_use && t.strict {
                    function["strict"] = serde_json::json!(true);
                }
                serde_json::json!({
                    "type": "function",
                    "function": function
                })
            })
            .collect(),
    )
}

pub(crate) fn responses_tools_json(
    tools: &[ToolDefinition],
    supports_strict_tool_use: bool,
) -> serde_json::Value {
    serde_json::Value::Array(
        tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                    "strict": supports_strict_tool_use && t.strict,
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod strict_tool_tests {
    use astrcode_core::tool::{ExecutionMode, ToolOrigin};

    use super::*;

    fn tool(strict: bool) -> ToolDefinition {
        ToolDefinition {
            name: "lookup".into(),
            description: String::new(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
            strict,
            origin: ToolOrigin::Bundled,
            execution_mode: ExecutionMode::Parallel,
        }
    }

    #[test]
    fn serializes_strict_at_each_openai_wire_location() {
        let cases = [
            (
                tools_to_json(&[tool(true)], true),
                "/0/function/strict",
                Some(true),
            ),
            (
                tools_to_json(&[tool(true)], false),
                "/0/function/strict",
                None,
            ),
            (
                tools_to_json(&[tool(false)], true),
                "/0/function/strict",
                None,
            ),
            (
                responses_tools_json(&[tool(true)], true),
                "/0/strict",
                Some(true),
            ),
            (
                responses_tools_json(&[tool(true)], false),
                "/0/strict",
                Some(false),
            ),
            (
                responses_tools_json(&[tool(false)], true),
                "/0/strict",
                Some(false),
            ),
        ];

        for (serialized, pointer, expected) in cases {
            assert_eq!(
                serialized
                    .pointer(pointer)
                    .and_then(serde_json::Value::as_bool),
                expected
            );
        }
    }
}

// ─── Chat Completions 消息 ──────────────────────────────────────────────

pub(crate) fn chat_message_to_json(message: &LlmMessage) -> serde_json::Value {
    match message.role {
        LlmRole::Tool => {
            let Some(LlmContent::ToolResult {
                tool_call_id,
                content,
                ..
            }) = message.content.first()
            else {
                return serde_json::json!({"role": "tool", "tool_call_id": "", "content": ""});
            };
            serde_json::json!({
                "role": "tool",
                "tool_call_id": tool_call_id,
                "content": openai_chat_tool_result_content(content),
            })
        },
        LlmRole::Assistant
            if message
                .content
                .iter()
                .any(|c| matches!(c, LlmContent::ToolCall { .. })) =>
        {
            let tool_calls: Vec<serde_json::Value> = message
                .content
                .iter()
                .filter_map(|content| match content {
                    LlmContent::ToolCall {
                        call_id,
                        name,
                        arguments,
                        raw_arguments,
                    } => Some(serde_json::json!({
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": tool_call_arguments_text(
                                arguments,
                                raw_arguments.as_deref(),
                            )
                        }
                    })),
                    _ => None,
                })
                .collect();
            let mut obj = serde_json::json!({
                "role": "assistant",
                "content": chat_content_to_json(&message.content),
                "tool_calls": tool_calls
            });
            set_reasoning_content(&mut obj, &message.reasoning_content);
            obj
        },
        _ => {
            let role = match message.role {
                LlmRole::System => "system",
                LlmRole::User => "user",
                LlmRole::Assistant => "assistant",
                LlmRole::Tool => "tool",
            };
            let mut obj = serde_json::json!({
                "role": role,
                "content": chat_content_to_json(&message.content),
            });
            if matches!(message.role, LlmRole::Assistant) {
                set_reasoning_content(&mut obj, &message.reasoning_content);
            }
            if matches!(message.role, LlmRole::Tool)
                && let Some(ref name) = message.name
            {
                obj["name"] = serde_json::json!(name);
            }
            obj
        },
    }
}

fn set_reasoning_content(obj: &mut serde_json::Value, reasoning_content: &Option<String>) {
    if let Some(rc) = reasoning_content {
        obj["reasoning_content"] = serde_json::json!(rc);
    }
}

fn chat_content_to_json(content: &[LlmContent]) -> serde_json::Value {
    let has_image = content
        .iter()
        .any(|p| matches!(p, LlmContent::Image { .. }));
    if !has_image {
        let text = LlmContent::join_text(content, "");
        return serde_json::json!(text);
    }
    serde_json::Value::Array(
        content
            .iter()
            .filter_map(|p| match p {
                LlmContent::Text { text } => {
                    Some(serde_json::json!({"type": "text", "text": text}))
                },
                LlmContent::Image {
                    base64, media_type, ..
                } => Some(serde_json::json!({
                    "type": "image_url",
                    "image_url": {"url": format!("data:{};base64,{}", media_type, base64)}
                })),
                _ => None,
            })
            .collect(),
    )
}

// ─── Responses 输入项 ──────────────────────────────────────────────────

pub(crate) fn responses_input_items(message: &LlmMessage) -> Vec<serde_json::Value> {
    match message.role {
        LlmRole::User => vec![serde_json::json!({
            "role": "user",
            "content": responses_message_content(&message.content, true)
        })],
        LlmRole::Assistant => {
            let mut items = Vec::new();
            let text_content = responses_message_content(&message.content, false);
            if text_content.as_array().is_some_and(|c| !c.is_empty()) {
                items.push(serde_json::json!({"role": "assistant", "content": text_content}));
            }
            for content in &message.content {
                if let LlmContent::ToolCall {
                    call_id,
                    name,
                    arguments,
                    raw_arguments,
                } = content
                {
                    items.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": call_id,
                        "name": name,
                        "arguments": tool_call_arguments_text(
                            arguments,
                            raw_arguments.as_deref(),
                        )
                    }));
                }
            }
            items
        },
        LlmRole::Tool => message
            .content
            .iter()
            .filter_map(|c| match c {
                LlmContent::ToolResult {
                    tool_call_id,
                    content,
                    ..
                } => Some(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": tool_call_id,
                    "output": openai_responses_tool_result_output(content),
                })),
                _ => None,
            })
            .collect(),
        LlmRole::System => Vec::new(),
    }
}

fn responses_message_content(content: &[LlmContent], input: bool) -> serde_json::Value {
    serde_json::Value::Array(
        content
            .iter()
            .filter_map(|p| match p {
                LlmContent::Text { text } => {
                    let kind = if input { "input_text" } else { "output_text" };
                    Some(serde_json::json!({"type": kind, "text": text}))
                },
                LlmContent::Image {
                    base64, media_type, ..
                } if input => Some(serde_json::json!({
                    "type": "input_image",
                    "image_url": format!("data:{};base64,{}", media_type, base64)
                })),
                _ => None,
            })
            .collect(),
    )
}

fn tool_call_arguments_text(arguments: &serde_json::Value, raw_arguments: Option<&str>) -> String {
    raw_arguments.map_or_else(|| arguments.to_string(), str::to_owned)
}

// ─── Prompt cache 辅助 ─────────────────────────────────────────────────

pub(crate) fn system_text(messages: &[LlmMessage]) -> String {
    LlmContent::join_text(
        messages
            .iter()
            .filter(|message| message.role == LlmRole::System)
            .flat_map(|message| &message.content),
        "\n\n",
    )
}

pub(crate) fn prompt_cache_retention_wire_value(
    api_mode: OpenAiApiMode,
    retention: PromptCacheRetention,
) -> &'static str {
    match (api_mode, retention) {
        (_, PromptCacheRetention::TwentyFourHours) => "24h",
        (OpenAiApiMode::ChatCompletions, PromptCacheRetention::InMemory) => "in_memory",
        (OpenAiApiMode::Responses, PromptCacheRetention::InMemory) => "in-memory",
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        event::stable_hash_hex,
        llm::{LlmContent, LlmMessage, LlmRole},
    };

    use super::{chat_message_to_json, responses_input_items};

    #[test]
    fn chat_tool_call_message_preserves_content_and_reasoning_content() {
        let message = LlmMessage {
            role: LlmRole::Assistant,
            content: vec![
                LlmContent::Text {
                    text: "checking".into(),
                },
                LlmContent::ToolCall {
                    call_id: "call_1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "a.rs"}),
                    raw_arguments: None,
                },
            ],
            name: None,
            reasoning_content: Some("private reasoning".into()),
        };

        let value = chat_message_to_json(&message);

        assert_eq!(value["role"], "assistant");
        assert_eq!(value["content"], "checking");
        assert_eq!(value["reasoning_content"], "private reasoning");
        assert_eq!(value["tool_calls"][0]["id"], "call_1");
    }

    #[test]
    fn openai_history_distinguishes_raw_and_valid_string_arguments() {
        let raw = r#"{"text">"news"}"#;
        let message = LlmMessage {
            role: LlmRole::Assistant,
            content: vec![
                LlmContent::ToolCall {
                    call_id: "call_bad".into(),
                    name: "interact".into(),
                    arguments: serde_json::Value::String(raw.into()),
                    raw_arguments: Some(raw.into()),
                },
                LlmContent::ToolCall {
                    call_id: "call_string".into(),
                    name: "echo".into(),
                    arguments: serde_json::Value::String("hello".into()),
                    raw_arguments: None,
                },
            ],
            name: None,
            reasoning_content: None,
        };

        let chat = chat_message_to_json(&message);
        let responses = responses_input_items(&message);

        assert_eq!(chat["tool_calls"][0]["function"]["arguments"], raw);
        assert_eq!(responses[0]["arguments"], raw);
        assert_eq!(
            chat["tool_calls"][1]["function"]["arguments"],
            serde_json::json!(r#""hello""#)
        );
        assert_eq!(responses[1]["arguments"], serde_json::json!(r#""hello""#));
    }

    #[test]
    fn stable_hash_hex_matches_reference_fnv1a_with_ff_separators() {
        // 独立参考实现：FNV-1a，每段 part 后插入 0xff 分隔。
        // prompt_cache_key 是与上游网关的事实线缆契约，输出必须与参考逐位一致。
        fn reference(parts: &[&str]) -> String {
            let mut hash = 0xcbf29ce484222325u64;
            for part in parts {
                for &byte in part.as_bytes() {
                    hash ^= u64::from(byte);
                    hash = hash.wrapping_mul(0x100000001b3);
                }
                hash ^= 0xff;
                hash = hash.wrapping_mul(0x100000001b3);
            }
            format!("{hash:016x}")
        }

        let cases: &[&[&str]] = &[
            &[],
            &[""],
            &["a"],
            &["system", "tools"],
            &["astrcode", "model-id", "system prompt", r#"{"tools":[]}"#],
            &["中文", "emoji 🎉"],
        ];
        for parts in cases {
            assert_eq!(
                stable_hash_hex(parts),
                reference(parts),
                "stable_hash_hex drift for {parts:?}"
            );
        }
        // 空 parts 锁定 FNV offset basis。
        assert_eq!(stable_hash_hex(&[]), "cbf29ce484222325");
    }
}
