//! OpenAI-compatible LLM provider implementation.

use astrcode_core::{config::OpenAiApiMode, llm::*, tool::ToolDefinition};
use tokio::sync::mpsc;

use crate::{cache::CacheTracker, retry::RetryPolicy};

/// OpenAI-compatible LLM provider.
pub struct OpenAiProvider {
    config: LlmClientConfig,
    api_mode: OpenAiApiMode,
    model_id: String,
    model_limits_val: ModelLimits,
    cache_tracker: CacheTracker,
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
            cache_tracker: CacheTracker::new(),
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
        let messages_json: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| {
                serde_json::json!({
                    "role": match m.role {
                        LlmRole::System => "system",
                        LlmRole::User => "user",
                        LlmRole::Assistant => "assistant",
                        LlmRole::Tool => "tool",
                    },
                    "content": m.content.iter().map(|c| match c {
                        LlmContent::Text { text } => serde_json::json!({"type": "text", "text": text}),
                        LlmContent::Image { base64, media_type } => serde_json::json!({
                            "type": "image_url",
                            "image_url": {"url": format!("data:{};base64,{}", media_type, base64)}
                        }),
                        LlmContent::ToolCall { call_id, name, arguments } => serde_json::json!({
                            "type": "text",
                            "text": format!("[Tool call {}: {}] {}", call_id, name, arguments)
                        }),
                        LlmContent::ToolResult { tool_call_id, content, is_error } => serde_json::json!({
                            "type": "text",
                            "text": format!("[Tool {}: {}] {}", tool_call_id, if *is_error { "Error" } else { "Result" }, content)
                        }),
                    }).collect::<Vec<_>>(),
                    "name": m.name,
                })
            })
            .collect();

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
                    String::new()
                }
            },
        }
    }
}

/// Accumulates SSE deltas into a coherent output.
pub struct LlmAccumulator {
    text: String,
    current_tool_call_id: Option<String>,
    current_tool_name: Option<String>,
    current_tool_args: String,
}

impl LlmAccumulator {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            current_tool_call_id: None,
            current_tool_name: None,
            current_tool_args: String::new(),
        }
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
                            if let Some(id) = tc["id"].as_str() {
                                self.current_tool_call_id = Some(id.to_string());
                            }
                            if let Some(func) = tc.get("function") {
                                if let Some(name) = func["name"].as_str() {
                                    self.current_tool_name = Some(name.to_string());
                                    let _ = tx.send(LlmEvent::ToolCallStart {
                                        call_id: idx.to_string(),
                                        name: name.to_string(),
                                        arguments: String::new(),
                                    });
                                }
                                if let Some(args) = func["arguments"].as_str() {
                                    self.current_tool_args.push_str(args);
                                    let _ = tx.send(LlmEvent::ToolCallDelta {
                                        call_id: idx.to_string(),
                                        delta: args.to_string(),
                                    });
                                }
                            }
                        }
                    }
                }
                if let Some(finish) = choice["finish_reason"].as_str() {
                    let _ = tx.send(LlmEvent::Done {
                        finish_reason: finish.to_string(),
                    });
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
