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

/// 根据 LlmClientConfig 构建 reqwest::Client。
pub fn build_client(config: &LlmClientConfig) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(config.connect_timeout_secs))
        .read_timeout(std::time::Duration::from_secs(config.read_timeout_secs))
        .build()
        .unwrap_or_else(|e| {
            tracing::error!("Failed to create HTTP client: {e}");
            reqwest::Client::new()
        })
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
    parse_sse_line: impl Fn(&str, &mpsc::UnboundedSender<LlmEvent>),
) -> Result<(), LlmError> {
    let mut attempt = 0;

    loop {
        attempt += 1;
        let mut req = client
            .post(&endpoint)
            .header("content-type", "application/json");
        for (key, value) in &headers {
            req = req.header(key.as_str(), value.as_str());
        }

        let response = match req.json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                if retry.should_retry_transport(attempt) {
                    let delay = retry.delay(attempt);
                    tracing::warn!(
                        "LLM request failed with transport error (attempt {attempt}/{}), retrying \
                         after {}ms: {e}",
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
            match parse_sse_response(response, &tx, &parse_sse_line).await {
                Ok(()) => return Ok(()),
                Err(LlmError::Transport(msg)) => {
                    if retry.should_retry_transport(attempt) {
                        let delay = retry.delay(attempt);
                        tracing::warn!(
                            "LLM stream read failed with transport error (attempt {attempt}/{}), \
                             retrying after {}ms: {msg}",
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
                "LLM request failed with {status}, retrying (attempt {attempt}/{}) after {}ms",
                retry.max_retries,
                delay.as_millis()
            );
            tokio::time::sleep(delay).await;
            continue;
        }

        let text = read_http_error_body(response, &endpoint).await;
        return Err(classify_error(status.as_u16(), text));
    }
}

/// 发起带重试的 SSE 流式请求，支持 `event:` + `data:` 行模式（Anthropic 风格）。
///
/// `handle_event` 参数为 `(event_type, data_json)`。
pub async fn stream_with_event_type(
    client: reqwest::Client,
    endpoint: String,
    headers: Vec<(String, String)>,
    body: serde_json::Value,
    retry: RetryPolicy,
    tx: mpsc::UnboundedSender<LlmEvent>,
    handle_event: impl Fn(&str, &serde_json::Value, &mpsc::UnboundedSender<LlmEvent>),
) -> Result<(), LlmError> {
    let mut attempt = 0;

    loop {
        attempt += 1;
        let mut req = client
            .post(&endpoint)
            .header("content-type", "application/json");
        for (key, value) in &headers {
            req = req.header(key.as_str(), value.as_str());
        }

        let response = match req.json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                if retry.should_retry_transport(attempt) {
                    let delay = retry.delay(attempt);
                    tracing::warn!(
                        "LLM request failed with transport error (attempt {attempt}/{}), retrying \
                         after {}ms: {e}",
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
            match parse_sse_response_with_event_type(response, &tx, &handle_event).await {
                Ok(()) => return Ok(()),
                Err(LlmError::Transport(msg)) => {
                    if retry.should_retry_transport(attempt) {
                        let delay = retry.delay(attempt);
                        tracing::warn!(
                            "LLM stream read failed with transport error (attempt {attempt}/{}), \
                             retrying after {}ms: {msg}",
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
                "LLM request failed with {status}, retrying (attempt {attempt}/{}) after {}ms",
                retry.max_retries,
                delay.as_millis()
            );
            tokio::time::sleep(delay).await;
            continue;
        }

        let text = read_http_error_body(response, &endpoint).await;
        return Err(classify_error(status.as_u16(), text));
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

async fn parse_sse_response(
    response: reqwest::Response,
    tx: &mpsc::UnboundedSender<LlmEvent>,
    parse_line: impl Fn(&str, &mpsc::UnboundedSender<LlmEvent>),
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
        if body_preview.is_empty() && !bytes.is_empty() {
            let preview_len = bytes.len().min(512);
            body_preview = String::from_utf8_lossy(&bytes[..preview_len]).to_string();
        }
        if let Some(text) = decoder.push(&bytes) {
            for line in line_reader.push_chunk(&text) {
                if line.starts_with("data:") {
                    has_data_line = true;
                }
                process_data_line(&line, tx, &parse_line);
            }
        }
    }
    drain_decoder(&mut decoder, &mut line_reader, tx, &parse_line);

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
    handle_event: impl Fn(&str, &serde_json::Value, &mpsc::UnboundedSender<LlmEvent>),
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
                        handle_event(&current_event_type, &event, tx);
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
                    handle_event("", &event, tx);
                }
            }
        }
    }
    if let Some(line) = line_reader.flush() {
        if let Some(data) = line.strip_prefix("data:") {
            has_data_line = true;
            if let Ok(event) = serde_json::from_str::<serde_json::Value>(data.trim()) {
                handle_event("", &event, tx);
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
    parse_line: impl Fn(&str, &mpsc::UnboundedSender<LlmEvent>),
) {
    if let Some(data) = line.strip_prefix("data:") {
        let data = data.trim();
        if data.is_empty() {
            return;
        }
        parse_line(data, tx);
    }
}

fn drain_decoder(
    decoder: &mut Utf8StreamDecoder,
    line_reader: &mut SseLineReader,
    tx: &mpsc::UnboundedSender<LlmEvent>,
    parse_line: impl Fn(&str, &mpsc::UnboundedSender<LlmEvent>),
) {
    if let Some(tail) = decoder.finish() {
        for line in line_reader.push_chunk(&tail) {
            process_data_line(&line, tx, &parse_line);
        }
    }
    if let Some(line) = line_reader.flush() {
        process_data_line(&line, tx, &parse_line);
    }
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
        let client = build_client(&config);
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
            },
        )
        .await
        .unwrap();

        let lines = lines.lock().unwrap().clone();
        assert_eq!(lines, vec!["first", "second", "third"]);
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
