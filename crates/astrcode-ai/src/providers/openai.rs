//! OpenAI 兼容的 Chat Completions / Responses 提供商。
//!
//! 泛型参数 `A` 为内容累积器，允许子提供商（如 Kimi）替换流解析逻辑，
//! 同时复用 HTTP 请求构造、SSE 传输、重试等基础设施。

use std::collections::BTreeMap;

use astrcode_core::{config::OpenAiApiMode, llm::*, tool::ToolDefinition};
use tokio::sync::mpsc;

use crate::{
    common::{build_client, read_http_error_body, stream_body_error, transport_error},
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
    emitted_call_id: Option<String>,
    name: Option<String>,
    started: bool,
    pending_arguments: String,
}

#[derive(Debug, Default)]
struct ResponseToolCallPartial {
    call_id: Option<String>,
    name: Option<String>,
    started: bool,
    arguments_delta_seen: bool,
    pending_arguments: String,
}

/// 标准 OpenAI 格式的流累积器。
#[derive(Default)]
pub struct StandardAccumulator {
    text: String,
    tool_calls: BTreeMap<u64, ToolCallPartial>,
    response_tool_items: BTreeMap<String, ResponseToolCallPartial>,
    done_sent: bool,
    cache_usage_reported: bool,
    /// MiniMax reasoning_split 模式下累计的 reasoning 文本，用于 diff 提取增量。
    reasoning_accumulated: String,
}

impl StandardAccumulator {
    pub fn text(&self) -> &str {
        &self.text
    }

    fn ingest_tool_call_like_delta(
        &mut self,
        index: u64,
        id: Option<&str>,
        fallback_id: Option<&str>,
        function: &serde_json::Value,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) {
        let partial = self.tool_calls.entry(index).or_default();
        if let Some(id) = id {
            partial.id = Some(id.to_string());
        } else if partial.id.is_none() {
            if let Some(fallback) = fallback_id {
                partial.id = Some(fallback.to_string());
            }
        }
        if let Some(name) = function["name"].as_str() {
            partial.name = Some(name.to_string());
        }

        let arguments = function.get("arguments").and_then(json_argument_fragment);
        if !partial.started {
            if let Some(name) = partial.name.clone() {
                let call_id = chat_tool_call_id(index, partial);
                partial.emitted_call_id = Some(call_id.clone());
                partial.started = true;
                let _ = tx.send(LlmEvent::ToolCallStart {
                    call_id: call_id.clone(),
                    name,
                    arguments: String::new(),
                });
                if !partial.pending_arguments.is_empty() {
                    let delta = std::mem::take(&mut partial.pending_arguments);
                    let _ = tx.send(LlmEvent::ToolCallDelta {
                        call_id: call_id.clone(),
                        delta,
                    });
                }
            }
        }

        if let Some(arguments) = arguments {
            if arguments.is_empty() {
                return;
            }
            if partial.started {
                let _ = tx.send(LlmEvent::ToolCallDelta {
                    call_id: chat_tool_call_id(index, partial),
                    delta: arguments,
                });
            } else {
                partial.pending_arguments.push_str(&arguments);
            }
        }
    }

    fn ingest_chat_tool_call_delta(
        &mut self,
        index: u64,
        tool_call: &serde_json::Value,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) {
        let id = tool_call["id"].as_str();
        let Some(function) = tool_call.get("function") else {
            return;
        };
        self.ingest_tool_call_like_delta(index, id, None, function, tx);
    }

    fn ingest_legacy_function_call_delta(
        &mut self,
        function_call: &serde_json::Value,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) {
        self.ingest_tool_call_like_delta(0, None, Some("function_call"), function_call, tx);
    }

    fn emit_response_tool_start(
        &mut self,
        item_id: &str,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) -> Option<String> {
        let partial = self
            .response_tool_items
            .entry(item_id.to_string())
            .or_default();
        if partial.started {
            return partial.call_id.clone();
        }
        let name = partial.name.clone()?;
        let call_id = partial
            .call_id
            .clone()
            .unwrap_or_else(|| item_id.to_string());
        partial.started = true;
        let _ = tx.send(LlmEvent::ToolCallStart {
            call_id: call_id.clone(),
            name,
            arguments: String::new(),
        });
        if !partial.pending_arguments.is_empty() {
            let delta = std::mem::take(&mut partial.pending_arguments);
            let _ = tx.send(LlmEvent::ToolCallDelta {
                call_id: call_id.clone(),
                delta,
            });
        }
        Some(call_id)
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
                    if let Some(reasoning) = delta
                        .get("reasoning_content")
                        .or_else(|| delta.get("reasoning"))
                        .or_else(|| delta.get("thinking"))
                        .and_then(|value| value.as_str())
                    {
                        let _ = tx.send(LlmEvent::ThinkingDelta {
                            delta: reasoning.to_string(),
                        });
                    }
                    // MiniMax reasoning_split: reasoning_details[].text is cumulative
                    if let Some(details) = delta.get("reasoning_details").and_then(|v| v.as_array())
                    {
                        let latest = details
                            .iter()
                            .filter_map(|d| d.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("");
                        if latest.len() > self.reasoning_accumulated.len() {
                            let incremental = &latest[self.reasoning_accumulated.len()..];
                            if !incremental.is_empty() {
                                let _ = tx.send(LlmEvent::ThinkingDelta {
                                    delta: incremental.to_string(),
                                });
                            }
                            self.reasoning_accumulated = latest;
                        }
                    }
                    if let Some(tool_calls) = delta["tool_calls"].as_array() {
                        for tc in tool_calls {
                            let idx = tc["index"].as_u64().unwrap_or(0);
                            self.ingest_chat_tool_call_delta(idx, tc, tx);
                        }
                    }
                    if let Some(function_call) = delta.get("function_call") {
                        self.ingest_legacy_function_call_delta(function_call, tx);
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
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    let _ = tx.send(LlmEvent::ThinkingDelta {
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
                    .unwrap_or(item_id.as_str())
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let partial = self.response_tool_items.entry(item_id.clone()).or_default();
                partial.call_id = Some(call_id);
                partial.name = Some(name);
                let item_arguments = item.get("arguments").and_then(json_argument_fragment);
                let started_call_id = self.emit_response_tool_start(&item_id, tx);
                if let (Some(call_id), Some(arguments)) = (started_call_id, item_arguments) {
                    if !arguments.is_empty() {
                        let partial = self.response_tool_items.entry(item_id.clone()).or_default();
                        partial.arguments_delta_seen = true;
                        let _ = tx.send(LlmEvent::ToolCallDelta {
                            call_id,
                            delta: arguments,
                        });
                    }
                }
            },
            "response.function_call_arguments.delta" => {
                let item_id = event["item_id"].as_str().unwrap_or_default();
                if let Some(delta) = event.get("delta").and_then(json_argument_fragment) {
                    if delta.is_empty() {
                        return;
                    }
                    let call_id = self
                        .response_tool_items
                        .get(item_id)
                        .and_then(|p| p.call_id.clone())
                        .unwrap_or_else(|| item_id.to_string());
                    let partial = self
                        .response_tool_items
                        .entry(item_id.to_string())
                        .or_default();
                    partial.arguments_delta_seen = true;
                    if partial.started {
                        let _ = tx.send(LlmEvent::ToolCallDelta { call_id, delta });
                    } else {
                        partial.pending_arguments.push_str(&delta);
                    }
                }
            },
            "response.function_call_arguments.done" => {
                let item_id = event["item_id"].as_str().unwrap_or_default().to_string();
                let partial = self.response_tool_items.entry(item_id.clone()).or_default();
                if let Some(call_id) = event["call_id"].as_str() {
                    partial.call_id = Some(call_id.to_string());
                }
                if let Some(name) = event["name"].as_str() {
                    partial.name = Some(name.to_string());
                }
                let fallback_call_id = partial.call_id.clone().unwrap_or_else(|| item_id.clone());
                let call_id = if partial.started {
                    fallback_call_id
                } else {
                    self.emit_response_tool_start(&item_id, tx)
                        .unwrap_or(fallback_call_id)
                };
                let partial = self.response_tool_items.entry(item_id).or_default();
                if !partial.arguments_delta_seen {
                    if let Some(arguments) = event.get("arguments").and_then(json_argument_fragment)
                    {
                        if arguments.is_empty() {
                            return;
                        }
                        let _ = tx.send(LlmEvent::ToolCallDelta {
                            call_id,
                            delta: arguments,
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
        self.apply_prompt_cache_fields(&mut body, messages, tools);
        if self.config.reasoning_split() {
            body["reasoning_split"] = serde_json::json!(true);
        }

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
        self.apply_prompt_cache_fields(&mut body, messages, tools);

        body
    }

    fn apply_prompt_cache_fields(
        &self,
        body: &mut serde_json::Value,
        messages: &[LlmMessage],
        tools: &[ToolDefinition],
    ) {
        if !self.config.supports_prompt_cache_key() {
            return;
        }

        body["prompt_cache_key"] = serde_json::json!(self.prompt_cache_key(messages, tools));
        if let Some(retention) = self.config.prompt_cache_retention() {
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
            let response = match req.json(&body).send().await {
                Ok(r) => r,
                Err(e) => {
                    if retry.should_retry_transport(attempt) {
                        let delay = retry.delay(attempt);
                        tracing::warn!(
                            "LLM request failed with transport error (attempt {attempt}/{}), \
                             retrying after {}ms: {e}",
                            retry.max_transport_retries,
                            delay.as_millis(),
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(transport_error("send request", &endpoint, e));
                },
            };

            let status = response.status();
            if status.is_success() {
                match Self::parse_stream::<A>(response, api_mode, &tx).await {
                    Ok(()) => return Ok(()),
                    Err(LlmError::Transport(msg)) => {
                        if retry.should_retry_transport(attempt) {
                            let delay = retry.delay(attempt);
                            tracing::warn!(
                                "LLM stream read failed with transport error (attempt \
                                 {attempt}/{}), retrying after {}ms: {msg}",
                                retry.max_transport_retries,
                                delay.as_millis(),
                            );
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                        return Err(LlmError::Transport(msg));
                    },
                    Err(e) => return Err(e),
                }
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

            let text = read_http_error_body(response, &endpoint).await;
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
        let endpoint = response.url().to_string();
        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let content_encoding = response
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let mut stream = response.bytes_stream();
        let mut decoder = Utf8StreamDecoder::new();
        let mut accumulator = ACC::default();
        let mut line_reader = SseLineReader::new();
        let mut bytes_read = 0usize;

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(|e| {
                stream_body_error(
                    &endpoint,
                    status.as_u16(),
                    content_type.as_deref(),
                    content_encoding.as_deref(),
                    bytes_read,
                    e,
                )
            })?;
            bytes_read += bytes.len();
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
            max_transport_retries: self.config.max_retries,
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
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    let Some(after_prefix) = trimmed.strip_prefix("data:") else {
        return;
    };
    let data = after_prefix.trim_start();
    if data == "[DONE]" {
        emit_done_once(accumulator, tx);
        return;
    }

    process_sse_data(data, accumulator, api_mode, tx);
}

fn emit_done_once(accumulator: &mut impl ChatAccumulator, tx: &mpsc::UnboundedSender<LlmEvent>) {
    if accumulator.done_sent() {
        return;
    }
    accumulator.mark_done();
    let _ = tx.send(LlmEvent::Done {
        finish_reason: "stop".into(),
    });
}

fn process_sse_data(
    data: &str,
    accumulator: &mut impl ChatAccumulator,
    api_mode: OpenAiApiMode,
    tx: &mpsc::UnboundedSender<LlmEvent>,
) {
    if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
        ingest_sse_event(&event, accumulator, api_mode, tx);
        return;
    }

    let cleaned: String = data
        .chars()
        .filter(|c| !c.is_control() || c.is_whitespace())
        .collect();
    if cleaned != data {
        if let Ok(event) = serde_json::from_str::<serde_json::Value>(&cleaned) {
            ingest_sse_event(&event, accumulator, api_mode, tx);
            return;
        }
    }

    let api_mode_name = match api_mode {
        OpenAiApiMode::ChatCompletions => "Chat Completions",
        OpenAiApiMode::Responses => "Responses",
    };
    tracing::warn!(
        "Failed to parse {} SSE data: {} bytes, preview: {:?}",
        api_mode_name,
        data.len(),
        &data[..data.len().min(80)]
    );
}

fn ingest_sse_event(
    event: &serde_json::Value,
    accumulator: &mut impl ChatAccumulator,
    api_mode: OpenAiApiMode,
    tx: &mpsc::UnboundedSender<LlmEvent>,
) {
    if emit_stream_error(event, accumulator, tx) {
        return;
    }
    match api_mode {
        OpenAiApiMode::ChatCompletions => accumulator.ingest_chat_completion(event, tx),
        OpenAiApiMode::Responses => accumulator.ingest_responses(event, tx),
    }
}

// ─── 辅助函数 ──────────────────────────────────────────────────────────

fn emit_stream_error(
    event: &serde_json::Value,
    accumulator: &mut impl ChatAccumulator,
    tx: &mpsc::UnboundedSender<LlmEvent>,
) -> bool {
    if !is_stream_error_event(event) {
        return false;
    }

    accumulator.mark_done();
    let _ = tx.send(LlmEvent::Error {
        message: stream_error_message(event).unwrap_or_else(|| event.to_string()),
    });
    true
}

fn is_stream_error_event(event: &serde_json::Value) -> bool {
    event.get("error").is_some_and(|value| !value.is_null())
        || event
            .pointer("/response/error")
            .is_some_and(|value| !value.is_null())
        || event.get("type").and_then(|value| value.as_str()) == Some("error")
        || event.get("type").and_then(|value| value.as_str()) == Some("response.failed")
}

fn stream_error_message(event: &serde_json::Value) -> Option<String> {
    event
        .pointer("/error/message")
        .or_else(|| event.pointer("/response/error/message"))
        .or_else(|| event.get("message"))
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .or_else(|| {
            event
                .get("error")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
}

fn chat_tool_call_id(index: u64, partial: &ToolCallPartial) -> String {
    partial
        .emitted_call_id
        .clone()
        .or_else(|| partial.id.clone())
        .unwrap_or_else(|| index.to_string())
}

fn json_argument_fragment(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(text) => Some(clean_json_fragment(text)),
        other => serde_json::to_string(other).ok(),
    }
}

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
        use astrcode_core::llm::{OpenAiProviderExtras, ProviderExtras};
        let config = LlmClientConfig {
            base_url: "https://api.test/v1".into(),
            api_key: "sk-test".into(),
            extras: ProviderExtras::OpenAi(OpenAiProviderExtras {
                supports_prompt_cache_key: supports_cache_key,
                prompt_cache_retention: supports_cache_key
                    .then_some(PromptCacheRetention::TwentyFourHours),
                ..OpenAiProviderExtras::default()
            }),
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
    fn chat_tool_call_buffers_arguments_until_name_arrives() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        acc.ingest_chat_completion(
            &serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "function": {"arguments": "{\"pattern\""}
                }]}}]
            }),
            &tx,
        );
        acc.ingest_chat_completion(
            &serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "function": {"name": "find", "arguments": ":\"*.rs\"}"}
                }]}}]
            }),
            &tx,
        );

        let events = drain_events(&mut rx);
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallStart { call_id, name, arguments }
            if call_id == "call_1" && name == "find" && arguments.is_empty()
        )));
        let arguments = events
            .into_iter()
            .filter_map(|e| match e {
                LlmEvent::ToolCallDelta { delta, .. } => Some(delta),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(arguments, "{\"pattern\":\"*.rs\"}");
    }

    #[test]
    fn chat_tool_call_accepts_object_arguments_from_compat_providers() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        acc.ingest_chat_completion(
            &serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "function": {"name": "grep", "arguments": {"pattern": "agent"}}
                }]}}]
            }),
            &tx,
        );

        let events = drain_events(&mut rx);
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallStart { name, .. } if name == "grep"
        )));
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallDelta { delta, .. } if delta == "{\"pattern\":\"agent\"}"
        )));
    }

    #[test]
    fn chat_stream_accepts_reasoning_aliases_from_compat_providers() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        acc.ingest_chat_completion(
            &serde_json::json!({
                "choices": [{"delta": {"reasoning": "plan"}}]
            }),
            &tx,
        );
        acc.ingest_chat_completion(
            &serde_json::json!({
                "choices": [{"delta": {"thinking": " more"}}]
            }),
            &tx,
        );

        let reasoning = drain_events(&mut rx)
            .into_iter()
            .filter_map(|event| match event {
                LlmEvent::ThinkingDelta { delta } => Some(delta),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(reasoning, "plan more");
    }

    #[test]
    fn chat_tool_call_keeps_call_id_stable_if_provider_sends_id_late() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        acc.ingest_chat_completion(
            &serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "function": {"name": "find"}
                }]}}]
            }),
            &tx,
        );
        acc.ingest_chat_completion(
            &serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "id": "late_id",
                    "function": {"arguments": "{\"pattern\":\"*.rs\"}"}
                }]}}]
            }),
            &tx,
        );

        let events = drain_events(&mut rx);
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallStart { call_id, .. } if call_id == "0"
        )));
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallDelta { call_id, delta }
            if call_id == "0" && delta == "{\"pattern\":\"*.rs\"}"
        )));
    }

    #[test]
    fn chat_legacy_function_call_streams_arguments() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        acc.ingest_chat_completion(
            &serde_json::json!({
                "choices": [{"delta": {"function_call": {
                    "name": "find",
                    "arguments": "{\"pattern\":\"*.rs\"}"
                }}}]
            }),
            &tx,
        );

        let events = drain_events(&mut rx);
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallStart { call_id, name, arguments }
            if call_id == "function_call" && name == "find" && arguments.is_empty()
        )));
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallDelta { call_id, delta }
            if call_id == "function_call" && delta == "{\"pattern\":\"*.rs\"}"
        )));
    }

    #[test]
    fn streaming_error_payload_emits_error_without_done() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        process_sse_line(
            r#"data: {"error":{"message":"compat provider rejected request"}}"#,
            &mut acc,
            OpenAiApiMode::ChatCompletions,
            &tx,
        );

        let events = drain_events(&mut rx);
        assert!(matches!(
            events.as_slice(),
            [LlmEvent::Error { message }] if message == "compat provider rejected request"
        ));
        assert!(acc.done_sent());
    }

    #[test]
    fn streaming_null_error_payload_is_not_treated_as_error() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        process_sse_line(
            r#"data: {"error":null,"choices":[{"delta":{"content":"ok"}}]}"#,
            &mut acc,
            OpenAiApiMode::ChatCompletions,
            &tx,
        );

        let events = drain_events(&mut rx);
        assert!(matches!(
            events.as_slice(),
            [LlmEvent::ContentDelta { delta }] if delta == "ok"
        ));
        assert!(!acc.done_sent());
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
    fn responses_arguments_delta_before_item_start_is_not_lost() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        acc.ingest_responses(
            &serde_json::json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "i1",
                "delta": "{\"path\":\"Cargo.toml\"}"
            }),
            &tx,
        );
        acc.ingest_responses(
            &serde_json::json!({
                "type": "response.output_item.added",
                "item": {
                    "type": "function_call",
                    "id": "i1",
                    "call_id": "c1",
                    "name": "read"
                }
            }),
            &tx,
        );

        let events = drain_events(&mut rx);
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallStart { call_id, name, .. }
            if call_id == "c1" && name == "read"
        )));
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallDelta { call_id, delta }
            if call_id == "c1" && delta == "{\"path\":\"Cargo.toml\"}"
        )));
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
    fn responses_stream_accepts_reasoning_delta_and_done_marker() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        process_sse_line(
            r#"data: {"type":"response.reasoning_summary_text.delta","delta":"thinking"}"#,
            &mut acc,
            OpenAiApiMode::Responses,
            &tx,
        );
        process_sse_line("data: [DONE]", &mut acc, OpenAiApiMode::Responses, &tx);

        let events = drain_events(&mut rx);
        assert!(matches!(
            events.as_slice(),
            [
                LlmEvent::ThinkingDelta { delta },
                LlmEvent::Done { finish_reason }
            ] if delta == "thinking" && finish_reason == "stop"
        ));
        assert!(acc.done_sent());
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
    fn responses_done_accepts_object_arguments_from_compat_providers() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        acc.ingest_responses(
            &serde_json::json!({
                "type": "response.function_call_arguments.done",
                "item_id": "i1",
                "name": "read",
                "arguments": {"path": "Cargo.toml"}
            }),
            &tx,
        );

        let events = drain_events(&mut rx);
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallDelta { delta, .. } if delta == "{\"path\":\"Cargo.toml\"}"
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
