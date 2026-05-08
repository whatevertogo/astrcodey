//! OpenAI 兼容的 LLM 提供商实现。
//!
//! 支持 Chat Completions API 模式（兼容 DeepSeek / OpenAI 等）。
//! 提供 SSE 流式响应解析、工具调用累积及 OpenAI 提示词缓存键注入。

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

pub struct OpenAiProvider {
    config: LlmClientConfig,
    api_mode: OpenAiApiMode,
    model_id: String,
    model_limits_val: ModelLimits,
    client: reqwest::Client,
}

impl OpenAiProvider {
    /// 为一个已解析的模型配置创建可复用提供者。
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
            model_limits_val: ModelLimits {
                max_input_tokens: context_limit.unwrap_or(65536),
                max_output_tokens: max_tokens.unwrap_or(8192) as usize,
            },
            client,
        }
    }

    /// 根据当前协议形态解析实际请求端点。
    ///
    /// 如果 `base_url` 已经以目标路径结尾（如用户直接配了完整 URL），
    /// 直接使用，避免重复拼接导致 405 错误。
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

    /// 构建最终请求体。
    ///
    /// Chat Completions 和 Responses 的消息 / 工具结构不兼容，
    /// 所以这里只做分发，具体序列化保持在各自函数里。
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

    // ─── Chat Completions ─────────────────────────────────────────────────

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

    // ─── Responses ────────────────────────────────────────────────────

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
}

// ─── LlmProvider 实现 ─────────────────────────────────────────────────

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

// ─── HTTP 流与 SSE 解析 ────────────────────────────────────────────────

impl OpenAiProvider {
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
                return Self::parse_stream(response, api_mode, &tx).await;
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

    async fn parse_stream(
        response: reqwest::Response,
        api_mode: OpenAiApiMode,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) -> Result<(), LlmError> {
        use futures_util::StreamExt;
        let mut stream = response.bytes_stream();
        let mut decoder = Utf8StreamDecoder::new();
        let mut accumulator = LlmAccumulator::new();
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

/// 处理单条 SSE 行。
fn process_sse_line(
    line: &str,
    accumulator: &mut LlmAccumulator,
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
                    accumulator.done_sent = true;
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

// ─── SSE 累积器 ───────────────────────────────────────────────────────

pub struct LlmAccumulator {
    text: String,
    tool_calls: BTreeMap<u64, ToolCallPartial>,
    response_tool_items: BTreeMap<String, ResponseToolCallPartial>,
    done_sent: bool,
    cache_usage_reported: bool,
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
    arguments_delta_seen: bool,
}

impl LlmAccumulator {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            tool_calls: BTreeMap::new(),
            response_tool_items: BTreeMap::new(),
            done_sent: false,
            cache_usage_reported: false,
        }
    }

    pub fn done_sent(&self) -> bool {
        self.done_sent
    }

    pub fn ingest_chat_completion(
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

    pub fn ingest_responses(
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
}

impl Default for LlmAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

fn trace_prompt_cache_usage(event: &serde_json::Value) {
    // 用量统计在不同 API 下位置和字段名不同；这里只做尽力记录，
    // 不把 provider 特有字段提升成公开事件。
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

    fn provider(api_mode: OpenAiApiMode, supports_cache_key: bool) -> OpenAiProvider {
        provider_with_retention(
            api_mode,
            if supports_cache_key {
                Some(PromptCacheRetention::TwentyFourHours)
            } else {
                None
            },
        )
    }

    fn provider_with_retention(
        api_mode: OpenAiApiMode,
        retention: Option<PromptCacheRetention>,
    ) -> OpenAiProvider {
        let config = LlmClientConfig {
            base_url: "https://api.test/v1".into(),
            api_key: "sk-test".into(),
            supports_prompt_cache_key: retention.is_some(),
            prompt_cache_retention: retention,
            ..LlmClientConfig::default()
        };
        OpenAiProvider::new(config, api_mode, "gpt-test".into(), Some(1024), Some(8192))
    }

    fn sample_tool() -> ToolDefinition {
        ToolDefinition {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }),
            origin: ToolOrigin::Builtin,
            execution_mode: ExecutionMode::Parallel,
        }
    }

    #[test]
    fn chat_request_includes_prompt_cache_key_when_supported() {
        let provider = provider(OpenAiApiMode::ChatCompletions, true);
        let messages = vec![
            LlmMessage::system("stable system"),
            LlmMessage::user("hello"),
        ];
        let tools = vec![sample_tool()];

        let body = provider.build_request_body(&messages, &tools);

        assert!(
            body["prompt_cache_key"]
                .as_str()
                .is_some_and(|key| key.starts_with("astrcode-"))
        );
        assert_eq!(body["prompt_cache_retention"], "24h");
    }

    #[test]
    fn responses_request_includes_prompt_cache_key_when_supported() {
        let provider = provider(OpenAiApiMode::Responses, true);
        let messages = vec![
            LlmMessage::system("stable system"),
            LlmMessage::user("hello"),
        ];

        let body = provider.build_request_body(&messages, &[sample_tool()]);

        assert!(body["prompt_cache_key"].as_str().is_some());
        assert_eq!(body["prompt_cache_retention"], "24h");
    }

    #[test]
    fn responses_request_uses_responses_retention_wire_value() {
        let provider = provider_with_retention(
            OpenAiApiMode::Responses,
            Some(PromptCacheRetention::InMemory),
        );
        let body = provider.build_request_body(&[LlmMessage::user("hello")], &[]);

        assert_eq!(body["prompt_cache_retention"], "in-memory");
    }

    #[test]
    fn chat_request_uses_chat_retention_wire_value() {
        let provider = provider_with_retention(
            OpenAiApiMode::ChatCompletions,
            Some(PromptCacheRetention::InMemory),
        );
        let body = provider.build_request_body(&[LlmMessage::user("hello")], &[]);

        assert_eq!(body["prompt_cache_retention"], "in_memory");
    }

    #[test]
    fn responses_request_enables_parallel_tool_calls_when_tools_exist() {
        let provider = provider(OpenAiApiMode::Responses, false);
        let body = provider.build_request_body(&[LlmMessage::user("hello")], &[sample_tool()]);

        assert_eq!(body["parallel_tool_calls"], true);
    }

    #[test]
    fn request_omits_prompt_cache_fields_when_unsupported() {
        let provider = provider(OpenAiApiMode::ChatCompletions, false);
        let messages = vec![
            LlmMessage::system("stable system"),
            LlmMessage::user("hello"),
        ];

        let body = provider.build_request_body(&messages, &[]);

        assert!(body.get("prompt_cache_key").is_none());
        assert!(body.get("prompt_cache_retention").is_none());
    }

    #[test]
    fn prompt_cache_key_ignores_non_system_messages() {
        let provider = provider(OpenAiApiMode::Responses, true);
        let tools = vec![sample_tool()];
        let first = vec![
            LlmMessage::system("stable system"),
            LlmMessage::user("hello"),
        ];
        let second = vec![
            LlmMessage::system("stable system"),
            LlmMessage::user("different user prompt"),
            LlmMessage::assistant("different assistant history"),
        ];

        let first_body = provider.build_request_body(&first, &tools);
        let second_body = provider.build_request_body(&second, &tools);

        assert_eq!(
            first_body["prompt_cache_key"],
            second_body["prompt_cache_key"]
        );
    }

    #[test]
    fn forked_compact_and_main_request_share_prompt_cache_key() {
        let provider = provider(OpenAiApiMode::Responses, true);
        let tools = vec![sample_tool()];
        let main_request = vec![
            LlmMessage::system("stable system"),
            LlmMessage::user("old user"),
            LlmMessage::assistant("old answer"),
            LlmMessage::user("current user"),
        ];
        let forked_compact_request = vec![
            LlmMessage::system("stable system"),
            LlmMessage::user("old user"),
            LlmMessage::assistant("old answer"),
            LlmMessage::user("return a compact <summary>"),
        ];

        let main_body = provider.build_request_body(&main_request, &tools);
        let compact_body = provider.build_request_body(&forked_compact_request, &tools);

        assert_eq!(
            main_body["prompt_cache_key"],
            compact_body["prompt_cache_key"]
        );
    }

    #[test]
    fn prompt_cache_key_changes_when_tools_change() {
        let provider = provider(OpenAiApiMode::Responses, true);
        let messages = vec![
            LlmMessage::system("stable system"),
            LlmMessage::user("hello"),
        ];
        let mut other_tool = sample_tool();
        other_tool.name = "otherTool".into();

        let first_body = provider.build_request_body(&messages, &[sample_tool()]);
        let second_body = provider.build_request_body(&messages, &[other_tool]);

        assert_ne!(
            first_body["prompt_cache_key"],
            second_body["prompt_cache_key"]
        );
    }

    #[test]
    fn responses_done_arguments_are_not_replayed_after_deltas() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut accumulator = LlmAccumulator::new();

        accumulator.ingest_responses(
            &serde_json::json!({
                "type": "response.output_item.added",
                "item": {
                    "type": "function_call",
                    "id": "item_1",
                    "call_id": "call_1",
                    "name": "read"
                }
            }),
            &tx,
        );
        accumulator.ingest_responses(
            &serde_json::json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "item_1",
                "delta": "{\"path\""
            }),
            &tx,
        );
        accumulator.ingest_responses(
            &serde_json::json!({
                "type": "response.function_call_arguments.done",
                "item_id": "item_1",
                "arguments": "{\"path\":\"Cargo.toml\"}"
            }),
            &tx,
        );

        let deltas: Vec<String> = drain_events(&mut rx)
            .into_iter()
            .filter_map(|event| match event {
                LlmEvent::ToolCallDelta { delta, .. } => Some(delta),
                _ => None,
            })
            .collect();

        assert_eq!(deltas, vec!["{\"path\""]);
    }

    #[test]
    fn responses_text_delta_uses_official_type_field() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut accumulator = LlmAccumulator::new();

        accumulator.ingest_responses(
            &serde_json::json!({
                "type": "response.output_text.delta",
                "delta": "hello"
            }),
            &tx,
        );

        let events = drain_events(&mut rx);

        assert!(events.iter().any(|event| matches!(
            event,
            LlmEvent::ContentDelta { delta } if delta == "hello"
        )));
    }

    #[test]
    fn responses_done_arguments_are_used_when_no_deltas_arrived() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut accumulator = LlmAccumulator::new();

        accumulator.ingest_responses(
            &serde_json::json!({
                "type": "response.function_call_arguments.done",
                "item_id": "item_1",
                "name": "read",
                "arguments": "{\"path\":\"Cargo.toml\"}"
            }),
            &tx,
        );

        let events = drain_events(&mut rx);

        assert!(events.iter().any(|event| matches!(
            event,
            LlmEvent::ToolCallStart { call_id, name, .. }
                if call_id == "item_1" && name == "read"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            LlmEvent::ToolCallDelta { call_id, delta }
                if call_id == "item_1" && delta == "{\"path\":\"Cargo.toml\"}"
        )));
    }

    #[test]
    fn responses_completed_emits_done_once() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut accumulator = LlmAccumulator::new();
        let event = serde_json::json!({"type": "response.completed"});

        accumulator.ingest_responses(&event, &tx);
        accumulator.ingest_responses(&event, &tx);

        let done_count = drain_events(&mut rx)
            .into_iter()
            .filter(|event| matches!(event, LlmEvent::Done { .. }))
            .count();

        assert_eq!(done_count, 1);
        assert!(accumulator.done_sent());
    }
}
