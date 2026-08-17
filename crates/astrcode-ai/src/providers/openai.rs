//! OpenAI 兼容的 Chat Completions / Responses 提供商。
//!
//! 厂商 wire DTO 和流累积器都封装在 `wire::openai` 内部，本模块只暴露标准 provider。

use std::sync::Arc;

use astrcode_core::{config::OpenAiApiMode, llm::*, tool::ToolDefinition};
use tokio::sync::mpsc;

use crate::{
    common::{ConnectionSnapshot, HttpPostRequest, build_client, report_stream_error},
    strict_tools::{StrictToolProvider, prepare_strict_tools},
    wire::openai as openai_wire,
};

// ─── StandardProvider ───────────────────────────────────────────────────

/// 标准 OpenAI 兼容 provider。厂商 stream accumulator 固定在 wire 层内部。
pub struct StandardProvider {
    config: LlmClientConfig,
    api_mode: OpenAiApiMode,
    model_id: String,
    model_limits_val: ModelLimits,
    client: reqwest::Client,
}

impl StandardProvider {
    pub fn new(
        config: LlmClientConfig,
        api_mode: OpenAiApiMode,
        model_id: String,
        max_tokens: u32,
        context_limit: usize,
    ) -> Result<Self, LlmError> {
        let client = build_client(&config)?;
        Ok(Self {
            config,
            api_mode,
            model_id,
            model_limits_val: ModelLimits {
                max_input_tokens: context_limit,
                max_output_tokens: max_tokens as usize,
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

    fn wire_config(
        &self,
        max_output_tokens: Option<usize>,
    ) -> openai_wire::OpenAiRequestConfig<'_> {
        openai_wire::OpenAiRequestConfig {
            api_mode: self.api_mode,
            model_id: &self.model_id,
            max_output_tokens: self
                .model_limits_val
                .effective_output_cap(max_output_tokens),
            supports_stream_usage: self.config.supports_stream_usage(),
            supports_prompt_cache_key: self.config.supports_prompt_cache_key(),
            supports_strict_tool_use: self.config.supports_strict_tool_use,
            prompt_cache_retention: self.config.prompt_cache_retention(),
            thinking: &self.config.thinking,
            thinking_capability: self.config.effective_thinking_capability(),
        }
    }

    fn build_request_body(
        &self,
        messages: &[Arc<LlmMessage>],
        tools: &[ToolDefinition],
        max_output_tokens: Option<usize>,
    ) -> serde_json::Value {
        openai_wire::build_request_body(self.wire_config(max_output_tokens), messages, tools)
    }

    fn build_responses_count_body(
        &self,
        messages: &[Arc<LlmMessage>],
        tools: &[ToolDefinition],
    ) -> serde_json::Value {
        openai_wire::build_input_token_count_body(self.wire_config(None), messages, tools)
    }
}

// ─── LlmProvider impl ──────────────────────────────────────────────────

#[async_trait::async_trait]
impl LlmProvider for StandardProvider {
    async fn generate_request(
        &self,
        request: LlmRequest,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let LlmRequest {
            messages,
            mut tools,
            max_output_tokens,
        } = request;
        prepare_strict_tools(
            &mut tools,
            self.config.supports_strict_tool_use,
            StrictToolProvider::OpenAi,
        )?;
        let body = self.build_request_body(&messages, &tools, max_output_tokens);

        let endpoint = self.endpoint();
        let client = self.client.clone();
        let api_mode = self.api_mode;
        let snapshot = ConnectionSnapshot::from_config(&self.config);

        tokio::spawn(async move {
            let result = openai_wire::transport::stream_request(
                client,
                endpoint,
                snapshot,
                body,
                api_mode,
                tx.clone(),
            )
            .await;
            report_stream_error(result, &tx);
        });

        Ok(rx)
    }

    async fn count_input_tokens(
        &self,
        messages: Vec<Arc<LlmMessage>>,
        mut tools: Vec<ToolDefinition>,
    ) -> Result<ProviderInputTokenCount, LlmError> {
        if self.api_mode != OpenAiApiMode::Responses {
            return Err(LlmError::Unsupported {
                message: "OpenAI Chat Completions does not expose provider-side input token \
                          counting"
                    .into(),
            });
        }
        prepare_strict_tools(
            &mut tools,
            self.config.supports_strict_tool_use,
            StrictToolProvider::OpenAi,
        )?;

        let snapshot = ConnectionSnapshot::from_config(&self.config);
        let value = HttpPostRequest {
            client: self.client.clone(),
            endpoint: self.input_tokens_endpoint(),
            headers: snapshot.headers,
            body: self.build_responses_count_body(&messages, &tools),
            retry: snapshot.retry,
        }
        .json()
        .await?;
        let input_tokens = value
            .get("input_tokens")
            .or_else(|| value.get("inputTokens"))
            .or_else(|| value.get("total_tokens"))
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                LlmError::stream_parse(format!(
                    "OpenAI input token count response missing input_tokens: {value}"
                ))
            })?;
        Ok(ProviderInputTokenCount::provider_count(input_tokens))
    }

    fn model_limits(&self) -> ModelLimits {
        self.model_limits_val.clone()
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        config::OpenAiApiMode,
        llm::thinking::ThinkingConfig,
        tool::{ToolDefinition, ToolOrigin},
    };

    use super::*;
    use crate::common::{ScriptedLlmServer, http_json_response};

    fn provider(
        api_mode: OpenAiApiMode,
        supports_cache_key: bool,
        thinking: ThinkingConfig,
        thinking_capability: Option<astrcode_core::llm::thinking::ThinkingCapability>,
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
            }),
            thinking_configured: thinking_capability.is_some(),
            thinking,
            thinking_capability,
            ..LlmClientConfig::default()
        };
        StandardProvider::new(config, api_mode, "gpt-test".into(), 1024, 8192).unwrap()
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
            strict: false,
            origin: ToolOrigin::Bundled,
        }
    }

    #[test]
    fn chat_request_includes_prompt_cache_key() {
        let p = provider(
            OpenAiApiMode::ChatCompletions,
            true,
            ThinkingConfig::default(),
            None,
        );
        let body = p.build_request_body(
            &[
                Arc::new(LlmMessage::system("s")),
                Arc::new(LlmMessage::user("hi")),
            ],
            &[sample_tool()],
            Some(123),
        );
        assert!(
            body["prompt_cache_key"]
                .as_str()
                .is_some_and(|k| k.starts_with("astrcode-"))
        );
        assert_eq!(body["prompt_cache_retention"], "24h");
        assert_eq!(body["max_tokens"], 123);
    }

    #[test]
    fn chat_request_includes_stream_usage_when_supported() {
        let p = provider(
            OpenAiApiMode::ChatCompletions,
            false,
            ThinkingConfig::default(),
            None,
        );
        let body = p.build_request_body(&[Arc::new(LlmMessage::user("hi"))], &[], None);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn responses_count_body_keeps_provider_visible_input_and_tools() {
        let p = provider(
            OpenAiApiMode::Responses,
            true,
            ThinkingConfig {
                enabled: true,
                effort: Some("medium".into()),
                budget_tokens: None,
            },
            Some(astrcode_core::llm::thinking::ThinkingCapability {
                wire_mapping: astrcode_core::llm::thinking::ThinkingWireMapping::OpenAiResponses,
                allowed_effort: Some(vec!["medium".into()]),
                budget_min: None,
                budget_max: None,
                can_disable: false,
            }),
        );
        let body = p.build_responses_count_body(
            &[
                Arc::new(LlmMessage::system("s")),
                Arc::new(LlmMessage::user("hi")),
            ],
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
        assert!(body.get("reasoning").is_none());
        assert!(body.get("stream").is_none());
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn request_omits_prompt_cache_fields_when_unsupported() {
        let p = provider(
            OpenAiApiMode::ChatCompletions,
            false,
            ThinkingConfig::default(),
            None,
        );
        let body = p.build_request_body(
            &[
                Arc::new(LlmMessage::system("s")),
                Arc::new(LlmMessage::user("hi")),
            ],
            &[],
            None,
        );
        assert!(body.get("prompt_cache_key").is_none());
    }

    #[tokio::test]
    async fn generate_rejects_invalid_strict_schema_before_transport() {
        let mut provider = provider(
            OpenAiApiMode::Responses,
            false,
            ThinkingConfig::default(),
            None,
        );
        provider.config.supports_strict_tool_use = true;
        let mut invalid = sample_tool();
        invalid.strict = true;
        invalid.parameters = serde_json::json!({"type": "string"});

        let result = provider
            .generate_request(LlmRequest::new(
                vec![Arc::new(LlmMessage::user("hi"))],
                vec![invalid],
            ))
            .await;

        assert!(matches!(
            result,
            Err(LlmError::Unsupported { message })
                if message.contains("strict tool `read` schema at `$`")
        ));
    }

    #[test]
    fn cache_key_identical_for_same_system() {
        let p = provider(
            OpenAiApiMode::Responses,
            true,
            ThinkingConfig::default(),
            None,
        );
        let t = vec![sample_tool()];
        let a = p.build_request_body(
            &[
                Arc::new(LlmMessage::system("s")),
                Arc::new(LlmMessage::user("a")),
            ],
            &t,
            None,
        );
        let b = p.build_request_body(
            &[
                Arc::new(LlmMessage::system("s")),
                Arc::new(LlmMessage::user("b")),
                Arc::new(LlmMessage::assistant("hist")),
            ],
            &t,
            None,
        );
        assert_eq!(a["prompt_cache_key"], b["prompt_cache_key"]);
    }

    #[test]
    fn cache_key_differs_when_tools_differ() {
        let p = provider(
            OpenAiApiMode::Responses,
            true,
            ThinkingConfig::default(),
            None,
        );
        let messages = [
            Arc::new(LlmMessage::system("s")),
            Arc::new(LlmMessage::user("hi")),
        ];
        let mut other = sample_tool();
        other.name = "other".into();

        let a = p.build_request_body(&messages, &[sample_tool()], None);
        let b = p.build_request_body(&messages, &[other], None);
        assert_ne!(a["prompt_cache_key"], b["prompt_cache_key"]);
    }

    #[test]
    fn responses_request_includes_reasoning_effort_when_thinking_enabled_with_effort() {
        use astrcode_core::llm::thinking::{ThinkingCapability, ThinkingWireMapping};
        let p = provider(
            OpenAiApiMode::Responses,
            false,
            ThinkingConfig {
                enabled: true,
                effort: Some("high".into()),
                budget_tokens: None,
            },
            Some(ThinkingCapability {
                wire_mapping: ThinkingWireMapping::OpenAiResponses,
                allowed_effort: Some(vec!["high".into()]),
                budget_min: None,
                budget_max: None,
                can_disable: false,
            }),
        );
        let body = p.build_request_body(
            &[
                Arc::new(LlmMessage::system("s")),
                Arc::new(LlmMessage::user("hi")),
            ],
            &[],
            None,
        );
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    fn count_tokens_provider(base_url: String) -> StandardProvider {
        let config = LlmClientConfig {
            base_url,
            max_retries: 1,
            retry_base_delay_ms: 1,
            ..LlmClientConfig::default()
        };
        StandardProvider::new(
            config,
            OpenAiApiMode::Responses,
            "gpt-test".into(),
            1024,
            8192,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn count_input_tokens_parses_provider_response() {
        let server =
            ScriptedLlmServer::spawn(vec![http_json_response(r#"{"input_tokens":42}"#)]).await;
        let provider = count_tokens_provider(server.base_url().into());

        let count = provider
            .count_input_tokens(vec![Arc::new(LlmMessage::user("hi"))], vec![])
            .await
            .unwrap();

        assert_eq!(count.input_tokens, 42);
        server.assert_consumed();
    }

    #[tokio::test]
    async fn count_input_tokens_exhausts_retries_on_server_error() {
        let server = ScriptedLlmServer::spawn(vec![
            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec(),
            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec(),
        ])
        .await;
        let provider = count_tokens_provider(server.base_url().into());

        let result = provider
            .count_input_tokens(vec![Arc::new(LlmMessage::user("hi"))], vec![])
            .await;

        assert!(matches!(
            result,
            Err(LlmError::ServerError { status: 500, .. })
        ));
        server.assert_consumed();
    }
}
