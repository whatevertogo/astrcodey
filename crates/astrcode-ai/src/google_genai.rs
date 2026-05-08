//! Google Gemini API provider.
//!
//! Implements [`LlmProvider`] for Google's generativelanguage API with SSE
//! streaming, function calling, and thinking support.

use astrcode_core::{llm::*, tool::ToolDefinition};
use tokio::sync::mpsc;

use crate::common::{build_client, stream_with_retry};
use crate::retry::RetryPolicy;

pub struct GeminiProvider {
    config: LlmClientConfig,
    model_id: String,
    model_limits_val: ModelLimits,
    client: reqwest::Client,
}

impl GeminiProvider {
    pub fn new(
        config: LlmClientConfig,
        model_id: String,
        max_tokens: Option<u32>,
        context_limit: Option<usize>,
    ) -> Self {
        let client = build_client(&config);
        Self {
            config,
            model_id,
            model_limits_val: ModelLimits {
                max_input_tokens: context_limit.unwrap_or(1_048_576),
                max_output_tokens: max_tokens.unwrap_or(8192) as usize,
            },
            client,
        }
    }

    fn endpoint(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        let model = &self.model_id;
        if base.contains("generateContent") || base.contains("streamGenerateContent") {
            return format!("{base}&key={}", self.config.api_key);
        }
        format!(
            "{base}/models/{model}:streamGenerateContent?alt=sse&key={}",
            self.config.api_key
        )
    }

    fn build_request_body(&self, messages: &[LlmMessage], tools: &[ToolDefinition]) -> serde_json::Value {
        let mut system_instruction: Option<serde_json::Value> = None;
        let mut contents: Vec<serde_json::Value> = Vec::new();
        let mut pending_tool_results: Vec<serde_json::Value> = Vec::new();

        for msg in messages {
            match msg.role {
                LlmRole::System => {
                    let text: String = msg
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            LlmContent::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        system_instruction = Some(serde_json::json!({
                            "parts": [{"text": text}]
                        }));
                    }
                }
                LlmRole::Assistant => {
                    flush_tool_results(&mut pending_tool_results, &mut contents);
                    contents.push(convert_assistant_to_gemini(msg));
                }
                LlmRole::User => {
                    flush_tool_results(&mut pending_tool_results, &mut contents);
                    contents.push(convert_user_to_gemini(msg));
                }
                LlmRole::Tool => {
                    pending_tool_results.push(convert_tool_result_to_gemini(msg));
                }
            }
        }
        flush_tool_results(&mut pending_tool_results, &mut contents);

        let mut body = serde_json::json!({
            "contents": contents,
            "generationConfig": {
                "maxOutputTokens": self.model_limits_val.max_output_tokens,
            }
        });

        if let Some(sys) = system_instruction {
            body["systemInstruction"] = sys;
        }
        if let Some(t) = self.config.temperature {
            body["generationConfig"]["temperature"] = serde_json::json!(t);
        }
        if !tools.is_empty() {
            body["tools"] = convert_tools_to_gemini(tools);
        }
        body
    }
}

#[async_trait::async_trait]
impl LlmProvider for GeminiProvider {
    async fn generate(
        &self,
        messages: Vec<LlmMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let body = self.build_request_body(&messages, &tools);

        let endpoint = self.endpoint();
        let headers: Vec<(String, String)> = self
            .config
            .extra_headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let client = self.client.clone();
        let retry = RetryPolicy {
            max_retries: self.config.max_retries,
            base_delay_ms: self.config.retry_base_delay_ms,
        };

        tokio::spawn(async move {
            let result = stream_with_retry(
                client,
                endpoint,
                headers,
                body,
                retry,
                tx.clone(),
                |data, tx| {
                    if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                        process_gemini_chunk(&event, tx);
                    }
                },
            )
            .await;
            if let Err(e) = result {
                let _ = tx.send(LlmEvent::Error {
                    message: e.to_string(),
                });
            }
        });

        Ok(rx)
    }

    fn model_limits(&self) -> ModelLimits {
        self.model_limits_val.clone()
    }
}

fn process_gemini_chunk(event: &serde_json::Value, tx: &mpsc::UnboundedSender<LlmEvent>) {
    let Some(candidates) = event.get("candidates").and_then(|v| v.as_array()) else {
        return;
    };

    for candidate in candidates {
        let Some(parts) = candidate.pointer("/content/parts").and_then(|v| v.as_array()) else {
            continue;
        };

        for part in parts {
            if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                if part.get("thought").and_then(|v| v.as_bool()).unwrap_or(false) {
                    let _ = tx.send(LlmEvent::ThinkingDelta { delta: text.to_string() });
                } else {
                    let _ = tx.send(LlmEvent::ContentDelta { delta: text.to_string() });
                }
            }

            if let Some(fc) = part.get("functionCall") {
                let name = fc.get("name").and_then(|v| v.as_str()).unwrap_or_default();
                let call_id = fc.get("id").and_then(|v| v.as_str()).unwrap_or(name);
                let args = fc.get("args").cloned().unwrap_or(serde_json::json!({}));
                let arguments_str = serde_json::to_string(&args).unwrap_or_default();
                let _ = tx.send(LlmEvent::ToolCallStart {
                    call_id: call_id.to_string(),
                    name: name.to_string(),
                    arguments: arguments_str,
                });
            }
        }

        if let Some(finish) = candidate.get("finishReason").and_then(|v| v.as_str()) {
            if !finish.is_empty() {
                let _ = tx.send(LlmEvent::Done {
                    finish_reason: finish.to_string(),
                });
            }
        }
    }
}

// ─── Message conversion ──────────────────────────────────────────────────

fn convert_user_to_gemini(msg: &LlmMessage) -> serde_json::Value {
    let mut parts: Vec<serde_json::Value> = Vec::new();
    for content in &msg.content {
        match content {
            LlmContent::Text { text } => {
                parts.push(serde_json::json!({"text": text}));
            }
            LlmContent::Image { base64, media_type } => {
                parts.push(serde_json::json!({
                    "inlineData": {"mimeType": media_type, "data": base64}
                }));
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        parts.push(serde_json::json!({"text": ""}));
    }
    serde_json::json!({"role": "user", "parts": parts})
}

fn convert_assistant_to_gemini(msg: &LlmMessage) -> serde_json::Value {
    let mut parts: Vec<serde_json::Value> = Vec::new();
    for content in &msg.content {
        match content {
            LlmContent::Text { text } => {
                parts.push(serde_json::json!({"text": text}));
            }
            LlmContent::ToolCall { name, arguments, .. } => {
                let args = match arguments {
                    serde_json::Value::String(s) => {
                        serde_json::from_str(s).unwrap_or(serde_json::json!({}))
                    }
                    other => other.clone(),
                };
                parts.push(serde_json::json!({
                    "functionCall": {"name": name, "args": args}
                }));
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        parts.push(serde_json::json!({"text": ""}));
    }
    serde_json::json!({"role": "model", "parts": parts})
}

fn convert_tool_result_to_gemini(msg: &LlmMessage) -> serde_json::Value {
    let mut name = String::new();
    let mut result_text = String::new();
    let mut is_error = false;
    for content in &msg.content {
        if let LlmContent::ToolResult { tool_call_id, content: text, is_error: err } = content {
            name = msg.name.clone().unwrap_or_else(|| tool_call_id.clone());
            result_text = text.clone();
            is_error = *err;
        }
    }
    serde_json::json!({
        "functionResponse": {
            "name": name,
            "response": {"output": result_text, "error": is_error}
        }
    })
}

fn flush_tool_results(pending: &mut Vec<serde_json::Value>, contents: &mut Vec<serde_json::Value>) {
    if pending.is_empty() {
        return;
    }
    let parts = std::mem::take(pending);
    contents.push(serde_json::json!({"role": "user", "parts": parts}));
}

fn convert_tools_to_gemini(tools: &[ToolDefinition]) -> serde_json::Value {
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
    use super::*;

    #[test]
    fn gemini_maps_assistant_to_model_role() {
        let msg = LlmMessage::assistant("hi");
        let json = convert_assistant_to_gemini(&msg);
        assert_eq!(json["role"], "model");
        assert_eq!(json["parts"][0]["text"], "hi");
    }

    #[test]
    fn gemini_tool_call_uses_function_call() {
        let msg = LlmMessage {
            role: LlmRole::Assistant,
            content: vec![LlmContent::ToolCall {
                call_id: "call_1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "foo.rs"}),
            }],
            name: None,
        };
        let json = convert_assistant_to_gemini(&msg);
        let fc = &json["parts"][0]["functionCall"];
        assert_eq!(fc["name"], "read");
        assert_eq!(fc["args"]["path"], "foo.rs");
    }

    #[test]
    fn gemini_tool_results_pack_into_single_user_turn() {
        let mut contents: Vec<serde_json::Value> = Vec::new();
        let mut pending: Vec<serde_json::Value> = Vec::new();
        contents.push(convert_assistant_to_gemini(&LlmMessage::assistant("checking")));
        pending.push(convert_tool_result_to_gemini(&LlmMessage::tool("read", "call_1", "content", false)));
        pending.push(convert_tool_result_to_gemini(&LlmMessage::tool("grep", "call_2", "match", false)));
        flush_tool_results(&mut pending, &mut contents);
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[1]["role"], "user");
        let parts = contents[1]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn endpoint_includes_model_and_key() {
        let provider = GeminiProvider::new(
            LlmClientConfig {
                base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
                api_key: "test-key".into(),
                ..LlmClientConfig::default()
            },
            "gemini-2.5-pro".into(),
            None,
            None,
        );
        let endpoint = provider.endpoint();
        assert!(endpoint.contains("gemini-2.5-pro"));
        assert!(endpoint.contains("streamGenerateContent"));
        assert!(endpoint.contains("test-key"));
    }
}
