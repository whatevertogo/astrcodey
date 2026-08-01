//! Provider 共享基础设施：HTTP 客户端构建、流式请求重试循环、SSE 行解析。
//!
//! 所有 LLM provider 的 HTTP 流式请求都遵循相同的模式：
//! 构建 client → 带重试的 POST 请求 → 解析 SSE 字节流。
//! 本模块将这一公共骨架提取为泛型函数，各 provider 只需提供
//! SSE 事件处理和请求体构造。

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use astrcode_core::{
    config::ProviderAuthScheme,
    llm::{LlmClientConfig, LlmError, LlmEvent, LlmTokenUsage},
};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::{
    retry::RetryPolicy,
    stream_decoder::{SseLineReader, StreamDecoderError, Utf8StreamDecoder},
};

fn stream_decoder_error(error: StreamDecoderError) -> LlmError {
    LlmError::stream_parse(error.to_string())
}

pub(crate) fn token_usage_has_value(usage: &LlmTokenUsage) -> bool {
    usage.input_tokens.is_some()
        || usage.cached_input_tokens.is_some()
        || usage.cache_creation_input_tokens.is_some()
        || usage.output_tokens.is_some()
        || usage.reasoning_output_tokens.is_some()
        || usage.total_tokens.is_some()
}

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
        .map_err(|error| LlmError::transport(format!("failed to create HTTP client: {error}")))
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
fn apply_auth_header(
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
    }
}

/// 构建 provider 基础请求头：用户自定义头 + 鉴权头。
///
/// 三个 provider 的流式与 count_tokens 路径共用同一套基础头，各路径再按需追加协议头
/// （如 `Accept: text/event-stream`、`anthropic-version`）。
pub(crate) fn base_headers(config: &LlmClientConfig) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = config
        .extra_headers
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    apply_auth_header(&mut headers, config.auth_scheme, &config.api_key);
    headers
}

/// 从流式片段中提取应向前端发送的增量文本。
///
/// 部分兼容 provider（如 glm Anthropic/OpenAI 网关）会在 SSE 中发送**累积全文**而非
/// 纯增量；若直接 append 会导致前缀重复。本函数同时兼容纯增量与累积两种格式。
///
/// 代价：判定累积前缀需 `fragment.starts_with(accumulated)`，单次为 O(accumulated.len())。
/// 对持续发送累积全文的 provider，长流（尤其长 reasoning）整体为 O(N²)。这是为兼容累积流
/// 而接受的已知成本；若后续 profiling 表明成为瓶颈，可在确认流为纯增量后跳过前缀检查。
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
    /// `stream_started` 由 `on_success` 在开始消费响应体（收到任何 SSE 行）时置位；
    /// 已置位后传输层错误不再重试，避免已投递事件重复。
    ///
    /// 重试逻辑：
    /// - 传输层错误（DNS/TLS/连接重置）→ 按 `max_transport_retries` 重试
    /// - 可重试 HTTP 状态码（408/429/500/502/503/504）→ 按 `max_retries` 重试
    /// - `on_success` 返回 `Transport` 错误且流尚未开始消费 → 按传输层错误重试
    /// - 其他错误 → 直接返回
    pub async fn run<F, Fut>(
        &self,
        stream_started: &AtomicBool,
        mut on_success: F,
    ) -> Result<(), LlmError>
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
                    Err(error) => {
                        if !stream_started.load(Ordering::SeqCst)
                            && self.retry.should_retry_transport(attempt)
                        {
                            if let LlmError::Transport { message } = &error {
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
                        }
                        return Err(error);
                    },
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

            let retry_after_ms = parse_retry_after_ms(response.headers());
            let text = read_http_error_body(response, &self.endpoint).await;
            return Err(classify_error(status.as_u16(), retry_after_ms, &text));
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
                    LlmError::stream_parse(format!(
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

            let retry_after_ms = parse_retry_after_ms(response.headers());
            let text = read_http_error_body(response, &self.endpoint).await;
            return Err(classify_error(status.as_u16(), retry_after_ms, &text));
        }
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

/// 完整消费一个 SSE 响应后供协议层做收尾校验的传输统计。
pub(crate) struct SseStreamSummary {
    content_type: Option<String>,
    bytes_read: usize,
    body_preview: String,
}

impl SseStreamSummary {
    /// 响应体非空却没有任何 `data:` 行 → 视为非 SSE，返回结构化错误。
    pub(crate) fn require_data_lines(&self, has_data_line: bool) -> Result<(), LlmError> {
        if self.bytes_read > 0 && !has_data_line {
            return Err(LlmError::stream_parse(format!(
                "LLM returned 200 but response is not valid SSE (no data: lines found). \
                 Content-Type: {}, bytes: {}, preview: {}",
                self.content_type.as_deref().unwrap_or("<missing>"),
                self.bytes_read,
                &self.body_preview[..self.body_preview.floor_char_boundary(256)],
            )));
        }
        Ok(())
    }
}

/// 解码 HTTP 响应并逐行分发 SSE 内容。
///
/// 返回 `None` 表示接收端关闭或回调主动停止，调用方不应继续发送收尾事件。
pub(crate) async fn consume_sse_lines(
    response: reqwest::Response,
    tx: &mpsc::UnboundedSender<LlmEvent>,
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
        if body_preview.is_empty() && !bytes.is_empty() {
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

// ─── 错误工具函数 ──────────────────────────────────────────────────────

/// 将 HTTP 状态码与错误响应体归一化为 [`LlmError`]。
///
/// 4xx/5xx 的细分(鉴权/模型/参数/配额/限流/上下文溢出/内容过滤)依据状态码与
/// 错误体关键词,与 vbot 的 provider 分类一致;`retry_after_ms` 来自 `Retry-After`
/// 响应头(仅 429 限流时携带)。该分类与 [`LlmError::is_retryable`] 共同构成重试决策。
pub fn classify_error(status: u16, retry_after_ms: Option<u64>, body: &str) -> LlmError {
    let parsed = serde_json::from_str::<serde_json::Value>(body).ok();
    let error_value = parsed
        .as_ref()
        .and_then(|value| value.get("error"))
        .or(parsed.as_ref());
    let field = |key: &str| {
        error_value
            .and_then(|error| error.get(key))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let message = field("message").unwrap_or_else(|| body.to_string());
    let code = field("code").or_else(|| field("type")).unwrap_or_default();
    let haystack = format!("{code} {message}").to_ascii_lowercase();

    match status {
        401 | 403 => LlmError::InvalidApiKey { status, message },
        404 => LlmError::ModelNotFound { status, message },
        400 if haystack.contains("context window")
            || haystack.contains("context length")
            || haystack.contains("context_length")
            || haystack.contains("prompt too long")
            || haystack.contains("maximum context") =>
        {
            LlmError::ContextWindowExceeded { message }
        },
        400 | 422 if haystack.contains("content_filter") || haystack.contains("content filter") => {
            LlmError::ContentFilter { message }
        },
        400 | 422 => LlmError::InvalidParameter { status, message },
        429 if haystack.contains("quota") || haystack.contains("billing") => {
            LlmError::QuotaExceeded { status, message }
        },
        402 => LlmError::QuotaExceeded { status, message },
        429 => LlmError::RateLimited {
            status,
            retry_after_ms,
            message,
        },
        status if (500..600).contains(&status) || status == 408 => {
            LlmError::ServerError { status, message }
        },
        _ => LlmError::ClientError { status, message },
    }
}

/// 从 `Retry-After` 响应头解析退避毫秒数。仅支持 delta-seconds 形式(常见于 LLM API)。
pub fn parse_retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .and_then(|seconds| seconds.checked_mul(1000))
}

pub fn transport_error(stage: &str, endpoint: &str, error: reqwest::Error) -> LlmError {
    let source_chain = error_source_chain(&error);
    let endpoint = redacted_endpoint(endpoint);
    LlmError::transport(format!(
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
    LlmError::transport(format!(
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

/// 极简 HTTP 测试服务器（仅测试用）：每收到一个请求调用 `respond(请求序号)` 并写入响应。
#[cfg(test)]
pub(crate) async fn spawn_test_server(
    respond: impl Fn(u32) -> Vec<u8> + Send + Sync + 'static,
) -> (String, std::sync::Arc<std::sync::atomic::AtomicU32>) {
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let requests = std::sync::Arc::new(AtomicU32::new(0));
    let counter = requests.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let count = counter.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            let body = respond(count);
            tokio::spawn(async move {
                let mut buf = [0_u8; 2048];
                let _ = socket.read(&mut buf).await;
                let _ = socket.write_all(&body).await;
            });
        }
    });
    (format!("http://{addr}"), requests)
}

#[cfg(test)]
mod tests {
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
    fn transport_errors_redact_sensitive_query_values() {
        let endpoint = redacted_endpoint("https://api.example.com/v1/models/m?alt=sse&key=secret");

        assert!(endpoint.contains("alt=sse"));
        assert!(endpoint.contains("key=%3Credacted%3E"));
        assert!(!endpoint.contains("secret"));
    }

    #[test]
    fn classify_error_maps_status_and_body_to_typed_variants() {
        use astrcode_core::llm::LlmError;

        // 鉴权 / 模型 / 参数 / 配额
        assert!(matches!(
            classify_error(401, None, r#"{"error":{"message":"bad key"}}"#),
            LlmError::InvalidApiKey { status: 401, .. }
        ));
        assert!(matches!(
            classify_error(403, None, "forbidden"),
            LlmError::InvalidApiKey { status: 403, .. }
        ));
        assert!(matches!(
            classify_error(404, None, "no model"),
            LlmError::ModelNotFound { status: 404, .. }
        ));
        assert!(matches!(
            classify_error(402, None, "no funds"),
            LlmError::QuotaExceeded { status: 402, .. }
        ));
        assert!(matches!(
            classify_error(400, None, r#"{"error":{"message":"bad shape"}}"#),
            LlmError::InvalidParameter { status: 400, .. }
        ));

        // 上下文溢出(关键词命中 → ContextWindowExceeded,触发压缩路径)
        assert!(matches!(
            classify_error(
                400,
                None,
                r#"{"error":{"message":"This model's maximum context length is exceeded"}}"#
            ),
            LlmError::ContextWindowExceeded { .. }
        ));
        assert!(matches!(
            classify_error(400, None, "prompt too long"),
            LlmError::ContextWindowExceeded { .. }
        ));

        // 内容过滤
        assert!(matches!(
            classify_error(
                400,
                None,
                r#"{"error":{"code":"content_filter","message":"blocked"}}"#
            ),
            LlmError::ContentFilter { .. }
        ));

        // 配额类 429 vs 限流 429(携带 retry_after)
        assert!(matches!(
            classify_error(
                429,
                None,
                r#"{"error":{"message":"billing quota exceeded"}}"#
            ),
            LlmError::QuotaExceeded { status: 429, .. }
        ));
        assert!(matches!(
            classify_error(429, Some(1500), r#"{"error":{"message":"slow down"}}"#),
            LlmError::RateLimited {
                retry_after_ms: Some(1500),
                ..
            }
        ));

        // 服务端 / 兜底客户端
        assert!(matches!(
            classify_error(503, None, "down"),
            LlmError::ServerError { status: 503, .. }
        ));
        assert!(matches!(
            classify_error(418, None, "teapot"),
            LlmError::ClientError { status: 418, .. }
        ));
    }

    #[test]
    fn parse_retry_after_ms_reads_delta_seconds_header() {
        use reqwest::header::{HeaderMap, RETRY_AFTER};

        let mut headers = HeaderMap::new();
        assert_eq!(parse_retry_after_ms(&headers), None);

        headers.insert(RETRY_AFTER, "2".parse().unwrap());
        assert_eq!(parse_retry_after_ms(&headers), Some(2000));

        headers.insert(RETRY_AFTER, u64::MAX.to_string().parse().unwrap());
        assert_eq!(parse_retry_after_ms(&headers), None);

        // 非 delta-seconds(HTTP-date 等)不解析,返回 None 而非猜测。
        headers.insert(
            RETRY_AFTER,
            "Wed, 21 Oct 2026 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(parse_retry_after_ms(&headers), None);
    }

    fn test_request(addr: String) -> HttpPostRequest {
        HttpPostRequest {
            client: reqwest::Client::new(),
            endpoint: addr,
            headers: vec![],
            body: serde_json::json!({}),
            retry: RetryPolicy {
                base_delay_ms: 1,
                max_delay_ms: 100,
                ..RetryPolicy::default()
            },
        }
    }

    #[tokio::test]
    async fn run_does_not_retry_transport_error_after_stream_lines_were_consumed() {
        let (addr, requests) = spawn_test_server(|_| {
            // 声明 Content-Length 但只发送部分 body 后断开 → 流中途传输错误。
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 10000\r\n\r\ndata: {\"type\":\"message_stop\"}\n\n"
                .to_vec()
        })
        .await;
        let stream_started = AtomicBool::new(false);
        let (tx, _rx) = mpsc::unbounded_channel();
        let result = test_request(addr)
            .run(&stream_started, |response| {
                let tx = &tx;
                let stream_started = &stream_started;
                async move {
                    consume_sse_lines(response, tx, |line| {
                        let _ = line;
                        stream_started.store(true, Ordering::SeqCst);
                        !tx.is_closed()
                    })
                    .await
                    .map(|_| ())
                }
            })
            .await;

        assert!(matches!(result, Err(LlmError::Transport { .. })));
        assert_eq!(requests.load(Ordering::SeqCst), 1, "已消费流后不应重试");
    }

    #[tokio::test]
    async fn run_retries_transport_error_before_any_stream_line() {
        let (addr, requests) = spawn_test_server(|_| {
            // 声明 Content-Length 但 body 未发送 → 首个 body read 即失败。
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 10000\r\n\r\n"
                .to_vec()
        })
        .await;
        let stream_started = AtomicBool::new(false);
        let (tx, _rx) = mpsc::unbounded_channel();
        let result = test_request(addr)
            .run(&stream_started, |response| {
                let tx = &tx;
                let stream_started = &stream_started;
                async move {
                    consume_sse_lines(response, tx, |line| {
                        let _ = line;
                        stream_started.store(true, Ordering::SeqCst);
                        !tx.is_closed()
                    })
                    .await
                    .map(|_| ())
                }
            })
            .await;

        assert!(matches!(result, Err(LlmError::Transport { .. })));
        // 默认 max_transport_retries = 2：尝试 1、2 重试，第 3 次失败后返回。
        assert_eq!(requests.load(Ordering::SeqCst), 3, "未消费任何行时应重试");
    }
}
