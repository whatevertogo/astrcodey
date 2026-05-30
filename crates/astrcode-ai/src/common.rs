//! Provider 共享基础设施：HTTP 客户端构建、流式请求重试循环、SSE 行解析。
//!
//! 所有 LLM provider 的 HTTP 流式请求都遵循相同的模式：
//! 构建 client → 带重试的 POST 请求 → 解析 SSE 字节流。
//! 本模块将这一公共骨架提取为泛型函数，各 provider 只需提供
//! SSE 事件处理和请求体构造。

use astrcode_core::llm::{LlmClientConfig, LlmError, LlmEvent};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::{
    retry::RetryPolicy,
    stream_decoder::{SseLineReader, Utf8StreamDecoder},
};

/// 根据 `LlmClientConfig` 构建 reqwest client。
///
/// 配置无效时返回 [`LlmError::Transport`]，不在 silently 降级到无 timeout 的默认 client。
pub fn build_client(config: &LlmClientConfig) -> Result<reqwest::Client, LlmError> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(config.connect_timeout_secs))
        .read_timeout(std::time::Duration::from_secs(config.read_timeout_secs))
        .build()
        .map_err(|error| LlmError::Transport(format!("failed to create HTTP client: {error}")))
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
        *accumulated = fragment.to_string();
        return (!incremental.is_empty()).then_some(incremental);
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
    fallback_call_id: u64,
}

impl StreamEventSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn done_sent(&self) -> bool {
        self.done_sent
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

/// 带重试的 HTTP POST 请求参数。
pub struct HttpPostRequest {
    pub client: reqwest::Client,
    pub endpoint: String,
    pub headers: Vec<(String, String)>,
    pub body: serde_json::Value,
    pub retry: RetryPolicy,
}

impl HttpPostRequest {
    pub async fn run<F, Fut>(&self, mut on_success: F) -> Result<(), LlmError>
    where
        F: FnMut(reqwest::Response) -> Fut,
        Fut: std::future::Future<Output = Result<(), LlmError>>,
    {
        let mut attempt = 0;

        loop {
            attempt += 1;
            let response = match self.send_once().await {
                Ok(response) => response,
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

    pub async fn stream_data_lines(
        &self,
        tx: &mpsc::UnboundedSender<LlmEvent>,
        parse_line: impl Fn(&str, &mpsc::UnboundedSender<LlmEvent>) -> bool,
    ) -> Result<(), LlmError> {
        let mut attempt = 0;

        loop {
            attempt += 1;
            let response = match self.send_once().await {
                Ok(response) => response,
                Err(error) => {
                    if self.retry.should_retry_transport(attempt) {
                        tokio::time::sleep(self.retry.delay(attempt)).await;
                        continue;
                    }
                    return Err(error);
                },
            };

            let status = response.status();
            if status.is_success() {
                match parse_sse_response(response, tx, &parse_line).await {
                    Ok(()) => return Ok(()),
                    Err(LlmError::Transport(message)) => {
                        if self.retry.should_retry_transport(attempt) {
                            tokio::time::sleep(self.retry.delay(attempt)).await;
                            continue;
                        }
                        return Err(LlmError::Transport(message));
                    },
                    Err(error) => return Err(error),
                }
            }

            if self.retry.should_retry(attempt, status.as_u16()) {
                tokio::time::sleep(self.retry.delay(attempt)).await;
                continue;
            }

            let text = read_http_error_body(response, &self.endpoint).await;
            return Err(classify_error(status.as_u16(), text));
        }
    }

    pub async fn stream_typed_events(
        &self,
        tx: &mpsc::UnboundedSender<LlmEvent>,
        handle_event: impl Fn(&str, &serde_json::Value, &mpsc::UnboundedSender<LlmEvent>) -> bool,
    ) -> Result<(), LlmError> {
        let mut attempt = 0;

        loop {
            attempt += 1;
            let response = match self.send_once().await {
                Ok(response) => response,
                Err(error) => {
                    if self.retry.should_retry_transport(attempt) {
                        tokio::time::sleep(self.retry.delay(attempt)).await;
                        continue;
                    }
                    return Err(error);
                },
            };

            let status = response.status();
            if status.is_success() {
                match parse_sse_response_with_event_type(response, tx, &handle_event).await {
                    Ok(()) => return Ok(()),
                    Err(LlmError::Transport(message)) => {
                        if self.retry.should_retry_transport(attempt) {
                            tokio::time::sleep(self.retry.delay(attempt)).await;
                            continue;
                        }
                        return Err(LlmError::Transport(message));
                    },
                    Err(error) => return Err(error),
                }
            }

            if self.retry.should_retry(attempt, status.as_u16()) {
                tokio::time::sleep(self.retry.delay(attempt)).await;
                continue;
            }

            let text = read_http_error_body(response, &self.endpoint).await;
            return Err(classify_error(status.as_u16(), text));
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

/// 发起带重试的 HTTP POST 流式请求，并通过回调解析 SSE 事件。
///
/// `parse_sse_line` 会在每条 SSE `data:` 行到达时被调用。
/// 对于 Anthropic 风格的 `event:` + `data:` 行，使用 [`stream_with_event_type`]。
pub async fn stream_with_retry(
    client: reqwest::Client,
    endpoint: String,
    headers: Vec<(String, String)>,
    body: serde_json::Value,
    retry: RetryPolicy,
    tx: mpsc::UnboundedSender<LlmEvent>,
    parse_sse_line: impl Fn(&str, &mpsc::UnboundedSender<LlmEvent>) -> bool,
) -> Result<(), LlmError> {
    HttpPostRequest {
        client,
        endpoint,
        headers,
        body,
        retry,
    }
    .stream_data_lines(&tx, parse_sse_line)
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
    handle_event: impl Fn(&str, &serde_json::Value, &mpsc::UnboundedSender<LlmEvent>) -> bool,
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

async fn parse_sse_response(
    response: reqwest::Response,
    tx: &mpsc::UnboundedSender<LlmEvent>,
    parse_line: &impl Fn(&str, &mpsc::UnboundedSender<LlmEvent>) -> bool,
) -> Result<(), LlmError> {
    let endpoint = response.url().to_string();
    let status = response.status();
    let content_type = header_value(response.headers(), reqwest::header::CONTENT_TYPE);
    let content_encoding = header_value(response.headers(), reqwest::header::CONTENT_ENCODING);
    let mut stream = response.bytes_stream();
    let mut decoder = Utf8StreamDecoder::new();
    let mut line_reader = SseLineReader::new();
    let mut bytes_read = 0usize;
    let mut has_data_line = false;
    let mut body_preview = String::new();

    while let Some(chunk) = stream.next().await {
        if tx.is_closed() {
            return Ok(());
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
        if body_preview.is_empty() && !bytes.is_empty() {
            let preview_len = bytes.len().min(512);
            body_preview = String::from_utf8_lossy(&bytes[..preview_len]).to_string();
        }
        if let Some(text) = decoder.push(&bytes) {
            for line in line_reader.push_chunk(&text) {
                if line.starts_with("data:") {
                    has_data_line = true;
                }
                if !process_data_line(&line, tx, &parse_line) {
                    return Ok(());
                }
            }
        }
    }
    if !drain_decoder(&mut decoder, &mut line_reader, tx, &parse_line) {
        return Ok(());
    }

    if bytes_read > 0 && !has_data_line {
        return Err(LlmError::StreamParse(format!(
            "LLM returned 200 but response is not valid SSE (no data: lines found). Content-Type: \
             {}, bytes: {}, preview: {}",
            content_type.as_deref().unwrap_or("<missing>"),
            bytes_read,
            truncate_str(&body_preview, 256),
        )));
    }

    Ok(())
}

async fn parse_sse_response_with_event_type(
    response: reqwest::Response,
    tx: &mpsc::UnboundedSender<LlmEvent>,
    handle_event: &impl Fn(&str, &serde_json::Value, &mpsc::UnboundedSender<LlmEvent>) -> bool,
) -> Result<(), LlmError> {
    let endpoint = response.url().to_string();
    let status = response.status();
    let content_type = header_value(response.headers(), reqwest::header::CONTENT_TYPE);
    let content_encoding = header_value(response.headers(), reqwest::header::CONTENT_ENCODING);
    let mut stream = response.bytes_stream();
    let mut decoder = Utf8StreamDecoder::new();
    let mut line_reader = SseLineReader::new();
    let mut current_event_type = String::new();
    let mut bytes_read = 0usize;
    let mut has_data_line = false;
    let mut body_preview = String::new();

    while let Some(chunk) = stream.next().await {
        if tx.is_closed() {
            return Ok(());
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
        if body_preview.is_empty() && !bytes.is_empty() {
            let preview_len = bytes.len().min(512);
            body_preview = String::from_utf8_lossy(&bytes[..preview_len]).to_string();
        }
        if let Some(text) = decoder.push(&bytes) {
            for line in line_reader.push_chunk(&text) {
                if let Some(ev_type) = line.strip_prefix("event:") {
                    current_event_type = ev_type.trim().to_string();
                    continue;
                }
                if let Some(data) = line.strip_prefix("data:") {
                    has_data_line = true;
                    let data = data.trim();
                    if data == "[DONE]" || data.is_empty() {
                        continue;
                    }
                    if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                        if !handle_event(&current_event_type, &event, tx) {
                            return Ok(());
                        }
                        current_event_type.clear();
                    }
                }
            }
        }
    }
    if let Some(tail) = decoder.finish() {
        for line in line_reader.push_chunk(&tail) {
            if let Some(data) = line.strip_prefix("data:") {
                has_data_line = true;
                if let Ok(event) = serde_json::from_str::<serde_json::Value>(data.trim()) {
                    if !handle_event("", &event, tx) {
                        return Ok(());
                    }
                }
            }
        }
    }
    if let Some(line) = line_reader.flush() {
        if let Some(data) = line.strip_prefix("data:") {
            has_data_line = true;
            if let Ok(event) = serde_json::from_str::<serde_json::Value>(data.trim()) {
                if !handle_event("", &event, tx) {
                    return Ok(());
                }
            }
        }
    }

    if bytes_read > 0 && !has_data_line {
        return Err(LlmError::StreamParse(format!(
            "LLM returned 200 but response is not valid SSE (no data: lines found). Content-Type: \
             {}, bytes: {}, preview: {}",
            content_type.as_deref().unwrap_or("<missing>"),
            bytes_read,
            truncate_str(&body_preview, 256),
        )));
    }

    Ok(())
}

fn process_data_line(
    line: &str,
    tx: &mpsc::UnboundedSender<LlmEvent>,
    parse_line: &impl Fn(&str, &mpsc::UnboundedSender<LlmEvent>) -> bool,
) -> bool {
    if tx.is_closed() {
        return false;
    }
    let Some(data) = line.strip_prefix("data:") else {
        return true;
    };
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return true;
    }
    parse_line(data, tx)
}

fn drain_decoder(
    decoder: &mut Utf8StreamDecoder,
    line_reader: &mut SseLineReader,
    tx: &mpsc::UnboundedSender<LlmEvent>,
    parse_line: &impl Fn(&str, &mpsc::UnboundedSender<LlmEvent>) -> bool,
) -> bool {
    if let Some(tail) = decoder.finish() {
        for line in line_reader.push_chunk(&tail) {
            if !process_data_line(&line, tx, parse_line) {
                return false;
            }
        }
    }
    if let Some(line) = line_reader.flush() {
        return process_data_line(&line, tx, parse_line);
    }
    true
}

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

fn redacted_endpoint(endpoint: &str) -> String {
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
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

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
    async fn streaming_client_uses_idle_read_timeout_not_total_timeout() {
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
            socket.write_all(b"data: first\n\n").await.unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(600)).await;
            socket.write_all(b"data: second\n\n").await.unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(600)).await;
            socket.write_all(b"data: third\n\n").await.unwrap();
            socket.flush().await.unwrap();
        });

        let config = LlmClientConfig {
            connect_timeout_secs: 1,
            read_timeout_secs: 1,
            ..LlmClientConfig::default()
        };
        let client = build_client(&config).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&lines);

        stream_with_retry(
            client,
            format!("http://{addr}/stream"),
            Vec::new(),
            serde_json::json!({}),
            RetryPolicy {
                max_retries: 0,
                base_delay_ms: 1,
                max_transport_retries: 0,
            },
            tx,
            move |line, _| {
                captured.lock().unwrap().push(line.to_string());
                true
            },
        )
        .await
        .unwrap();

        let lines = lines.lock().unwrap().clone();
        assert_eq!(lines, vec!["first", "second", "third"]);
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
        parse_sse_response_with_event_type(response, &tx, &|event_type, event, _| {
            captured.lock().unwrap().push((
                event_type.to_string(),
                event
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            ));
            true
        })
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
