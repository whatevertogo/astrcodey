//! OpenAI-compatible streaming transport.

use std::collections::HashMap;

use astrcode_core::{
    config::{OpenAiApiMode, ProviderAuthScheme},
    llm::{LlmError, LlmEvent},
};
use tokio::sync::mpsc;

use super::parser::{ChatAccumulator, emit_done_once, process_sse_line};
use crate::{
    common::{
        HttpPostRequest, SseBodyPreview, apply_auth_header, consume_sse_lines, ensure_header,
    },
    retry::RetryPolicy,
};

#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_request<A: ChatAccumulator>(
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
    auth_scheme: ProviderAuthScheme,
    extra_headers: HashMap<String, String>,
    body: serde_json::Value,
    api_mode: OpenAiApiMode,
    retry: RetryPolicy,
    tx: mpsc::UnboundedSender<LlmEvent>,
) -> Result<(), LlmError> {
    let mut headers: Vec<(String, String)> = extra_headers.into_iter().collect();
    apply_auth_header(&mut headers, auth_scheme, &api_key);
    ensure_header(&mut headers, "Accept", "text/event-stream");

    HttpPostRequest {
        client,
        endpoint,
        headers,
        body,
        retry,
    }
    .run(|response| parse_stream::<A>(response, api_mode, &tx))
    .await
}

async fn parse_stream<ACC: ChatAccumulator>(
    response: reqwest::Response,
    api_mode: OpenAiApiMode,
    tx: &mpsc::UnboundedSender<LlmEvent>,
) -> Result<(), LlmError> {
    let mut accumulator = ACC::default();
    let completed = consume_sse_lines(response, tx, SseBodyPreview::Skip, |line| {
        process_sse_line(line, &mut accumulator, api_mode, tx);
        !tx.is_closed()
    })
    .await?
    .is_some();
    if completed && !accumulator.done_sent() && !tx.is_closed() {
        emit_done_once(&mut accumulator, tx);
    }
    Ok(())
}
