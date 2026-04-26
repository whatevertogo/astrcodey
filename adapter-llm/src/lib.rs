//! # LLM 提供者运行时
//!
//! 本 crate 实现了对 OpenAI 家族 LLM API 后端的统一抽象，包括 OpenAI Responses、
//! OpenAI Chat Completions 以及兼容 OpenAI 协议的服务（如 DeepSeek、本地 Ollama/vLLM 等）。
//!
//! ## 架构设计
//!
//! 核心是 [`LlmProvider`] trait，它定义了运行时与 LLM 后端交互的最小契约：
//! - `generate()` 执行一次模型调用，支持流式和非流式两种模式
//! - `model_limits()` 返回模型的上下文窗口和最大输出 token 估算
//!
//! 各提供者实现封装了各自的协议细节，对外暴露统一的接口。
//!
//! ## 流式处理模型
//!
//! 流式响应通过 SSE（Server-Sent Events）协议传输，本 crate 使用 [`LlmAccumulator`]
//! 将增量事件重新组装为完整的 [`LlmOutput`]：
//! 1. HTTP 响应流逐块读取字节
//! 2. 按 SSE 协议解析出事件块（Chat Completions 使用单行 `data: {...}`，Responses 使用
//!    `event:/data:` 事件块）
//! 3. 每个事件通过 [`emit_event`] 同时发送到外部 `EventSink` 和内部累加器
//! 4. 流结束后，累加器输出包含完整文本、工具调用和推理内容的 [`LlmOutput`]
//!
//! ## 容错与重试
//!
//! 所有提供者内置指数退避重试逻辑：
//! - 可重试状态码：408（超时）、429（限流）、5xx（服务器错误）
//! - 传输层错误（DNS 解析失败、连接断开等）也会重试
//! - 重试期间持续监听 [`CancelToken`]，取消请求会立即中断
//! - 最大重试次数由运行时 `LlmClientConfig` 控制（默认 2 次）
//!
//! ## Prompt Caching
//!
//! OpenAI 家族接口依赖自动前缀缓存（prefix caching），不发送额外显式缓存控制头。
//!
//! ## 模块结构
//!
//! - [`openai`] — OpenAI Responses / Chat Completions 实现

use std::{collections::HashMap, time::Duration};

use astrcode_core::{AstrError, CancelToken, ReasoningContent, Result, ToolCallRequest};
use astrcode_runtime_contract::llm::LlmEvent;
use log::warn;
use serde_json::Value;
use tokio::{select, time::sleep};

pub mod cache_tracker;
pub mod openai;

pub use astrcode_runtime_contract::llm::{
    LlmEventSink as EventSink, LlmFinishReason as FinishReason, LlmOutput, LlmProvider, LlmRequest,
    LlmUsage, ModelLimits, PromptCacheBreakReason, PromptCacheDiagnostics,
    PromptCacheGlobalStrategy, PromptCacheHints, PromptLayerFingerprints,
};

// ---------------------------------------------------------------------------
// Structured LLM error types (P4.3)
// ---------------------------------------------------------------------------

/// 结构化的 LLM 错误分类，用于 turn 级别的错误恢复决策。
///
/// 替代原先基于字符串匹配的 `is_prompt_too_long()`，让上层能够
/// 通过类型匹配精确判断错误性质并采取对应恢复策略。
#[derive(Debug, Clone)]
pub enum LlmError {
    /// Prompt 超出模型上下文窗口 (HTTP 400/413)
    PromptTooLong { status: u16, body: String },
    /// 其他不可重试的客户端错误 (4xx, 非 413)
    ClientError { status: u16, body: String },
    /// 服务端错误 (5xx)
    ServerError { status: u16, body: String },
    /// 传输层错误 (DNS 失败、连接断开等)
    Transport(String),
    /// 请求被取消
    Interrupted,
    /// 流解析错误 (SSE 协议解析失败、JSON 无效等)
    StreamParse(String),
}

impl LlmError {
    /// 判断是否为 prompt too long 错误。
    pub fn is_prompt_too_long(&self) -> bool {
        matches!(self, LlmError::PromptTooLong { .. })
    }

    /// 判断是否为可恢复的错误 (prompt too long 可触发 compact).
    pub fn is_recoverable(&self) -> bool {
        self.is_prompt_too_long()
    }
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::PromptTooLong { status, body } => {
                write!(f, "prompt too long (HTTP {status}): {body}")
            },
            LlmError::ClientError { status, body } => {
                write!(f, "client error (HTTP {status}): {body}")
            },
            LlmError::ServerError { status, body } => {
                write!(f, "server error (HTTP {status}): {body}")
            },
            LlmError::Transport(msg) => write!(f, "transport error: {msg}"),
            LlmError::Interrupted => write!(f, "LLM request interrupted"),
            LlmError::StreamParse(msg) => write!(f, "stream parse error: {msg}"),
        }
    }
}

impl From<LlmError> for AstrError {
    fn from(err: LlmError) -> Self {
        match err {
            LlmError::PromptTooLong { status, body } => {
                AstrError::LlmRequestFailed { status, body }
            },
            LlmError::ClientError { status, body } => AstrError::LlmRequestFailed { status, body },
            LlmError::ServerError { status, body } => AstrError::LlmRequestFailed { status, body },
            LlmError::Transport(msg) => AstrError::Network(msg),
            LlmError::Interrupted => AstrError::LlmInterrupted,
            LlmError::StreamParse(msg) => AstrError::LlmStreamError(msg),
        }
    }
}

/// 从 HTTP 响应状态和 body 中分类 LLM 错误。
///
/// 优先匹配 prompt too long 特征，其次按状态码范围分类。
pub fn classify_http_error(status: u16, body: &str) -> LlmError {
    let body_lower = body.to_ascii_lowercase();
    let is_context_exceeded = body_lower.contains("prompt too long")
        || body_lower.contains("context length")
        || body_lower.contains("maximum context")
        || body_lower.contains("too many tokens");

    if is_context_exceeded && matches!(status, 400 | 413) {
        return LlmError::PromptTooLong {
            status,
            body: body.to_string(),
        };
    }

    if status < 500 {
        LlmError::ClientError {
            status,
            body: body.to_string(),
        }
    } else {
        LlmError::ServerError {
            status,
            body: body.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Finish reason (P4.2)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Cancel helper (moved from runtime::cancel)
// ---------------------------------------------------------------------------

/// 轮询取消令牌直到被标记为已取消。
///
/// 用于 `select!` 分支中监听取消信号，每 25ms 检查一次状态。
pub async fn cancelled(cancel: CancelToken) {
    while !cancel.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// ---------------------------------------------------------------------------
// Shared constants & helpers used by all LLM providers
// ---------------------------------------------------------------------------

/// LLM HTTP 客户端配置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LlmClientConfig {
    /// TCP 连接超时。
    pub connect_timeout: Duration,
    /// 读取超时。
    pub read_timeout: Duration,
    /// 最大自动重试次数（瞬态故障）。
    pub max_retries: u32,
    /// 首次重试延迟，后续重试指数退避。
    pub retry_base_delay: Duration,
}

impl Default for LlmClientConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(90),
            max_retries: 2,
            retry_base_delay: Duration::from_millis(250),
        }
    }
}

/// 构建共享超时策略的 HTTP 客户端。
///
/// 不在库层 panic，统一返回 `AstrError` 交由上层决定是降级、重试还是失败。
pub fn build_http_client(config: LlmClientConfig) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(config.connect_timeout)
        .read_timeout(config.read_timeout)
        .build()
        .map_err(|error| {
            AstrError::http_with_source("failed to build shared http client", false, error)
        })
}

/// 判断 HTTP 状态码是否可重试
///
/// 包括 408（超时）、429（限流）、所有 5xx 和网关错误。
pub fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::REQUEST_TIMEOUT
            | reqwest::StatusCode::TOO_MANY_REQUESTS
            | reqwest::StatusCode::BAD_GATEWAY
            | reqwest::StatusCode::SERVICE_UNAVAILABLE
            | reqwest::StatusCode::GATEWAY_TIMEOUT
    ) || status.is_server_error()
}

/// 等待指数退避延迟（或被取消）
pub async fn wait_retry_delay(
    attempt: u32,
    cancel: CancelToken,
    retry_base_delay: Duration,
) -> Result<()> {
    let delay_ms = (retry_base_delay.as_millis().min(u64::MAX as u128) as u64)
        .saturating_mul(1_u64 << attempt);
    select! {
        _ = cancelled(cancel) => Err(AstrError::LlmInterrupted),
        _ = sleep(Duration::from_millis(delay_ms)) => Ok(()),
    }
}

/// 转发事件到外部汇并同时累加到内部。
///
/// 这是流式处理的核心函数：每个事件既发送给外部消费者（用于实时 UI 更新），
/// 也累加到内部状态（用于流结束后组装完整响应）。
pub fn emit_event(event: LlmEvent, accumulator: &mut LlmAccumulator, sink: &EventSink) {
    sink(event.clone());
    accumulator.apply(&event);
}

/// 增量 UTF-8 流式解码器。
///
/// HTTP/SSE 是按字节块返回的，TCP 分片可能把一个多字节字符拆到两个 chunk 里。
/// 如果直接对每个 chunk 调 `from_utf8`，遇到中文等非 ASCII 内容就会误报 UTF-8 错误。
/// 这里保留尾部不完整字节，等下一个 chunk 到达后再继续解码。
#[derive(Debug, Default)]
pub struct Utf8StreamDecoder {
    pending: Vec<u8>,
}

impl Utf8StreamDecoder {
    /// 追加一个新的字节块，并返回当前已经确认完整的 UTF-8 文本。
    pub fn push(&mut self, chunk: &[u8], context: &str) -> Result<Option<String>> {
        if chunk.is_empty() {
            return Ok(None);
        }

        self.pending.extend_from_slice(chunk);
        self.decode_available(context)
    }

    /// 在流结束时刷新尾部缓冲。
    ///
    /// 流结束时也做容错恢复：如果尾部是损坏/不完整 UTF-8，替换为 U+FFFD 并继续。
    /// 这样可以避免单个网关脏字节导致整轮会话失败。
    pub fn finish(&mut self, context: &str) -> Result<Option<String>> {
        if self.pending.is_empty() {
            return Ok(None);
        }

        let mut decoded = String::new();

        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    decoded.push_str(text);
                    self.pending.clear();
                    break;
                },
                Err(error) => {
                    let valid_up_to = error.valid_up_to();
                    if valid_up_to > 0 {
                        let valid_prefix = std::str::from_utf8(&self.pending[..valid_up_to])
                            .expect("valid_up_to should always point to a valid utf-8 prefix");
                        decoded.push_str(valid_prefix);
                    }

                    if let Some(invalid_len) = error.error_len() {
                        warn!(
                            "stream decoder recovered invalid utf-8 sequence at stream end in {}: \
                             valid_up_to={}, invalid_len={}, bytes={}",
                            context,
                            valid_up_to,
                            invalid_len,
                            debug_utf8_bytes(&self.pending, valid_up_to, Some(invalid_len))
                        );
                        decoded.push(char::REPLACEMENT_CHARACTER);
                        self.pending.drain(..valid_up_to + invalid_len);
                        if self.pending.is_empty() {
                            break;
                        }
                    } else {
                        // `error_len == None` 表示尾部是"可能缺失字节"的不完整序列。
                        // 流已经结束，不会再有后续字节，因此直接用替换符收尾并清空缓存。
                        warn!(
                            "stream decoder recovered incomplete utf-8 tail at stream end in {}: \
                             valid_up_to={}, bytes={}",
                            context,
                            valid_up_to,
                            debug_utf8_bytes(&self.pending, valid_up_to, None)
                        );
                        decoded.push(char::REPLACEMENT_CHARACTER);
                        self.pending.clear();
                        break;
                    }
                },
            }
        }

        Ok((!decoded.is_empty()).then_some(decoded))
    }

    fn decode_available(&mut self, context: &str) -> Result<Option<String>> {
        let mut decoded = String::new();

        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    decoded.push_str(text);
                    self.pending.clear();
                    return Ok((!decoded.is_empty()).then_some(decoded));
                },
                Err(error) => {
                    let valid_up_to = error.valid_up_to();
                    if valid_up_to > 0 {
                        let valid_prefix = std::str::from_utf8(&self.pending[..valid_up_to])
                            .expect("valid_up_to should always point to a valid utf-8 prefix");
                        decoded.push_str(valid_prefix);
                    }

                    let Some(invalid_len) = error.error_len() else {
                        if decoded.is_empty() {
                            return Ok(None);
                        }

                        // 只消费已经确认完整的前缀，把尾部不完整字符留给下一个 chunk。
                        let tail = self.pending.split_off(valid_up_to);
                        self.pending = tail;
                        return Ok(Some(decoded));
                    };

                    warn!(
                        "stream decoder recovered invalid utf-8 sequence in {}: valid_up_to={}, \
                         invalid_len={}, bytes={}",
                        context,
                        valid_up_to,
                        invalid_len,
                        debug_utf8_bytes(&self.pending, valid_up_to, Some(invalid_len))
                    );

                    // 某些第三方网关会在 SSE 文本中混入坏字节。这里把坏字节替换为 U+FFFD，
                    // 继续保住整轮输出，而不是因为单个脏字节直接终止会话。
                    decoded.push(char::REPLACEMENT_CHARACTER);
                    self.pending.drain(..valid_up_to + invalid_len);
                    if self.pending.is_empty() {
                        return Ok(Some(decoded));
                    }
                },
            }
        }
    }
}

fn debug_utf8_bytes(bytes: &[u8], valid_up_to: usize, invalid_len: Option<usize>) -> String {
    let start = valid_up_to.saturating_sub(8);
    let end = invalid_len
        .map(|len| (valid_up_to + len + 8).min(bytes.len()))
        .unwrap_or(bytes.len().min(valid_up_to + 8));

    bytes[start..end]
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            let absolute_index = start + index;
            if absolute_index == valid_up_to {
                format!("[{byte:02X}]")
            } else {
                format!("{byte:02X}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Test helpers (shared across provider test modules)
// ---------------------------------------------------------------------------

/// 创建记录所有事件的 EventSink（用于测试）。
///
/// 返回的 sink 会将每个事件追加到提供的 `Mutex<Vec<LlmEvent>>` 中，
/// 方便测试断言验证事件序列。
#[cfg(test)]
pub fn sink_collector(events: std::sync::Arc<std::sync::Mutex<Vec<LlmEvent>>>) -> EventSink {
    std::sync::Arc::new(move |event| {
        events.lock().expect("lock").push(event);
    })
}

#[derive(Default)]
pub struct LlmAccumulator {
    pub content: String,
    thinking: String,
    thinking_signature: Option<String>,
    tool_calls: HashMap<usize, AccToolCall>,
}

#[derive(Default)]
pub struct AccToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl LlmAccumulator {
    pub fn apply(&mut self, event: &LlmEvent) {
        match event {
            LlmEvent::TextDelta(text) => {
                self.content.push_str(text);
            },
            LlmEvent::ThinkingDelta(text) => {
                self.thinking.push_str(text);
            },
            LlmEvent::ThinkingSignature(signature) => {
                self.thinking_signature = Some(signature.clone());
            },
            LlmEvent::StreamRetryStarted { .. } => {},
            LlmEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                let entry = self.tool_calls.entry(*index).or_default();
                if let Some(value) = id {
                    entry.id = value.clone();
                }
                if let Some(value) = name {
                    entry.name = value.clone();
                }
                entry.arguments.push_str(arguments_delta);
            },
        }
    }

    pub fn finish(self) -> LlmOutput {
        let mut entries: Vec<_> = self.tool_calls.into_iter().collect();
        entries.sort_by_key(|(index, _)| *index);

        let tool_calls: Vec<ToolCallRequest> = entries
            .into_iter()
            .map(|(_, call)| {
                let args = match serde_json::from_str(&call.arguments) {
                    Ok(value) => value,
                    Err(error) => {
                        // JSON 解析失败时降级为原始字符串，并记录警告日志
                        // 这通常意味着 LLM 返回了格式错误的工具参数
                        warn!(
                            "failed to parse tool call '{}' arguments as JSON: {}, falling back \
                             to raw string",
                            call.name, error
                        );
                        Value::String(call.arguments)
                    },
                };
                ToolCallRequest {
                    id: call.id,
                    name: call.name,
                    args,
                }
            })
            .collect();

        // 根据是否有工具调用推断 finish_reason（流式路径下 API 不显式返回）
        let finish_reason = if !tool_calls.is_empty() {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        };

        LlmOutput {
            content: self.content,
            tool_calls,
            reasoning: if self.thinking.is_empty() {
                None
            } else {
                Some(ReasoningContent {
                    content: self.thinking,
                    signature: self.thinking_signature,
                })
            },
            usage: None,
            finish_reason,
            prompt_cache_diagnostics: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn accumulator_handles_text_thinking_and_tool_calls() {
        let mut acc = LlmAccumulator::default();

        acc.apply(&LlmEvent::TextDelta("Hel".to_string()));
        acc.apply(&LlmEvent::TextDelta("lo".to_string()));
        acc.apply(&LlmEvent::ThinkingDelta("reasoning".to_string()));
        acc.apply(&LlmEvent::ThinkingSignature("sig".to_string()));
        acc.apply(&LlmEvent::ToolCallDelta {
            index: 1,
            id: Some("call_1".to_string()),
            name: Some("search".to_string()),
            arguments_delta: "{\"q\":\"hello\"}".to_string(),
        });
        acc.apply(&LlmEvent::ToolCallDelta {
            index: 0,
            id: Some("call_0".to_string()),
            name: Some("other".to_string()),
            arguments_delta: "{\"a\":1}".to_string(),
        });

        let output = acc.finish();
        assert_eq!(output.content, "Hello");
        assert_eq!(
            output.reasoning.as_ref().map(|r| r.content.as_str()),
            Some("reasoning")
        );
        assert_eq!(
            output
                .reasoning
                .as_ref()
                .and_then(|r| r.signature.as_deref()),
            Some("sig")
        );
        assert_eq!(output.tool_calls.len(), 2);
        assert_eq!(output.tool_calls[0].id, "call_0");
        assert_eq!(output.tool_calls[0].args, json!({ "a": 1 }));
        assert_eq!(output.tool_calls[1].id, "call_1");
        assert_eq!(output.tool_calls[1].args, json!({ "q": "hello" }));
    }

    // -----------------------------------------------------------------------
    // P4.3: LlmError classification tests
    // -----------------------------------------------------------------------

    #[test]
    fn llm_error_detects_prompt_too_long_413() {
        let error = classify_http_error(413, "prompt too long for this model");
        assert!(error.is_prompt_too_long());
        assert!(error.is_recoverable());
    }

    #[test]
    fn llm_error_detects_prompt_too_long_400() {
        let error = classify_http_error(400, "context length exceeded");
        assert!(error.is_prompt_too_long());
        assert!(error.is_recoverable());
    }

    #[test]
    fn llm_error_detects_maximum_context() {
        let error = classify_http_error(413, "maximum context length reached");
        assert!(error.is_prompt_too_long());
    }

    #[test]
    fn llm_error_detects_too_many_tokens() {
        let error = classify_http_error(400, "too many tokens in request");
        assert!(error.is_prompt_too_long());
    }

    #[test]
    fn llm_error_classifies_client_errors() {
        let error = classify_http_error(401, "invalid api key");
        assert!(!error.is_prompt_too_long());
        assert!(!error.is_recoverable());
        matches!(error, LlmError::ClientError { status: 401, .. });
    }

    #[test]
    fn llm_error_classifies_server_errors() {
        let error = classify_http_error(500, "internal server error");
        assert!(!error.is_prompt_too_long());
        assert!(!error.is_recoverable());
        matches!(error, LlmError::ServerError { status: 500, .. });
    }

    #[test]
    fn llm_error_display_formats_correctly() {
        let error = LlmError::PromptTooLong {
            status: 413,
            body: "prompt too long".to_string(),
        };
        let display = format!("{error}");
        assert!(display.contains("413"));
        assert!(display.contains("prompt too long"));
    }

    #[test]
    fn llm_error_converts_to_astr_error() {
        let llm_error = LlmError::PromptTooLong {
            status: 413,
            body: "context length exceeded".to_string(),
        };
        let astr_error: AstrError = llm_error.into();
        matches!(astr_error, AstrError::LlmRequestFailed { status: 413, .. });
    }

    // -----------------------------------------------------------------------
    // P4.2: FinishReason tests
    // -----------------------------------------------------------------------

    #[test]
    fn finish_reason_parses_openai_values() {
        assert_eq!(FinishReason::from_api_value("stop"), FinishReason::Stop);
        assert_eq!(
            FinishReason::from_api_value("max_tokens"),
            FinishReason::MaxTokens
        );
        assert_eq!(
            FinishReason::from_api_value("tool_calls"),
            FinishReason::ToolCalls
        );
        assert_eq!(
            FinishReason::from_api_value("length"),
            FinishReason::MaxTokens
        );
        assert_eq!(
            FinishReason::from_api_value("content_filter"),
            FinishReason::Other("content_filter".to_string())
        );
    }

    #[test]
    fn finish_reason_is_max_tokens_detects_correctly() {
        assert!(FinishReason::MaxTokens.is_max_tokens());
        assert!(!FinishReason::Stop.is_max_tokens());
        assert!(!FinishReason::ToolCalls.is_max_tokens());
    }

    #[test]
    fn utf8_stream_decoder_handles_multibyte_char_split_across_chunks() {
        let mut decoder = Utf8StreamDecoder::default();
        let bytes = "你好".as_bytes();

        let first = decoder
            .push(&bytes[..4], "test utf-8 stream")
            .expect("first chunk should parse");
        let second = decoder
            .push(&bytes[4..], "test utf-8 stream")
            .expect("second chunk should parse");
        let tail = decoder
            .finish("test utf-8 stream")
            .expect("finish should parse");

        assert_eq!(first.as_deref(), Some("你"));
        assert_eq!(second.as_deref(), Some("好"));
        assert_eq!(tail, None);
    }

    #[test]
    fn utf8_stream_decoder_rejects_invalid_utf8_sequences() {
        let mut decoder = Utf8StreamDecoder::default();
        let decoded = decoder
            .push(&[0xFF], "test utf-8 stream")
            .expect("invalid utf-8 should be recovered");

        assert_eq!(decoded.as_deref(), Some("\u{FFFD}"));
    }

    #[test]
    fn utf8_stream_decoder_keeps_valid_suffix_after_invalid_bytes() {
        let mut decoder = Utf8StreamDecoder::default();
        let decoded = decoder
            .push(&[b'a', 0xFF, b'b'], "test utf-8 stream")
            .expect("invalid utf-8 should be recovered");

        assert_eq!(decoded.as_deref(), Some("a\u{FFFD}b"));
    }

    #[test]
    fn utf8_stream_decoder_finish_recovers_incomplete_trailing_sequence() {
        let mut decoder = Utf8StreamDecoder::default();
        let first = decoder
            .push(&[b'a', 0xE4, 0xBD], "test utf-8 stream")
            .expect("partial utf-8 should be buffered");
        assert_eq!(first.as_deref(), Some("a"));

        let tail = decoder
            .finish("test utf-8 stream")
            .expect("finish should recover incomplete trailing utf-8");

        assert_eq!(tail.as_deref(), Some("\u{FFFD}"));
    }

    #[test]
    fn debug_utf8_bytes_marks_failure_boundary() {
        let snippet = debug_utf8_bytes(&[0x61, 0x62, 0xFF, 0x63], 2, Some(1));
        assert_eq!(snippet, "61 62 [FF] 63");
    }
}
