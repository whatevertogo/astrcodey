//! OpenAI Responses API 适配。
//!
//! 这里刻意使用较宽松的 `serde_json::Value` 解析：
//! - 请求体只发送当前实现确认使用的稳定字段
//! - 响应体和 SSE 事件尽量容错，降低官方对象轻微扩展带来的脆弱性

use serde_json::{Value, json};

use super::*;

/// Responses API 的 SSE 事件块处理器。
///
/// OpenAI Responses 使用 `event:` + `data:` 的事件块协议，
/// 并可能在 `response.completed` 中携带完整输出对象。
pub(super) struct ResponsesSseProcessor {
    sse_buffer: String,
    completed_output: Option<LlmOutput>,
}

impl ResponsesSseProcessor {
    pub fn new() -> Self {
        Self {
            sse_buffer: String::new(),
            completed_output: None,
        }
    }
}

impl super::dto::SseProcessor for ResponsesSseProcessor {
    fn process_chunk(
        &mut self,
        chunk_text: &str,
        accumulator: &mut LlmAccumulator,
        sink: &EventSink,
    ) -> Result<(bool, Option<String>, Option<LlmUsage>)> {
        let done = consume_sse_text_chunk(
            chunk_text,
            &mut self.sse_buffer,
            accumulator,
            sink,
            &mut self.completed_output,
        )?;
        Ok((done, None, None))
    }

    fn flush(
        &mut self,
        accumulator: &mut LlmAccumulator,
        sink: &EventSink,
    ) -> Result<(Option<String>, Option<LlmUsage>)> {
        flush_sse_buffer(
            &mut self.sse_buffer,
            accumulator,
            sink,
            &mut self.completed_output,
        )?;
        Ok((None, None))
    }

    fn take_completed_output(&mut self) -> Option<LlmOutput> {
        self.completed_output.take()
    }
}

pub(super) fn build_request(
    provider: &OpenAiProvider,
    request: &LlmRequest,
    stream: bool,
) -> Value {
    let mut body = json!({
        "model": provider.model,
        "input": build_input_items(&request.messages),
        "store": false,
        "stream": stream,
        "max_output_tokens": request
            .max_output_tokens_override
            .unwrap_or(provider.limits.max_output_tokens)
            .min(provider.limits.max_output_tokens),
    });

    if let Some(instructions) = build_instructions(
        request.system_prompt.as_deref(),
        &request.system_prompt_blocks,
    ) {
        body["instructions"] = Value::String(instructions);
    }

    if !request.tools.is_empty() {
        body["parallel_tool_calls"] = Value::Bool(true);
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|tool| {
                    let tool_def = to_openai_tool_def(tool);
                    json!({
                        "type": tool_def.tool_type,
                        "name": tool_def.function.name,
                        "description": tool_def.function.description,
                        "parameters": tool_def.function.parameters,
                    })
                })
                .collect(),
        );
    }

    body
}

pub(super) async fn parse_non_streaming_response(response: reqwest::Response) -> Result<LlmOutput> {
    let payload: Value = response.json().await.map_err(|error| {
        AstrError::http_with_source(
            "failed to parse openai responses payload",
            error.is_timeout() || error.is_connect() || error.is_body(),
            error,
        )
    })?;

    Ok(response_value_to_output(&payload))
}

pub(super) fn consume_sse_text_chunk(
    chunk_text: &str,
    sse_buffer: &mut String,
    accumulator: &mut LlmAccumulator,
    sink: &EventSink,
    completed_output: &mut Option<LlmOutput>,
) -> Result<bool> {
    sse_buffer.push_str(chunk_text);

    while let Some(block_end) = find_sse_block_end(sse_buffer) {
        let mut block = sse_buffer[..block_end].to_string();
        let drain_len = if sse_buffer[block_end..].starts_with("\r\n\r\n") {
            4
        } else {
            2
        };
        sse_buffer.drain(..block_end + drain_len);
        trim_trailing_newlines(&mut block);

        if process_sse_block(&block, accumulator, sink, completed_output)? {
            return Ok(true);
        }
    }

    Ok(false)
}

pub(super) fn flush_sse_buffer(
    sse_buffer: &mut String,
    accumulator: &mut LlmAccumulator,
    sink: &EventSink,
    completed_output: &mut Option<LlmOutput>,
) -> Result<()> {
    if sse_buffer.trim().is_empty() {
        sse_buffer.clear();
        return Ok(());
    }

    let block = sse_buffer.clone();
    sse_buffer.clear();
    let _ = process_sse_block(block.trim(), accumulator, sink, completed_output)?;
    Ok(())
}

fn build_instructions(
    system_prompt: Option<&str>,
    system_prompt_blocks: &[astrcode_runtime_contract::prompt::SystemPromptBlock],
) -> Option<String> {
    if !system_prompt_blocks.is_empty() {
        let rendered = system_prompt_blocks
            .iter()
            .map(astrcode_runtime_contract::prompt::SystemPromptBlock::render)
            .collect::<Vec<_>>()
            .join("\n\n");
        return (!rendered.is_empty()).then_some(rendered);
    }

    system_prompt
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn build_input_items(messages: &[LlmMessage]) -> Vec<Value> {
    let mut items = Vec::new();

    for message in messages {
        match message {
            LlmMessage::User { content, .. } => {
                items.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": [{
                        "type": "input_text",
                        "text": content,
                    }],
                }));
            },
            LlmMessage::Assistant {
                content,
                tool_calls,
                reasoning: _,
            } => {
                if !content.is_empty() {
                    items.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": content,
                        }],
                    }));
                }

                for call in tool_calls {
                    items.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.args.to_string(),
                    }));
                }
            },
            LlmMessage::Tool {
                tool_call_id,
                content,
            } => {
                items.push(json!({
                    "type": "function_call_output",
                    "call_id": tool_call_id,
                    "output": content,
                }));
            },
            LlmMessage::System { content, .. } => {
                items.push(json!({
                    "type": "message",
                    "role": "system",
                    "content": [{
                        "type": "input_text",
                        "text": content,
                    }],
                }));
            },
        }
    }

    items
}

fn response_value_to_output(payload: &Value) -> LlmOutput {
    let usage = payload.get("usage").and_then(parse_usage);
    let tool_calls = payload
        .get("output")
        .and_then(Value::as_array)
        .map(|items| parse_tool_calls(items))
        .unwrap_or_default();

    let content = payload
        .get("output_text")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| extract_output_text(payload.get("output")));

    let reasoning = extract_reasoning_text(payload.get("output")).map(|content| ReasoningContent {
        content,
        signature: None,
    });

    LlmOutput {
        finish_reason: infer_finish_reason(payload, &tool_calls),
        content,
        tool_calls,
        reasoning,
        usage,
        prompt_cache_diagnostics: None,
    }
}

fn parse_tool_calls(items: &[Value]) -> Vec<ToolCallRequest> {
    items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
        .filter_map(|item| {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .or_else(|| item.get("id").and_then(Value::as_str))?;
            let name = item.get("name").and_then(Value::as_str)?;
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");

            Some(ToolCallRequest {
                id: call_id.to_string(),
                name: name.to_string(),
                args: serde_json::from_str::<Value>(arguments)
                    .unwrap_or_else(|_| Value::String(arguments.to_string())),
            })
        })
        .collect()
}

fn extract_output_text(output: Option<&Value>) -> String {
    output
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| {
            item.get("type").and_then(Value::as_str) == Some("message")
                && item.get("role").and_then(Value::as_str) == Some("assistant")
        })
        .flat_map(|item| {
            item.get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|part| match part.get("type").and_then(Value::as_str) {
            Some("output_text") | Some("text") | Some("input_text") => {
                part.get("text").and_then(Value::as_str)
            },
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn extract_reasoning_text(output: Option<&Value>) -> Option<String> {
    let mut parts = Vec::new();

    for item in output.and_then(Value::as_array).into_iter().flatten() {
        if item.get("type").and_then(Value::as_str) == Some("reasoning") {
            if let Some(summary) = item.get("summary").and_then(Value::as_array) {
                parts.extend(
                    summary
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str)),
                );
            }
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                parts.push(text);
            }
        }
    }

    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn infer_finish_reason(payload: &Value, tool_calls: &[ToolCallRequest]) -> FinishReason {
    if !tool_calls.is_empty() {
        return FinishReason::ToolCalls;
    }

    let incomplete_reason = payload
        .get("incomplete_details")
        .and_then(|value| value.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or_default();

    if incomplete_reason.contains("max_output_tokens") || incomplete_reason.contains("max_tokens") {
        return FinishReason::MaxTokens;
    }

    FinishReason::Stop
}

fn parse_usage(value: &Value) -> Option<LlmUsage> {
    Some(LlmUsage {
        input_tokens: value.get("input_tokens")?.as_u64()? as usize,
        output_tokens: value
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default() as usize,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: value
            .get("input_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or_default() as usize,
    })
}

fn find_sse_block_end(buffer: &str) -> Option<usize> {
    buffer.find("\n\n").or_else(|| buffer.find("\r\n\r\n"))
}

fn trim_trailing_newlines(block: &mut String) {
    while block.ends_with('\n') || block.ends_with('\r') {
        block.pop();
    }
}

fn process_sse_block(
    block: &str,
    accumulator: &mut LlmAccumulator,
    sink: &EventSink,
    completed_output: &mut Option<LlmOutput>,
) -> Result<bool> {
    let mut event_name: Option<&str> = None;
    let mut data_lines = Vec::new();

    for raw_line in block.lines() {
        let line = raw_line.trim_end_matches('\r');
        if let Some(value) = line.strip_prefix("event:") {
            event_name = Some(value.trim());
        } else if let Some(value) = line.strip_prefix("data:") {
            data_lines.push(value.trim_start());
        }
    }

    if data_lines.is_empty() {
        return Ok(false);
    }

    let data = data_lines.join("\n");
    if data == "[DONE]" {
        return Ok(true);
    }

    let payload: Value = serde_json::from_str(&data)
        .map_err(|error| AstrError::parse("failed to parse openai responses sse payload", error))?;
    let event_type = payload
        .get("type")
        .and_then(Value::as_str)
        .or(event_name)
        .unwrap_or_default();

    match event_type {
        "response.output_text.delta" => {
            if let Some(delta) = payload.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    emit_event(LlmEvent::TextDelta(delta.to_string()), accumulator, sink);
                }
            }
        },
        "response.function_call_arguments.done" => {
            let index = payload
                .get("output_index")
                .and_then(Value::as_u64)
                .unwrap_or_default() as usize;
            let id = payload
                .get("call_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let arguments = payload
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();

            emit_event(
                LlmEvent::ToolCallDelta {
                    index,
                    id,
                    name,
                    arguments_delta: arguments,
                },
                accumulator,
                sink,
            );
        },
        "response.reasoning_summary_text.delta"
        | "response.reasoning_summary.delta"
        | "response.reasoning_text.delta" => {
            if let Some(delta) = payload.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    emit_event(
                        LlmEvent::ThinkingDelta(delta.to_string()),
                        accumulator,
                        sink,
                    );
                }
            }
        },
        "response.reasoning_summary_part.done" | "response.reasoning_summary.done" => {
            if let Some(text) = payload.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    emit_event(LlmEvent::ThinkingDelta(text.to_string()), accumulator, sink);
                }
            }
        },
        "response.completed" => {
            if let Some(response) = payload.get("response") {
                *completed_output = Some(response_value_to_output(response));
            }
            return Ok(true);
        },
        "response.failed" => {
            let message = payload
                .get("response")
                .and_then(|value| value.get("error"))
                .and_then(|value| value.get("message"))
                .and_then(Value::as_str)
                .or_else(|| {
                    payload
                        .get("error")
                        .and_then(|value| value.get("message"))
                        .and_then(Value::as_str)
                })
                .unwrap_or("openai responses stream failed");
            return Err(AstrError::LlmStreamError(message.to_string()));
        },
        _ => {},
    }

    Ok(false)
}

#[cfg(test)]
mod tests {
    use astrcode_core::{LlmMessage, SystemMessageOrigin};

    use super::*;

    #[test]
    fn responses_output_maps_message_and_function_call() {
        let payload = json!({
            "output_text": "Final answer",
            "output": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "Final answer" }]
                },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "search",
                    "arguments": "{\"q\":\"hello\"}"
                }
            ],
            "usage": {
                "input_tokens": 12,
                "output_tokens": 4,
                "input_tokens_details": { "cached_tokens": 3 }
            }
        });

        let output = response_value_to_output(&payload);
        assert_eq!(output.content, "Final answer");
        assert_eq!(output.tool_calls.len(), 1);
        assert_eq!(output.tool_calls[0].id, "call_1");
        assert_eq!(output.usage.expect("usage").cache_read_input_tokens, 3);
        assert_eq!(output.finish_reason, FinishReason::ToolCalls);
    }

    #[test]
    fn responses_input_items_preserve_system_message_role() {
        let items = build_input_items(&[LlmMessage::System {
            content: "Plan mode instructions".to_string(),
            origin: SystemMessageOrigin::Mode {
                mode_id: "plan".to_string(),
            },
        }]);

        assert_eq!(
            items,
            vec![json!({
                "type": "message",
                "role": "system",
                "content": [{
                    "type": "input_text",
                    "text": "Plan mode instructions",
                }],
            })]
        );
    }

    #[test]
    fn responses_sse_emits_text_and_completes() {
        let sink_events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = crate::sink_collector(sink_events.clone());
        let mut accumulator = LlmAccumulator::default();
        let mut buffer = String::new();
        let mut completed = None;

        let done = consume_sse_text_chunk(
            "event: response.output_text.delta\ndata: \
             {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\nevent: \
             response.completed\ndata: \
             {\"type\":\"response.completed\",\"response\":{\"output_text\":\"Hi\",\"output\":[{\"\
             type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"\
             text\":\"Hi\"}]}]}}\n\n",
            &mut buffer,
            &mut accumulator,
            &sink,
            &mut completed,
        )
        .expect("stream should parse");

        assert!(done);
        assert_eq!(completed.expect("completed output").content, "Hi");
        assert_eq!(
            sink_events.lock().expect("lock").as_slice(),
            &[LlmEvent::TextDelta("Hi".to_string())]
        );
    }
}
