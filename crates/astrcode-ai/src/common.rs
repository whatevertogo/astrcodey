//! Provider 共享基础设施：HTTP 客户端构建、流式请求重试循环、SSE 行解析。
//!
//! 所有 LLM provider 的 HTTP 流式请求都遵循相同的模式：
//! 构建 client → 带重试的 POST 请求 → 解析 SSE 字节流。
//! 本模块将这一公共骨架提取为泛型函数，各 provider 只需提供
//! SSE 事件处理和请求体构造。

use astrcode_core::llm::{LlmClientConfig, LlmError, LlmEvent};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::retry::RetryPolicy;
use crate::stream_decoder::{SseLineReader, Utf8StreamDecoder};

/// 根据 LlmClientConfig 构建 reqwest::Client。
pub fn build_client(config: &LlmClientConfig) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(config.connect_timeout_secs))
        .timeout(std::time::Duration::from_secs(config.read_timeout_secs))
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

        let response = req
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;

        let status = response.status();
        if status.is_success() {
            return parse_sse_response(response, &tx, &parse_sse_line).await;
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

        let text = response.text().await.unwrap_or_default();
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

        let response = req
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;

        let status = response.status();
        if status.is_success() {
            return parse_sse_response_with_event_type(response, &tx, &handle_event).await;
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

        let text = response.text().await.unwrap_or_default();
        return Err(classify_error(status.as_u16(), text));
    }
}

async fn parse_sse_response(
    response: reqwest::Response,
    tx: &mpsc::UnboundedSender<LlmEvent>,
    parse_line: impl Fn(&str, &mpsc::UnboundedSender<LlmEvent>),
) -> Result<(), LlmError> {
    let mut stream = response.bytes_stream();
    let mut decoder = Utf8StreamDecoder::new();
    let mut line_reader = SseLineReader::new();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| LlmError::Transport(e.to_string()))?;
        if let Some(text) = decoder.push(&bytes) {
            for line in line_reader.push_chunk(&text) {
                process_data_line(&line, tx, &parse_line);
            }
        }
    }
    drain_decoder(&mut decoder, &mut line_reader, tx, &parse_line);
    Ok(())
}

async fn parse_sse_response_with_event_type(
    response: reqwest::Response,
    tx: &mpsc::UnboundedSender<LlmEvent>,
    handle_event: impl Fn(&str, &serde_json::Value, &mpsc::UnboundedSender<LlmEvent>),
) -> Result<(), LlmError> {
    let mut stream = response.bytes_stream();
    let mut decoder = Utf8StreamDecoder::new();
    let mut line_reader = SseLineReader::new();
    let mut current_event_type = String::new();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| LlmError::Transport(e.to_string()))?;
        if let Some(text) = decoder.push(&bytes) {
            for line in line_reader.push_chunk(&text) {
                if let Some(ev_type) = line.strip_prefix("event:") {
                    current_event_type = ev_type.trim().to_string();
                    continue;
                }
                if let Some(data) = line.strip_prefix("data:") {
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
                if let Ok(event) = serde_json::from_str::<serde_json::Value>(data.trim()) {
                    handle_event("", &event, tx);
                }
            }
        }
    }
    if let Some(line) = line_reader.flush() {
        if let Some(data) = line.strip_prefix("data:") {
            if let Ok(event) = serde_json::from_str::<serde_json::Value>(data.trim()) {
                handle_event("", &event, tx);
            }
        }
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

pub fn classify_error(status: u16, text: String) -> LlmError {
    if status >= 500 {
        LlmError::ServerError { status, message: text }
    } else {
        LlmError::ClientError { status, message: text }
    }
}
