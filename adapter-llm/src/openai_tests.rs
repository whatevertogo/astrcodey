use std::{
    net::TcpListener,
    sync::{Arc, Mutex},
    time::Duration,
};

use astrcode_core::{CancelToken, UserMessageOrigin};
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    task::JoinHandle,
};

use super::*;
use crate::sink_collector;

fn spawn_server(response: String) -> (String, JoinHandle<()>) {
    spawn_server_responses(vec![response])
}

fn spawn_server_responses(responses: Vec<String>) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener should bind");
    let addr = listener.local_addr().expect("listener should have addr");
    listener
        .set_nonblocking(true)
        .expect("listener should be nonblocking");
    let listener = tokio::net::TcpListener::from_std(listener).expect("tokio listener");

    let handle = tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.expect("accept should work");
            let mut buf = [0_u8; 4096];
            // 故意忽略：读取残余数据仅用于清理，失败无影响
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(response.as_bytes())
                .await
                .expect("response should be written");
            // 故意忽略：关闭 socket 时连接可能已断开
            let _ = socket.shutdown().await;
        }
    });

    (format!("http://{}", addr), handle)
}

#[test]
fn sse_line_parser_handles_done_and_data_prefix_variants() {
    assert!(matches!(
        parse_sse_line("data: [DONE]").expect("should parse"),
        ParsedSseLine::Done
    ));
    assert!(matches!(
        parse_sse_line("data:[DONE]").expect("should parse"),
        ParsedSseLine::Done
    ));
    assert!(matches!(
        parse_sse_line("   ").expect("should parse"),
        ParsedSseLine::Ignore
    ));

    let parsed =
        parse_sse_line(r#"data: {"choices":[{"delta":{"content":"Hello"},"finish_reason":null}]}"#)
            .expect("should parse");
    assert!(matches!(parsed, ParsedSseLine::Chunk(_)));
}

#[test]
fn build_request_prepends_system_message_when_present() {
    let provider = OpenAiProvider::new(
        "http://127.0.0.1:12345".to_string(),
        "sk-test".to_string(),
        "model-a".to_string(),
        ModelLimits {
            context_window: 128_000,
            max_output_tokens: 2048,
        },
        LlmClientConfig::default(),
    )
    .expect("provider should build");
    let messages = [LlmMessage::User {
        content: "hi".to_string(),
        origin: UserMessageOrigin::User,
    }];
    let request = provider.build_request(OpenAiBuildRequestInput {
        messages: &messages,
        tools: &[],
        system_prompt: Some("Follow the rules"),
        system_prompt_blocks: &[],
        prompt_cache_hints: None,
        max_output_tokens_override: None,
        stream: false,
    });

    assert_eq!(request.messages[0].role, "system");
    assert_eq!(
        request.messages[0].content.as_deref(),
        Some("Follow the rules")
    );
}

#[test]
fn build_request_uses_system_blocks_without_explicit_cache_control() {
    // OpenAI 的 prompt caching 是自动的（基于 prefix matching），
    // 不需要显式 cache_control 标记。分层排列的 system blocks 天然提供稳定前缀。
    let provider = OpenAiProvider::new(
        "http://127.0.0.1:12345".to_string(),
        "sk-test".to_string(),
        "model-a".to_string(),
        ModelLimits {
            context_window: 128_000,
            max_output_tokens: 2048,
        },
        LlmClientConfig::default(),
    )
    .expect("provider should build");
    let messages = [LlmMessage::User {
        content: "hi".to_string(),
        origin: UserMessageOrigin::User,
    }];
    let system_blocks = vec![
        astrcode_runtime_contract::prompt::SystemPromptBlock {
            title: "Stable 1".to_string(),
            content: "stable content 1".to_string(),
            cache_boundary: false,
            layer: astrcode_runtime_contract::prompt::SystemPromptLayer::Stable,
        },
        astrcode_runtime_contract::prompt::SystemPromptBlock {
            title: "Stable 2".to_string(),
            content: "stable content 2".to_string(),
            cache_boundary: true,
            layer: astrcode_runtime_contract::prompt::SystemPromptLayer::Stable,
        },
        astrcode_runtime_contract::prompt::SystemPromptBlock {
            title: "Semi 1".to_string(),
            content: "semi content 1".to_string(),
            cache_boundary: true,
            layer: astrcode_runtime_contract::prompt::SystemPromptLayer::SemiStable,
        },
        astrcode_runtime_contract::prompt::SystemPromptBlock {
            title: "Inherited 1".to_string(),
            content: "inherited content 1".to_string(),
            cache_boundary: true,
            layer: astrcode_runtime_contract::prompt::SystemPromptLayer::Inherited,
        },
    ];
    let request = provider.build_request(OpenAiBuildRequestInput {
        messages: &messages,
        tools: &[],
        system_prompt: None,
        system_prompt_blocks: &system_blocks,
        prompt_cache_hints: None,
        max_output_tokens_override: None,
        stream: false,
    });
    let body = serde_json::to_value(&request).expect("request should serialize");

    // 应该有 4 个 system 消息 + 1 个 user 消息，无 cache_control 字段
    assert_eq!(request.messages.len(), 5);
    assert_eq!(request.messages[0].role, "system");
    assert_eq!(request.messages[1].role, "system");
    assert_eq!(request.messages[2].role, "system");
    assert_eq!(request.messages[3].role, "system");
    assert_eq!(request.messages[4].role, "user");

    // OpenAI 不发送 cache_control：分层 system blocks 的稳定排列顺序
    // 自然构成自动缓存的最优前缀
    for i in 0..4 {
        assert!(
            body["messages"][i].get("cache_control").is_none(),
            "system block {} should not have cache_control (OpenAI uses automatic caching)",
            i
        );
    }
    assert!(
        body["messages"][4].get("cache_control").is_none(),
        "user message should not have cache_control"
    );
}

#[test]
fn build_request_sends_prompt_cache_key_only_for_official_openai_endpoint() {
    let messages = [LlmMessage::User {
        content: "hi".to_string(),
        origin: UserMessageOrigin::User,
    }];
    let official = OpenAiProvider::new(
        "https://api.openai.com/v1/chat/completions".to_string(),
        "sk-test".to_string(),
        "gpt-4.1".to_string(),
        ModelLimits {
            context_window: 128_000,
            max_output_tokens: 2048,
        },
        LlmClientConfig::default(),
    )
    .expect("provider should build");
    let compatible = OpenAiProvider::new(
        "https://gateway.example.com/v1/chat/completions".to_string(),
        "sk-test".to_string(),
        "model-a".to_string(),
        ModelLimits {
            context_window: 128_000,
            max_output_tokens: 2048,
        },
        LlmClientConfig::default(),
    )
    .expect("provider should build");

    let official_body = serde_json::to_value(official.build_request(OpenAiBuildRequestInput {
        messages: &messages,
        tools: &[],
        system_prompt: Some("Follow the rules"),
        system_prompt_blocks: &[],
        prompt_cache_hints: None,
        max_output_tokens_override: None,
        stream: false,
    }))
    .expect("request should serialize");
    let compatible_body = serde_json::to_value(compatible.build_request(OpenAiBuildRequestInput {
        messages: &messages,
        tools: &[],
        system_prompt: Some("Follow the rules"),
        system_prompt_blocks: &[],
        prompt_cache_hints: None,
        max_output_tokens_override: None,
        stream: false,
    }))
    .expect("request should serialize");

    assert!(
        official_body
            .get("prompt_cache_key")
            .and_then(Value::as_str)
            .is_some()
    );
    assert!(official_body.get("prompt_cache_retention").is_none());
    assert!(compatible_body.get("prompt_cache_key").is_none());
}

#[test]
fn build_request_includes_stream_usage_options_only_for_official_endpoint() {
    let messages = [LlmMessage::User {
        content: "hi".to_string(),
        origin: UserMessageOrigin::User,
    }];
    let official = OpenAiProvider::new(
        "https://api.openai.com/v1/chat/completions".to_string(),
        "sk-test".to_string(),
        "gpt-4.1".to_string(),
        ModelLimits {
            context_window: 128_000,
            max_output_tokens: 2048,
        },
        LlmClientConfig::default(),
    )
    .expect("provider should build");
    let compatible = OpenAiProvider::new(
        "https://gateway.example.com/v1/chat/completions".to_string(),
        "sk-test".to_string(),
        "model-a".to_string(),
        ModelLimits {
            context_window: 128_000,
            max_output_tokens: 2048,
        },
        LlmClientConfig::default(),
    )
    .expect("provider should build");

    let official_body = serde_json::to_value(official.build_request(OpenAiBuildRequestInput {
        messages: &messages,
        tools: &[],
        system_prompt: None,
        system_prompt_blocks: &[],
        prompt_cache_hints: None,
        max_output_tokens_override: None,
        stream: true,
    }))
    .expect("request should serialize");
    let compatible_body = serde_json::to_value(compatible.build_request(OpenAiBuildRequestInput {
        messages: &messages,
        tools: &[],
        system_prompt: None,
        system_prompt_blocks: &[],
        prompt_cache_hints: None,
        max_output_tokens_override: None,
        stream: true,
    }))
    .expect("request should serialize");

    assert_eq!(
        official_body["stream_options"]["include_usage"].as_bool(),
        Some(true)
    );
    assert!(compatible_body.get("stream_options").is_none());
}

#[test]
fn compatible_endpoint_can_enable_stream_usage_via_explicit_capabilities() {
    let provider = OpenAiProvider::new_with_capabilities(
        "https://gateway.example.com/v1/chat/completions".to_string(),
        "sk-test".to_string(),
        "model-a".to_string(),
        ModelLimits {
            context_window: 128_000,
            max_output_tokens: 2048,
        },
        LlmClientConfig::default(),
        OpenAiProviderCapabilities {
            supports_prompt_cache_key: false,
            supports_stream_usage: true,
        },
    )
    .expect("provider should build");
    let messages = [LlmMessage::User {
        content: "hi".to_string(),
        origin: UserMessageOrigin::User,
    }];

    let body = serde_json::to_value(provider.build_request(OpenAiBuildRequestInput {
        messages: &messages,
        tools: &[],
        system_prompt: None,
        system_prompt_blocks: &[],
        prompt_cache_hints: None,
        max_output_tokens_override: None,
        stream: true,
    }))
    .expect("request should serialize");

    assert_eq!(
        body["stream_options"]["include_usage"].as_bool(),
        Some(true)
    );
    assert!(body.get("prompt_cache_key").is_none());
}

#[test]
fn build_request_normalizes_tool_order_for_payload_and_cache_key() {
    let provider = OpenAiProvider::new(
        "https://api.openai.com/v1/chat/completions".to_string(),
        "sk-test".to_string(),
        "gpt-4.1".to_string(),
        ModelLimits {
            context_window: 128_000,
            max_output_tokens: 2048,
        },
        LlmClientConfig::default(),
    )
    .expect("provider should build");
    let messages = [LlmMessage::User {
        content: "hi".to_string(),
        origin: UserMessageOrigin::User,
    }];
    let first_tools = vec![
        ToolDefinition {
            name: "zzz_plugin_search".to_string(),
            description: "Plugin Search".to_string(),
            parameters: json!({"type":"object"}),
        },
        ToolDefinition {
            name: "readFile".to_string(),
            description: "Read".to_string(),
            parameters: json!({"type":"object"}),
        },
        ToolDefinition {
            name: "mcp__search".to_string(),
            description: "MCP Search".to_string(),
            parameters: json!({"type":"object"}),
        },
    ];
    let second_tools = vec![
        ToolDefinition {
            name: "mcp__search".to_string(),
            description: "MCP Search".to_string(),
            parameters: json!({"type":"object"}),
        },
        ToolDefinition {
            name: "zzz_plugin_search".to_string(),
            description: "Plugin Search".to_string(),
            parameters: json!({"type":"object"}),
        },
        ToolDefinition {
            name: "readFile".to_string(),
            description: "Read".to_string(),
            parameters: json!({"type":"object"}),
        },
    ];

    let first = provider.build_request(OpenAiBuildRequestInput {
        messages: &messages,
        tools: &first_tools,
        system_prompt: None,
        system_prompt_blocks: &[],
        prompt_cache_hints: None,
        max_output_tokens_override: None,
        stream: false,
    });
    let second = provider.build_request(OpenAiBuildRequestInput {
        messages: &messages,
        tools: &second_tools,
        system_prompt: None,
        system_prompt_blocks: &[],
        prompt_cache_hints: None,
        max_output_tokens_override: None,
        stream: false,
    });

    let first_names: Vec<&str> = first
        .tools
        .as_ref()
        .expect("tools should exist")
        .iter()
        .map(|tool| tool.function.name.as_str())
        .collect();
    let second_names: Vec<&str> = second
        .tools
        .as_ref()
        .expect("tools should exist")
        .iter()
        .map(|tool| tool.function.name.as_str())
        .collect();

    assert_eq!(
        first_names,
        vec!["readFile", "mcp__search", "zzz_plugin_search"]
    );
    assert_eq!(
        second_names,
        vec!["readFile", "mcp__search", "zzz_plugin_search"]
    );
    assert_eq!(first.prompt_cache_key, second.prompt_cache_key);
}

#[test]
fn build_prompt_cache_key_changes_with_global_cache_strategy() {
    let tools = vec![ToolDefinition {
        name: "read_file".to_string(),
        description: "Read".to_string(),
        parameters: json!({"type":"object"}),
    }];
    let ordered_tools = order_tools_for_cache(&tools);
    let base_hints = PromptCacheHints {
        layer_fingerprints: astrcode_runtime_contract::prompt::PromptLayerFingerprints {
            stable: Some("stable-a".to_string()),
            semi_stable: Some("semi-a".to_string()),
            inherited: Some("inherited-a".to_string()),
            dynamic: None,
        },
        global_cache_strategy: PromptCacheGlobalStrategy::SystemPrompt,
        unchanged_layers: Vec::new(),
        compacted: false,
        tool_result_rebudgeted: false,
    };
    let tool_based_hints = PromptCacheHints {
        global_cache_strategy: PromptCacheGlobalStrategy::ToolBased,
        ..base_hints.clone()
    };

    let system_key =
        build_prompt_cache_key("gpt-4.1", None, &[], Some(&base_hints), &ordered_tools);
    let tool_key = build_prompt_cache_key(
        "gpt-4.1",
        None,
        &[],
        Some(&tool_based_hints),
        &ordered_tools,
    );

    assert_ne!(system_key, tool_key);
}

#[test]
fn build_request_honors_request_level_max_output_tokens_override() {
    let provider = OpenAiProvider::new(
        "https://api.openai.com/v1/chat/completions".to_string(),
        "sk-test".to_string(),
        "gpt-4.1".to_string(),
        ModelLimits {
            context_window: 128_000,
            max_output_tokens: 2048,
        },
        LlmClientConfig::default(),
    )
    .expect("provider should build");
    let messages = [LlmMessage::User {
        content: "hi".to_string(),
        origin: UserMessageOrigin::User,
    }];

    let capped = provider.build_request(OpenAiBuildRequestInput {
        messages: &messages,
        tools: &[],
        system_prompt: None,
        system_prompt_blocks: &[],
        prompt_cache_hints: None,
        max_output_tokens_override: Some(1024),
        stream: false,
    });
    let clamped = provider.build_request(OpenAiBuildRequestInput {
        messages: &messages,
        tools: &[],
        system_prompt: None,
        system_prompt_blocks: &[],
        prompt_cache_hints: None,
        max_output_tokens_override: Some(4096),
        stream: false,
    });

    assert_eq!(capped.max_tokens, 1024);
    assert_eq!(clamped.max_tokens, 2048);
}

#[tokio::test]
async fn generate_non_streaming_parses_text_and_tool_calls() {
    let body = json!({
        "choices": [{
            "message": {
                "content": "hello",
                "tool_calls": [{
                    "id": "call_1",
                    "function": {
                        "name": "search",
                        "arguments": "{\"q\":\"hello\"}"
                    }
                }]
            }
        }],
        "usage": {
            "prompt_tokens": 1200,
            "completion_tokens": 20,
            "prompt_tokens_details": {
                "cached_tokens": 1024
            }
        }
    })
    .to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: \
         close\r\n\r\n{}",
        body.len(),
        body
    );
    let (base_url, handle) = spawn_server(response);
    let provider = OpenAiProvider::new(
        base_url,
        "sk-test".to_string(),
        "model-a".to_string(),
        ModelLimits {
            context_window: 128_000,
            max_output_tokens: 2048,
        },
        LlmClientConfig::default(),
    )
    .expect("provider should build");

    let output = provider
        .generate(
            LlmRequest::new(
                vec![LlmMessage::User {
                    content: "hi".to_string(),
                    origin: UserMessageOrigin::User,
                }],
                vec![],
                CancelToken::new(),
            ),
            None,
        )
        .await
        .expect("generate should succeed");

    handle.await.expect("server should join");
    assert_eq!(output.content, "hello");
    assert_eq!(output.tool_calls.len(), 1);
    assert_eq!(output.tool_calls[0].name, "search");
    assert_eq!(
        output.usage,
        Some(LlmUsage {
            input_tokens: 1200,
            output_tokens: 20,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 1024,
        })
    );
}

#[tokio::test]
async fn generate_streaming_emits_events_and_accumulates_output() {
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({
            "choices": [{
                "delta": { "content": "hel" },
                "finish_reason": null
            }]
        }),
        json!({
            "choices": [{
                "delta": {
                    "content": "lo",
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": {
                            "name": "search",
                            "arguments": "{\"q\":\"hello\"}"
                        }
                    }]
                },
                "finish_reason": "stop"
            }]
        }),
        json!({
            "choices": [],
            "usage": {
                "prompt_tokens": 1500,
                "completion_tokens": 25,
                "prompt_tokens_details": {
                    "cached_tokens": 1200
                }
            }
        })
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: \
         no-cache\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let (base_url, handle) = spawn_server(response);
    let provider = OpenAiProvider::new(
        base_url,
        "sk-test".to_string(),
        "model-a".to_string(),
        ModelLimits {
            context_window: 128_000,
            max_output_tokens: 2048,
        },
        LlmClientConfig::default(),
    )
    .expect("provider should build");
    let events = Arc::new(Mutex::new(Vec::new()));

    let output = provider
        .generate(
            LlmRequest::new(
                vec![LlmMessage::User {
                    content: "hi".to_string(),
                    origin: UserMessageOrigin::User,
                }],
                vec![],
                CancelToken::new(),
            ),
            Some(sink_collector(events.clone())),
        )
        .await
        .expect("generate should succeed");

    handle.await.expect("server should join");
    let events = events.lock().expect("lock").clone();

    assert!(
        events
            .iter()
            .any(|event| matches!(event, LlmEvent::TextDelta(text) if text == "hel"))
    );
    assert!(events.iter().any(|event| {
        matches!(
            event,
            LlmEvent::ToolCallDelta { index, id, name, arguments_delta }
            if *index == 0
                && id.as_deref() == Some("call_1")
                && name.as_deref() == Some("search")
                && arguments_delta == "{\"q\":\"hello\"}"
        )
    }));
    assert_eq!(output.content, "hello");
    assert_eq!(output.tool_calls.len(), 1);
    assert_eq!(output.tool_calls[0].args, json!({ "q": "hello" }));
    assert_eq!(
        output.usage,
        Some(LlmUsage {
            input_tokens: 1500,
            output_tokens: 25,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 1200,
        })
    );
}

#[tokio::test]
async fn generate_streaming_retries_bad_body_and_resets_live_draft() {
    let first_body = format!(
        "data: {}\n\n",
        json!({
            "choices": [{
                "delta": { "content": "bad" },
                "finish_reason": null
            }]
        })
    );
    let first_response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: \
         close\r\n\r\n{}",
        first_body.len() + 128,
        first_body
    );
    let second_body = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({
            "choices": [{
                "delta": { "content": "ok" },
                "finish_reason": "stop"
            }]
        })
    );
    let second_response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: \
         close\r\n\r\n{}",
        second_body.len(),
        second_body
    );
    let (base_url, handle) = spawn_server_responses(vec![first_response, second_response]);
    let provider = OpenAiProvider::new(
        base_url,
        "sk-test".to_string(),
        "model-a".to_string(),
        ModelLimits {
            context_window: 128_000,
            max_output_tokens: 2048,
        },
        LlmClientConfig {
            max_retries: 1,
            retry_base_delay: Duration::from_millis(1),
            ..LlmClientConfig::default()
        },
    )
    .expect("provider should build");
    let events = Arc::new(Mutex::new(Vec::new()));

    let output = provider
        .generate(
            LlmRequest::new(
                vec![LlmMessage::User {
                    content: "hi".to_string(),
                    origin: UserMessageOrigin::User,
                }],
                vec![],
                CancelToken::new(),
            ),
            Some(sink_collector(events.clone())),
        )
        .await
        .expect("generate should retry and succeed");

    handle.await.expect("server should join");
    let events = events.lock().expect("lock").clone();

    assert_eq!(output.content, "ok");
    assert!(matches!(
        events.as_slice(),
        [
            LlmEvent::TextDelta(first),
            LlmEvent::StreamRetryStarted {
                attempt: 2,
                max_attempts: 2,
                ..
            },
            LlmEvent::TextDelta(second),
        ] if first == "bad" && second == "ok"
    ));
}

#[tokio::test]
async fn generate_streaming_reports_attempts_after_retry_exhaustion() {
    let body = format!(
        "data: {}\n\n",
        json!({
            "choices": [{
                "delta": { "content": "bad" },
                "finish_reason": null
            }]
        })
    );
    let bad_response = || {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: \
             {}\r\nconnection: close\r\n\r\n{}",
            body.len() + 128,
            body
        )
    };
    let (base_url, handle) = spawn_server_responses(vec![bad_response(), bad_response()]);
    let provider = OpenAiProvider::new(
        base_url,
        "sk-test".to_string(),
        "model-a".to_string(),
        ModelLimits {
            context_window: 128_000,
            max_output_tokens: 2048,
        },
        LlmClientConfig {
            max_retries: 1,
            retry_base_delay: Duration::from_millis(1),
            ..LlmClientConfig::default()
        },
    )
    .expect("provider should build");

    let error = provider
        .generate(
            LlmRequest::new(
                vec![LlmMessage::User {
                    content: "hi".to_string(),
                    origin: UserMessageOrigin::User,
                }],
                vec![],
                CancelToken::new(),
            ),
            Some(sink_collector(Arc::new(Mutex::new(Vec::new())))),
        )
        .await
        .expect_err("generate should exhaust retries");

    handle.await.expect("server should join");
    assert!(error.is_retryable());
    assert!(error.to_string().contains("after 2 attempts"));
}

#[test]
fn sse_stream_handles_multibyte_text_split_across_chunks() {
    let mut accumulator = LlmAccumulator::default();
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = sink_collector(events.clone());
    let mut sse_buffer = String::new();
    let mut decoder = Utf8StreamDecoder::default();
    let mut finish_reason_out = None;
    let mut usage_out = None;
    let line = r#"data: {"choices":[{"delta":{"content":"你好"},"finish_reason":null}]}"#;
    let bytes = line.as_bytes();
    let split_index = line.find("好").expect("line should contain multibyte char") + 1;

    let first_text = decoder
        .push(
            &bytes[..split_index],
            "openai response stream was not valid utf-8",
        )
        .expect("first split should decode");
    let second_text = decoder
        .push(
            &bytes[split_index..],
            "openai response stream was not valid utf-8",
        )
        .expect("second split should decode");

    let first_done = first_text
        .as_deref()
        .map(|text| {
            consume_sse_text_chunk(
                text,
                &mut sse_buffer,
                &mut accumulator,
                &sink,
                &mut finish_reason_out,
                &mut usage_out,
            )
        })
        .transpose()
        .expect("first chunk should parse")
        .unwrap_or(false);
    let second_done = second_text
        .as_deref()
        .map(|text| {
            consume_sse_text_chunk(
                text,
                &mut sse_buffer,
                &mut accumulator,
                &sink,
                &mut finish_reason_out,
                &mut usage_out,
            )
        })
        .transpose()
        .expect("second chunk should parse")
        .unwrap_or(false);

    assert!(!first_done);
    assert!(!second_done);

    flush_sse_buffer(
        &mut sse_buffer,
        &mut accumulator,
        &sink,
        &mut finish_reason_out,
        &mut usage_out,
    )
    .expect("flush should parse");

    let output = accumulator.finish();
    let events = events.lock().expect("lock").clone();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, LlmEvent::TextDelta(text) if text == "你好"))
    );
    assert_eq!(output.content, "你好");
}

#[test]
fn openai_compatible_provider_reports_cache_metrics_support() {
    let provider = OpenAiProvider::new(
        "http://127.0.0.1:12345".to_string(),
        "sk-test".to_string(),
        "model-a".to_string(),
        ModelLimits {
            context_window: 128_000,
            max_output_tokens: 2048,
        },
        LlmClientConfig::default(),
    )
    .expect("provider should build");

    assert!(provider.supports_cache_metrics());
}

#[test]
fn official_openai_provider_reports_cache_metrics_support() {
    let provider = OpenAiProvider::new(
        "https://api.openai.com/v1/chat/completions".to_string(),
        "sk-test".to_string(),
        "gpt-4.1".to_string(),
        ModelLimits {
            context_window: 128_000,
            max_output_tokens: 2048,
        },
        LlmClientConfig::default(),
    )
    .expect("provider should build");

    assert!(provider.supports_cache_metrics());
}
