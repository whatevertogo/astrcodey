//! Google GenAI wire request construction.
//!
//! This module owns the Google Generative Language JSON contract: endpoint
//! resolution, message/tool conversion, and count-token body shape. Transport
//! and SSE chunk parsing stay in the provider wrapper.

use astrcode_core::{
    llm::{LlmContent, LlmMessage, LlmRole},
    tool::ToolDefinition,
};

use crate::{serialization::ContentMapper, tool_result_wire::gemini_tool_result_parts};

#[derive(Debug, Clone, Copy)]
pub(crate) struct GoogleGenAiRequestConfig {
    pub max_output_tokens: usize,
}

pub(crate) fn endpoint_url(base_url: &str, model_id: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.contains("generateContent") || base.contains("streamGenerateContent") {
        return base.to_string();
    }
    format!("{base}/models/{model_id}:streamGenerateContent?alt=sse")
}

pub(crate) fn count_tokens_endpoint(base_url: &str, model_id: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let base = base.split('?').next().unwrap_or(base);
    if base.contains(":countTokens") {
        return base.to_string();
    }
    if let Some(prefix) = base.strip_suffix(":streamGenerateContent") {
        return format!("{prefix}:countTokens");
    }
    if let Some(prefix) = base.strip_suffix(":generateContent") {
        return format!("{prefix}:countTokens");
    }
    format!("{base}/models/{model_id}:countTokens")
}

pub(crate) fn build_request_body(
    config: GoogleGenAiRequestConfig,
    messages: &[LlmMessage],
    tools: &[ToolDefinition],
) -> serde_json::Value {
    let (system_text, contents) = convert_messages(messages);
    let mut body = serde_json::json!({
        "contents": contents,
        "generationConfig": {
            "maxOutputTokens": config.max_output_tokens,
        }
    });

    if let Some(system_text) = system_text {
        body["systemInstruction"] = serde_json::json!({
            "parts": [{"text": system_text}]
        });
    }
    if !tools.is_empty() {
        body["tools"] = convert_tools(tools);
    }
    body
}

pub(crate) fn build_count_tokens_body(
    config: GoogleGenAiRequestConfig,
    messages: &[LlmMessage],
    tools: &[ToolDefinition],
) -> serde_json::Value {
    let mut body = build_request_body(config, messages, tools);
    if let Some(obj) = body.as_object_mut() {
        obj.remove("generationConfig");
    }
    body
}

fn convert_messages(messages: &[LlmMessage]) -> (Option<String>, Vec<serde_json::Value>) {
    let mut system_texts: Vec<String> = Vec::new();
    let mut contents: Vec<serde_json::Value> = Vec::new();
    let mut pending_tool_results: Vec<serde_json::Value> = Vec::new();

    for msg in messages {
        match msg.role {
            LlmRole::System => {
                let text = msg.joined_text("\n");
                if !text.trim().is_empty() {
                    system_texts.push(text);
                }
            },
            LlmRole::Assistant => {
                flush_tool_results(&mut pending_tool_results, &mut contents);
                contents.push(GoogleGenAiMapper::map_assistant(msg));
            },
            LlmRole::User => {
                flush_tool_results(&mut pending_tool_results, &mut contents);
                contents.push(GoogleGenAiMapper::map_user(msg));
            },
            LlmRole::Tool => {
                pending_tool_results.extend(convert_tool_result(msg));
            },
        }
    }
    flush_tool_results(&mut pending_tool_results, &mut contents);

    let system_text = if system_texts.is_empty() {
        None
    } else {
        Some(system_texts.join("\n\n"))
    };
    (system_text, contents)
}

struct GoogleGenAiMapper;

impl ContentMapper for GoogleGenAiMapper {
    fn text(text: &str) -> serde_json::Value {
        serde_json::json!({"text": text})
    }

    fn image(base64: &str, media_type: &str) -> serde_json::Value {
        serde_json::json!({
            "inlineData": {"mimeType": media_type, "data": base64}
        })
    }

    fn tool_call(_call_id: &str, name: &str, arguments: &serde_json::Value) -> serde_json::Value {
        let args = match arguments {
            serde_json::Value::String(s) => {
                serde_json::from_str(s).unwrap_or(serde_json::json!({}))
            },
            other => other.clone(),
        };
        serde_json::json!({"functionCall": {"name": name, "args": args}})
    }

    fn tool_result(_: &str, _: &str, _: bool) -> Option<serde_json::Value> {
        None
    }

    fn empty() -> serde_json::Value {
        serde_json::json!({"text": ""})
    }

    fn wrap_user(parts: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({"role": "user", "parts": parts})
    }

    fn wrap_assistant(parts: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({"role": "model", "parts": parts})
    }
}

fn convert_tool_result(msg: &LlmMessage) -> Vec<serde_json::Value> {
    let mut name = String::new();
    let mut result_text = String::new();
    let mut is_error = false;
    for content in &msg.content {
        if let LlmContent::ToolResult {
            tool_call_id,
            content: text,
            is_error: err,
        } = content
        {
            name = msg.name.clone().unwrap_or_else(|| tool_call_id.clone());
            result_text = text.clone();
            is_error = *err;
        }
    }
    gemini_tool_result_parts(&name, &result_text, is_error)
}

fn flush_tool_results(pending: &mut Vec<serde_json::Value>, contents: &mut Vec<serde_json::Value>) {
    if pending.is_empty() {
        return;
    }
    let parts = std::mem::take(pending);
    contents.push(serde_json::json!({"role": "user", "parts": parts}));
}

fn convert_tools(tools: &[ToolDefinition]) -> serde_json::Value {
    serde_json::json!([{
        "functionDeclarations": tools.iter().map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
            })
        }).collect::<Vec<_>>()
    }])
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        llm::{LlmContent, LlmMessage, LlmRole},
        tool::{ExecutionMode, ToolDefinition, ToolOrigin},
    };

    use super::*;

    #[test]
    fn assistant_message_converts_to_model_role() {
        let msg = LlmMessage::assistant("hi");
        let json = GoogleGenAiMapper::map_assistant(&msg);
        assert_eq!(json["role"], "model");
        assert_eq!(json["parts"][0]["text"], "hi");
    }

    #[test]
    fn assistant_tool_call_uses_function_call() {
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
        let json = GoogleGenAiMapper::map_assistant(&msg);
        let function_call = &json["parts"][0]["functionCall"];
        assert_eq!(function_call["name"], "read");
        assert_eq!(function_call["args"]["path"], "foo.rs");
    }

    #[test]
    fn tool_results_pack_into_single_user_turn() {
        let messages = vec![
            LlmMessage::assistant("checking"),
            LlmMessage::tool("read", "call_1", "content", false),
            LlmMessage::tool("grep", "call_2", "match", false),
        ];
        let (_system, contents) = convert_messages(&messages);
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[1]["role"], "user");
        let parts = contents[1]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn request_body_preserves_multiple_system_messages() {
        let config = GoogleGenAiRequestConfig {
            max_output_tokens: 1024,
        };
        let body = build_request_body(
            config,
            &[
                LlmMessage::system("static instructions"),
                LlmMessage::system("project instructions"),
                LlmMessage::user("hi"),
            ],
            &[],
        );

        assert_eq!(
            body["systemInstruction"]["parts"][0]["text"],
            "static instructions\n\nproject instructions"
        );
    }

    #[test]
    fn count_tokens_request_reuses_generate_content_shape_without_generation_config() {
        let tools = vec![ToolDefinition {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: serde_json::json!({"type": "object"}),
            origin: ToolOrigin::Builtin,
            execution_mode: ExecutionMode::Parallel,
        }];
        let config = GoogleGenAiRequestConfig {
            max_output_tokens: 1024,
        };
        let body = build_count_tokens_body(
            config,
            &[LlmMessage::system("s"), LlmMessage::user("hi")],
            &tools,
        );

        assert_eq!(
            count_tokens_endpoint(
                "https://generativelanguage.googleapis.com/v1beta",
                "gemini-test"
            ),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-test:countTokens"
        );
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "s");
        assert!(body["contents"].is_array());
        assert!(body["tools"].is_array());
        assert!(body.get("generationConfig").is_none());
    }

    #[test]
    fn endpoint_includes_model_without_api_key_query() {
        let endpoint = endpoint_url(
            "https://generativelanguage.googleapis.com/v1beta",
            "gemini-2.5-pro",
        );
        assert!(endpoint.contains("gemini-2.5-pro"));
        assert!(endpoint.contains("streamGenerateContent"));
        assert!(!endpoint.contains("test-key"));
    }

    #[test]
    fn count_tokens_endpoint_preserves_full_count_tokens_url() {
        assert_eq!(
            count_tokens_endpoint(
                "https://generativelanguage.googleapis.com/v1beta/models/gemini-test:countTokens",
                "ignored"
            ),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-test:countTokens"
        );
    }
}
