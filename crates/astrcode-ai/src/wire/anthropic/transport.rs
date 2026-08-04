//! Anthropic Messages streaming transport.
//!
//! 镜像 [`crate::wire::openai::transport`]：构建请求头后驱动 [`HttpPostRequest::run`]，
//! `parse_stream` 本地拥有 [`AnthropicStreamState`]，经共享的 [`consume_sse_lines`] 逐行喂数据，
//! 因此不需要 `Arc<Mutex<…>>` 来跨回调共享状态。

use std::sync::atomic::{AtomicBool, Ordering};

use astrcode_core::llm::{LlmClientConfig, LlmError, LlmEvent};
use tokio::sync::mpsc;

use super::{
    body::ANTHROPIC_API_VERSION,
    parser::{AnthropicStreamState, process_sse_line},
};
use crate::{
    common::{HttpPostRequest, base_headers, consume_sse_lines, ensure_header},
    retry::RetryPolicy,
};

pub(crate) async fn stream_request(
    client: reqwest::Client,
    endpoint: String,
    config: LlmClientConfig,
    body: serde_json::Value,
    retry: RetryPolicy,
    tx: mpsc::UnboundedSender<LlmEvent>,
) -> Result<(), LlmError> {
    let mut headers = base_headers(&config);
    ensure_header(&mut headers, "anthropic-version", ANTHROPIC_API_VERSION);
    ensure_header(&mut headers, "Accept", "text/event-stream");
    let stream_started = AtomicBool::new(false);

    HttpPostRequest {
        client,
        endpoint,
        headers,
        body,
        retry,
    }
    .run(&stream_started, &tx, |response| {
        parse_stream(response, &tx, &stream_started)
    })
    .await
}

async fn parse_stream(
    response: reqwest::Response,
    tx: &mpsc::UnboundedSender<LlmEvent>,
    stream_started: &AtomicBool,
) -> Result<(), LlmError> {
    let mut state = AnthropicStreamState::default();
    let mut current_event_type = String::new();
    let mut has_data_line = false;
    let completed = consume_sse_lines(response, tx, |line| {
        stream_started.store(true, Ordering::SeqCst);
        process_sse_line(
            line,
            tx,
            &mut state,
            &mut current_event_type,
            &mut has_data_line,
        )
    })
    .await?;

    // 接收端关闭或回调主动停止 → 不再补发收尾事件。
    let Some(summary) = completed else {
        return Ok(());
    };
    summary.require_data_lines(has_data_line)?;

    if !tx.is_closed() {
        state.emit_pending_tool_completions(tx);
    }
    if !state.sink.done_sent() && !tx.is_closed() {
        state.sink.ensure_done(tx);
    }
    Ok(())
}
