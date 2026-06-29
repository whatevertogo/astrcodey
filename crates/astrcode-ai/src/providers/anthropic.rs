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
        HttpPostRequest, StreamEventSink, build_client, report_stream_error,
        retry_policy_from_config, send_event, stream_text_delta, stream_with_event_type,
    },
    serialization::ContentMapper,
    tool_result_wire::anthropic_tool_result_content,
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
        let base = self.config.base_url.trim_end_matches('/');
        if base.ends_with("/messages") {
            base.to_string()
        } else if is_versioned_path(base) {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        }
    }

    fn count_tokens_endpoint(&self) -> String {
        let endpoint = self.endpoint();
        if endpoint.ends_with("/count_tokens") {
            endpoint
        } else {
            format!("{endpoint}/count_tokens")
        }
    }

    fn convert_messages(
        &self,
        messages: &[LlmMessage],
    ) -> (Option<serde_json::Value>, Vec<serde_json::Value>) {
        let mut system_blocks: Vec<serde_json::Value> = Vec::new();
        let mut api_messages: Vec<serde_json::Value> = Vec::new();

        for msg in messages {
            match msg.role {
                LlmRole::System => {
                    let text: String = msg
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            LlmContent::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        system_blocks.push(serde_json::json!({
                            "type": "text",
                            "text": text,
                            "cache_control": {"type": "ephemeral"}
                        }));
                    }
                },
                LlmRole::User => {
                    api_messages.push(AnthropicMapper::map_user(msg));
                },
                LlmRole::Assistant => {
                    api_messages.push(AnthropicMapper::map_assistant(msg));
                },
                LlmRole::Tool => {
                    let block = convert_tool_result_block(msg);
                    if let Some(last) = api_messages.last_mut() {
                        if last["role"] == "user" && has_only_tool_results(last) {
                            if let Some(content) =
                                last.get_mut("content").and_then(|c| c.as_array_mut())
                            {
                                content.push(block);
                                continue;
                            }
                            // has_only_tool_results returned true but content is not an array;
                            // fall through to create a new message block.
                            tracing::warn!(
                                "tool result merge: last user message content is not an array, \
                                 creating new block"
                            );
                        }
                    }
                    api_messages.push(serde_json::json!({
                        "role": "user",
                        "content": [block]
                    }));
                },
            }
        }

        // 在历史末尾（最后一条非当前 user 输入的消息）加第二个 cache marker，
        // 使 "system + tools + 历史" 整段成为可缓存前缀。
        // Anthropic 允许最多 4 个 cache breakpoint，当前用 2 个：
        //   1. system block 末尾（已有，见上面循环）
        //   2. 历史末尾（这里）
        // 当前轮的 user input 在 marker 之后，每次变化但前缀仍命中。
        mark_history_cache_breakpoint(&mut api_messages);

        let system = if system_blocks.is_empty() {
            None
        } else {
            Some(serde_json::Value::Array(system_blocks))
        };

        (system, api_messages)
    }

    fn build_request_body(
        &self,
        messages: &[LlmMessage],
        tools: &[ToolDefinition],
        stream: bool,
    ) -> serde_json::Value {
        let (system, api_messages) = self.convert_messages(messages);
        let mut request_body = serde_json::json!({
            "model": self.model_id,
            "messages": api_messages,
            "max_tokens": self.model_limits_val.max_output_tokens,
        });
        if stream {
            request_body["stream"] = serde_json::json!(true);
        }
        if let Some(sys) = system {
            request_body["system"] = sys;
        }
        if !tools.is_empty() {
            request_body["tools"] = convert_tools(tools);
        }
        request_body
    }

    fn build_count_tokens_body(
        &self,
        messages: &[LlmMessage],
        tools: &[ToolDefinition],
    ) -> serde_json::Value {
        let mut body = self.build_request_body(messages, tools, false);
        if let Some(obj) = body.as_object_mut() {
            obj.remove("max_tokens");
        }
        body
    }

    fn headers(&self) -> Vec<(String, String)> {
        let headers = vec![
            ("x-api-key".into(), self.config.api_key.clone()),
            ("anthropic-version".into(), "2023-06-01".into()),
        ];
        let extra: Vec<(String, String)> = self
            .config
            .extra_headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        [headers, extra].concat()
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
        let headers = self.headers();
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

fn token_usage_has_value(usage: &LlmTokenUsage) -> bool {
    usage.input_tokens.is_some()
        || usage.cached_input_tokens.is_some()
        || usage.cache_creation_input_tokens.is_some()
        || usage.output_tokens.is_some()
        || usage.reasoning_output_tokens.is_some()
        || usage.total_tokens.is_some()
}

// ─── Message conversion ──────────────────────────────────────────────────

struct AnthropicMapper;

impl ContentMapper for AnthropicMapper {
    fn text(text: &str) -> serde_json::Value {
        serde_json::json!({"type": "text", "text": text})
    }

    fn image(base64: &str, media_type: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "image",
            "source": {"type": "base64", "data": base64, "media_type": media_type}
        })
    }

    fn tool_call(call_id: &str, name: &str, arguments: &serde_json::Value) -> serde_json::Value {
        let args_str = match arguments {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        serde_json::json!({
            "type": "tool_use",
            "id": call_id,
            "name": name,
            "input": serde_json::from_str::<serde_json::Value>(&args_str)
                .unwrap_or(serde_json::json!({}))
        })
    }

    fn tool_result(id: &str, content: &str, is_error: bool) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "tool_result",
            "tool_use_id": id,
            "content": content,
            "is_error": is_error,
        }))
    }

    fn empty() -> serde_json::Value {
        serde_json::json!({"type": "text", "text": ""})
    }

    fn wrap_user(parts: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({"role": "user", "content": parts})
    }

    fn wrap_assistant(parts: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({"role": "assistant", "content": parts})
    }
}

fn convert_tool_result_block(msg: &LlmMessage) -> serde_json::Value {
    for content in &msg.content {
        if let LlmContent::ToolResult {
            tool_call_id,
            content,
            is_error,
        } = content
        {
            return serde_json::json!({
                "type": "tool_result",
                "tool_use_id": tool_call_id,
                "content": anthropic_tool_result_content(content),
                "is_error": is_error,
            });
        }
    }
    serde_json::json!({"type": "tool_result", "tool_use_id": "", "content": "", "is_error": false})
}

fn has_only_tool_results(msg: &serde_json::Value) -> bool {
    let Some(content) = msg.get("content").and_then(|v| v.as_array()) else {
        return false;
    };
    !content.is_empty() && content.iter().all(|b| b["type"] == "tool_result")
}

fn convert_tools(tools: &[ToolDefinition]) -> serde_json::Value {
    let mut converted: Vec<serde_json::Value> = tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            })
        })
        .collect();
    // 在最后一个 tool 上加 cache_control，让 tool schema 整体进入缓存前缀。
    // tools 数组在请求中位于 system 之后、messages 之前，标记最后一个等于
    // 标记整段 tool schema 的末尾。
    if let Some(last) = converted.last_mut() {
        last["cache_control"] = serde_json::json!({"type": "ephemeral"});
    }
    serde_json::Value::Array(converted)
}

/// 判断 URL 路径是否已包含版本段（如 `/v1`、`/v2`）。
fn is_versioned_path(url: &str) -> bool {
    url.rsplit('/').next().is_some_and(|seg| {
        seg.starts_with('v') && seg.len() > 1 && seg[1..].chars().all(|c| c.is_ascii_digit())
    })
}

/// 在最后一条非空 message 的最后一个 content block 上加 cache_control，
/// 把"历史末尾"标为缓存边界。当前轮的 user input 通常作为最后一条 user message
/// 出现，调用方决定是否把它包含在 messages 中——这里只标记最后一项，缓存命中
/// 由前缀稳定性保证。
fn mark_history_cache_breakpoint(api_messages: &mut [serde_json::Value]) {
    let Some(last_msg) = api_messages.last_mut() else {
        return;
    };
    let Some(content) = last_msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
        return;
    };
    let Some(last_block) = content.last_mut() else {
        return;
    };
    if let Some(obj) = last_block.as_object_mut() {
        obj.insert(
            "cache_control".into(),
            serde_json::json!({"type": "ephemeral"}),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_converts_text() {
        let msg = LlmMessage::user("hello");
        let json = AnthropicMapper::map_user(&msg);
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"][0]["type"], "text");
        assert_eq!(json["content"][0]["text"], "hello");
    }

    #[test]
    fn assistant_message_converts_tool_call() {
        let msg = LlmMessage {
            role: LlmRole::Assistant,
            content: vec![LlmContent::ToolCall {
                call_id: "call_1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "foo.rs"}),
            }],
            name: None,
            reasoning_content: None,
        };
        let json = AnthropicMapper::map_assistant(&msg);
        let block = &json["content"][0];
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["id"], "call_1");
        assert_eq!(block["name"], "read");
        assert_eq!(block["input"]["path"], "foo.rs");
    }

    #[test]
    fn tool_results_merge_into_same_user_message() {
        let mut api_messages: Vec<serde_json::Value> = Vec::new();
        let msgs = vec![
            LlmMessage::assistant("I'll check"),
            LlmMessage::tool("read", "call_1", "file content", false),
            LlmMessage::tool("grep", "call_2", "match found", false),
        ];
        for msg in &msgs {
            match msg.role {
                LlmRole::Assistant => api_messages.push(AnthropicMapper::map_assistant(msg)),
                LlmRole::Tool => {
                    let block = convert_tool_result_block(msg);
                    if let Some(last) = api_messages.last_mut() {
                        if last["role"] == "user" && has_only_tool_results(last) {
                            last["content"]
                                .as_array_mut()
                                .expect("content array")
                                .push(block);
                            continue;
                        }
                    }
                    api_messages.push(serde_json::json!({"role": "user", "content": [block]}));
                },
                _ => {},
            }
        }
        assert_eq!(api_messages.len(), 2);
        let content = api_messages[1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["tool_use_id"], "call_1");
        assert_eq!(content[1]["tool_use_id"], "call_2");
    }

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
    fn convert_messages_extracts_system() {
        let provider = AnthropicProvider::new(
            LlmClientConfig::default(),
            "claude-sonnet-4-6".into(),
            Some(4096),
            Some(200_000),
        )
        .unwrap();
        let msgs = vec![
            LlmMessage::system("You are helpful"),
            LlmMessage::user("hello"),
        ];
        let (system, api_messages) = provider.convert_messages(&msgs);
        let sys = system.expect("system should be present");
        assert_eq!(sys[0]["text"], "You are helpful");
        assert_eq!(api_messages.len(), 1);
        assert_eq!(api_messages[0]["role"], "user");
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
