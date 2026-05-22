//! Anthropic Messages API provider.
//!
//! Implements [`LlmProvider`] for Anthropic's Messages API with SSE streaming,
//! tool use, and thinking support.

use std::{collections::HashMap, sync::Mutex};

use astrcode_core::{llm::*, tool::ToolDefinition};
use tokio::sync::mpsc;

use crate::{
    common::{build_client, stream_with_event_type},
    retry::RetryPolicy,
    serialization::ContentMapper,
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
    ) -> Self {
        let client = build_client(&config);
        Self {
            config,
            model_id,
            model_limits_val: ModelLimits {
                max_input_tokens: context_limit.unwrap_or(200_000),
                max_output_tokens: max_tokens.unwrap_or(8192) as usize,
            },
            client,
        }
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
}

#[async_trait::async_trait]
impl LlmProvider for AnthropicProvider {
    async fn generate(
        &self,
        messages: Vec<LlmMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let (system, api_messages) = self.convert_messages(&messages);

        let mut request_body = serde_json::json!({
            "model": self.model_id,
            "messages": api_messages,
            "max_tokens": self.model_limits_val.max_output_tokens,
            "stream": true,
        });
        if let Some(sys) = system {
            request_body["system"] = sys;
        }
        if !tools.is_empty() {
            request_body["tools"] = convert_tools(&tools);
        }
        if let Some(t) = self.config.temperature {
            request_body["temperature"] = serde_json::json!(t);
        }

        let endpoint = self.endpoint();
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
        let headers = [headers, extra].concat();
        let client = self.client.clone();
        let retry = RetryPolicy {
            max_retries: self.config.max_retries,
            base_delay_ms: self.config.retry_base_delay_ms,
        };

        tokio::spawn(async move {
            // SSE content block index → actual tool call id 的映射。
            // content_block_start 带 id（如 "call_549f..."）和 index（如 0），
            // content_block_delta 只有 index，需要通过此映射找到真实 call_id。
            let index_to_call_id: Mutex<HashMap<u64, String>> = Mutex::new(HashMap::new());

            let result = stream_with_event_type(
                client,
                endpoint,
                headers,
                request_body,
                retry,
                tx.clone(),
                |event_type, event, tx| {
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
                                        if let Some(index) =
                                            event.get("index").and_then(|v| v.as_u64())
                                        {
                                            index_to_call_id
                                                .lock()
                                                .unwrap()
                                                .insert(index, call_id.clone());
                                        }
                                        // 部分兼容 provider 把完整参数放在 input 字段，
                                        // 而不是通过 input_json_delta 增量发送。
                                        let initial_args = block
                                            .get("input")
                                            .filter(|v| {
                                                v.is_object() && !v.as_object().unwrap().is_empty()
                                            })
                                            .map(|v| v.to_string())
                                            .unwrap_or_default();
                                        let _ = tx.send(LlmEvent::ToolCallStart {
                                            call_id,
                                            name,
                                            arguments: initial_args,
                                        });
                                    },
                                    Some("thinking") => {
                                        if let Some(thinking) =
                                            block.get("thinking").and_then(|v| v.as_str())
                                        {
                                            if !thinking.is_empty() {
                                                let _ = tx.send(LlmEvent::ThinkingDelta {
                                                    delta: thinking.to_string(),
                                                });
                                            }
                                        }
                                    },
                                    Some("text") => {
                                        if let Some(text) =
                                            block.get("text").and_then(|v| v.as_str())
                                        {
                                            if !text.is_empty() {
                                                let _ = tx.send(LlmEvent::ContentDelta {
                                                    delta: text.to_string(),
                                                });
                                            }
                                        }
                                    },
                                    _ => {},
                                }
                            }
                        },
                        "content_block_delta" => {
                            if let Some(delta) = event.get("delta") {
                                match delta.get("type").and_then(|v| v.as_str()) {
                                    Some("text_delta") => {
                                        if let Some(text) =
                                            delta.get("text").and_then(|v| v.as_str())
                                        {
                                            let _ = tx.send(LlmEvent::ContentDelta {
                                                delta: text.to_string(),
                                            });
                                        }
                                    },
                                    Some("thinking_delta") => {
                                        if let Some(thinking) =
                                            delta.get("thinking").and_then(|v| v.as_str())
                                        {
                                            let _ = tx.send(LlmEvent::ThinkingDelta {
                                                delta: thinking.to_string(),
                                            });
                                        }
                                    },
                                    Some("input_json_delta") => {
                                        let index = event
                                            .get("index")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or_default();
                                        let call_id = index_to_call_id
                                            .lock()
                                            .unwrap()
                                            .get(&index)
                                            .cloned()
                                            .unwrap_or_else(|| index.to_string());
                                        if let Some(partial) =
                                            delta.get("partial_json").and_then(|v| v.as_str())
                                        {
                                            let _ = tx.send(LlmEvent::ToolCallDelta {
                                                call_id,
                                                delta: partial.to_string(),
                                            });
                                        }
                                    },
                                    _ => {},
                                }
                            }
                        },
                        "message_delta" => {
                            if let Some(stop_reason) =
                                event.pointer("/delta/stop_reason").and_then(|v| v.as_str())
                            {
                                let _ = tx.send(LlmEvent::Done {
                                    finish_reason: stop_reason.to_string(),
                                });
                            }
                        },
                        "error" => {
                            let message = event
                                .pointer("/error/message")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown Anthropic error")
                                .to_string();
                            let _ = tx.send(LlmEvent::Error { message });
                        },
                        _ => {},
                    }
                },
            )
            .await;
            if let Err(e) = result {
                let _ = tx.send(LlmEvent::Error {
                    message: e.to_string(),
                });
            }
        });

        Ok(rx)
    }

    fn model_limits(&self) -> ModelLimits {
        self.model_limits_val.clone()
    }
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
                "content": content,
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

/// 在最后一条非空 message 的最后一个 content block 上加 cache_control，
/// 把"历史末尾"标为缓存边界。当前轮的 user input 通常作为最后一条 user message
/// 出现，调用方决定是否把它包含在 messages 中——这里只标记最后一项，缓存命中
/// 由前缀稳定性保证。
/// 判断 URL 路径是否已包含版本段（如 `/v1`、`/v2`）。
fn is_versioned_path(url: &str) -> bool {
    url.rsplit('/').next().is_some_and(|seg| {
        seg.starts_with('v') && seg.len() > 1 && seg[1..].chars().all(|c| c.is_ascii_digit())
    })
}

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
        );
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
        );
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
        );
        assert_eq!(provider.endpoint(), "https://custom.proxy/messages");
    }

    #[test]
    fn convert_messages_extracts_system() {
        let provider = AnthropicProvider::new(
            LlmClientConfig::default(),
            "claude-sonnet-4-6".into(),
            Some(4096),
            Some(200_000),
        );
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
}
