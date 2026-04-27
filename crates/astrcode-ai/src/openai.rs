//! OpenAI-compatible LLM provider implementation.

use std::collections::BTreeMap;

use astrcode_core::{config::OpenAiApiMode, llm::*, tool::ToolDefinition};
use tokio::sync::mpsc;

use crate::{cache::CacheTracker, retry::RetryPolicy};

/// OpenAI-compatible LLM provider.
pub struct OpenAiProvider {
    config: LlmClientConfig,
    api_mode: OpenAiApiMode,
    model_id: String,
    model_limits_val: ModelLimits,
    _cache_tracker: CacheTracker,
    client: reqwest::Client,
}

impl OpenAiProvider {
    pub fn new(
        config: LlmClientConfig,
        api_mode: OpenAiApiMode,
        model_id: String,
        max_tokens: Option<u32>,
        context_limit: Option<usize>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(config.connect_timeout_secs))
            .timeout(std::time::Duration::from_secs(config.read_timeout_secs))
            .build()
            .expect("Failed to create HTTP client");

        let model_limits_val = ModelLimits {
            max_input_tokens: context_limit.unwrap_or(65536),
            max_output_tokens: max_tokens.unwrap_or(8192) as usize,
        };

        Self {
            config,
            api_mode,
            model_id,
            model_limits_val,
            _cache_tracker: CacheTracker::new(),
            client,
        }
    }

    fn endpoint(&self) -> String {
        match self.api_mode {
            OpenAiApiMode::ChatCompletions => {
                format!(
                    "{}/chat/completions",
                    self.config.base_url.trim_end_matches('/')
                )
            },
            OpenAiApiMode::Responses => {
                format!("{}/responses", self.config.base_url.trim_end_matches('/'))
            },
        }
    }

    fn build_request_body(
        &self,
        messages: &[LlmMessage],
        tools: &[ToolDefinition],
    ) -> serde_json::Value {
        match self.api_mode {
            OpenAiApiMode::ChatCompletions => self.build_chat_request_body(messages, tools),
            OpenAiApiMode::Responses => self.build_responses_request_body(messages, tools),
        }
    }

    fn build_chat_request_body(
        &self,
        messages: &[LlmMessage],
        tools: &[ToolDefinition],
    ) -> serde_json::Value {
        let messages_json: Vec<serde_json::Value> =
            messages.iter().map(chat_message_to_json).collect();

        let tools_json: Vec<serde_json::Value> = tools
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
            .collect();

        serde_json::json!({
            "model": self.model_id,
            "messages": messages_json,
            "tools": tools_json,
            "stream": true,
            "stream_options": {"include_usage": true},
        })
    }

    fn build_responses_request_body(
        &self,
        messages: &[LlmMessage],
        tools: &[ToolDefinition],
    ) -> serde_json::Value {
        let instructions = messages
            .iter()
            .filter(|message| matches!(message.role, LlmRole::System))
            .flat_map(|message| message.content.iter())
            .filter_map(|content| match content {
                LlmContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let input: Vec<serde_json::Value> = messages
            .iter()
            .filter(|message| !matches!(message.role, LlmRole::System))
            .flat_map(responses_input_items)
            .collect();
        let tools_json: Vec<serde_json::Value> = tools
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
            .collect();

        serde_json::json!({
            "model": self.model_id,
            "instructions": instructions,
            "input": input,
            "tools": tools_json,
            "stream": true,
        })
    }
}

fn chat_message_to_json(message: &LlmMessage) -> serde_json::Value {
    match message.role {
        LlmRole::Tool => {
            let Some(LlmContent::ToolResult {
                tool_call_id,
                content,
                ..
            }) = message.content.first()
            else {
                return serde_json::json!({
                    "role": "tool",
                    "tool_call_id": "",
                    "content": ""
                });
            };
            serde_json::json!({
                "role": "tool",
                "tool_call_id": tool_call_id,
                "content": content
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
            serde_json::json!({
                "role": "assistant",
                "content": serde_json::Value::Null,
                "tool_calls": tool_calls
            })
        },
        _ => {
            serde_json::json!({
                "role": match message.role {
                    LlmRole::System => "system",
                    LlmRole::User => "user",
                    LlmRole::Assistant => "assistant",
                    LlmRole::Tool => "tool",
                },
                "content": chat_content_to_json(&message.content),
                "name": message.name,
            })
        },
    }
}

fn chat_content_to_json(content: &[LlmContent]) -> serde_json::Value {
    let has_image = content
        .iter()
        .any(|part| matches!(part, LlmContent::Image { .. }));
    if !has_image {
        let text = content
            .iter()
            .filter_map(|part| match part {
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
            .filter_map(|part| match part {
                LlmContent::Text { text } => {
                    Some(serde_json::json!({"type": "text", "text": text}))
                },
                LlmContent::Image { base64, media_type } => Some(serde_json::json!({
                    "type": "image_url",
                    "image_url": {"url": format!("data:{};base64,{}", media_type, base64)}
                })),
                _ => None,
            })
            .collect(),
    )
}

fn responses_input_items(message: &LlmMessage) -> Vec<serde_json::Value> {
    match message.role {
        LlmRole::User => vec![serde_json::json!({
            "role": "user",
            "content": responses_message_content(&message.content, true)
        })],
        LlmRole::Assistant => {
            let mut items = Vec::new();
            let text_content = responses_message_content(&message.content, false);
            if text_content
                .as_array()
                .is_some_and(|content| !content.is_empty())
            {
                items.push(serde_json::json!({
                    "role": "assistant",
                    "content": text_content
                }));
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
            .filter_map(|content| match content {
                LlmContent::ToolResult {
                    tool_call_id,
                    content,
                    ..
                } => Some(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": tool_call_id,
                    "output": content
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
            .filter_map(|part| match part {
                LlmContent::Text { text } => {
                    let kind = if input { "input_text" } else { "output_text" };
                    Some(serde_json::json!({"type": kind, "text": text}))
                },
                LlmContent::Image { base64, media_type } if input => Some(serde_json::json!({
                    "type": "input_image",
                    "image_url": format!("data:{};base64,{}", media_type, base64)
                })),
                _ => None,
            })
            .collect(),
    )
}

#[async_trait::async_trait]
impl LlmProvider for OpenAiProvider {
    async fn generate(
        &self,
        messages: Vec<LlmMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let body = self.build_request_body(&messages, &tools);
        let endpoint = self.endpoint();
        let api_key = self.config.api_key.clone();
        let client = self.client.clone();
        let api_mode = self.api_mode;

        tokio::spawn(async move {
            let result =
                Self::stream_request(client, endpoint, api_key, body, api_mode, tx.clone()).await;
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

impl OpenAiProvider {
    async fn stream_request(
        client: reqwest::Client,
        endpoint: String,
        api_key: String,
        body: serde_json::Value,
        api_mode: OpenAiApiMode,
        tx: mpsc::UnboundedSender<LlmEvent>,
    ) -> Result<(), LlmError> {
        let retry = RetryPolicy::default();
        let mut attempt = 0;

        loop {
            attempt += 1;
            let response = client
                .post(&endpoint)
                .header("Authorization", format!("Bearer {}", api_key))
                .json(&body)
                .send()
                .await
                .map_err(|e| LlmError::Transport(e.to_string()))?;

            let status = response.status();
            if status.is_success() {
                return Self::parse_stream(response, api_mode, &tx).await;
            }

            if retry.should_retry(attempt, status.as_u16()) {
                let delay = retry.delay(attempt);
                tokio::time::sleep(delay).await;
                continue;
            }

            let text = response.text().await.unwrap_or_default();
            if status.as_u16() >= 500 {
                return Err(LlmError::ServerError {
                    status: status.as_u16(),
                    message: text,
                });
            }
            return Err(LlmError::ClientError {
                status: status.as_u16(),
                message: text,
            });
        }
    }

    async fn parse_stream(
        response: reqwest::Response,
        api_mode: OpenAiApiMode,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) -> Result<(), LlmError> {
        use futures_util::StreamExt;
        let mut stream = response.bytes_stream();
        let mut decoder = Utf8StreamDecoder::new();
        let mut accumulator = LlmAccumulator::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(|e| LlmError::Transport(e.to_string()))?;
            let text = decoder.decode(&bytes);

            match api_mode {
                OpenAiApiMode::ChatCompletions => {
                    for line in text.lines() {
                        if line.is_empty() || !line.starts_with("data: ") {
                            continue;
                        }
                        let data = &line[6..]; // Strip "data: " prefix
                        if data == "[DONE]" {
                            accumulator.done_sent = true;
                            let _ = tx.send(LlmEvent::Done {
                                finish_reason: "stop".into(),
                            });
                            return Ok(());
                        }
                        if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                            accumulator.ingest_chat_completion(&event, tx);
                        }
                    }
                },
                OpenAiApiMode::Responses => {
                    for line in text.lines() {
                        if line.is_empty() {
                            continue;
                        }
                        if let Some(data) = line.strip_prefix("data: ") {
                            if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                                accumulator.ingest_responses(&event, tx);
                            }
                        }
                    }
                },
            }
        }
        // Send Done if stream ended without explicit [DONE] marker or finish_reason
        if !accumulator.done_sent() {
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "stop".into(),
            });
        }
        Ok(())
    }
}

/// Streaming UTF-8 decoder for handling multi-byte boundary splits.
pub struct Utf8StreamDecoder {
    buffer: Vec<u8>,
}

impl Utf8StreamDecoder {
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    pub fn decode(&mut self, bytes: &[u8]) -> String {
        self.buffer.extend_from_slice(bytes);
        match std::str::from_utf8(&self.buffer) {
            Ok(s) => {
                let result = s.to_string();
                self.buffer.clear();
                result
            },
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                if valid_up_to > 0 {
                    let result = std::str::from_utf8(&self.buffer[..valid_up_to])
                        .unwrap()
                        .to_string();
                    self.buffer = self.buffer[valid_up_to..].to_vec();
                    result
                } else {
                    // All bytes are invalid UTF-8 — discard to prevent unbounded growth
                    if self.buffer.len() > 4096 {
                        tracing::warn!(
                            "Discarding {} bytes of invalid UTF-8 in SSE stream",
                            self.buffer.len()
                        );
                        self.buffer.clear();
                    }
                    String::new()
                }
            },
        }
    }
}

impl Default for Utf8StreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Accumulates SSE deltas into a coherent output.
pub struct LlmAccumulator {
    text: String,
    tool_calls: BTreeMap<u64, ToolCallPartial>,
    response_tool_items: BTreeMap<String, ResponseToolCallPartial>,
    done_sent: bool,
}

#[derive(Debug, Default)]
struct ToolCallPartial {
    id: Option<String>,
    name: Option<String>,
    started: bool,
}

#[derive(Debug, Default)]
struct ResponseToolCallPartial {
    call_id: Option<String>,
    name: Option<String>,
    started: bool,
}

impl LlmAccumulator {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            tool_calls: BTreeMap::new(),
            response_tool_items: BTreeMap::new(),
            done_sent: false,
        }
    }

    /// Whether a Done event was already emitted for this stream.
    pub fn done_sent(&self) -> bool {
        self.done_sent
    }

    pub fn ingest_chat_completion(
        &mut self,
        event: &serde_json::Value,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) {
        if let Some(choices) = event["choices"].as_array() {
            for choice in choices {
                if let Some(delta) = choice.get("delta") {
                    // Text content
                    if let Some(content) = delta["content"].as_str() {
                        self.text.push_str(content);
                        let _ = tx.send(LlmEvent::ContentDelta {
                            delta: content.to_string(),
                        });
                    }
                    // Tool calls
                    if let Some(tool_calls) = delta["tool_calls"].as_array() {
                        for tc in tool_calls {
                            let idx = tc["index"].as_u64().unwrap_or(0);
                            let partial = self.tool_calls.entry(idx).or_default();
                            if let Some(id) = tc["id"].as_str() {
                                partial.id = Some(id.to_string());
                            }
                            if let Some(func) = tc.get("function") {
                                if let Some(name) = func["name"].as_str() {
                                    partial.name = Some(name.to_string());
                                }
                                // Chat Completions 的后续 tool result 必须使用真实 call id；
                                // 若提供商没有给 id，则退回 index，保证同一轮内仍可串起来。
                                if !partial.started {
                                    if let Some(name) = &partial.name {
                                        let call_id =
                                            partial.id.clone().unwrap_or_else(|| idx.to_string());
                                        partial.started = true;
                                        let _ = tx.send(LlmEvent::ToolCallStart {
                                            call_id,
                                            name: name.clone(),
                                            arguments: String::new(),
                                        });
                                    }
                                }
                                if let Some(args) = func["arguments"].as_str() {
                                    let call_id =
                                        partial.id.clone().unwrap_or_else(|| idx.to_string());
                                    let _ = tx.send(LlmEvent::ToolCallDelta {
                                        call_id,
                                        delta: args.to_string(),
                                    });
                                }
                            }
                        }
                    }
                }
                if let Some(finish) = choice["finish_reason"].as_str() {
                    if !self.done_sent {
                        self.done_sent = true;
                        let _ = tx.send(LlmEvent::Done {
                            finish_reason: finish.to_string(),
                        });
                    }
                }
            }
        }
    }

    pub fn ingest_responses(
        &mut self,
        event: &serde_json::Value,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) {
        // Handle Responses API format
        if let Some(event_type) = event["event"].as_str() {
            match event_type {
                "response.output_text.delta" => {
                    if let Some(delta) = event["delta"].as_str() {
                        let _ = tx.send(LlmEvent::ContentDelta {
                            delta: delta.to_string(),
                        });
                    }
                },
                "response.output_item.added" => {
                    let Some(item) = event["item"].as_object() else {
                        return;
                    };
                    if item.get("type").and_then(|v| v.as_str()) != Some("function_call") {
                        return;
                    }
                    let item_id = item
                        .get("id")
                        .and_then(|v| v.as_str())
                        .or_else(|| event["item_id"].as_str())
                        .unwrap_or_default()
                        .to_string();
                    let call_id = item
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&item_id)
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let partial = self.response_tool_items.entry(item_id).or_default();
                    partial.call_id = Some(call_id.clone());
                    partial.name = Some(name.clone());
                    partial.started = true;
                    let _ = tx.send(LlmEvent::ToolCallStart {
                        call_id,
                        name,
                        arguments: String::new(),
                    });
                },
                "response.function_call_arguments.delta" => {
                    let item_id = event["item_id"].as_str().unwrap_or_default();
                    let call_id = self
                        .response_tool_items
                        .get(item_id)
                        .and_then(|partial| partial.call_id.clone())
                        .unwrap_or_else(|| item_id.to_string());
                    if let Some(delta) = event["delta"].as_str() {
                        let _ = tx.send(LlmEvent::ToolCallDelta {
                            call_id,
                            delta: delta.to_string(),
                        });
                    }
                },
                "response.function_call_arguments.done" => {
                    let item_id = event["item_id"].as_str().unwrap_or_default().to_string();
                    let partial = self.response_tool_items.entry(item_id.clone()).or_default();
                    if let Some(name) = event["name"].as_str() {
                        partial.name = Some(name.to_string());
                    }
                    let call_id = partial.call_id.clone().unwrap_or(item_id);
                    if !partial.started {
                        partial.started = true;
                        let _ = tx.send(LlmEvent::ToolCallStart {
                            call_id: call_id.clone(),
                            name: partial.name.clone().unwrap_or_default(),
                            arguments: String::new(),
                        });
                    }
                    if let Some(arguments) = event["arguments"].as_str() {
                        let _ = tx.send(LlmEvent::ToolCallDelta {
                            call_id,
                            delta: arguments.to_string(),
                        });
                    }
                },
                "response.completed" => {
                    let _ = tx.send(LlmEvent::Done {
                        finish_reason: "stop".into(),
                    });
                },
                _ => {},
            }
        }
    }
}

impl Default for LlmAccumulator {
    fn default() -> Self {
        Self::new()
    }
}
