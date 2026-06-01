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

pub(crate) fn tools_to_json(tools: &[ToolDefinition]) -> serde_json::Value {
    serde_json::Value::Array(
        tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect(),
    )
}

pub(crate) fn responses_tools_json(tools: &[ToolDefinition]) -> serde_json::Value {
    serde_json::Value::Array(
        tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                    "strict": false,
                })
            })
            .collect(),
    )
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
                    } => Some(serde_json::json!({
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments.to_string()
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
            if matches!(message.role, LlmRole::Tool) {
                if let Some(ref name) = message.name {
                    obj["name"] = serde_json::json!(name);
                }
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
        let text = content
            .iter()
            .filter_map(|p| match p {
                LlmContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
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
                } = content
                {
                    items.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": call_id,
                        "name": name,
                        "arguments": arguments.to_string()
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

// ─── Prompt cache 辅助 ─────────────────────────────────────────────────

pub(crate) fn system_text(messages: &[LlmMessage]) -> String {
    messages
        .iter()
        .filter(|m| matches!(m.role, LlmRole::System))
        .flat_map(|m| m.content.iter())
        .filter_map(|c| match c {
            LlmContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub(crate) fn stable_hash_hex(parts: &[&str]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for part in parts {
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
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

// ─── Provider-agnostic content mapping ──────────────────────────────────

/// 将 `LlmContent` 枚举映射为提供商特定的 JSON 结构。
///
/// Anthropic 和 Gemini 的消息转换都遵循"遍历 content 数组 → 按 variant 分发 → 收集 parts"模式，
/// 只是字段名和输出结构不同。此 trait 将公共遍历逻辑与提供商特定的字段映射解耦。
pub(crate) trait ContentMapper {
    fn text(text: &str) -> serde_json::Value;
    fn image(base64: &str, media_type: &str) -> serde_json::Value;
    fn tool_call(call_id: &str, name: &str, arguments: &serde_json::Value) -> serde_json::Value;
    /// 返回 `None` 表示此提供商不在用户消息内联 ToolResult。
    fn tool_result(id: &str, content: &str, is_error: bool) -> Option<serde_json::Value>;
    fn empty() -> serde_json::Value;
    fn wrap_user(parts: Vec<serde_json::Value>) -> serde_json::Value;
    fn wrap_assistant(parts: Vec<serde_json::Value>) -> serde_json::Value;

    fn map_user(msg: &LlmMessage) -> serde_json::Value {
        let mut parts: Vec<serde_json::Value> = msg
            .content
            .iter()
            .filter_map(|c| match c {
                LlmContent::Text { text } => Some(Self::text(text)),
                LlmContent::Image {
                    base64, media_type, ..
                } => Some(Self::image(base64, media_type)),
                LlmContent::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                } => Self::tool_result(tool_call_id, content, *is_error),
                _ => None,
            })
            .collect();
        if parts.is_empty() {
            parts.push(Self::empty());
        }
        Self::wrap_user(parts)
    }

    fn map_assistant(msg: &LlmMessage) -> serde_json::Value {
        let mut parts: Vec<serde_json::Value> = msg
            .content
            .iter()
            .filter_map(|c| match c {
                LlmContent::Text { text } => Some(Self::text(text)),
                LlmContent::ToolCall {
                    call_id,
                    name,
                    arguments,
                } => Some(Self::tool_call(call_id, name, arguments)),
                _ => None,
            })
            .collect();
        if parts.is_empty() {
            parts.push(Self::empty());
        }
        Self::wrap_assistant(parts)
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::llm::{LlmContent, LlmMessage, LlmRole};

    use super::chat_message_to_json;

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
}
