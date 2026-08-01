//! Anthropic Messages SSE event state machine.
//!
//! 将解码后的 SSE 行（`event:`/`data:` 配对在此完成）规范化为
//! [`LlmEvent`]。拥有「`Done` 至多发一次」守卫，以及与 OpenAI parser 共享的
//! 累积/增量文本去重（复用 [`crate::common::stream_text_delta`]）。

use std::collections::{HashMap, HashSet};

use astrcode_core::llm::{LlmEvent, LlmTokenUsage, LlmTokenUsageSource};
use tokio::sync::mpsc;

use crate::common::{send_event, stream_text_delta, token_usage_has_value};

/// 流式响应的 `Done` 事件守卫，保证至多发送一次 `Done`。
#[derive(Debug, Default)]
pub(crate) struct StreamEventSink {
    done_sent: bool,
}

impl StreamEventSink {
    pub(crate) fn done_sent(&self) -> bool {
        self.done_sent
    }

    pub(crate) fn emit_done(
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

    pub(crate) fn ensure_done(&mut self, tx: &mpsc::UnboundedSender<LlmEvent>) -> bool {
        self.emit_done(tx, "stop")
    }
}

#[derive(Debug, Default)]
pub(crate) struct AnthropicStreamState {
    pub(crate) sink: StreamEventSink,
    usage_reported: bool,
    /// SSE content block index → actual tool call id。
    index_to_call_id: HashMap<u64, String>,
    block_stream_state: HashMap<u64, BlockStreamState>,
    /// 已开始但尚未收到 `content_block_stop` 的 tool call id；流结束时补发完成事件。
    started_tool_call_ids: HashSet<String>,
}

impl AnthropicStreamState {
    /// 流结束时补发未收到 `content_block_stop` 的工具调用完成事件，防止调用方卡死等待。
    pub(crate) fn emit_pending_tool_completions(
        &mut self,
        tx: &mpsc::UnboundedSender<LlmEvent>,
    ) -> bool {
        for call_id in self.started_tool_call_ids.drain() {
            if !send_event(tx, LlmEvent::ToolCallCompleted { call_id }) {
                return false;
            }
        }
        true
    }
}

/// 读取 content_block 事件的 `index` 字段；缺失视为协议违规，告警后由调用方跳过该事件。
fn block_index(event: &serde_json::Value, what: &str) -> Option<u64> {
    let index = event.get("index").and_then(|v| v.as_u64());
    if index.is_none() {
        tracing::warn!("Anthropic {what} event missing `index`; skipping event");
    }
    index
}

#[derive(Debug, Default)]
struct BlockStreamState {
    text: String,
    thinking: String,
}

fn emit_block_stream_delta(
    state: &mut BlockStreamState,
    tx: &mpsc::UnboundedSender<LlmEvent>,
    fragment: &str,
    is_thinking: bool,
) -> bool {
    let accumulated = if is_thinking {
        &mut state.thinking
    } else {
        &mut state.text
    };
    let Some(incremental) = stream_text_delta(accumulated, fragment) else {
        return true;
    };
    let event = if is_thinking {
        LlmEvent::ThinkingDelta { delta: incremental }
    } else {
        LlmEvent::ContentDelta { delta: incremental }
    };
    send_event(tx, event)
}

fn handle_anthropic_event(
    event_type: &str,
    event: &serde_json::Value,
    tx: &mpsc::UnboundedSender<LlmEvent>,
    state: &mut AnthropicStreamState,
) -> bool {
    match event_type {
        "content_block_start" => {
            if let Some(block) = event.get("content_block") {
                match block.get("type").and_then(|v| v.as_str()) {
                    Some("tool_use") => {
                        let Some(call_id) = block.get("id").and_then(|v| v.as_str()) else {
                            tracing::warn!(
                                "Anthropic content_block_start tool_use missing `id`; skipping \
                                 event"
                            );
                            return true;
                        };
                        let name = match block.get("name").and_then(|v| v.as_str()) {
                            Some(name) => name,
                            None => {
                                tracing::warn!(
                                    "Anthropic content_block_start tool_use missing `name`"
                                );
                                ""
                            },
                        };
                        if let Some(index) = event.get("index").and_then(|v| v.as_u64()) {
                            state.index_to_call_id.insert(index, call_id.to_string());
                        }
                        state.started_tool_call_ids.insert(call_id.to_string());
                        let initial_args = block
                            .get("input")
                            .filter(|v| v.as_object().is_some_and(|obj| !obj.is_empty()))
                            .map(|v| v.to_string())
                            .unwrap_or_default();
                        send_event(
                            tx,
                            LlmEvent::ToolCallStart {
                                call_id: call_id.to_string(),
                                name: name.to_string(),
                                arguments: initial_args,
                            },
                        )
                    },
                    Some("thinking") => {
                        let Some(index) = block_index(event, "content_block_start thinking") else {
                            return true;
                        };
                        state
                            .block_stream_state
                            .insert(index, BlockStreamState::default());
                        if let Some(thinking) = block.get("thinking").and_then(|v| v.as_str()) {
                            if thinking.is_empty() {
                                true
                            } else if let Some(block_state) =
                                state.block_stream_state.get_mut(&index)
                            {
                                emit_block_stream_delta(block_state, tx, thinking, true)
                            } else {
                                true
                            }
                        } else {
                            true
                        }
                    },
                    Some("text") => {
                        let Some(index) = block_index(event, "content_block_start text") else {
                            return true;
                        };
                        state
                            .block_stream_state
                            .insert(index, BlockStreamState::default());
                        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                            if text.is_empty() {
                                true
                            } else if let Some(block_state) =
                                state.block_stream_state.get_mut(&index)
                            {
                                emit_block_stream_delta(block_state, tx, text, false)
                            } else {
                                true
                            }
                        } else {
                            true
                        }
                    },
                    _ => true,
                }
            } else {
                true
            }
        },
        "content_block_delta" => {
            if let Some(delta) = event.get("delta") {
                match delta.get("type").and_then(|v| v.as_str()) {
                    Some("text_delta") => {
                        let Some(index) = block_index(event, "content_block_delta text_delta")
                        else {
                            return true;
                        };
                        if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                            let block_state = state.block_stream_state.entry(index).or_default();
                            emit_block_stream_delta(block_state, tx, text, false)
                        } else {
                            true
                        }
                    },
                    Some("thinking_delta") => {
                        let Some(index) = block_index(event, "content_block_delta thinking_delta")
                        else {
                            return true;
                        };
                        if let Some(thinking) = delta.get("thinking").and_then(|v| v.as_str()) {
                            let block_state = state.block_stream_state.entry(index).or_default();
                            emit_block_stream_delta(block_state, tx, thinking, true)
                        } else {
                            true
                        }
                    },
                    Some("input_json_delta") => {
                        let Some(index) =
                            block_index(event, "content_block_delta input_json_delta")
                        else {
                            return true;
                        };
                        let call_id = state
                            .index_to_call_id
                            .get(&index)
                            .cloned()
                            .unwrap_or_else(|| index.to_string());
                        if let Some(partial) = delta.get("partial_json").and_then(|v| v.as_str()) {
                            send_event(
                                tx,
                                LlmEvent::ToolCallDelta {
                                    call_id,
                                    delta: partial.to_string(),
                                },
                            )
                        } else {
                            true
                        }
                    },
                    _ => true,
                }
            } else {
                true
            }
        },
        "content_block_stop" => {
            let Some(index) = event.get("index").and_then(|v| v.as_u64()) else {
                tracing::warn!(
                    "Anthropic content_block_stop event missing `index`; tool call completion \
                     deferred to stream end"
                );
                return true;
            };
            if let Some(call_id) = state.index_to_call_id.get(&index) {
                state.started_tool_call_ids.remove(call_id);
                return send_event(
                    tx,
                    LlmEvent::ToolCallCompleted {
                        call_id: call_id.clone(),
                    },
                );
            }
            true
        },
        "message_delta" => {
            if !state.usage_reported {
                if let Some(usage) = extract_anthropic_token_usage(event) {
                    if !send_event(tx, LlmEvent::Usage { usage }) {
                        return false;
                    }
                    state.usage_reported = true;
                }
            }
            if let Some(stop_reason) = event.pointer("/delta/stop_reason").and_then(|v| v.as_str())
            {
                state.sink.emit_done(tx, stop_reason)
            } else {
                true
            }
        },
        "message_stop" => state.sink.ensure_done(tx),
        "error" => {
            let message = event
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown Anthropic error")
                .to_string();
            send_event(tx, LlmEvent::Error { message })
        },
        _ => {
            tracing::debug!("Ignoring unknown Anthropic event type: {event_type}");
            true
        },
    }
}

fn extract_anthropic_token_usage(event: &serde_json::Value) -> Option<LlmTokenUsage> {
    let usage = event.get("usage")?;
    let token_usage = LlmTokenUsage {
        input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()),
        cached_input_tokens: usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64()),
        cache_creation_input_tokens: usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64()),
        output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()),
        reasoning_output_tokens: None,
        total_tokens: None,
        source: Some(LlmTokenUsageSource::ProviderUsage),
    };
    token_usage_has_value(&token_usage).then_some(token_usage)
}

/// 处理单行 SSE 输出。返回 `false` 表示接收端已关闭。
///
/// 跟踪 `event:` 行并在每条 `data:` 派发后清空事件类型（Anthropic SSE 语义）；
/// `[DONE]` 与空 `data:` 行被静默跳过。
pub(crate) fn process_sse_line(
    line: &str,
    tx: &mpsc::UnboundedSender<LlmEvent>,
    state: &mut AnthropicStreamState,
    current_event_type: &mut String,
    has_data_line: &mut bool,
) -> bool {
    // event: 行
    if let Some(ev_type) = line.strip_prefix("event:") {
        *current_event_type = ev_type.trim().to_string();
        return true;
    }

    // data: 行
    let Some(data) = line.strip_prefix("data:") else {
        return true;
    };
    *has_data_line = true;
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return true;
    }

    if tx.is_closed() {
        return false;
    }

    match serde_json::from_str::<serde_json::Value>(data) {
        Ok(event) => {
            if !handle_anthropic_event(current_event_type, &event, tx, state) {
                return false;
            }
        },
        Err(error) => {
            tracing::warn!(
                "Failed to parse Anthropic SSE data (event type: {:?}): {} bytes, preview: {:?}: \
                 {error}",
                current_event_type,
                data.len(),
                &data[..data.floor_char_boundary(80)]
            );
        },
    }
    current_event_type.clear();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_event_sink_emits_done_once() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut sink = StreamEventSink::default();
        assert!(sink.emit_done(&tx, "stop"));
        assert!(sink.emit_done(&tx, "stop"));
        assert!(matches!(
            rx.try_recv().unwrap(),
            LlmEvent::Done { finish_reason } if finish_reason == "stop"
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn process_sse_line_resets_event_type_after_each_data_line() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut state = AnthropicStreamState::default();
        let mut current_event_type = String::new();
        let mut has_data_line = false;

        assert!(process_sse_line(
            "event: content_block_start",
            &tx,
            &mut state,
            &mut current_event_type,
            &mut has_data_line,
        ));
        assert_eq!(current_event_type, "content_block_start");

        // 第一条 data: 以记录的 event_type 派发，随后清空。
        assert!(process_sse_line(
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            &tx,
            &mut state,
            &mut current_event_type,
            &mut has_data_line,
        ));
        assert_eq!(current_event_type, "");

        // 空行被忽略。
        assert!(process_sse_line(
            "",
            &tx,
            &mut state,
            &mut current_event_type,
            &mut has_data_line,
        ));

        // 没有 event: 前导的下一条 data: 以空事件类型派发。
        assert!(process_sse_line(
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            &tx,
            &mut state,
            &mut current_event_type,
            &mut has_data_line,
        ));
        assert_eq!(current_event_type, "");
        assert!(has_data_line);
    }

    #[test]
    fn message_delta_emits_done_once() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = AnthropicStreamState::default();
        let event = serde_json::json!({"delta": {"stop_reason": "end_turn"}});

        assert!(handle_anthropic_event(
            "message_delta",
            &event,
            &tx,
            &mut state,
        ));
        assert!(handle_anthropic_event(
            "message_delta",
            &event,
            &tx,
            &mut state,
        ));

        let done_count = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|event| matches!(event, LlmEvent::Done { .. }))
            .count();
        assert_eq!(done_count, 1);
        assert!(state.sink.done_sent());
    }

    #[test]
    fn message_delta_usage_emits_token_usage_before_done() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = AnthropicStreamState::default();
        let event = serde_json::json!({
            "usage": {
                "input_tokens": 100,
                "cache_read_input_tokens": 40,
                "cache_creation_input_tokens": 7,
                "output_tokens": 20
            },
            "delta": {"stop_reason": "end_turn"}
        });

        assert!(handle_anthropic_event(
            "message_delta",
            &event,
            &tx,
            &mut state,
        ));

        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(matches!(
            events.as_slice(),
            [
                LlmEvent::Usage { usage },
                LlmEvent::Done { finish_reason }
            ] if usage.input_tokens == Some(100)
                && usage.cached_input_tokens == Some(40)
                && usage.cache_creation_input_tokens == Some(7)
                && usage.output_tokens == Some(20)
                && usage.reasoning_output_tokens.is_none()
                && usage.total_tokens.is_none()
                && usage.source == Some(LlmTokenUsageSource::ProviderUsage)
                && finish_reason == "end_turn"
        ));
    }

    #[test]
    fn message_stop_emits_done_once() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = AnthropicStreamState::default();

        assert!(handle_anthropic_event(
            "message_stop",
            &serde_json::json!({}),
            &tx,
            &mut state
        ));
        assert!(handle_anthropic_event(
            "message_stop",
            &serde_json::json!({}),
            &tx,
            &mut state
        ));

        let done_count = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|event| matches!(event, LlmEvent::Done { .. }))
            .count();
        assert_eq!(done_count, 1);
        assert!(state.sink.done_sent());
    }

    #[test]
    fn thinking_start_plus_cumulative_delta_does_not_duplicate() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = AnthropicStreamState::default();

        let start = serde_json::json!({
            "index": 0,
            "content_block": {"type": "thinking", "thinking": "The"}
        });
        assert!(handle_anthropic_event(
            "content_block_start",
            &start,
            &tx,
            &mut state,
        ));

        let delta = serde_json::json!({
            "index": 0,
            "delta": {"type": "thinking_delta", "thinking": "The user"}
        });
        assert!(handle_anthropic_event(
            "content_block_delta",
            &delta,
            &tx,
            &mut state,
        ));

        let thinking: String = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|event| match event {
                LlmEvent::ThinkingDelta { delta } => Some(delta),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, "The user");
    }

    #[test]
    fn content_block_delta_missing_index_is_skipped() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = AnthropicStreamState::default();

        let delta = serde_json::json!({
            "delta": {"type": "text_delta", "text": "hi"}
        });
        assert!(handle_anthropic_event(
            "content_block_delta",
            &delta,
            &tx,
            &mut state,
        ));
        assert!(rx.try_recv().is_err(), "缺 index 的增量不应落入默认块");
    }

    #[test]
    fn tool_use_start_missing_id_is_skipped() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = AnthropicStreamState::default();

        let start = serde_json::json!({
            "index": 0,
            "content_block": {"type": "tool_use", "name": "read", "input": {}}
        });
        assert!(handle_anthropic_event(
            "content_block_start",
            &start,
            &tx,
            &mut state,
        ));
        assert!(rx.try_recv().is_err());
        assert!(state.started_tool_call_ids.is_empty());
    }

    #[test]
    fn tool_call_without_stop_is_completed_at_stream_end() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = AnthropicStreamState::default();

        let start = serde_json::json!({
            "index": 0,
            "content_block": {"type": "tool_use", "id": "call_1", "name": "read", "input": {}}
        });
        assert!(handle_anthropic_event(
            "content_block_start",
            &start,
            &tx,
            &mut state,
        ));
        assert!(rx.try_recv().is_ok(), "ToolCallStart 应已发送");

        assert!(state.emit_pending_tool_completions(&tx));
        let completed: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|event| matches!(event, LlmEvent::ToolCallCompleted { .. }))
            .collect();
        assert!(completed.iter().any(
            |event| matches!(event, LlmEvent::ToolCallCompleted { call_id } if call_id == "call_1")
        ));
    }

    #[test]
    fn content_block_stop_removes_tool_call_from_pending() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = AnthropicStreamState::default();

        let start = serde_json::json!({
            "index": 0,
            "content_block": {"type": "tool_use", "id": "call_1", "name": "read"}
        });
        assert!(handle_anthropic_event(
            "content_block_start",
            &start,
            &tx,
            &mut state,
        ));
        let stop = serde_json::json!({"index": 0});
        assert!(handle_anthropic_event(
            "content_block_stop",
            &stop,
            &tx,
            &mut state,
        ));
        let completed: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|event| matches!(event, LlmEvent::ToolCallCompleted { .. }))
            .collect();
        assert!(completed.iter().any(
            |event| matches!(event, LlmEvent::ToolCallCompleted { call_id } if call_id == "call_1")
        ));

        assert!(state.emit_pending_tool_completions(&tx));
        assert!(
            rx.try_recv().is_err(),
            "已收到 stop 的工具调用不应在流结束时重复补发"
        );
    }
}
