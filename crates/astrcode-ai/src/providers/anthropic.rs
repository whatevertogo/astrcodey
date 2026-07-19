//! Anthropic Messages API provider.
//!
//! Implements [`LlmProvider`] for Anthropic's Messages API with SSE streaming,
//! tool use, and thinking support.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use astrcode_core::{llm::*, tool::ToolDefinition};
use tokio::sync::mpsc;

use crate::{
    common::{
        HttpPostRequest, StreamEventSink, apply_auth_header, build_client, ensure_header,
        report_stream_error, retry_policy_from_config, send_event, stream_text_delta,
        stream_with_event_type, token_usage_has_value,
    },
    wire::anthropic as anthropic_wire,
};

pub struct AnthropicProvider {
    config: LlmClientConfig,
    model_id: String,
    model_limits_val: ModelLimits,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(
        config: LlmClientConfig,
        model_id: String,
        max_tokens: Option<u32>,
        context_limit: Option<usize>,
    ) -> Result<Self, LlmError> {
        let client = build_client(&config)?;
        Ok(Self {
            config,
            model_id,
            model_limits_val: ModelLimits {
                max_input_tokens: context_limit.unwrap_or(200_000),
                max_output_tokens: max_tokens.unwrap_or(8192) as usize,
            },
            client,
        })
    }

    fn endpoint(&self) -> String {
        anthropic_wire::endpoint_url(&self.config.base_url)
    }

    fn count_tokens_endpoint(&self) -> String {
        anthropic_wire::count_tokens_endpoint(&self.config.base_url)
    }

    fn wire_config(&self) -> anthropic_wire::AnthropicRequestConfig<'_> {
        anthropic_wire::AnthropicRequestConfig {
            model_id: &self.model_id,
            max_output_tokens: self.model_limits_val.max_output_tokens,
        }
    }

    fn build_request_body(
        &self,
        messages: &[LlmMessage],
        tools: &[ToolDefinition],
        stream: bool,
    ) -> serde_json::Value {
        anthropic_wire::build_request_body(self.wire_config(), messages, tools, stream)
    }

    fn build_count_tokens_body(
        &self,
        messages: &[LlmMessage],
        tools: &[ToolDefinition],
    ) -> serde_json::Value {
        anthropic_wire::build_count_tokens_body(self.wire_config(), messages, tools)
    }

    fn headers(&self) -> Vec<(String, String)> {
        let mut headers: Vec<(String, String)> = self
            .config
            .extra_headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        apply_auth_header(&mut headers, self.config.auth_scheme, &self.config.api_key);
        ensure_header(&mut headers, "anthropic-version", "2023-06-01");
        headers
    }
}

#[async_trait::async_trait]
impl LlmProvider for AnthropicProvider {
    async fn generate(
        &self,
        messages: Vec<LlmMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let request_body = self.build_request_body(&messages, &tools, true);
        let endpoint = self.endpoint();
        let mut headers = self.headers();
        ensure_header(&mut headers, "Accept", "text/event-stream");
        let client = self.client.clone();
        let retry = retry_policy_from_config(&self.config);

        tokio::spawn(async move {
            let stream_state = Arc::new(Mutex::new(AnthropicStreamState::default()));
            let stream_state_for_events = Arc::clone(&stream_state);

            let result = stream_with_event_type(
                client,
                endpoint,
                headers,
                request_body,
                retry,
                tx.clone(),
                move |event_type, event, tx| {
                    let mut state = stream_state_for_events
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    handle_anthropic_event(event_type, event, tx, &mut state)
                },
            )
            .await;
            if result.is_ok() {
                let mut state = stream_state.lock().unwrap_or_else(|e| e.into_inner());
                if !state.sink.done_sent() {
                    state.sink.ensure_done(&tx);
                }
            }
            report_stream_error(result, &tx);
        });

        Ok(rx)
    }

    async fn count_input_tokens(
        &self,
        messages: Vec<LlmMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Result<ProviderInputTokenCount, LlmError> {
        let value = HttpPostRequest {
            client: self.client.clone(),
            endpoint: self.count_tokens_endpoint(),
            headers: self.headers(),
            body: self.build_count_tokens_body(&messages, &tools),
            retry: retry_policy_from_config(&self.config),
        }
        .json()
        .await?;
        let input_tokens = value
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                LlmError::StreamParse(format!(
                    "Anthropic count_tokens response missing input_tokens: {value}"
                ))
            })?;
        Ok(ProviderInputTokenCount::provider_count(input_tokens))
    }

    fn model_limits(&self) -> ModelLimits {
        self.model_limits_val.clone()
    }
}

// ─── SSE event handling ──────────────────────────────────────────────────

#[derive(Debug, Default)]
struct AnthropicStreamState {
    sink: StreamEventSink,
    usage_reported: bool,
    /// SSE content block index → actual tool call id。
    index_to_call_id: HashMap<u64, String>,
    block_stream_state: HashMap<u64, BlockStreamState>,
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
                        let call_id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        if let Some(index) = event.get("index").and_then(|v| v.as_u64()) {
                            state.index_to_call_id.insert(index, call_id.clone());
                        }
                        let initial_args = block
                            .get("input")
                            .filter(|v| v.as_object().is_some_and(|obj| !obj.is_empty()))
                            .map(|v| v.to_string())
                            .unwrap_or_default();
                        send_event(
                            tx,
                            LlmEvent::ToolCallStart {
                                call_id,
                                name,
                                arguments: initial_args,
                            },
                        )
                    },
                    Some("thinking") => {
                        let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
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
                        let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
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
                        let index = event
                            .get("index")
                            .and_then(|v| v.as_u64())
                            .unwrap_or_default();
                        if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                            let block_state = state.block_stream_state.entry(index).or_default();
                            emit_block_stream_delta(block_state, tx, text, false)
                        } else {
                            true
                        }
                    },
                    Some("thinking_delta") => {
                        let index = event
                            .get("index")
                            .and_then(|v| v.as_u64())
                            .unwrap_or_default();
                        if let Some(thinking) = delta.get("thinking").and_then(|v| v.as_str()) {
                            let block_state = state.block_stream_state.entry(index).or_default();
                            emit_block_stream_delta(block_state, tx, thinking, true)
                        } else {
                            true
                        }
                    },
                    Some("input_json_delta") => {
                        let index = event
                            .get("index")
                            .and_then(|v| v.as_u64())
                            .unwrap_or_default();
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
            if let Some(index) = event.get("index").and_then(|v| v.as_u64()) {
                if let Some(call_id) = state.index_to_call_id.get(&index) {
                    return send_event(
                        tx,
                        LlmEvent::ToolCallCompleted {
                            call_id: call_id.clone(),
                        },
                    );
                }
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
        _ => true,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_appends_messages() {
        let provider = AnthropicProvider::new(
            LlmClientConfig {
                base_url: "https://api.anthropic.com/v1".into(),
                ..LlmClientConfig::default()
            },
            "claude-sonnet-4-6".into(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(provider.endpoint(), "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn endpoint_auto_adds_v1_for_bare_base() {
        let provider = AnthropicProvider::new(
            LlmClientConfig {
                base_url: "https://open.bigmodel.cn/api/anthropic".into(),
                ..LlmClientConfig::default()
            },
            "glm-5.1".into(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            provider.endpoint(),
            "https://open.bigmodel.cn/api/anthropic/v1/messages"
        );
    }

    #[test]
    fn endpoint_preserves_full_messages_url() {
        let provider = AnthropicProvider::new(
            LlmClientConfig {
                base_url: "https://custom.proxy/messages".into(),
                ..LlmClientConfig::default()
            },
            "test-model".into(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(provider.endpoint(), "https://custom.proxy/messages");
    }

    #[test]
    fn count_tokens_request_reuses_messages_system_and_tools() {
        let provider = AnthropicProvider::new(
            LlmClientConfig {
                base_url: "https://api.anthropic.com/v1".into(),
                ..LlmClientConfig::default()
            },
            "claude-test".into(),
            Some(1024),
            Some(200_000),
        )
        .unwrap();
        let tools = vec![ToolDefinition {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: serde_json::json!({"type": "object"}),
            origin: astrcode_core::tool::ToolOrigin::Builtin,
            execution_mode: astrcode_core::tool::ExecutionMode::Parallel,
        }];
        let body = provider
            .build_count_tokens_body(&[LlmMessage::system("s"), LlmMessage::user("hi")], &tools);

        assert_eq!(
            provider.count_tokens_endpoint(),
            "https://api.anthropic.com/v1/messages/count_tokens"
        );
        assert_eq!(body["model"], "claude-test");
        assert_eq!(body["system"][0]["text"], "s");
        assert!(body["messages"].is_array());
        assert!(body["tools"].is_array());
        assert!(body.get("stream").is_none());
        assert!(body.get("max_tokens").is_none());
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
}
