//! OpenAI 兼容的 LLM 提供商实现。
//!
//! 支持 Chat Completions 和 Responses 两种 API 模式，提供 SSE 流式响应解析、
//! UTF-8 多字节边界安全解码、工具调用累积以及 prompt 缓存断点注入。

use std::{collections::BTreeMap, sync::Mutex};

use astrcode_core::{config::OpenAiApiMode, llm::*, tool::ToolDefinition};
use tokio::sync::mpsc;

use crate::{cache::CacheTracker, retry::RetryPolicy};

/// OpenAI 兼容的 LLM 提供商。
///
/// 封装了与 OpenAI 兼容 API 的通信逻辑，支持 Chat Completions 和 Responses 两种模式，
/// 内置缓存追踪、重试策略和流式响应解析。
pub struct OpenAiProvider {
    /// LLM 客户端配置（API 密钥、超时、重试参数等）
    config: LlmClientConfig,
    /// API 模式：Chat Completions 或 Responses
    api_mode: OpenAiApiMode,
    /// 模型标识符
    model_id: String,
    /// 模型令牌限制
    model_limits_val: ModelLimits,
    /// 跟踪 system prompt 和 tool schema 的缓存状态，用于设置 cache_control 断点
    cache_tracker: Mutex<CacheTracker>,
    /// HTTP 客户端
    client: reqwest::Client,
}

impl OpenAiProvider {
    /// 创建新的 OpenAI 提供商实例。
    ///
    /// # 参数
    /// - `config`: LLM 客户端配置
    /// - `api_mode`: API 模式（Chat Completions 或 Responses）
    /// - `model_id`: 模型标识符
    /// - `max_tokens`: 最大输出令牌数（默认 8192）
    /// - `context_limit`: 上下文窗口大小限制（默认 65536）
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
            cache_tracker: Mutex::new(CacheTracker::new()),
            client,
        }
    }

    /// 根据 API 模式构建请求端点 URL。
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

    /// 根据 API 模式构建请求体。
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

    /// 构建 Chat Completions API 的请求体。
    ///
    /// 将消息和工具定义转换为 OpenAI Chat Completions 格式的 JSON，
    /// 启用流式响应并请求 usage 统计信息。
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

    /// 构建 Responses API 的请求体。
    ///
    /// 将消息拆分为 `instructions`（system prompt）和 `input`（非 system 消息），
    /// 工具定义使用 Responses API 的扁平格式。
    fn build_responses_request_body(
        &self,
        messages: &[LlmMessage],
        tools: &[ToolDefinition],
    ) -> serde_json::Value {
        // 提取所有 system 消息的文本内容作为 instructions
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
        // 非 system 消息作为 input
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

/// 将 LLM 消息转换为 Chat Completions API 格式的 JSON。
///
/// 处理三种情况：
/// - Tool 角色：提取 tool_call_id 和内容
/// - Assistant 且包含 ToolCall：序列化 tool_calls 数组
/// - 其他（System/User/Assistant 纯文本）：标准角色 + 内容格式
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
            // Assistant 消息包含工具调用时，序列化为 tool_calls 数组格式
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

/// 将 LLM 内容列表转换为 Chat Completions API 的 content 字段格式。
///
/// 如果内容中包含图片，则使用多部分数组格式（text + image_url）；
/// 否则合并为纯文本字符串。
fn chat_content_to_json(content: &[LlmContent]) -> serde_json::Value {
    let has_image = content
        .iter()
        .any(|part| matches!(part, LlmContent::Image { .. }));
    if !has_image {
        // 纯文本内容：合并为单个字符串
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

    // 包含图片时使用多部分内容数组格式
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

/// 将 LLM 消息转换为 Responses API 的 input item 格式。
///
/// Responses API 使用不同的消息结构：
/// - User → `{role: "user", content: [...]}`
/// - Assistant → 分离为 assistant 消息 + function_call 项
/// - Tool → `function_call_output` 项
fn responses_input_items(message: &LlmMessage) -> Vec<serde_json::Value> {
    match message.role {
        LlmRole::User => vec![serde_json::json!({
            "role": "user",
            "content": responses_message_content(&message.content, true)
        })],
        LlmRole::Assistant => {
            let mut items = Vec::new();
            // 先添加文本内容部分
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
            // 再添加工具调用部分
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

/// 将 LLM 内容列表转换为 Responses API 的 content 格式。
///
/// Responses API 使用 `input_text`/`output_text`/`input_image` 类型标识，
/// 通过 `input` 参数区分是用户输入还是助手输出。
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
    /// 发送消息到 LLM 并返回流式事件接收器。
    ///
    /// 自动检测 system prompt 和 tool schema 的缓存状态，在缓存命中时
    /// 注入 `cache_control` 断点以复用已缓存的 prompt 前缀，降低成本和延迟。
    ///
    /// # 参数
    /// - `messages`: 对话消息列表
    /// - `tools`: 可用工具定义列表
    ///
    /// # 返回
    /// 流式事件的 `UnboundedReceiver`，调用者通过它接收 `LlmEvent`。
    async fn generate(
        &self,
        messages: Vec<LlmMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();

        // 检测 system prompt 和 tool schema 是否与上次请求相同，
        // 相同则标记 cache_control 断点让 API 提供商复用已缓存的 prompt，降低成本和延迟
        let system_cache_hit = {
            let system_text: String = messages
                .iter()
                .filter(|m| matches!(m.role, LlmRole::System))
                .flat_map(|m| m.content.iter())
                .filter_map(|c| match c {
                    LlmContent::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let mut tracker = self.cache_tracker.lock().unwrap_or_else(|e| e.into_inner());
            tracker.check_system_cache(&system_text)
        };
        let tools_cache_hit = {
            let tools_json = serde_json::to_string(&tools).unwrap_or_default();
            let mut tracker = self.cache_tracker.lock().unwrap_or_else(|e| e.into_inner());
            tracker.check_tool_cache(&tools_json)
        };

        let mut body = self.build_request_body(&messages, &tools);

        // 缓存命中时在 system message 末尾和 tools 列表末尾插入 ephemeral 断点，
        // 指示 API 提供商这些前缀可以复用已缓存的 prompt 前缀
        if matches!(system_cache_hit, crate::cache::CacheStatus::Hit) {
            if let Some(msgs) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
                // 在最后一个 system message 的 content 后追加 cache_control
                for msg in msgs.iter_mut().rev() {
                    let role = msg.get("role").and_then(|r| r.as_str());
                    if role == Some("system") {
                        if let Some(content) = msg.get_mut("content") {
                            *content = serde_json::json!([
                                { "type": "text", "text": content },
                                { "type": "text", "text": "", "cache_control": { "type": "ephemeral" } }
                            ]);
                        }
                        break;
                    }
                }
            }
        }
        if matches!(tools_cache_hit, crate::cache::CacheStatus::Hit) {
            if let Some(tools_arr) = body.get_mut("tools").and_then(|t| t.as_array_mut()) {
                if let Some(last_tool) = tools_arr.last_mut() {
                    last_tool["cache_control"] = serde_json::json!({ "type": "ephemeral" });
                }
            }
        }

        let endpoint = self.endpoint();
        let api_key = self.config.api_key.clone();
        let client = self.client.clone();
        let api_mode = self.api_mode;
        // 从配置读取重试参数，而非硬编码 default 值
        let retry = RetryPolicy {
            max_retries: self.config.max_retries,
            base_delay_ms: self.config.retry_base_delay_ms,
        };

        // 在后台任务中执行流式请求，避免阻塞调用者
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

    /// 返回当前模型的令牌限制。
    fn model_limits(&self) -> ModelLimits {
        self.model_limits_val.clone()
    }
}

impl OpenAiProvider {
    /// 带重试的流式 HTTP 请求。
    ///
    /// 向 LLM API 发送 POST 请求，在遇到可重试错误时按指数退避策略重试。
    /// 成功响应后交给 `parse_stream` 解析 SSE 事件流。
    ///
    /// # 参数
    /// - `client`: HTTP 客户端
    /// - `endpoint`: API 端点 URL
    /// - `api_key`: API 密钥
    /// - `body`: 请求体 JSON
    /// - `api_mode`: API 模式
    /// - `retry`: 重试策略
    /// - `tx`: 事件发送通道
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

            // 判断是否应该重试
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

            // 不可重试的错误，直接返回
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

    /// 解析 SSE 流式响应。
    ///
    /// 从 HTTP 响应中逐块读取字节，经过 UTF-8 解码后按行解析 SSE 事件，
    /// 使用 `LlmAccumulator` 将增量数据累积为完整的 `LlmEvent` 发送给调用者。
    ///
    /// 如果流结束时未收到显式的完成标记，会自动发送 `Done` 事件。
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
                        let data = &line[6..]; // 去掉 "data: " 前缀
                        if data == "[DONE]" {
                            accumulator.done_sent = true;
                            let _ = tx.send(LlmEvent::Done {
                                finish_reason: "stop".into(),
                            });
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
                                    "Failed to parse Responses SSE data as JSON: {} bytes",
                                    data.len()
                                );
                            }
                        }
                    }
                },
            }
        }
        // 如果流结束但未收到显式的完成标记，自动发送 Done 事件
        if !accumulator.done_sent() {
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "stop".into(),
            });
        }
        Ok(())
    }
}

/// 流式 UTF-8 解码器，处理跨块的多字节字符边界分割。
///
/// SSE 流的数据块可能在 UTF-8 多字节字符的中间被截断，
/// 此解码器将不完整的尾部字节暂存到缓冲区，等待下一个块到达后继续解码。
pub struct Utf8StreamDecoder {
    /// 暂存不完整的尾部字节
    buffer: Vec<u8>,
}

impl Utf8StreamDecoder {
    /// 创建新的 UTF-8 流解码器。
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// 解码一个字节块，返回已完成的 UTF-8 字符串。
    ///
    /// 将新字节追加到内部缓冲区后尝试整体解码为 UTF-8：
    /// - 成功：清空缓冲区并返回完整字符串
    /// - 部分成功：返回已有效的部分，保留剩余字节等待下次解码
    /// - 完全无效：当缓冲区超过 4096 字节时丢弃，防止无限增长
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
                    // 部分有效：返回已解码的前缀，保留剩余字节
                    let result = std::str::from_utf8(&self.buffer[..valid_up_to])
                        .unwrap()
                        .to_string();
                    self.buffer = self.buffer[valid_up_to..].to_vec();
                    result
                } else {
                    // 所有字节都是无效 UTF-8 — 丢弃以防止缓冲区无限增长
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

/// SSE 增量数据累积器，将流式增量合并为连贯的输出。
///
/// 维护文本内容、工具调用的中间状态，在收到完整的增量数据后
/// 通过事件通道发送 `LlmEvent` 给调用者。
pub struct LlmAccumulator {
    /// 累积的文本内容
    text: String,
    /// Chat Completions 模式下的工具调用中间状态（按索引组织）
    tool_calls: BTreeMap<u64, ToolCallPartial>,
    /// Responses 模式下的工具调用中间状态（按 item_id 组织）
    response_tool_items: BTreeMap<String, ResponseToolCallPartial>,
    /// 是否已发送 Done 事件
    done_sent: bool,
}

/// Chat Completions 模式下工具调用的中间状态。
///
/// 由于工具调用的 id、name 和 arguments 可能分多个 delta 到达，
/// 需要暂存已收到的部分信息。
#[derive(Debug, Default)]
struct ToolCallPartial {
    /// 工具调用的唯一标识
    id: Option<String>,
    /// 工具名称
    name: Option<String>,
    /// 是否已发送 ToolCallStart 事件
    started: bool,
}

/// Responses 模式下工具调用的中间状态。
#[derive(Debug, Default)]
struct ResponseToolCallPartial {
    /// 工具调用的唯一标识
    call_id: Option<String>,
    /// 工具名称
    name: Option<String>,
    /// 是否已发送 ToolCallStart 事件
    started: bool,
}

impl LlmAccumulator {
    /// 创建新的累积器。
    pub fn new() -> Self {
        Self {
            text: String::new(),
            tool_calls: BTreeMap::new(),
            response_tool_items: BTreeMap::new(),
            done_sent: false,
        }
    }

    /// 是否已为此流发送过 Done 事件。
    pub fn done_sent(&self) -> bool {
        self.done_sent
    }

    /// 处理 Chat Completions API 的 SSE 事件。
    ///
    /// 解析 `choices` 数组中的 `delta`，提取文本增量和工具调用增量，
    /// 并在适当时机发送 `ContentDelta`、`ToolCallStart`、`ToolCallDelta` 和 `Done` 事件。
    pub fn ingest_chat_completion(
        &mut self,
        event: &serde_json::Value,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) {
        if let Some(choices) = event["choices"].as_array() {
            for choice in choices {
                if let Some(delta) = choice.get("delta") {
                    // 文本内容增量
                    if let Some(content) = delta["content"].as_str() {
                        self.text.push_str(content);
                        let _ = tx.send(LlmEvent::ContentDelta {
                            delta: content.to_string(),
                        });
                    }
                    // 工具调用增量
                    if let Some(tool_calls) = delta["tool_calls"].as_array() {
                        for tc in tool_calls {
                            let idx = tc["index"].as_u64().unwrap_or(0);
                            let partial = self.tool_calls.entry(idx).or_default();
                            // 收到工具调用 ID
                            if let Some(id) = tc["id"].as_str() {
                                partial.id = Some(id.to_string());
                            }
                            if let Some(func) = tc.get("function") {
                                // 收到工具名称
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
                                // 收到参数增量
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
                // 收到完成原因
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

    /// 处理 Responses API 的 SSE 事件。
    ///
    /// 解析 Responses API 特有的事件类型（如 `response.output_text.delta`、
    /// `response.output_item.added` 等），提取文本增量和工具调用信息。
    pub fn ingest_responses(
        &mut self,
        event: &serde_json::Value,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) {
        if let Some(event_type) = event["event"].as_str() {
            match event_type {
                // 文本增量输出
                "response.output_text.delta" => {
                    if let Some(delta) = event["delta"].as_str() {
                        let _ = tx.send(LlmEvent::ContentDelta {
                            delta: delta.to_string(),
                        });
                    }
                },
                // 新的输出项添加（可能是工具调用）
                "response.output_item.added" => {
                    let Some(item) = event["item"].as_object() else {
                        return;
                    };
                    // 仅处理 function_call 类型的输出项
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
                    // 记录工具调用的中间状态
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
                // 工具调用参数增量
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
                // 工具调用参数完成
                "response.function_call_arguments.done" => {
                    let item_id = event["item_id"].as_str().unwrap_or_default().to_string();
                    let partial = self.response_tool_items.entry(item_id.clone()).or_default();
                    if let Some(name) = event["name"].as_str() {
                        partial.name = Some(name.to_string());
                    }
                    let call_id = partial.call_id.clone().unwrap_or(item_id);
                    // 如果之前没有发送过 ToolCallStart（例如缺少 output_item.added 事件），在此补发
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
                // 响应完成
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
