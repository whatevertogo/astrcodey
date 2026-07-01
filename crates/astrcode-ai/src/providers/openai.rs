//! OpenAI 兼容的 Chat Completions / Responses 提供商。
//!
//! 泛型参数 `A` 为内容累积器，允许子提供商（如 Kimi）替换流解析逻辑，
//! 同时复用 HTTP 请求构造、SSE 传输、重试等基础设施。

use astrcode_core::{config::OpenAiApiMode, llm::*, tool::ToolDefinition};
use tokio::sync::mpsc;

#[cfg(test)]
use crate::wire::openai::parser::process_sse_line;
pub use crate::wire::openai::parser::{ChatAccumulator, StandardAccumulator};
use crate::{
    common::{
        HttpPostRequest, apply_auth_header, build_client, report_stream_error,
        retry_policy_from_config,
    },
    wire::openai as openai_wire,
};

// ─── OpenAiProvider ─────────────────────────────────────────────────────

pub struct OpenAiProvider<A: ChatAccumulator = StandardAccumulator> {
    config: LlmClientConfig,
    api_mode: OpenAiApiMode,
    model_id: String,
    model_limits_val: ModelLimits,
    client: reqwest::Client,
    _phantom: std::marker::PhantomData<A>,
}

impl<A: ChatAccumulator> OpenAiProvider<A> {
    pub fn new(
        config: LlmClientConfig,
        api_mode: OpenAiApiMode,
        model_id: String,
        max_tokens: Option<u32>,
        context_limit: Option<usize>,
    ) -> Result<Self, LlmError> {
        let client = build_client(&config)?;
        Ok(Self {
            config,
            api_mode,
            model_id,
            _phantom: std::marker::PhantomData,
            model_limits_val: ModelLimits {
                max_input_tokens: context_limit.unwrap_or(65536),
                max_output_tokens: max_tokens.unwrap_or(8192) as usize,
            },
            client,
        })
    }

    fn endpoint(&self) -> String {
        openai_wire::endpoint_url(self.api_mode, &self.config.base_url)
    }

    fn input_tokens_endpoint(&self) -> String {
        openai_wire::input_tokens_endpoint(&self.config.base_url)
    }

    fn wire_config(&self) -> openai_wire::OpenAiRequestConfig<'_> {
        openai_wire::OpenAiRequestConfig {
            api_mode: self.api_mode,
            model_id: &self.model_id,
            max_output_tokens: self.model_limits_val.max_output_tokens,
            supports_stream_usage: self.config.supports_stream_usage(),
            supports_prompt_cache_key: self.config.supports_prompt_cache_key(),
            prompt_cache_retention: self.config.prompt_cache_retention(),
            thinking_level: self.config.thinking_level(),
        }
    }

    fn build_request_body(
        &self,
        messages: &[LlmMessage],
        tools: &[ToolDefinition],
    ) -> serde_json::Value {
        openai_wire::build_request_body(self.wire_config(), messages, tools)
    }

    fn build_responses_count_body(
        &self,
        messages: &[LlmMessage],
        tools: &[ToolDefinition],
    ) -> serde_json::Value {
        openai_wire::build_input_token_count_body(self.wire_config(), messages, tools)
    }
}

// ─── LlmProvider impl ──────────────────────────────────────────────────

#[async_trait::async_trait]
impl<A: ChatAccumulator> LlmProvider for OpenAiProvider<A> {
    async fn generate(
        &self,
        messages: Vec<LlmMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let body = self.build_request_body(&messages, &tools);

        let endpoint = self.endpoint();
        let api_key = self.config.api_key.clone();
        let auth_scheme = self.config.auth_scheme;
        let extra_headers = self.config.extra_headers.clone();
        let client = self.client.clone();
        let api_mode = self.api_mode;
        let retry = retry_policy_from_config(&self.config);

        tokio::spawn(async move {
            let result = openai_wire::transport::stream_request::<A>(
                client,
                endpoint,
                api_key,
                auth_scheme,
                extra_headers,
                body,
                api_mode,
                retry,
                tx.clone(),
            )
            .await;
            report_stream_error(result, &tx);
        });

        Ok(rx)
    }

    async fn count_input_tokens(
        &self,
        messages: Vec<LlmMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Result<ProviderInputTokenCount, LlmError> {
        if self.api_mode != OpenAiApiMode::Responses {
            return Err(LlmError::Unsupported(
                "OpenAI Chat Completions does not expose provider-side input token counting".into(),
            ));
        }

        let mut headers: Vec<(String, String)> =
            self.config.extra_headers.clone().into_iter().collect();
        apply_auth_header(&mut headers, self.config.auth_scheme, &self.config.api_key);
        let value = HttpPostRequest {
            client: self.client.clone(),
            endpoint: self.input_tokens_endpoint(),
            headers,
            body: self.build_responses_count_body(&messages, &tools),
            retry: retry_policy_from_config(&self.config),
        }
        .json()
        .await?;
        let input_tokens = value
            .get("input_tokens")
            .or_else(|| value.get("inputTokens"))
            .or_else(|| value.get("total_tokens"))
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                LlmError::StreamParse(format!(
                    "OpenAI input token count response missing input_tokens: {value}"
                ))
            })?;
        Ok(ProviderInputTokenCount::provider_count(input_tokens))
    }

    fn model_limits(&self) -> ModelLimits {
        self.model_limits_val.clone()
    }
}

// ─── 便捷类型别名 ──────────────────────────────────────────────────────

/// 标准 OpenAI 提供商。
pub type StandardProvider = OpenAiProvider<StandardAccumulator>;

#[cfg(test)]
mod tests {
    use astrcode_core::{
        config::OpenAiApiMode,
        tool::{ExecutionMode, ToolDefinition, ToolOrigin},
    };

    use super::*;

    fn drain_events(rx: &mut mpsc::UnboundedReceiver<LlmEvent>) -> Vec<LlmEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    fn provider(
        api_mode: OpenAiApiMode,
        supports_cache_key: bool,
        thinking_level: Option<ThinkingLevel>,
    ) -> StandardProvider {
        use astrcode_core::llm::{OpenAiProviderExtras, ProviderExtras};
        let config = LlmClientConfig {
            base_url: "https://api.test/v1".into(),
            api_key: "sk-test".into(),
            extras: ProviderExtras::OpenAi(OpenAiProviderExtras {
                supports_prompt_cache_key: supports_cache_key,
                supports_stream_usage: true,
                prompt_cache_retention: supports_cache_key
                    .then_some(PromptCacheRetention::TwentyFourHours),
                thinking_level,
            }),
            ..LlmClientConfig::default()
        };
        StandardProvider::new(config, api_mode, "gpt-test".into(), Some(1024), Some(8192)).unwrap()
    }

    fn sample_tool() -> ToolDefinition {
        ToolDefinition {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            origin: ToolOrigin::Builtin,
            execution_mode: ExecutionMode::Parallel,
        }
    }

    #[test]
    fn chat_request_includes_prompt_cache_key() {
        let p = provider(OpenAiApiMode::ChatCompletions, true, None);
        let body = p.build_request_body(
            &[LlmMessage::system("s"), LlmMessage::user("hi")],
            &[sample_tool()],
        );
        assert!(
            body["prompt_cache_key"]
                .as_str()
                .is_some_and(|k| k.starts_with("astrcode-"))
        );
        assert_eq!(body["prompt_cache_retention"], "24h");
    }

    #[test]
    fn chat_request_includes_stream_usage_when_supported() {
        let p = provider(OpenAiApiMode::ChatCompletions, false, None);
        let body = p.build_request_body(&[LlmMessage::user("hi")], &[]);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn responses_count_body_keeps_provider_visible_input_and_tools() {
        let p = provider(OpenAiApiMode::Responses, true, Some(ThinkingLevel::Medium));
        let body = p.build_responses_count_body(
            &[LlmMessage::system("s"), LlmMessage::user("hi")],
            &[sample_tool()],
        );

        assert_eq!(
            p.input_tokens_endpoint(),
            "https://api.test/v1/responses/input_tokens"
        );
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["instructions"], "s");
        assert!(body["input"].is_array());
        assert!(body["tools"].is_array());
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert!(body.get("stream").is_none());
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn request_omits_prompt_cache_fields_when_unsupported() {
        let p = provider(OpenAiApiMode::ChatCompletions, false, None);
        let body = p.build_request_body(&[LlmMessage::system("s"), LlmMessage::user("hi")], &[]);
        assert!(body.get("prompt_cache_key").is_none());
    }

    #[test]
    fn cache_key_identical_for_same_system() {
        let p = provider(OpenAiApiMode::Responses, true, None);
        let t = vec![sample_tool()];
        let a = p.build_request_body(&[LlmMessage::system("s"), LlmMessage::user("a")], &t);
        let b = p.build_request_body(
            &[
                LlmMessage::system("s"),
                LlmMessage::user("b"),
                LlmMessage::assistant("hist"),
            ],
            &t,
        );
        assert_eq!(a["prompt_cache_key"], b["prompt_cache_key"]);
    }

    #[test]
    fn cache_key_differs_when_tools_differ() {
        let p = provider(OpenAiApiMode::Responses, true, None);
        let messages = [LlmMessage::system("s"), LlmMessage::user("hi")];
        let mut other = sample_tool();
        other.name = "other".into();

        let a = p.build_request_body(&messages, &[sample_tool()]);
        let b = p.build_request_body(&messages, &[other]);
        assert_ne!(a["prompt_cache_key"], b["prompt_cache_key"]);
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
    fn responses_request_includes_reasoning_effort_when_thinking_level_set() {
        let p = provider(OpenAiApiMode::Responses, false, Some(ThinkingLevel::High));
        let body = p.build_request_body(&[LlmMessage::system("s"), LlmMessage::user("hi")], &[]);
        assert_eq!(body["reasoning"]["effort"], "high");
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
