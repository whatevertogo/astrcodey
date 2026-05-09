//! OpenAI 兼容的 Chat Completions / Responses 提供商。
//!
//! 泛型参数 `A` 为内容累积器，允许子提供商（如 Kimi）替换流解析逻辑，
//! 同时复用 HTTP 请求构造、SSE 传输、重试等基础设施。

use std::collections::BTreeMap;

use astrcode_core::{config::OpenAiApiMode, llm::*, tool::ToolDefinition};
use tokio::sync::mpsc;

use crate::{
    common::build_client,
    retry::RetryPolicy,
    serialization::{
        chat_message_to_json, prompt_cache_retention_wire_value, responses_input_items,
        responses_tools_json, stable_hash_hex, system_text, tools_to_json,
    },
    stream_decoder::{SseLineReader, Utf8StreamDecoder, clean_json_fragment},
};

// ─── ChatAccumulator trait ──────────────────────────────────────────────

/// Chat Completions / Responses 流的内容累积器。
///
/// 每个提供商可以实现自己的累积策略（标准 OpenAI、Kimi 内联令牌等），
/// HTTP/SSE 基础设施通过此 trait 做静态分发。
pub trait ChatAccumulator: Default + Send + Sync + 'static {
    fn ingest_chat_completion(
        &mut self,
        event: &serde_json::Value,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    );
    fn ingest_responses(&mut self, event: &serde_json::Value, tx: &mpsc::UnboundedSender<LlmEvent>);
    fn done_sent(&self) -> bool;
    fn mark_done(&mut self);
}

// ─── StandardAccumulator ────────────────────────────────────────────────

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
    arguments_delta_seen: bool,
}

/// 标准 OpenAI 格式的流累积器。
#[derive(Default)]
pub struct StandardAccumulator {
    text: String,
    tool_calls: BTreeMap<u64, ToolCallPartial>,
    response_tool_items: BTreeMap<String, ResponseToolCallPartial>,
    done_sent: bool,
    cache_usage_reported: bool,
}

impl StandardAccumulator {
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl ChatAccumulator for StandardAccumulator {
    fn ingest_chat_completion(
        &mut self,
        event: &serde_json::Value,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) {
        if !self.cache_usage_reported {
            trace_prompt_cache_usage(event);
            self.cache_usage_reported = true;
        }
        if let Some(choices) = event["choices"].as_array() {
            for choice in choices {
                if let Some(delta) = choice.get("delta") {
                    if let Some(content) = delta["content"].as_str() {
                        self.text.push_str(content);
                        let _ = tx.send(LlmEvent::ContentDelta {
                            delta: content.to_string(),
                        });
                    }
                    if let Some(reasoning) = delta["reasoning_content"].as_str() {
                        let _ = tx.send(LlmEvent::ThinkingDelta {
                            delta: reasoning.to_string(),
                        });
                    }
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
                                    let cleaned_args = clean_json_fragment(args);
                                    let _ = tx.send(LlmEvent::ToolCallDelta {
                                        call_id,
                                        delta: cleaned_args,
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

    fn ingest_responses(
        &mut self,
        event: &serde_json::Value,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) {
        if !self.cache_usage_reported {
            trace_prompt_cache_usage(event);
            self.cache_usage_reported = true;
        }
        let Some(event_type) = event["type"].as_str() else {
            return;
        };
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
                    .and_then(|p| p.call_id.clone())
                    .unwrap_or_else(|| item_id.to_string());
                if let Some(delta) = event["delta"].as_str() {
                    self.response_tool_items
                        .entry(item_id.to_string())
                        .or_default()
                        .arguments_delta_seen = true;
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
                if !partial.arguments_delta_seen {
                    if let Some(arguments) = event["arguments"].as_str() {
                        let _ = tx.send(LlmEvent::ToolCallDelta {
                            call_id,
                            delta: arguments.to_string(),
                        });
                    }
                }
            },
            "response.completed" if !self.done_sent => {
                self.done_sent = true;
                let _ = tx.send(LlmEvent::Done {
                    finish_reason: "stop".into(),
                });
            },
            _ => {},
        }
    }

    fn done_sent(&self) -> bool {
        self.done_sent
    }

    fn mark_done(&mut self) {
        self.done_sent = true;
    }
}

// ─── OpenAiProvider ─────────────────────────────────────────────────────

pub struct OpenAiProvider<A: ChatAccumulator = StandardAccumulator> {
    config: LlmClientConfig,
    api_mode: OpenAiApiMode,
    model_id: String,
    model_limits_val: ModelLimits,
    client: reqwest::Client,
    _phantom: std::marker::PhantomData<A>,
}

impl<A: ChatAccumulator> OpenAiProvider<A> {
    pub fn new(
        config: LlmClientConfig,
        api_mode: OpenAiApiMode,
        model_id: String,
        max_tokens: Option<u32>,
        context_limit: Option<usize>,
    ) -> Self {
        let client = build_client(&config);
        Self {
            config,
            api_mode,
            model_id,
            _phantom: std::marker::PhantomData,
            model_limits_val: ModelLimits {
                max_input_tokens: context_limit.unwrap_or(65536),
                max_output_tokens: max_tokens.unwrap_or(8192) as usize,
            },
            client,
        }
    }

    fn endpoint(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        match self.api_mode {
            OpenAiApiMode::ChatCompletions => {
                if base.ends_with("/chat/completions") {
                    base.to_string()
                } else {
                    format!("{}/chat/completions", base)
                }
            },
            OpenAiApiMode::Responses => {
                if base.ends_with("/responses") {
                    base.to_string()
                } else {
                    format!("{}/responses", base)
                }
            },
        }
    }

    fn is_official_openai(&self) -> bool {
        reqwest::Url::parse(&self.config.base_url)
            .ok()
            .and_then(|url| {
                url.host_str()
                    .map(|host| host.eq_ignore_ascii_case("api.openai.com"))
            })
            .unwrap_or(false)
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

        let mut body = serde_json::json!({
            "model": self.model_id,
            "messages": messages_json,
            "max_tokens": self.model_limits_val.max_output_tokens,
            "stream": true,
        });
        if self.is_official_openai() {
            body["stream_options"] = serde_json::json!({ "include_usage": true });
        }

        if !tools.is_empty() {
            body["tools"] = tools_to_json(tools);
            body["tool_choice"] = serde_json::json!("auto");
        }
        if let Some(t) = self.config.temperature {
            body["temperature"] = serde_json::json!(t);
        }
        self.apply_prompt_cache_fields(&mut body, messages, tools);

        body
    }

    fn build_responses_request_body(
        &self,
        messages: &[LlmMessage],
        tools: &[ToolDefinition],
    ) -> serde_json::Value {
        let instructions = messages
            .iter()
            .filter(|m| matches!(m.role, LlmRole::System))
            .flat_map(|m| m.content.iter())
            .filter_map(|c| match c {
                LlmContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let input: Vec<serde_json::Value> = messages
            .iter()
            .filter(|m| !matches!(m.role, LlmRole::System))
            .flat_map(responses_input_items)
            .collect();

        let mut body = serde_json::json!({
            "model": self.model_id,
            "instructions": instructions,
            "input": input,
            "max_output_tokens": self.model_limits_val.max_output_tokens,
            "stream": true,
        });

        if !tools.is_empty() {
            body["parallel_tool_calls"] = serde_json::json!(true);
            body["tools"] = responses_tools_json(tools);
        }
        if let Some(t) = self.config.temperature {
            body["temperature"] = serde_json::json!(t);
        }
        self.apply_prompt_cache_fields(&mut body, messages, tools);

        body
    }

    fn apply_prompt_cache_fields(
        &self,
        body: &mut serde_json::Value,
        messages: &[LlmMessage],
        tools: &[ToolDefinition],
    ) {
        if !self.config.supports_prompt_cache_key {
            return;
        }

        body["prompt_cache_key"] = serde_json::json!(self.prompt_cache_key(messages, tools));
        if let Some(retention) = self.config.prompt_cache_retention {
            body["prompt_cache_retention"] =
                serde_json::json!(prompt_cache_retention_wire_value(self.api_mode, retention));
        }
    }

    fn prompt_cache_key(&self, messages: &[LlmMessage], tools: &[ToolDefinition]) -> String {
        let sys = system_text(messages);
        let tools_json = match self.api_mode {
            OpenAiApiMode::ChatCompletions => tools_to_json(tools),
            OpenAiApiMode::Responses => responses_tools_json(tools),
        };
        let tools_text = serde_json::to_string(&tools_json).unwrap_or_default();
        format!(
            "astrcode-{}",
            stable_hash_hex(&[self.model_id.as_str(), sys.as_str(), tools_text.as_str()])
        )
    }

    // ─── HTTP / SSE ─────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    async fn stream_request(
        client: reqwest::Client,
        endpoint: String,
        api_key: String,
        extra_headers: std::collections::HashMap<String, String>,
        body: serde_json::Value,
        api_mode: OpenAiApiMode,
        retry: RetryPolicy,
        tx: mpsc::UnboundedSender<LlmEvent>,
    ) -> Result<(), LlmError> {
        let mut attempt = 0;

        loop {
            attempt += 1;
            let mut req = client
                .post(&endpoint)
                .header("Authorization", format!("Bearer {}", api_key));
            for (key, value) in &extra_headers {
                req = req.header(key.as_str(), value.as_str());
            }
            let response = req
                .json(&body)
                .send()
                .await
                .map_err(|e| LlmError::Transport(e.to_string()))?;

            let status = response.status();
            if status.is_success() {
                return Self::parse_stream::<A>(response, api_mode, &tx).await;
            }

            if retry.should_retry(attempt, status.as_u16()) {
                let delay = retry.delay(attempt);
                tracing::warn!(
                    "LLM request failed with {}, retrying (attempt {}/{}) after {}ms",
                    status,
                    attempt,
                    retry.max_retries,
                    delay.as_millis()
                );
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

    async fn parse_stream<ACC: ChatAccumulator>(
        response: reqwest::Response,
        api_mode: OpenAiApiMode,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) -> Result<(), LlmError> {
        use futures_util::StreamExt;
        let mut stream = response.bytes_stream();
        let mut decoder = Utf8StreamDecoder::new();
        let mut accumulator = ACC::default();
        let mut line_reader = SseLineReader::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(|e| LlmError::Transport(e.to_string()))?;
            if let Some(text) = decoder.push(&bytes) {
                for line in line_reader.push_chunk(&text) {
                    process_sse_line(&line, &mut accumulator, api_mode, tx);
                }
            }
        }
        if let Some(tail_text) = decoder.finish() {
            for line in line_reader.push_chunk(&tail_text) {
                process_sse_line(&line, &mut accumulator, api_mode, tx);
            }
        }
        if let Some(line) = line_reader.flush() {
            process_sse_line(&line, &mut accumulator, api_mode, tx);
        }
        if !accumulator.done_sent() {
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "stop".into(),
            });
        }
        Ok(())
    }
}

// ─── LlmProvider impl ──────────────────────────────────────────────────

#[async_trait::async_trait]
impl<A: ChatAccumulator> LlmProvider for OpenAiProvider<A> {
    async fn generate(
        &self,
        messages: Vec<LlmMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let body = self.build_request_body(&messages, &tools);

        let endpoint = self.endpoint();
        let api_key = self.config.api_key.clone();
        let extra_headers = self.config.extra_headers.clone();
        let client = self.client.clone();
        let api_mode = self.api_mode;
        let retry = RetryPolicy {
            max_retries: self.config.max_retries,
            base_delay_ms: self.config.retry_base_delay_ms,
        };

        tokio::spawn(async move {
            let result = Self::stream_request(
                client,
                endpoint,
                api_key,
                extra_headers,
                body,
                api_mode,
                retry,
                tx.clone(),
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

// ─── SSE 行处理 ─────────────────────────────────────────────────────────

fn process_sse_line(
    line: &str,
    accumulator: &mut impl ChatAccumulator,
    api_mode: OpenAiApiMode,
    tx: &mpsc::UnboundedSender<LlmEvent>,
) {
    match api_mode {
        OpenAiApiMode::ChatCompletions => {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return;
            }
            let Some(after_prefix) = trimmed.strip_prefix("data:") else {
                return;
            };
            let data = after_prefix.trim_start();
            if data == "[DONE]" {
                if !accumulator.done_sent() {
                    accumulator.mark_done();
                    let _ = tx.send(LlmEvent::Done {
                        finish_reason: "stop".into(),
                    });
                }
                return;
            }
            if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                accumulator.ingest_chat_completion(&event, tx);
            } else {
                let cleaned: String = data
                    .chars()
                    .filter(|c| !c.is_control() || c.is_whitespace())
                    .collect();
                if cleaned != data {
                    if let Ok(event) = serde_json::from_str::<serde_json::Value>(&cleaned) {
                        accumulator.ingest_chat_completion(&event, tx);
                        return;
                    }
                }
                tracing::warn!(
                    "Failed to parse SSE data as JSON: {} bytes, preview: {:?}",
                    data.len(),
                    &data[..data.len().min(80)]
                );
            }
        },
        OpenAiApiMode::Responses => {
            if line.is_empty() {
                return;
            }
            if let Some(after_prefix) = line.strip_prefix("data:") {
                let data = after_prefix.trim_start();
                if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                    accumulator.ingest_responses(&event, tx);
                } else {
                    let cleaned: String = data
                        .chars()
                        .filter(|c| !c.is_control() || c.is_whitespace())
                        .collect();
                    if cleaned != data {
                        if let Ok(event) = serde_json::from_str::<serde_json::Value>(&cleaned) {
                            accumulator.ingest_responses(&event, tx);
                            return;
                        }
                    }
                    tracing::warn!(
                        "Failed to parse Responses SSE data: {} bytes, preview: {:?}",
                        data.len(),
                        &data[..data.len().min(80)]
                    );
                }
            }
        },
    }
}

// ─── 辅助函数 ──────────────────────────────────────────────────────────

fn trace_prompt_cache_usage(event: &serde_json::Value) {
    let usage = event
        .get("usage")
        .or_else(|| event.pointer("/response/usage"));
    let Some(usage) = usage else {
        return;
    };

    let prompt_tokens = usage
        .get("prompt_tokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(|v| v.as_u64());
    let cached_tokens = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .or_else(|| usage.pointer("/input_tokens_details/cached_tokens"))
        .and_then(|v| v.as_u64());

    if prompt_tokens.is_some() || cached_tokens.is_some() {
        tracing::debug!(
            ?prompt_tokens,
            ?cached_tokens,
            "LLM prompt cache usage reported"
        );
    }
}

// ─── 便捷类型别名 ──────────────────────────────────────────────────────

/// 标准 OpenAI 提供商。
pub type StandardProvider = OpenAiProvider<StandardAccumulator>;

#[cfg(test)]
mod tests {
    use astrcode_core::{
        config::OpenAiApiMode,
        llm::*,
        tool::{ExecutionMode, ToolDefinition, ToolOrigin},
    };

    use super::*;

    fn drain_events(rx: &mut mpsc::UnboundedReceiver<LlmEvent>) -> Vec<LlmEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    fn provider(api_mode: OpenAiApiMode, supports_cache_key: bool) -> StandardProvider {
        let config = LlmClientConfig {
            base_url: "https://api.test/v1".into(),
            api_key: "sk-test".into(),
            supports_prompt_cache_key: supports_cache_key,
            prompt_cache_retention: supports_cache_key
                .then_some(PromptCacheRetention::TwentyFourHours),
            ..LlmClientConfig::default()
        };
        StandardProvider::new(config, api_mode, "gpt-test".into(), Some(1024), Some(8192))
    }

    fn sample_tool() -> ToolDefinition {
        ToolDefinition {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            origin: ToolOrigin::Builtin,
            execution_mode: ExecutionMode::Parallel,
        }
    }

    #[test]
    fn chat_request_includes_prompt_cache_key() {
        let p = provider(OpenAiApiMode::ChatCompletions, true);
        let body = p.build_request_body(
            &[LlmMessage::system("s"), LlmMessage::user("hi")],
            &[sample_tool()],
        );
        assert!(
            body["prompt_cache_key"]
                .as_str()
                .is_some_and(|k| k.starts_with("astrcode-"))
        );
        assert_eq!(body["prompt_cache_retention"], "24h");
    }

    #[test]
    fn request_omits_prompt_cache_fields_when_unsupported() {
        let p = provider(OpenAiApiMode::ChatCompletions, false);
        let body = p.build_request_body(&[LlmMessage::system("s"), LlmMessage::user("hi")], &[]);
        assert!(body.get("prompt_cache_key").is_none());
    }

    #[test]
    fn cache_key_identical_for_same_system() {
        let p = provider(OpenAiApiMode::Responses, true);
        let t = vec![sample_tool()];
        let a = p.build_request_body(&[LlmMessage::system("s"), LlmMessage::user("a")], &t);
        let b = p.build_request_body(
            &[
                LlmMessage::system("s"),
                LlmMessage::user("b"),
                LlmMessage::assistant("hist"),
            ],
            &t,
        );
        assert_eq!(a["prompt_cache_key"], b["prompt_cache_key"]);
    }

    #[test]
    fn cache_key_differs_when_tools_differ() {
        let p = provider(OpenAiApiMode::Responses, true);
        let messages = [LlmMessage::system("s"), LlmMessage::user("hi")];
        let mut other = sample_tool();
        other.name = "other".into();

        let a = p.build_request_body(&messages, &[sample_tool()]);
        let b = p.build_request_body(&messages, &[other]);
        assert_ne!(a["prompt_cache_key"], b["prompt_cache_key"]);
    }

    #[test]
    fn responses_delta_then_done_does_not_replay_arguments() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        acc.ingest_responses(
            &serde_json::json!({
                "type": "response.output_item.added",
                "item": { "type": "function_call", "id": "i1", "call_id": "c1", "name": "r" }
            }),
            &tx,
        );
        acc.ingest_responses(
            &serde_json::json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "i1", "delta": "{\"path\""
            }),
            &tx,
        );
        acc.ingest_responses(
            &serde_json::json!({
                "type": "response.function_call_arguments.done",
                "item_id": "i1", "arguments": "{\"path\":\"Cargo.toml\"}"
            }),
            &tx,
        );

        let deltas: Vec<_> = drain_events(&mut rx)
            .into_iter()
            .filter_map(|e| match e {
                LlmEvent::ToolCallDelta { delta, .. } => Some(delta),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["{\"path\""]);
    }

    #[test]
    fn responses_text_delta() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();
        acc.ingest_responses(
            &serde_json::json!({"type": "response.output_text.delta", "delta": "hi"}),
            &tx,
        );
        let events = drain_events(&mut rx);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, LlmEvent::ContentDelta { delta } if delta == "hi"))
        );
    }

    #[test]
    fn responses_done_without_deltas_still_emits_arguments() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();
        acc.ingest_responses(
            &serde_json::json!({
                "type": "response.function_call_arguments.done",
                "item_id": "i1", "name": "read", "arguments": "{\"path\":\"Cargo.toml\"}"
            }),
            &tx,
        );
        let events = drain_events(&mut rx);
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallStart { call_id, name, .. }
            if call_id == "i1" && name == "read"
        )));
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallDelta { call_id, delta }
            if call_id == "i1" && delta == "{\"path\":\"Cargo.toml\"}"
        )));
    }

    #[test]
    fn responses_completed_emits_done_once() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();
        let event = serde_json::json!({"type": "response.completed"});
        acc.ingest_responses(&event, &tx);
        acc.ingest_responses(&event, &tx);
        let count = drain_events(&mut rx)
            .into_iter()
            .filter(|e| matches!(e, LlmEvent::Done { .. }))
            .count();
        assert_eq!(count, 1);
        assert!(acc.done_sent());
    }
}
