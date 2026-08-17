//! Anthropic Messages streaming transport.
//!
//! 镜像 [`crate::wire::openai::transport`]：构建请求头后驱动 [`HttpPostRequest::run`]，
//! `parse_stream` 本地拥有 [`AnthropicStreamState`]，经共享的 [`consume_sse_lines`] 逐行喂数据，
//! 因此不需要 `Arc<Mutex<…>>` 来跨回调共享状态。

use std::sync::atomic::{AtomicBool, Ordering};

use astrcode_core::llm::{LlmError, LlmEvent};
use tokio::sync::mpsc;

use super::{
    body::ANTHROPIC_API_VERSION,
    parser::{AnthropicStreamState, process_sse_line},
};
use crate::common::{ConnectionSnapshot, HttpPostRequest, consume_sse_lines, ensure_header};

pub(crate) async fn stream_request(
    client: reqwest::Client,
    endpoint: String,
    snapshot: ConnectionSnapshot,
    body: serde_json::Value,
    tx: mpsc::UnboundedSender<LlmEvent>,
) -> Result<(), LlmError> {
    let mut headers = snapshot.headers;
    ensure_header(&mut headers, "anthropic-version", ANTHROPIC_API_VERSION);
    ensure_header(&mut headers, "Accept", "text/event-stream");
    let stream_started = AtomicBool::new(false);
    let stream_replay_safe = AtomicBool::new(true);

    HttpPostRequest {
        client,
        endpoint,
        headers,
        body,
        retry: snapshot.retry,
    }
    .run(&stream_started, &stream_replay_safe, &tx, |response| {
        parse_stream(response, &tx, &stream_started, &stream_replay_safe)
    })
    .await
}

async fn parse_stream(
    response: reqwest::Response,
    tx: &mpsc::UnboundedSender<LlmEvent>,
    stream_started: &AtomicBool,
    stream_replay_safe: &AtomicBool,
) -> Result<(), LlmError> {
    let mut state = AnthropicStreamState::default();
    let mut current_event_type = String::new();
    let mut has_data_line = false;
    let completed = consume_sse_lines(response, tx, |line| {
        stream_started.store(true, Ordering::SeqCst);
        let keep_reading = process_sse_line(
            line,
            tx,
            &mut state,
            &mut current_event_type,
            &mut has_data_line,
        );
        if state.has_started_tool_call() {
            stream_replay_safe.store(false, Ordering::SeqCst);
        }
        keep_reading
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
    if !state.sink.sent() && !tx.is_closed() {
        state.sink.emit(tx, "stop");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::ScriptedLlmServer;

    async fn parse_from_server(
        body: Vec<u8>,
    ) -> (Result<(), LlmError>, mpsc::UnboundedReceiver<LlmEvent>) {
        let server = ScriptedLlmServer::spawn(vec![body]).await;
        let client = reqwest::Client::new();
        let response = client
            .post(server.base_url())
            .body("{}")
            .send()
            .await
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let stream_started = AtomicBool::new(false);
        let stream_replay_safe = AtomicBool::new(true);
        let result = parse_stream(response, &tx, &stream_started, &stream_replay_safe).await;
        server.assert_consumed();
        (result, rx)
    }

    #[tokio::test]
    async fn parse_stream_rejects_200_response_without_data_lines() {
        let result = parse_from_server(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n<html>gateway error page</html>".to_vec(),
        )
        .await;
        assert!(matches!(result.0, Err(LlmError::StreamParse { .. })));
    }

    #[tokio::test]
    async fn parse_stream_accepts_valid_sse_and_passes_through_stop_reason() {
        let (result, mut rx) = parse_from_server(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n\
event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n\
event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n\
event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
                .to_vec(),
        )
        .await;
        assert!(result.is_ok());
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, LlmEvent::ContentDelta { delta } if delta == "hi"))
        );
        assert!(
            events.iter().any(
                |e| matches!(e, LlmEvent::Done { finish_reason } if finish_reason == "end_turn")
            )
        );
    }
}
