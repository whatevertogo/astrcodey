//! Provider 共享基础设施：HTTP 客户端构建、流式请求重试循环、SSE 行解析。
//!
//! 所有 LLM provider 的 HTTP 流式请求都遵循相同的模式：
//! 构建 client → 带重试的 POST 请求 → 解析 SSE 字节流。
//! 本模块将这一公共骨架提取为泛型函数，各 provider 只需提供
//! SSE 事件处理和请求体构造。

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use astrcode_core::{
    config::ProviderAuthScheme,
    llm::{LlmClientConfig, LlmError, LlmEvent, LlmTokenUsage},
};
use futures_util::StreamExt;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::{
    retry::RetryPolicy,
    stream_decoder::{SseLineReader, StreamDecoderError, Utf8StreamDecoder},
};

fn stream_decoder_error(error: StreamDecoderError) -> LlmError {
    LlmError::StreamParse(error.to_string())
}

pub(crate) fn token_usage_has_value(usage: &LlmTokenUsage) -> bool {
    usage.input_tokens.is_some()
        || usage.cached_input_tokens.is_some()
        || usage.cache_creation_input_tokens.is_some()
        || usage.output_tokens.is_some()
        || usage.reasoning_output_tokens.is_some()
        || usage.total_tokens.is_some()
}

/// SSE 事件回调类型：接收 (event_type, parsed_json, tx)，返回 false 停止处理。
type SseCallback =
    Arc<dyn Fn(&str, &serde_json::Value, &mpsc::UnboundedSender<LlmEvent>) -> bool + Send + Sync>;

/// 根据 `LlmClientConfig` 构建 reqwest client。
///
/// 配置无效时返回 [`LlmError::Transport`]，不在 silently 降级到无 timeout 的默认 client。
pub fn build_client(config: &LlmClientConfig) -> Result<reqwest::Client, LlmError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(config.connect_timeout_secs))
        // reqwest resets read_timeout whenever bytes arrive. Keep this idle-timeout
        // semantic for long-lived SSE streams; a total request timeout would abort
        // healthy model responses that continue producing chunks.
        .read_timeout(Duration::from_secs(config.read_timeout_secs))
        .pool_max_idle_per_host(8)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(30))
        .build()
        .map_err(|error| LlmError::Transport(format!("failed to create HTTP client: {error}")))
}

/// 添加 HTTP 头；若调用方已显式传入同名头（大小写无关）则保留调用方设置。
pub(crate) fn ensure_header(
    headers: &mut Vec<(String, String)>,
    key: &str,
    value: impl Into<String>,
) {
    if headers
        .iter()
        .any(|(existing_key, _)| existing_key.eq_ignore_ascii_case(key))
    {
        return;
    }
    headers.push((key.to_string(), value.into()));
}

/// 根据 provider 的鉴权方案补齐 API key 请求头。
pub(crate) fn apply_auth_header(
    headers: &mut Vec<(String, String)>,
    scheme: ProviderAuthScheme,
    api_key: &str,
) {
    match scheme {
        ProviderAuthScheme::None => {},
        ProviderAuthScheme::Bearer => {
            ensure_header(headers, "Authorization", format!("Bearer {api_key}"));
        },
        ProviderAuthScheme::XApiKey => {
            ensure_header(headers, "x-api-key", api_key);
        },
        ProviderAuthScheme::XGoogApiKey => {
            ensure_header(headers, "x-goog-api-key", api_key);
        },
    }
}

/// 从流式片段中提取应向前端发送的增量文本。
///
/// 部分兼容 provider（如 glm Anthropic/OpenAI 网关）会在 SSE 中发送**累积全文**而非
/// 纯增量；若直接 append 会导致前缀重复。本函数同时兼容纯增量与累积两种格式。
pub fn stream_text_delta(accumulated: &mut String, fragment: &str) -> Option<String> {
    if fragment.is_empty() {
        return None;
    }
    if accumulated.is_empty() {
        accumulated.push_str(fragment);
        return Some(fragment.to_string());
    }
    if fragment.starts_with(accumulated.as_str()) {
        if fragment.len() <= accumulated.len() {
            return None;
        }
        let incremental = fragment[accumulated.len()..].to_string();
        accumulated.clear();
        accumulated.push_str(fragment);
        return Some(incremental);
    }
    if accumulated.starts_with(fragment) {
        return None;
    }
    accumulated.push_str(fragment);
    Some(fragment.to_string())
}

/// 向 LLM 事件通道发送事件；接收端已 drop 时返回 `false`。
pub fn send_event(tx: &mpsc::UnboundedSender<LlmEvent>, event: LlmEvent) -> bool {
    match tx.send(event) {
        Ok(()) => true,
        Err(_) => {
            tracing::debug!("LLM event receiver dropped, stopping stream processing");
            false
        },
    }
}

/// 流式响应的 `Done` 事件守卫，保证至多发送一次 `Done`。
#[derive(Debug, Default)]
pub struct StreamEventSink {
    done_sent: bool,
    usage_reported: bool,
    fallback_call_id: u64,
}

impl StreamEventSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn done_sent(&self) -> bool {
        self.done_sent
    }

    pub fn usage_reported(&self) -> bool {
        self.usage_reported
    }

    pub fn mark_usage_reported(&mut self) {
        self.usage_reported = true;
    }

    pub fn tool_call_id(&mut self, provider_id: Option<&str>) -> String {
        if let Some(id) = provider_id.filter(|id| !id.is_empty()) {
            return id.to_string();
        }
        self.fallback_call_id += 1;
        format!("call_{}", self.fallback_call_id)
    }

    pub fn emit_done(
        &mut self,
        tx: &mpsc::UnboundedSender<LlmEvent>,
        finish_reason: impl Into<String>,
    ) -> bool {
        if self.done_sent {
            return true;
        }
        self.done_sent = true;
        send_event(
            tx,
            LlmEvent::Done {
                finish_reason: finish_reason.into(),
            },
        )
    }

    pub fn ensure_done(&mut self, tx: &mpsc::UnboundedSender<LlmEvent>) -> bool {
        self.emit_done(tx, "stop")
    }
}

/// 跨 SSE 回调共享的 [`StreamEventSink`]，封装 `Arc<Mutex<_>>` 与收尾逻辑。
pub struct SharedStreamSink {
    inner: Arc<Mutex<StreamEventSink>>,
}

impl SharedStreamSink {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(StreamEventSink::new())),
        }
    }

    pub fn with_mut<R>(&self, f: impl FnOnce(&mut StreamEventSink) -> R) -> R {
        let mut guard = self.inner.lock();
        f(&mut guard)
    }

    /// 包装 SSE 事件处理器：自动加锁 sink 后交给 `handler`。
    pub fn wrap<F>(
        &self,
        handler: F,
    ) -> impl Fn(&str, &serde_json::Value, &mpsc::UnboundedSender<LlmEvent>) -> bool
    + Send
    + Sync
    + 'static
    where
        F: Fn(
                &mut StreamEventSink,
                &str,
                &serde_json::Value,
                &mpsc::UnboundedSender<LlmEvent>,
            ) -> bool
            + Send
            + Sync
            + 'static,
    {
        let sink = Arc::clone(&self.inner);
        move |event_type, event, tx| {
            let mut guard = sink.lock();
            handler(&mut guard, event_type, event, tx)
        }
    }

    /// 流结束后统一处理：成功时补发 `Done`，失败时发送 `Error`。
    pub fn finalize(&self, result: Result<(), LlmError>, tx: &mpsc::UnboundedSender<LlmEvent>) {
        if result.is_ok() {
            self.with_mut(|sink| {
                if !sink.done_sent() {
                    sink.ensure_done(tx);
                }
            });
        }
        report_stream_error(result, tx);
    }
}

/// 流式请求失败时向通道发送 `Error` 事件。
pub fn report_stream_error(result: Result<(), LlmError>, tx: &mpsc::UnboundedSender<LlmEvent>) {
    if let Err(error) = result {
        send_event(
            tx,
            LlmEvent::Error {
                message: error.to_string(),
            },
        );
    }
}

/// 从 `LlmClientConfig` 的公共字段构建重试策略。
///
/// 三个 LLM provider 使用相同的重试参数推导逻辑，提取为公共函数避免重复。
pub fn retry_policy_from_config(config: &LlmClientConfig) -> RetryPolicy {
    RetryPolicy {
        max_retries: config.max_retries,
        base_delay_ms: config.retry_base_delay_ms,
        max_delay_ms: crate::retry::DEFAULT_MAX_DELAY_MS,
        max_transport_retries: config.max_retries,
    }
}

// ─── HTTP 重试 + SSE 流解析 ─────────────────────────────────────────────

/// 带重试的 HTTP POST 请求参数。
pub struct HttpPostRequest {
    pub client: reqwest::Client,
    pub endpoint: String,
    pub headers: Vec<(String, String)>,
    pub body: serde_json::Value,
    pub retry: RetryPolicy,
}

impl HttpPostRequest {
    /// 发起带重试的 POST 请求，成功时调用 `on_success` 处理响应流。
    ///
    /// 重试逻辑：
    /// - 传输层错误（DNS/TLS/连接重置）→ 按 `max_transport_retries` 重试
    /// - 可重试 HTTP 状态码（408/429/500/502/503/504）→ 按 `max_retries` 重试
    /// - `on_success` 返回 `Transport` 错误 → 按传输层错误重试
    /// - 其他错误 → 直接返回
    pub async fn run<F, Fut>(&self, mut on_success: F) -> Result<(), LlmError>
    where
        F: FnMut(reqwest::Response) -> Fut,
        Fut: std::future::Future<Output = Result<(), LlmError>>,
    {
        let mut attempt = 0;

        loop {
            attempt += 1;
            let attempt_started = Instant::now();
            let response = match self.send_once().await {
                Ok(response) => {
                    tracing::debug!(
                        endpoint = %redacted_endpoint(&self.endpoint),
                        status = %response.status(),
                        attempt,
                        elapsed_ms = attempt_started.elapsed().as_millis(),
                        "LLM response headers received"
                    );
                    response
                },
                Err(error) => {
                    if self.retry.should_retry_transport(attempt) {
                        let delay = self.retry.delay(attempt);
                        tracing::warn!(
                            "LLM request failed with transport error (attempt {attempt}/{}), \
                             retrying after {}ms: {error}",
                            self.retry.max_transport_retries,
                            delay.as_millis(),
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(error);
                },
            };

            let status = response.status();
            if status.is_success() {
                match on_success(response).await {
                    Ok(()) => return Ok(()),
                    Err(LlmError::Transport(message)) => {
                        if self.retry.should_retry_transport(attempt) {
                            let delay = self.retry.delay(attempt);
                            tracing::warn!(
                                "LLM stream read failed with transport error (attempt \
                                 {attempt}/{}), retrying after {}ms: {message}",
                                self.retry.max_transport_retries,
                                delay.as_millis(),
                            );
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                        return Err(LlmError::Transport(message));
                    },
                    Err(error) => return Err(error),
                }
            }

            if self.retry.should_retry(attempt, status.as_u16()) {
                let delay = self.retry.delay(attempt);
                tracing::warn!(
                    "LLM request failed with {status}, retrying (attempt {attempt}/{}) after {}ms",
                    self.retry.max_retries,
                    delay.as_millis()
                );
                tokio::time::sleep(delay).await;
                continue;
            }

            let text = read_http_error_body(response, &self.endpoint).await;
            return Err(classify_error(status.as_u16(), text));
        }
    }

    /// 发起带重试的 JSON POST 请求，返回 JSON 响应体。
    pub async fn json(&self) -> Result<serde_json::Value, LlmError> {
        let mut attempt = 0;

        loop {
            attempt += 1;
            let response = match self.send_once().await {
                Ok(response) => response,
                Err(error) => {
                    if self.retry.should_retry_transport(attempt) {
                        let delay = self.retry.delay(attempt);
                        tracing::warn!(
                            "LLM JSON request failed with transport error (attempt {attempt}/{}), \
                             retrying after {}ms: {error}",
                            self.retry.max_transport_retries,
                            delay.as_millis(),
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(error);
                },
            };

            let status = response.status();
            if status.is_success() {
                let endpoint = response.url().to_string();
                let text = response
                    .text()
                    .await
                    .map_err(|error| transport_error("read JSON response", &endpoint, error))?;
                return serde_json::from_str(&text).map_err(|error| {
                    LlmError::StreamParse(format!(
                        "failed to parse LLM JSON response from {}: {error}",
                        redacted_endpoint(&endpoint)
                    ))
                });
            }

            if self.retry.should_retry(attempt, status.as_u16()) {
                let delay = self.retry.delay(attempt);
                tracing::warn!(
                    "LLM JSON request failed with {status}, retrying (attempt {attempt}/{}) after \
                     {}ms",
                    self.retry.max_retries,
                    delay.as_millis()
                );
                tokio::time::sleep(delay).await;
                continue;
            }

            let text = read_http_error_body(response, &self.endpoint).await;
            return Err(classify_error(status.as_u16(), text));
        }
    }

    /// 发起带重试的 SSE `data:` 行流式请求。
    ///
    /// `on_data` 在每条成功解析为 JSON 的 `data:` 行到达时被调用，参数为
    /// `(event_type, parsed_json, tx)`。Data-only 模式下 `event_type` 始终为 `""`。
    /// 返回 `false` 表示接收端已关闭，停止处理。
    pub async fn stream_data_lines(
        &self,
        tx: &mpsc::UnboundedSender<LlmEvent>,
        on_data: impl Fn(&str, &serde_json::Value, &mpsc::UnboundedSender<LlmEvent>) -> bool
        + Send
        + Sync
        + 'static,
    ) -> Result<(), LlmError> {
        let tx = tx.clone();
        let on_data: SseCallback = Arc::new(on_data);
        self.run(move |response| parse_sse_bytes(response, tx.clone(), false, Arc::clone(&on_data)))
            .await
    }

    /// 发起带重试的 SSE 流式请求，支持 `event:` + `data:` 行模式（Anthropic 风格）。
    ///
    /// `handle_event` 参数为 `(event_type, parsed_json, tx)`；返回 `false` 表示接收端已关闭。
    pub async fn stream_typed_events(
        &self,
        tx: &mpsc::UnboundedSender<LlmEvent>,
        handle_event: impl Fn(&str, &serde_json::Value, &mpsc::UnboundedSender<LlmEvent>) -> bool
        + Send
        + Sync
        + 'static,
    ) -> Result<(), LlmError> {
        let tx = tx.clone();
        let handle_event: SseCallback = Arc::new(handle_event);
        self.run(move |response| {
            parse_sse_bytes(response, tx.clone(), true, Arc::clone(&handle_event))
        })
        .await
    }

    async fn send_once(&self) -> Result<reqwest::Response, LlmError> {
        let mut req = self
            .client
            .post(&self.endpoint)
            .header("content-type", "application/json");
        for (key, value) in &self.headers {
            req = req.header(key.as_str(), value.as_str());
        }
        req.json(&self.body)
            .send()
            .await
            .map_err(|error| transport_error("send request", &self.endpoint, error))
    }
}

// ─── 便捷入口函数 ──────────────────────────────────────────────────────

/// 发起带重试的 HTTP POST 流式请求，并通过回调解析 SSE 事件。
///
/// `on_data` 会在每条 SSE `data:` 行到达并成功解析为 JSON 后被调用。
/// 对于 Anthropic 风格的 `event:` + `data:` 行，使用 [`stream_with_event_type`]。
pub async fn stream_with_retry(
    client: reqwest::Client,
    endpoint: String,
    headers: Vec<(String, String)>,
    body: serde_json::Value,
    retry: RetryPolicy,
    tx: mpsc::UnboundedSender<LlmEvent>,
    on_data: impl Fn(&str, &serde_json::Value, &mpsc::UnboundedSender<LlmEvent>) -> bool
    + Send
    + Sync
    + 'static,
) -> Result<(), LlmError> {
    HttpPostRequest {
        client,
        endpoint,
        headers,
        body,
        retry,
    }
    .stream_data_lines(&tx, on_data)
    .await
}

/// 发起带重试的 SSE 流式请求，支持 `event:` + `data:` 行模式（Anthropic 风格）。
///
/// `handle_event` 参数为 `(event_type, data_json)`；返回 `false` 表示接收端已关闭。
pub async fn stream_with_event_type(
    client: reqwest::Client,
    endpoint: String,
    headers: Vec<(String, String)>,
    body: serde_json::Value,
    retry: RetryPolicy,
    tx: mpsc::UnboundedSender<LlmEvent>,
    handle_event: impl Fn(&str, &serde_json::Value, &mpsc::UnboundedSender<LlmEvent>) -> bool
    + Send
    + Sync
    + 'static,
) -> Result<(), LlmError> {
    HttpPostRequest {
        client,
        endpoint,
        headers,
        body,
        retry,
    }
    .stream_typed_events(&tx, handle_event)
    .await
}

/// 读取非 2xx 响应体；传输失败时记录并返回空串（仍附带 HTTP 状态码）。
pub async fn read_http_error_body(response: reqwest::Response, endpoint: &str) -> String {
    match response.text().await {
        Ok(text) => text,
        Err(error) => {
            tracing::warn!(
                endpoint = %redacted_endpoint(endpoint),
                error = %error,
                "failed to read LLM error response body"
            );
            String::new()
        },
    }
}

// ─── SSE 字节流解析 ─────────────────────────────────────────────────────

/// 解析 SSE 字节流，提取 `data:` 行并回调处理。
///
/// 统一了 data-only 模式（Gemini/OpenAI）和 event+data 模式（Anthropic）：
/// - `track_event_type = false`：忽略 `event:` 行，回调的 `event_type` 参数始终为 `""`
/// - `track_event_type = true`：跟踪 `event:` 行，回调的 `event_type` 参数为实际值
///
/// `[DONE]` 标记和空的 `data:` 行被静默跳过。
/// 如果响应体非空但未包含任何 `data:` 行，返回 `StreamParse` 错误。
async fn parse_sse_bytes(
    response: reqwest::Response,
    tx: mpsc::UnboundedSender<LlmEvent>,
    track_event_type: bool,
    on_event: SseCallback,
) -> Result<(), LlmError> {
    let mut current_event_type = String::new();
    let mut has_data_line = false;
    let Some(summary) = consume_sse_lines(response, &tx, SseBodyPreview::Capture, |line| {
        process_sse_line(
            line,
            &tx,
            track_event_type,
            &mut current_event_type,
            &mut has_data_line,
            &on_event,
        )
    })
    .await?
    else {
        return Ok(());
    };

    if summary.bytes_read > 0 && !has_data_line {
        return Err(LlmError::StreamParse(format!(
            "LLM returned 200 but response is not valid SSE (no data: lines found). Content-Type: \
             {}, bytes: {}, preview: {}",
            summary.content_type.as_deref().unwrap_or("<missing>"),
            summary.bytes_read,
            truncate_str(&summary.body_preview, 256),
        )));
    }

    Ok(())
}

/// 完整消费一个 SSE 响应后供协议层做收尾校验的传输统计。
pub(crate) struct SseStreamSummary {
    content_type: Option<String>,
    bytes_read: usize,
    body_preview: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseBodyPreview {
    Capture,
    Skip,
}

/// 解码 HTTP 响应并逐行分发 SSE 内容。
///
/// 返回 `None` 表示接收端关闭或回调主动停止，调用方不应继续发送收尾事件。
pub(crate) async fn consume_sse_lines(
    response: reqwest::Response,
    tx: &mpsc::UnboundedSender<LlmEvent>,
    preview: SseBodyPreview,
    mut on_line: impl FnMut(&str) -> bool,
) -> Result<Option<SseStreamSummary>, LlmError> {
    let endpoint = response.url().to_string();
    let status = response.status();
    let content_type = header_value(response.headers(), reqwest::header::CONTENT_TYPE);
    let content_encoding = header_value(response.headers(), reqwest::header::CONTENT_ENCODING);
    let mut stream = response.bytes_stream();
    let mut decoder = Utf8StreamDecoder::new();
    let mut line_reader = SseLineReader::new();
    let mut bytes_read = 0usize;
    let mut body_preview = String::new();
    let stream_started = Instant::now();
    let mut first_chunk_reported = false;

    while let Some(chunk) = stream.next().await {
        if tx.is_closed() {
            return Ok(None);
        }
        let bytes = chunk.map_err(|error| {
            stream_body_error(
                &endpoint,
                status.as_u16(),
                content_type.as_deref(),
                content_encoding.as_deref(),
                bytes_read,
                error,
            )
        })?;
        bytes_read += bytes.len();
        if !first_chunk_reported && !bytes.is_empty() {
            first_chunk_reported = true;
            tracing::debug!(
                endpoint = %redacted_endpoint(&endpoint),
                status = status.as_u16(),
                bytes = bytes.len(),
                elapsed_ms = stream_started.elapsed().as_millis(),
                "LLM stream first bytes received"
            );
        }
        if preview == SseBodyPreview::Capture && body_preview.is_empty() && !bytes.is_empty() {
            body_preview = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]).to_string();
        }
        if let Some(text) = decoder.push(&bytes).map_err(stream_decoder_error)? {
            if !consume_decoded_lines(&mut line_reader, &text, &mut on_line)? {
                return Ok(None);
            }
        }
    }
    if let Some(tail) = decoder.finish() {
        if !consume_decoded_lines(&mut line_reader, &tail, &mut on_line)? {
            return Ok(None);
        }
    }
    if line_reader.flush().is_some_and(|line| !on_line(&line)) {
        return Ok(None);
    }

    Ok(Some(SseStreamSummary {
        content_type,
        bytes_read,
        body_preview,
    }))
}

fn consume_decoded_lines(
    line_reader: &mut SseLineReader,
    text: &str,
    on_line: &mut impl FnMut(&str) -> bool,
) -> Result<bool, LlmError> {
    for line in line_reader.push_chunk(text).map_err(stream_decoder_error)? {
        if !on_line(&line) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 处理单行 SSE 输出。返回 `false` 表示接收端已关闭。
fn process_sse_line(
    line: &str,
    tx: &mpsc::UnboundedSender<LlmEvent>,
    track_event_type: bool,
    current_event_type: &mut String,
    has_data_line: &mut bool,
    on_event: &SseCallback,
) -> bool {
    // event: 行
    if let Some(ev_type) = line.strip_prefix("event:") {
        if track_event_type {
            *current_event_type = ev_type.trim().to_string();
        }
        return true;
    }

    // data: 行
    let Some(data) = line.strip_prefix("data:") else {
        return true;
    };
    *has_data_line = true;
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return true;
    }

    if tx.is_closed() {
        return false;
    }

    if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
        if !on_event(current_event_type, &event, tx) {
            return false;
        }
        current_event_type.clear();
    }
    true
}

// ─── 错误工具函数 ──────────────────────────────────────────────────────

fn truncate_str(s: &str, max_chars: usize) -> &str {
    if s.len() <= max_chars {
        s
    } else {
        let mut boundary = max_chars;
        while !s.is_char_boundary(boundary) {
            boundary -= 1;
        }
        &s[..boundary]
    }
}

pub fn classify_error(status: u16, text: String) -> LlmError {
    if status >= 500 {
        LlmError::ServerError {
            status,
            message: text,
        }
    } else {
        LlmError::ClientError {
            status,
            message: text,
        }
    }
}

pub fn transport_error(stage: &str, endpoint: &str, error: reqwest::Error) -> LlmError {
    let source_chain = error_source_chain(&error);
    let endpoint = redacted_endpoint(endpoint);
    LlmError::Transport(format!(
        "{stage} failed for {endpoint}: {error}{source_chain}"
    ))
}

pub fn stream_body_error(
    endpoint: &str,
    status: u16,
    content_type: Option<&str>,
    content_encoding: Option<&str>,
    bytes_read: usize,
    error: reqwest::Error,
) -> LlmError {
    let source_chain = error_source_chain(&error);
    let endpoint = redacted_endpoint(endpoint);
    LlmError::Transport(format!(
        "read streaming response body failed for {endpoint}: status={status}, content-type={}, \
         content-encoding={}, bytes-read={bytes_read}: {error}{source_chain}",
        content_type.unwrap_or("<missing>"),
        content_encoding.unwrap_or("<missing>"),
    ))
}

fn header_value(
    headers: &reqwest::header::HeaderMap,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn error_source_chain(error: &reqwest::Error) -> String {
    let mut message = String::new();
    let mut source = std::error::Error::source(error);
    while let Some(error) = source {
        message.push_str("; caused by: ");
        message.push_str(&error.to_string());
        source = error.source();
    }
    message
}

pub(crate) fn redacted_endpoint(endpoint: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(endpoint) else {
        return endpoint
            .split_once('?')
            .map(|(base, _)| format!("{base}?<redacted>"))
            .unwrap_or_else(|| endpoint.to_string());
    };
    let Some(query) = url.query() else {
        return url.to_string();
    };
    if query.is_empty() {
        return url.to_string();
    }
    let pairs = url
        .query_pairs()
        .map(|(key, value)| {
            let redacted = is_sensitive_query_key(&key);
            (
                key.into_owned(),
                if redacted {
                    "<redacted>".to_string()
                } else {
                    value.into_owned()
                },
            )
        })
        .collect::<Vec<_>>();
    url.query_pairs_mut().clear().extend_pairs(pairs);
    url.to_string()
}

fn is_sensitive_query_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "key" | "api_key" | "apikey" | "access_token" | "token" | "authorization"
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use astrcode_core::llm::LlmClientConfig;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::mpsc,
    };

    use super::*;

    #[test]
    fn stream_text_delta_handles_cumulative_and_incremental_fragments() {
        let mut accumulated = String::new();
        assert_eq!(
            stream_text_delta(&mut accumulated, "The"),
            Some("The".into())
        );
        assert_eq!(
            stream_text_delta(&mut accumulated, "The user"),
            Some(" user".into())
        );
        assert_eq!(stream_text_delta(&mut accumulated, "The user"), None);
        assert_eq!(
            stream_text_delta(&mut accumulated, " asks"),
            Some(" asks".into())
        );
        assert_eq!(accumulated, "The user asks");
    }

    #[test]
    fn stream_event_sink_emits_done_once() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut sink = StreamEventSink::new();
        assert!(sink.emit_done(&tx, "stop"));
        assert!(sink.emit_done(&tx, "stop"));
        assert!(matches!(
            rx.try_recv().unwrap(),
            LlmEvent::Done { finish_reason } if finish_reason == "stop"
        ));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn typed_sse_event_type_resets_after_each_data_line() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\n\
                      content-type: text/event-stream\r\n\
                      connection: close\r\n\
                      \r\n",
                )
                .await
                .unwrap();
            socket
                .write_all(
                    b"event: ping\n\
                      data: {\"kind\":\"first\"}\n\
                      \n\
                      data: {\"kind\":\"second\"}\n\
                      \n",
                )
                .await
                .unwrap();
        });

        let client = build_client(&LlmClientConfig::default()).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let response = client
            .get(format!("http://{addr}/stream"))
            .send()
            .await
            .unwrap();
        let event_types = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&event_types);
        parse_sse_bytes(
            response,
            tx.clone(),
            true,
            Arc::new(move |event_type, event, _| {
                captured.lock().unwrap().push((
                    event_type.to_string(),
                    event
                        .get("kind")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                ));
                true
            }),
        )
        .await
        .unwrap();
        drop(tx);

        assert_eq!(
            event_types.lock().unwrap().clone(),
            vec![
                ("ping".into(), "first".into()),
                (String::new(), "second".into()),
            ]
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn stream_event_sink_allocates_unique_fallback_call_ids() {
        let mut sink = StreamEventSink::new();
        assert_eq!(sink.tool_call_id(Some("provider-id")), "provider-id");
        assert_eq!(sink.tool_call_id(None), "call_1");
        assert_eq!(sink.tool_call_id(None), "call_2");
    }

    #[test]
    fn transport_errors_redact_sensitive_query_values() {
        let endpoint = redacted_endpoint(
            "https://generativelanguage.googleapis.com/v1/models/m:streamGenerateContent?alt=sse&key=secret",
        );

        assert!(endpoint.contains("alt=sse"));
        assert!(endpoint.contains("key=%3Credacted%3E"));
        assert!(!endpoint.contains("secret"));
    }
}
