//! OpenAI 兼容的 LLM 提供商实现。
//!
//! 支持 Chat Completions API 模式（兼容 DeepSeek / OpenAI 等）。
//! 提供 SSE 流式响应解析、工具调用累积及 OpenAI 提示词缓存键注入。

use std::collections::BTreeMap;

use astrcode_core::{config::OpenAiApiMode, llm::*, tool::ToolDefinition};
use tokio::sync::mpsc;

use crate::retry::RetryPolicy;

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
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(config.connect_timeout_secs))
            .timeout(std::time::Duration::from_secs(config.read_timeout_secs))
            .build()
            .expect("Failed to create HTTP client");

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
            "stream_options": { "include_usage": true },
        });

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
        // 只有明确声明支持的 OpenAI 配置才发送提示词缓存字段；
        // DeepSeek 等 OpenAI 兼容提供者不一定接受这些扩展字段。
        if !self.config.supports_prompt_cache_key {
            return;
        }

        body["prompt_cache_key"] = serde_json::json!(self.prompt_cache_key(messages, tools));
        if let Some(retention) = self.config.prompt_cache_retention {
            body["prompt_cache_retention"] = serde_json::json!(retention.as_wire_value());
        }
    }

    fn prompt_cache_key(&self, messages: &[LlmMessage], tools: &[ToolDefinition]) -> String {
        // 缓存键只包含稳定前缀相关内容：模型、系统提示词、工具结构。
        // 用户消息和历史上下文不参与，避免每轮对话都打散同一个静态前缀缓存。
        let system_text = system_text(messages);
        let tools_json = match self.api_mode {
            OpenAiApiMode::ChatCompletions => tools_to_json(tools),
            OpenAiApiMode::Responses => responses_tools_json(tools),
        };
        let tools_text = serde_json::to_string(&tools_json).unwrap_or_default();
        format!(
            "astrcode-{}",
            stable_hash_hex(&[
                self.model_id.as_str(),
                system_text.as_str(),
                tools_text.as_str()
            ])
        )
    }
}

fn system_text(messages: &[LlmMessage]) -> String {
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

fn stable_hash_hex(parts: &[&str]) -> String {
    // FNV-1a：不引入额外依赖，只需要跨进程稳定，不需要密码学安全。
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

// ─── 工具序列化 ────────────────────────────────────────────────────────

fn tools_to_json(tools: &[ToolDefinition]) -> serde_json::Value {
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

fn responses_tools_json(tools: &[ToolDefinition]) -> serde_json::Value {
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

// ─── 消息序列化 ────────────────────────────────────────────────────────

fn chat_message_to_json(message: &LlmMessage) -> serde_json::Value {
    match message.role {
        LlmRole::Tool => {
            // Chat Completions 要求工具结果作为独立消息发送，
            // 并通过原始 tool_call_id 关联到上一条 assistant 工具调用。
            let Some(LlmContent::ToolResult {
                tool_call_id,
                content,
                ..
            }) = message.content.first()
            else {
                return serde_json::json!({"role": "tool", "tool_call_id": "", "content": ""});
            };
            serde_json::json!({"role": "tool", "tool_call_id": tool_call_id, "content": content})
        },
        LlmRole::Assistant
            if message
                .content
                .iter()
                .any(|c| matches!(c, LlmContent::ToolCall { .. })) =>
        {
            // assistant 的工具调用要和文本内容分开序列化。
            // 后续 agent 追加工具结果时，会用同一组 call_id / arguments 还原关联。
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
            // 有 tool_calls 时 content 设为空字符串而不是 null，
            // DeepSeek 等不认 null content 的 assistant 消息
            serde_json::json!({
                "role": "assistant",
                "content": "",
                "tool_calls": tool_calls
            })
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
            // name 只对 tool 消息有意义，其他角色不发送
            if matches!(message.role, LlmRole::Tool) {
                if let Some(ref name) = message.name {
                    obj["name"] = serde_json::json!(name);
                }
            }
            obj
        },
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
                LlmContent::Image { base64, media_type } => Some(serde_json::json!({
                    "type": "image_url",
                    "image_url": {"url": format!("data:{};base64,{}", media_type, base64)}
                })),
                _ => None,
            })
            .collect(),
    )
}

// ─── Responses 输入项 ─────────────────────────────────────────────────

fn responses_input_items(message: &LlmMessage) -> Vec<serde_json::Value> {
    match message.role {
        LlmRole::User => vec![serde_json::json!({
            "role": "user",
            "content": responses_message_content(&message.content, true)
        })],
        LlmRole::Assistant => {
            // Responses 把 assistant 文本和 function_call 表示为同级输入项，
            // 不能像 Chat Completions 那样塞进一个 assistant 消息的 tool_calls 字段。
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
                } => {
                    // function_call_output 通过 call_id 续接前面的 function_call，
                    // 因此这里必须保持扁平输入项，不能再包一层 role 消息。
                    Some(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": tool_call_id,
                        "output": content
                    }))
                },
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
                LlmContent::Image { base64, media_type } if input => Some(serde_json::json!({
                    "type": "input_image",
                    "image_url": format!("data:{};base64,{}", media_type, base64)
                })),
                _ => None,
            })
            .collect(),
    )
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
        let client = self.client.clone();
        let api_mode = self.api_mode;
        let retry = RetryPolicy {
            max_retries: self.config.max_retries,
            base_delay_ms: self.config.retry_base_delay_ms,
        };

        tokio::spawn(async move {
            let result =
                Self::stream_request(client, endpoint, api_key, body, api_mode, retry, tx.clone())
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
    async fn stream_request(
        client: reqwest::Client,
        endpoint: String,
        api_key: String,
        body: serde_json::Value,
        api_mode: OpenAiApiMode,
        retry: RetryPolicy,
        tx: mpsc::UnboundedSender<LlmEvent>,
    ) -> Result<(), LlmError> {
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
        // 解析器对外只发标准化 LlmEvent；不同 API 的流式细节都收敛在累积器内。
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
                        let data = &line[6..];
                        if data == "[DONE]" {
                            if !accumulator.done_sent() {
                                accumulator.done_sent = true;
                                let _ = tx.send(LlmEvent::Done {
                                    finish_reason: "stop".into(),
                                });
                            }
                            return Ok(());
                        }
                        if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                            accumulator.ingest_chat_completion(&event, tx);
                        } else {
                            tracing::warn!(
                                "Failed to parse SSE data as JSON: {} bytes",
                                data.len()
                            );
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
                            } else {
                                tracing::warn!(
                                    "Failed to parse Responses SSE data: {} bytes",
                                    data.len()
                                );
                            }
                        }
                    }
                },
            }
        }
        if !accumulator.done_sent() {
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "stop".into(),
            });
        }
        Ok(())
    }
}

// ─── SSE 累积器 ───────────────────────────────────────────────────────

pub struct LlmAccumulator {
    text: String,
    // Chat Completions 在真正 id 到达前，只能先用数组下标追踪进行中的工具调用。
    tool_calls: BTreeMap<u64, ToolCallPartial>,
    // Responses 先流出 function_call 输出项，再用 item_id 继续发送参数增量。
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
    arguments_delta_seen: bool,
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

    pub fn done_sent(&self) -> bool {
        self.done_sent
    }

    pub fn ingest_chat_completion(
        &mut self,
        event: &serde_json::Value,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) {
        trace_prompt_cache_usage(event);
        if let Some(choices) = event["choices"].as_array() {
            for choice in choices {
                if let Some(delta) = choice.get("delta") {
                    if let Some(content) = delta["content"].as_str() {
                        self.text.push_str(content);
                        let _ = tx.send(LlmEvent::ContentDelta {
                            delta: content.to_string(),
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
                                // 只有拿到工具名后才发 ToolCallStart；
                                // 某些兼容 API 会晚一点才给出真正的 call id。
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
        trace_prompt_cache_usage(event);
        let Some(event_type) = event["event"].as_str() else {
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
                // 有些 Responses 流会跳过前置 output_item.added，
                // 直接给最终 arguments；此时补发一次 ToolCallStart。
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

impl Default for LlmAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

// ─── UTF-8 解码器 ─────────────────────────────────────────────────────

pub struct Utf8StreamDecoder {
    buffer: Vec<u8>,
}

impl Utf8StreamDecoder {
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// 解码分块字节流，避免把多字节 UTF-8 字符切坏。
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

#[cfg(test)]
mod tests {
    use astrcode_core::tool::ToolOrigin;

    use super::*;

    fn drain_events(rx: &mut mpsc::UnboundedReceiver<LlmEvent>) -> Vec<LlmEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    fn provider(api_mode: OpenAiApiMode, supports_cache_key: bool) -> OpenAiProvider {
        let config = LlmClientConfig {
            base_url: "https://api.test/v1".into(),
            api_key: "sk-test".into(),
            supports_prompt_cache_key: supports_cache_key,
            prompt_cache_retention: if supports_cache_key {
                Some(PromptCacheRetention::TwentyFourHours)
            } else {
                None
            },
            ..LlmClientConfig::default()
        };
        OpenAiProvider::new(config, api_mode, "gpt-test".into(), Some(1024), Some(8192))
    }

    fn sample_tool() -> ToolDefinition {
        ToolDefinition {
            name: "readFile".into(),
            description: "Read a file".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }),
            origin: ToolOrigin::Builtin,
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
    fn responses_done_arguments_are_not_replayed_after_deltas() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut accumulator = LlmAccumulator::new();

        accumulator.ingest_responses(
            &serde_json::json!({
                "event": "response.output_item.added",
                "item": {
                    "type": "function_call",
                    "id": "item_1",
                    "call_id": "call_1",
                    "name": "readFile"
                }
            }),
            &tx,
        );
        accumulator.ingest_responses(
            &serde_json::json!({
                "event": "response.function_call_arguments.delta",
                "item_id": "item_1",
                "delta": "{\"path\""
            }),
            &tx,
        );
        accumulator.ingest_responses(
            &serde_json::json!({
                "event": "response.function_call_arguments.done",
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
    fn responses_done_arguments_are_used_when_no_deltas_arrived() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut accumulator = LlmAccumulator::new();

        accumulator.ingest_responses(
            &serde_json::json!({
                "event": "response.function_call_arguments.done",
                "item_id": "item_1",
                "name": "readFile",
                "arguments": "{\"path\":\"Cargo.toml\"}"
            }),
            &tx,
        );

        let events = drain_events(&mut rx);

        assert!(events.iter().any(|event| matches!(
            event,
            LlmEvent::ToolCallStart { call_id, name, .. }
                if call_id == "item_1" && name == "readFile"
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
        let event = serde_json::json!({"event": "response.completed"});

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
