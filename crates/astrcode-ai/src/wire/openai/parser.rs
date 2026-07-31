//! OpenAI-compatible SSE parsing and event accumulation.

use std::collections::BTreeMap;

use astrcode_core::{
    config::OpenAiApiMode,
    llm::{LlmEvent, LlmTokenUsage, LlmTokenUsageSource},
};
use tokio::sync::mpsc;

use crate::{
    common::{send_event, stream_text_delta, token_usage_has_value},
    stream_decoder::clean_json_fragment,
};

// ─── StandardAccumulator ────────────────────────────────────────────────

#[derive(Debug, Default)]
struct ToolCallPartial {
    id: Option<String>,
    emitted_call_id: Option<String>,
    name: Option<String>,
    started: bool,
    completed: bool,
    pending_arguments: String,
}

#[derive(Debug, Default)]
struct ResponseToolCallPartial {
    call_id: Option<String>,
    name: Option<String>,
    started: bool,
    completed: bool,
    arguments_delta_seen: bool,
    pending_arguments: String,
}

/// 标准 OpenAI 格式的流累积器。
#[derive(Default)]
pub(crate) struct StandardAccumulator {
    text: String,
    tool_calls: BTreeMap<u64, ToolCallPartial>,
    response_tool_items: BTreeMap<String, ResponseToolCallPartial>,
    done_sent: bool,
    finish_reason: Option<String>,
    cache_usage_reported: bool,
    /// 累计的 reasoning 文本，用于 diff 提取增量。
    reasoning_accumulated: String,
}

impl StandardAccumulator {
    #[cfg(test)]
    pub fn text(&self) -> &str {
        &self.text
    }

    fn ingest_tool_call_like_delta(
        &mut self,
        index: u64,
        id: Option<&str>,
        fallback_id: Option<&str>,
        function: &serde_json::Value,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) {
        let partial = self.tool_calls.entry(index).or_default();
        if let Some(id) = id {
            partial.id = Some(id.to_string());
        } else if partial.id.is_none() {
            if let Some(fallback) = fallback_id {
                partial.id = Some(fallback.to_string());
            }
        }
        if let Some(name) = function["name"].as_str() {
            partial.name = Some(name.to_string());
        }

        let arguments = function.get("arguments").and_then(json_argument_fragment);
        if !partial.started {
            if let Some(name) = partial.name.clone() {
                let call_id = chat_tool_call_id(index, partial);
                partial.emitted_call_id = Some(call_id.clone());
                partial.started = true;
                send_event(
                    tx,
                    LlmEvent::ToolCallStart {
                        call_id: call_id.clone(),
                        name,
                        arguments: String::new(),
                    },
                );
                if !partial.pending_arguments.is_empty() {
                    let delta = std::mem::take(&mut partial.pending_arguments);
                    send_event(tx, LlmEvent::ToolCallDelta { call_id, delta });
                }
            }
        }

        if let Some(arguments) = arguments {
            if arguments.is_empty() {
                return;
            }
            if partial.started {
                send_event(
                    tx,
                    LlmEvent::ToolCallDelta {
                        call_id: chat_tool_call_id(index, partial),
                        delta: arguments,
                    },
                );
            } else {
                partial.pending_arguments.push_str(&arguments);
            }
        }
    }

    fn ingest_chat_tool_call_delta(
        &mut self,
        index: u64,
        tool_call: &serde_json::Value,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) {
        let id = tool_call["id"].as_str();
        let Some(function) = tool_call.get("function") else {
            return;
        };
        self.ingest_tool_call_like_delta(index, id, None, function, tx);
    }

    fn ingest_legacy_function_call_delta(
        &mut self,
        function_call: &serde_json::Value,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) {
        self.ingest_tool_call_like_delta(0, None, Some("function_call"), function_call, tx);
    }

    fn emit_response_tool_start(
        &mut self,
        item_id: &str,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) -> Option<String> {
        let partial = self
            .response_tool_items
            .entry(item_id.to_string())
            .or_default();
        if partial.started {
            return partial.call_id.clone();
        }
        let name = partial.name.clone()?;
        let call_id = partial
            .call_id
            .clone()
            .unwrap_or_else(|| item_id.to_string());
        partial.started = true;
        send_event(
            tx,
            LlmEvent::ToolCallStart {
                call_id: call_id.clone(),
                name,
                arguments: String::new(),
            },
        );
        if !partial.pending_arguments.is_empty() {
            let delta = std::mem::take(&mut partial.pending_arguments);
            send_event(
                tx,
                LlmEvent::ToolCallDelta {
                    call_id: call_id.clone(),
                    delta,
                },
            );
        }
        Some(call_id)
    }
}

impl StandardAccumulator {
    pub(crate) fn ingest_chat_completion(
        &mut self,
        event: &serde_json::Value,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) {
        if !self.cache_usage_reported {
            if let Some(usage) = extract_token_usage(event) {
                send_event(tx, LlmEvent::Usage { usage });
                self.cache_usage_reported = true;
            }
        }
        if let Some(choices) = event["choices"].as_array() {
            for choice in choices {
                if let Some(delta) = choice.get("delta") {
                    if let Some(content) = delta["content"].as_str() {
                        if let Some(incremental) = stream_text_delta(&mut self.text, content) {
                            send_event(tx, LlmEvent::ContentDelta { delta: incremental });
                        }
                    }
                    if let Some(reasoning) = delta
                        .get("reasoning_content")
                        .or_else(|| delta.get("reasoning"))
                        .or_else(|| delta.get("thinking"))
                        .and_then(|value| value.as_str())
                    {
                        if let Some(incremental) =
                            stream_text_delta(&mut self.reasoning_accumulated, reasoning)
                        {
                            send_event(tx, LlmEvent::ThinkingDelta { delta: incremental });
                        }
                    }
                    // Some providers emit cumulative reasoning_details[].text.
                    if let Some(details) = delta.get("reasoning_details").and_then(|v| v.as_array())
                    {
                        let latest = details
                            .iter()
                            .filter_map(|d| d.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("");
                        if let Some(incremental) =
                            stream_text_delta(&mut self.reasoning_accumulated, &latest)
                        {
                            send_event(tx, LlmEvent::ThinkingDelta { delta: incremental });
                        }
                    }
                    if let Some(tool_calls) = delta["tool_calls"].as_array() {
                        for tc in tool_calls {
                            let idx = tc["index"].as_u64().unwrap_or(0);
                            self.ingest_chat_tool_call_delta(idx, tc, tx);
                        }
                    }
                    if let Some(function_call) = delta.get("function_call") {
                        self.ingest_legacy_function_call_delta(function_call, tx);
                    }
                }
                if let Some(finish) = choice["finish_reason"].as_str() {
                    self.finish_reason = Some(finish.to_string());
                }
            }
        }
    }

    pub(crate) fn ingest_responses(
        &mut self,
        event: &serde_json::Value,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) {
        if !self.cache_usage_reported {
            if let Some(usage) = extract_token_usage(event) {
                send_event(tx, LlmEvent::Usage { usage });
                self.cache_usage_reported = true;
            }
        }
        let Some(event_type) = event["type"].as_str() else {
            return;
        };
        match event_type {
            "response.output_text.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    if let Some(incremental) = stream_text_delta(&mut self.text, delta) {
                        send_event(tx, LlmEvent::ContentDelta { delta: incremental });
                    }
                }
            },
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(delta) = event["delta"].as_str() {
                    if let Some(incremental) =
                        stream_text_delta(&mut self.reasoning_accumulated, delta)
                    {
                        send_event(tx, LlmEvent::ThinkingDelta { delta: incremental });
                    }
                }
            },
            "response.output_item.added" => {
                let Some(item) = event["item"].as_object() else {
                    return;
                };
                if item.get("type").and_then(|v| v.as_str()) != Some("function_call") {
                    return;
                }
                let item_id = item
                    .get("id")
                    .and_then(|v| v.as_str())
                    .or_else(|| event["item_id"].as_str())
                    .unwrap_or_default()
                    .to_string();
                let call_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or(item_id.as_str())
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let partial = self.response_tool_items.entry(item_id.clone()).or_default();
                partial.call_id = Some(call_id);
                partial.name = Some(name);
                let item_arguments = item.get("arguments").and_then(json_argument_fragment);
                let started_call_id = self.emit_response_tool_start(&item_id, tx);
                if let (Some(call_id), Some(arguments)) = (started_call_id, item_arguments) {
                    if !arguments.is_empty() {
                        let partial = self.response_tool_items.entry(item_id.clone()).or_default();
                        partial.arguments_delta_seen = true;
                        send_event(
                            tx,
                            LlmEvent::ToolCallDelta {
                                call_id,
                                delta: arguments,
                            },
                        );
                    }
                }
            },
            "response.function_call_arguments.delta" => {
                let item_id = event["item_id"].as_str().unwrap_or_default();
                if let Some(delta) = event.get("delta").and_then(json_argument_fragment) {
                    if delta.is_empty() {
                        return;
                    }
                    let call_id = self
                        .response_tool_items
                        .get(item_id)
                        .and_then(|p| p.call_id.clone())
                        .unwrap_or_else(|| item_id.to_string());
                    let partial = self
                        .response_tool_items
                        .entry(item_id.to_string())
                        .or_default();
                    partial.arguments_delta_seen = true;
                    if partial.started {
                        send_event(tx, LlmEvent::ToolCallDelta { call_id, delta });
                    } else {
                        partial.pending_arguments.push_str(&delta);
                    }
                }
            },
            "response.function_call_arguments.done" => {
                let item_id = event["item_id"].as_str().unwrap_or_default().to_string();
                let partial = self.response_tool_items.entry(item_id.clone()).or_default();
                if let Some(call_id) = event["call_id"].as_str() {
                    partial.call_id = Some(call_id.to_string());
                }
                if let Some(name) = event["name"].as_str() {
                    partial.name = Some(name.to_string());
                }
                let fallback_call_id = partial.call_id.clone().unwrap_or_else(|| item_id.clone());
                let call_id = if partial.started {
                    fallback_call_id
                } else {
                    self.emit_response_tool_start(&item_id, tx)
                        .unwrap_or(fallback_call_id)
                };
                let partial = self.response_tool_items.entry(item_id).or_default();
                if !partial.arguments_delta_seen {
                    if let Some(arguments) = event.get("arguments").and_then(json_argument_fragment)
                    {
                        if arguments.is_empty() {
                            return;
                        }
                        send_event(
                            tx,
                            LlmEvent::ToolCallDelta {
                                call_id: call_id.clone(),
                                delta: arguments,
                            },
                        );
                    }
                }
                partial.completed = true;
                send_event(tx, LlmEvent::ToolCallCompleted { call_id });
            },
            "response.completed" if !self.done_sent => {
                self.emit_pending_tool_completions(tx);
                self.done_sent = true;
                send_event(
                    tx,
                    LlmEvent::Done {
                        finish_reason: "stop".into(),
                    },
                );
            },
            _ => {},
        }
    }

    pub(crate) fn done_sent(&self) -> bool {
        self.done_sent
    }

    fn finish_reason(&self) -> Option<&str> {
        self.finish_reason.as_deref()
    }

    fn mark_done(&mut self) {
        self.done_sent = true;
    }

    fn emit_pending_tool_completions(&mut self, tx: &mpsc::UnboundedSender<LlmEvent>) {
        for partial in self.tool_calls.values_mut() {
            if partial.started && !partial.completed {
                let call_id = partial
                    .emitted_call_id
                    .clone()
                    .or_else(|| partial.id.clone())
                    .unwrap_or_default();
                if !call_id.is_empty() && !send_event(tx, LlmEvent::ToolCallCompleted { call_id }) {
                    return;
                }
                partial.completed = true;
            }
        }
        for (item_id, partial) in &mut self.response_tool_items {
            if partial.started && !partial.completed {
                let call_id = partial.call_id.clone().unwrap_or_else(|| item_id.clone());
                if !send_event(tx, LlmEvent::ToolCallCompleted { call_id }) {
                    return;
                }
                partial.completed = true;
            }
        }
    }
}

// ─── SSE 行处理 ─────────────────────────────────────────────────────────

pub(crate) fn process_sse_line(
    line: &str,
    accumulator: &mut StandardAccumulator,
    api_mode: OpenAiApiMode,
    tx: &mpsc::UnboundedSender<LlmEvent>,
) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    let Some(after_prefix) = trimmed.strip_prefix("data:") else {
        return;
    };
    let data = after_prefix.trim_start();
    if data == "[DONE]" {
        emit_done_once(accumulator, tx);
        return;
    }

    process_sse_data(data, accumulator, api_mode, tx);
}

pub(crate) fn emit_done_once(
    accumulator: &mut StandardAccumulator,
    tx: &mpsc::UnboundedSender<LlmEvent>,
) {
    if accumulator.done_sent() {
        return;
    }
    accumulator.emit_pending_tool_completions(tx);
    accumulator.mark_done();
    let finish_reason = accumulator.finish_reason().unwrap_or("stop").to_string();
    send_event(tx, LlmEvent::Done { finish_reason });
}

fn process_sse_data(
    data: &str,
    accumulator: &mut StandardAccumulator,
    api_mode: OpenAiApiMode,
    tx: &mpsc::UnboundedSender<LlmEvent>,
) {
    if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
        ingest_sse_event(&event, accumulator, api_mode, tx);
        return;
    }

    let cleaned = clean_json_fragment(data);
    if cleaned != data {
        if let Ok(event) = serde_json::from_str::<serde_json::Value>(&cleaned) {
            ingest_sse_event(&event, accumulator, api_mode, tx);
            return;
        }
    }

    let api_mode_name = match api_mode {
        OpenAiApiMode::ChatCompletions => "Chat Completions",
        OpenAiApiMode::Responses => "Responses",
    };
    tracing::warn!(
        "Failed to parse {} SSE data: {} bytes, preview: {:?}",
        api_mode_name,
        data.len(),
        &data[..data.len().min(80)]
    );
}

fn ingest_sse_event(
    event: &serde_json::Value,
    accumulator: &mut StandardAccumulator,
    api_mode: OpenAiApiMode,
    tx: &mpsc::UnboundedSender<LlmEvent>,
) {
    if emit_stream_error(event, accumulator, tx) {
        return;
    }
    match api_mode {
        OpenAiApiMode::ChatCompletions => accumulator.ingest_chat_completion(event, tx),
        OpenAiApiMode::Responses => accumulator.ingest_responses(event, tx),
    }
}

// ─── 辅助函数 ──────────────────────────────────────────────────────────

fn emit_stream_error(
    event: &serde_json::Value,
    accumulator: &mut StandardAccumulator,
    tx: &mpsc::UnboundedSender<LlmEvent>,
) -> bool {
    if !is_stream_error_event(event) {
        return false;
    }

    accumulator.mark_done();
    send_event(
        tx,
        LlmEvent::Error {
            message: stream_error_message(event).unwrap_or_else(|| event.to_string()),
        },
    );
    true
}

fn is_stream_error_event(event: &serde_json::Value) -> bool {
    event.get("error").is_some_and(|value| !value.is_null())
        || event
            .pointer("/response/error")
            .is_some_and(|value| !value.is_null())
        || event.get("type").and_then(|value| value.as_str()) == Some("error")
        || event.get("type").and_then(|value| value.as_str()) == Some("response.failed")
}

fn stream_error_message(event: &serde_json::Value) -> Option<String> {
    event
        .pointer("/error/message")
        .or_else(|| event.pointer("/response/error/message"))
        .or_else(|| event.get("message"))
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .or_else(|| {
            event
                .get("error")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
}

fn chat_tool_call_id(index: u64, partial: &ToolCallPartial) -> String {
    partial
        .emitted_call_id
        .clone()
        .or_else(|| partial.id.clone())
        .unwrap_or_else(|| index.to_string())
}

fn json_argument_fragment(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(text) => Some(clean_json_fragment(text)),
        other => serde_json::to_string(other).ok(),
    }
}

fn extract_token_usage(event: &serde_json::Value) -> Option<LlmTokenUsage> {
    let usage = event
        .get("usage")
        .or_else(|| event.pointer("/response/usage"))?;

    let input_tokens = usage
        .get("prompt_tokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(|v| v.as_u64());
    let cached_input_tokens = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .or_else(|| usage.pointer("/input_tokens_details/cached_tokens"))
        .and_then(|v| v.as_u64());
    let output_tokens = usage
        .get("completion_tokens")
        .or_else(|| usage.get("output_tokens"))
        .and_then(|v| v.as_u64());
    let reasoning_output_tokens = usage
        .pointer("/completion_tokens_details/reasoning_tokens")
        .or_else(|| usage.pointer("/output_tokens_details/reasoning_tokens"))
        .and_then(|v| v.as_u64());
    let total_tokens = usage.get("total_tokens").and_then(|v| v.as_u64());

    let usage = LlmTokenUsage {
        input_tokens,
        cached_input_tokens,
        cache_creation_input_tokens: None,
        output_tokens,
        reasoning_output_tokens,
        total_tokens,
        source: Some(LlmTokenUsageSource::ProviderUsage),
    };
    if token_usage_has_value(&usage) {
        Some(usage)
    } else {
        None
    }
}

#[cfg(test)]
mod fixture_tests {
    use super::*;

    fn parse_fixture(api_mode: OpenAiApiMode, fixture: &str) -> Vec<LlmEvent> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut accumulator = StandardAccumulator::default();
        for line in fixture.lines() {
            process_sse_line(line, &mut accumulator, api_mode, &tx);
        }
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn openai_compatible_chat_fixture_streams_tool_call() {
        let events = parse_fixture(
            OpenAiApiMode::ChatCompletions,
            include_str!("../../../tests/fixtures/openai_compatible_tool_call.sse"),
        );

        assert!(matches!(
            events.as_slice(),
            [
                LlmEvent::ContentDelta { delta },
                LlmEvent::ToolCallStart { call_id, name, .. },
                LlmEvent::ToolCallDelta { call_id: delta_call_id, delta: first_delta },
                LlmEvent::ToolCallDelta { call_id: second_delta_call_id, delta: second_delta },
                LlmEvent::ToolCallCompleted { call_id: completed_call_id },
                LlmEvent::Done { finish_reason },
            ]
            if delta == "I will inspect it."
                && call_id == "call_read"
                && name == "read"
                && delta_call_id == "call_read"
                && first_delta == "{\"path\""
                && second_delta_call_id == "call_read"
                && second_delta == ":\"Cargo.toml\"}"
                && completed_call_id == "call_read"
                && finish_reason == "tool_calls"
        ));
    }

    #[test]
    fn qwen_chat_fixture_streams_reasoning_alias() {
        let events = parse_fixture(
            OpenAiApiMode::ChatCompletions,
            include_str!("../../../tests/fixtures/qwen_reasoning_chat.sse"),
        );

        assert!(matches!(
            events.as_slice(),
            [
                LlmEvent::ThinkingDelta { delta: thinking },
                LlmEvent::ContentDelta { delta: content },
                LlmEvent::Done { finish_reason },
            ]
            if thinking == "先看上下文。"
                && content == "可以这样做。"
                && finish_reason == "stop"
        ));
    }

    #[test]
    fn ark_responses_fixture_streams_tool_call_and_usage() {
        let events = parse_fixture(
            OpenAiApiMode::Responses,
            include_str!("../../../tests/fixtures/ark_responses_tool_call.sse"),
        );

        assert!(matches!(
            events.as_slice(),
            [
                LlmEvent::ContentDelta { delta },
                LlmEvent::ToolCallStart { call_id, name, .. },
                LlmEvent::ToolCallDelta { call_id: delta_call_id, delta: arguments_delta },
                LlmEvent::ToolCallCompleted { call_id: completed_call_id },
                LlmEvent::Usage { usage },
                LlmEvent::Done { finish_reason },
            ]
            if delta == "我来读文件。"
                && call_id == "call_1"
                && name == "read"
                && delta_call_id == "call_1"
                && arguments_delta == "{\"path\""
                && completed_call_id == "call_1"
                && usage.input_tokens == Some(10)
                && usage.output_tokens == Some(5)
                && usage.total_tokens == Some(15)
                && finish_reason == "stop"
        ));
    }
}

/// 直接针对 [`StandardAccumulator`] / [`process_sse_line`] 的单元测试。
///
/// 这些场景覆盖 fixture（端到端 SSE）之外的累积语义：usage 与 done 的先后顺序、
/// 迟到的 tool-call id、reasoning 别名、累积/增量去重、null-error 载荷、Responses
/// 的参数增量/完成顺序等。
#[cfg(test)]
mod accumulator_tests {
    use super::*;

    fn drain_events(rx: &mut mpsc::UnboundedReceiver<LlmEvent>) -> Vec<LlmEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    #[test]
    fn chat_completion_usage_emits_token_usage() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        acc.ingest_chat_completion(
            &serde_json::json!({
                "usage": {
                    "prompt_tokens": 100,
                    "prompt_tokens_details": {"cached_tokens": 64},
                    "completion_tokens": 20,
                    "completion_tokens_details": {"reasoning_tokens": 5},
                    "total_tokens": 120
                },
                "choices": []
            }),
            &tx,
        );

        let events = drain_events(&mut rx);
        assert!(matches!(
            events.as_slice(),
            [LlmEvent::Usage { usage }]
                if usage.input_tokens == Some(100)
                    && usage.cached_input_tokens == Some(64)
                    && usage.output_tokens == Some(20)
                    && usage.reasoning_output_tokens == Some(5)
                    && usage.total_tokens == Some(120)
                    && usage.source == Some(LlmTokenUsageSource::ProviderUsage)
        ));
    }

    #[test]
    fn chat_completion_usage_after_finish_is_emitted_before_done_marker() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        process_sse_line(
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            &mut acc,
            OpenAiApiMode::ChatCompletions,
            &tx,
        );
        process_sse_line(
            r#"data: {"usage":{"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":64},"completion_tokens":20,"total_tokens":120},"choices":[]}"#,
            &mut acc,
            OpenAiApiMode::ChatCompletions,
            &tx,
        );
        process_sse_line(
            "data: [DONE]",
            &mut acc,
            OpenAiApiMode::ChatCompletions,
            &tx,
        );

        let events = drain_events(&mut rx);
        assert!(matches!(
            events.as_slice(),
            [
                LlmEvent::Usage { usage },
                LlmEvent::Done { finish_reason }
            ] if usage.input_tokens == Some(100)
                && usage.cached_input_tokens == Some(64)
                && usage.output_tokens == Some(20)
                && usage.total_tokens == Some(120)
                && usage.source == Some(LlmTokenUsageSource::ProviderUsage)
                && finish_reason == "stop"
        ));
        assert!(acc.done_sent());
    }

    #[test]
    fn responses_usage_emits_after_initial_delta_without_usage() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        acc.ingest_responses(
            &serde_json::json!({
                "type": "response.output_text.delta",
                "delta": "ok"
            }),
            &tx,
        );
        acc.ingest_responses(
            &serde_json::json!({
                "type": "response.completed",
                "response": {
                    "usage": {
                        "input_tokens": 50,
                        "input_tokens_details": {"cached_tokens": 32},
                        "output_tokens": 10,
                        "output_tokens_details": {"reasoning_tokens": 3},
                        "total_tokens": 60
                    }
                }
            }),
            &tx,
        );

        let events = drain_events(&mut rx);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LlmEvent::ContentDelta { delta } if delta == "ok"))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            LlmEvent::Usage { usage }
                if usage.input_tokens == Some(50)
                    && usage.cached_input_tokens == Some(32)
                    && usage.output_tokens == Some(10)
                    && usage.reasoning_output_tokens == Some(3)
                    && usage.total_tokens == Some(60)
                    && usage.source == Some(LlmTokenUsageSource::ProviderUsage)
        )));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LlmEvent::Done { .. }))
        );
    }

    #[test]
    fn chat_tool_call_buffers_arguments_until_name_arrives() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        acc.ingest_chat_completion(
            &serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "function": {"arguments": "{\"pattern\""}
                }]}}]
            }),
            &tx,
        );
        acc.ingest_chat_completion(
            &serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "function": {"name": "glob", "arguments": ":\"*.rs\"}"}
                }]}}]
            }),
            &tx,
        );

        let events = drain_events(&mut rx);
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallStart { call_id, name, arguments }
            if call_id == "call_1" && name == "glob" && arguments.is_empty()
        )));
        let arguments = events
            .into_iter()
            .filter_map(|e| match e {
                LlmEvent::ToolCallDelta { delta, .. } => Some(delta),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(arguments, "{\"pattern\":\"*.rs\"}");
    }

    #[test]
    fn chat_tool_call_accepts_object_arguments_from_compat_providers() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        acc.ingest_chat_completion(
            &serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "function": {"name": "grep", "arguments": {"pattern": "agent"}}
                }]}}]
            }),
            &tx,
        );

        let events = drain_events(&mut rx);
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallStart { name, .. } if name == "grep"
        )));
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallDelta { delta, .. } if delta == "{\"pattern\":\"agent\"}"
        )));
    }

    #[test]
    fn chat_stream_accepts_reasoning_aliases_from_compat_providers() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        acc.ingest_chat_completion(
            &serde_json::json!({
                "choices": [{"delta": {"reasoning": "plan"}}]
            }),
            &tx,
        );
        acc.ingest_chat_completion(
            &serde_json::json!({
                "choices": [{"delta": {"thinking": " more"}}]
            }),
            &tx,
        );

        let reasoning = drain_events(&mut rx)
            .into_iter()
            .filter_map(|event| match event {
                LlmEvent::ThinkingDelta { delta } => Some(delta),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(reasoning, "plan more");
    }

    #[test]
    fn chat_stream_deduplicates_cumulative_content_and_reasoning() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        acc.ingest_chat_completion(
            &serde_json::json!({
                "choices": [{"delta": {"reasoning": "The"}}]
            }),
            &tx,
        );
        acc.ingest_chat_completion(
            &serde_json::json!({
                "choices": [{"delta": {"reasoning": "The user"}}]
            }),
            &tx,
        );
        acc.ingest_chat_completion(
            &serde_json::json!({
                "choices": [{"delta": {"content": "说实话，"}}]
            }),
            &tx,
        );
        acc.ingest_chat_completion(
            &serde_json::json!({
                "choices": [{"delta": {"content": "说实话，逗人开心"}}]
            }),
            &tx,
        );

        let events = drain_events(&mut rx);
        let thinking: String = events
            .iter()
            .filter_map(|event| match event {
                LlmEvent::ThinkingDelta { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        let content: String = events
            .iter()
            .filter_map(|event| match event {
                LlmEvent::ContentDelta { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, "The user");
        assert_eq!(content, "说实话，逗人开心");
        assert_eq!(acc.text(), "说实话，逗人开心");
    }

    #[test]
    fn chat_tool_call_keeps_call_id_stable_if_provider_sends_id_late() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        acc.ingest_chat_completion(
            &serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "function": {"name": "glob"}
                }]}}]
            }),
            &tx,
        );
        acc.ingest_chat_completion(
            &serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "id": "late_id",
                    "function": {"arguments": "{\"pattern\":\"*.rs\"}"}
                }]}}]
            }),
            &tx,
        );

        let events = drain_events(&mut rx);
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallStart { call_id, .. } if call_id == "0"
        )));
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallDelta { call_id, delta }
            if call_id == "0" && delta == "{\"pattern\":\"*.rs\"}"
        )));
    }

    #[test]
    fn chat_legacy_function_call_streams_arguments() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        acc.ingest_chat_completion(
            &serde_json::json!({
                "choices": [{"delta": {"function_call": {
                    "name": "glob",
                    "arguments": "{\"pattern\":\"*.rs\"}"
                }}}]
            }),
            &tx,
        );

        let events = drain_events(&mut rx);
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallStart { call_id, name, arguments }
            if call_id == "function_call" && name == "glob" && arguments.is_empty()
        )));
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallDelta { call_id, delta }
            if call_id == "function_call" && delta == "{\"pattern\":\"*.rs\"}"
        )));
    }

    #[test]
    fn streaming_error_payload_emits_error_without_done() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        process_sse_line(
            r#"data: {"error":{"message":"compat provider rejected request"}}"#,
            &mut acc,
            OpenAiApiMode::ChatCompletions,
            &tx,
        );

        let events = drain_events(&mut rx);
        assert!(matches!(
            events.as_slice(),
            [LlmEvent::Error { message }] if message == "compat provider rejected request"
        ));
        assert!(acc.done_sent());
    }

    #[test]
    fn streaming_null_error_payload_is_not_treated_as_error() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        process_sse_line(
            r#"data: {"error":null,"choices":[{"delta":{"content":"ok"}}]}"#,
            &mut acc,
            OpenAiApiMode::ChatCompletions,
            &tx,
        );

        let events = drain_events(&mut rx);
        assert!(matches!(
            events.as_slice(),
            [LlmEvent::ContentDelta { delta }] if delta == "ok"
        ));
        assert!(!acc.done_sent());
    }

    #[test]
    fn responses_delta_then_done_does_not_replay_arguments() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        acc.ingest_responses(
            &serde_json::json!({
                "type": "response.output_item.added",
                "item": { "type": "function_call", "id": "i1", "call_id": "c1", "name": "r" }
            }),
            &tx,
        );
        acc.ingest_responses(
            &serde_json::json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "i1", "delta": "{\"path\""
            }),
            &tx,
        );
        acc.ingest_responses(
            &serde_json::json!({
                "type": "response.function_call_arguments.done",
                "item_id": "i1", "arguments": "{\"path\":\"Cargo.toml\"}"
            }),
            &tx,
        );

        let deltas: Vec<_> = drain_events(&mut rx)
            .into_iter()
            .filter_map(|e| match e {
                LlmEvent::ToolCallDelta { delta, .. } => Some(delta),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["{\"path\""]);
    }

    #[test]
    fn responses_done_then_completed_does_not_duplicate_tool_completion() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        acc.ingest_responses(
            &serde_json::json!({
                "type": "response.output_item.added",
                "item": { "type": "function_call", "id": "i1", "call_id": "c1", "name": "read" }
            }),
            &tx,
        );
        acc.ingest_responses(
            &serde_json::json!({
                "type": "response.function_call_arguments.done",
                "item_id": "i1",
                "arguments": "{\"path\":\"Cargo.toml\"}"
            }),
            &tx,
        );
        acc.ingest_responses(&serde_json::json!({"type": "response.completed"}), &tx);

        let completed_count = drain_events(&mut rx)
            .into_iter()
            .filter(
                |event| matches!(event, LlmEvent::ToolCallCompleted { call_id } if call_id == "c1"),
            )
            .count();
        assert_eq!(completed_count, 1);
    }

    #[test]
    fn responses_completed_completes_started_tool_calls() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        acc.ingest_responses(
            &serde_json::json!({
                "type": "response.output_item.added",
                "item": { "type": "function_call", "id": "i1", "call_id": "c1", "name": "read" }
            }),
            &tx,
        );
        acc.ingest_responses(&serde_json::json!({"type": "response.completed"}), &tx);

        let events = drain_events(&mut rx);
        assert!(events.iter().any(|event| {
            matches!(event, LlmEvent::ToolCallCompleted { call_id } if call_id == "c1")
        }));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LlmEvent::Done { .. }))
        );
    }

    #[test]
    fn responses_arguments_delta_before_item_start_is_not_lost() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        acc.ingest_responses(
            &serde_json::json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "i1",
                "delta": "{\"path\":\"Cargo.toml\"}"
            }),
            &tx,
        );
        acc.ingest_responses(
            &serde_json::json!({
                "type": "response.output_item.added",
                "item": {
                    "type": "function_call",
                    "id": "i1",
                    "call_id": "c1",
                    "name": "read"
                }
            }),
            &tx,
        );

        let events = drain_events(&mut rx);
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallStart { call_id, name, .. }
            if call_id == "c1" && name == "read"
        )));
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallDelta { call_id, delta }
            if call_id == "c1" && delta == "{\"path\":\"Cargo.toml\"}"
        )));
    }

    #[test]
    fn responses_text_delta() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();
        acc.ingest_responses(
            &serde_json::json!({"type": "response.output_text.delta", "delta": "hi"}),
            &tx,
        );
        let events = drain_events(&mut rx);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, LlmEvent::ContentDelta { delta } if delta == "hi"))
        );
    }

    #[test]
    fn responses_stream_accepts_reasoning_delta_and_done_marker() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        process_sse_line(
            r#"data: {"type":"response.reasoning_summary_text.delta","delta":"thinking"}"#,
            &mut acc,
            OpenAiApiMode::Responses,
            &tx,
        );
        process_sse_line("data: [DONE]", &mut acc, OpenAiApiMode::Responses, &tx);

        let events = drain_events(&mut rx);
        assert!(matches!(
            events.as_slice(),
            [
                LlmEvent::ThinkingDelta { delta },
                LlmEvent::Done { finish_reason }
            ] if delta == "thinking" && finish_reason == "stop"
        ));
        assert!(acc.done_sent());
    }

    #[test]
    fn responses_done_without_deltas_still_emits_arguments() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();
        acc.ingest_responses(
            &serde_json::json!({
                "type": "response.function_call_arguments.done",
                "item_id": "i1", "name": "read", "arguments": "{\"path\":\"Cargo.toml\"}"
            }),
            &tx,
        );
        let events = drain_events(&mut rx);
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallStart { call_id, name, .. }
            if call_id == "i1" && name == "read"
        )));
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallDelta { call_id, delta }
            if call_id == "i1" && delta == "{\"path\":\"Cargo.toml\"}"
        )));
    }

    #[test]
    fn responses_done_accepts_object_arguments_from_compat_providers() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();

        acc.ingest_responses(
            &serde_json::json!({
                "type": "response.function_call_arguments.done",
                "item_id": "i1",
                "name": "read",
                "arguments": {"path": "Cargo.toml"}
            }),
            &tx,
        );

        let events = drain_events(&mut rx);
        assert!(events.iter().any(|e| matches!(
            e, LlmEvent::ToolCallDelta { delta, .. } if delta == "{\"path\":\"Cargo.toml\"}"
        )));
    }

    #[test]
    fn responses_completed_emits_done_once() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut acc = StandardAccumulator::default();
        let event = serde_json::json!({"type": "response.completed"});
        acc.ingest_responses(&event, &tx);
        acc.ingest_responses(&event, &tx);
        let count = drain_events(&mut rx)
            .into_iter()
            .filter(|e| matches!(e, LlmEvent::Done { .. }))
            .count();
        assert_eq!(count, 1);
        assert!(acc.done_sent());
    }
}
