//! Anthropic Messages streaming transport.
//!
//! 镜像 [`crate::wire::openai::transport`]：构建请求头后驱动 [`HttpPostRequest::run`]，
//! `parse_stream` 本地拥有 [`AnthropicStreamState`]，经共享的 [`consume_sse_lines`] 逐行喂数据，
//! 因此不需要 `Arc<Mutex<…>>` 来跨回调共享状态。

use astrcode_core::llm::{LlmClientConfig, LlmError, LlmEvent};
use tokio::sync::mpsc;

use super::{
    body::ANTHROPIC_API_VERSION,
    parser::{AnthropicStreamState, process_sse_line},
};
use crate::{
    common::{HttpPostRequest, SseBodyPreview, base_headers, consume_sse_lines, ensure_header},
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

    HttpPostRequest {
        client,
        endpoint,
        headers,
        body,
        retry,
    }
    .run(|response| parse_stream(response, &tx))
    .await
}

async fn parse_stream(
    response: reqwest::Response,
    tx: &mpsc::UnboundedSender<LlmEvent>,
) -> Result<(), LlmError> {
    let mut state = AnthropicStreamState::default();
    let mut current_event_type = String::new();
    let mut has_data_line = false;
    let completed = consume_sse_lines(response, tx, SseBodyPreview::Capture, |line| {
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

    if !state.sink.done_sent() && !tx.is_closed() {
        state.sink.ensure_done(tx);
    }
    Ok(())
}
