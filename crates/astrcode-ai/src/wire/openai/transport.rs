//! OpenAI-compatible streaming transport.

use std::{collections::HashMap, time::Instant};

use astrcode_core::{
    config::{OpenAiApiMode, ProviderAuthScheme},
    llm::{LlmError, LlmEvent},
};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use super::parser::{ChatAccumulator, emit_done_once, process_sse_line};
use crate::{
    common::{HttpPostRequest, apply_auth_header, ensure_header, stream_body_error},
    retry::RetryPolicy,
    stream_decoder::{SseLineReader, StreamDecoderError, Utf8StreamDecoder},
};

fn stream_decoder_error(error: StreamDecoderError) -> LlmError {
    LlmError::StreamParse(error.to_string())
}

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
    let endpoint = response.url().to_string();
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let content_encoding = response
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let mut stream = response.bytes_stream();
    let mut decoder = Utf8StreamDecoder::new();
    let mut accumulator = ACC::default();
    let mut line_reader = SseLineReader::new();
    let mut bytes_read = 0usize;
    let stream_started = Instant::now();
    let mut first_chunk_reported = false;

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
        if !first_chunk_reported && !bytes.is_empty() {
            first_chunk_reported = true;
            tracing::debug!(
                endpoint = %crate::common::redacted_endpoint(&endpoint),
                status = status.as_u16(),
                bytes = bytes.len(),
                elapsed_ms = stream_started.elapsed().as_millis(),
                "OpenAI stream first bytes received"
            );
        }
        if let Some(text) = decoder.push(&bytes).map_err(stream_decoder_error)? {
            for line in line_reader
                .push_chunk(&text)
                .map_err(stream_decoder_error)?
            {
                process_sse_line(&line, &mut accumulator, api_mode, tx);
                if tx.is_closed() {
                    return Ok(());
                }
            }
        }
    }
    if let Some(tail_text) = decoder.finish() {
        for line in line_reader
            .push_chunk(&tail_text)
            .map_err(stream_decoder_error)?
        {
            process_sse_line(&line, &mut accumulator, api_mode, tx);
            if tx.is_closed() {
                return Ok(());
            }
        }
    }
    if let Some(line) = line_reader.flush() {
        process_sse_line(&line, &mut accumulator, api_mode, tx);
    }
    if !accumulator.done_sent() && !tx.is_closed() {
        emit_done_once(&mut accumulator, tx);
    }
    Ok(())
}
