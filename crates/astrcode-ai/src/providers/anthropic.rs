//! Anthropic Messages API provider.
//!
//! 厂商 wire DTO、SSE 事件状态机与字节流传输都封装在 [`crate::wire::anthropic`] 内部，
//! 本模块只暴露 provider 并连接配置/模型状态——结构与 [`crate::providers::openai`] 对称。

use std::sync::Arc;

use astrcode_core::{llm::*, tool::ToolDefinition};
use tokio::sync::mpsc;

use crate::{
    common::{
        ConnectionSnapshot, HttpPostRequest, build_client, ensure_header, report_stream_error,
    },
    strict_tools::{StrictToolProvider, prepare_strict_tools},
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
        max_tokens: u32,
        context_limit: usize,
    ) -> Result<Self, LlmError> {
        let client = build_client(&config)?;
        Ok(Self {
            config,
            model_id,
            model_limits_val: ModelLimits {
                max_input_tokens: context_limit,
                max_output_tokens: max_tokens as usize,
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

    fn wire_config(
        &self,
        max_output_tokens: Option<usize>,
    ) -> anthropic_wire::AnthropicRequestConfig<'_> {
        anthropic_wire::AnthropicRequestConfig {
            model_id: &self.model_id,
            max_output_tokens: self
                .model_limits_val
                .effective_output_cap(max_output_tokens),
            supports_strict_tool_use: self.config.supports_strict_tool_use,
            thinking: &self.config.thinking,
            thinking_capability: self.config.effective_thinking_capability(),
        }
    }

    fn build_request_body(
        &self,
        messages: &[Arc<LlmMessage>],
        tools: &[ToolDefinition],
        max_output_tokens: Option<usize>,
        stream: bool,
    ) -> Result<serde_json::Value, LlmError> {
        anthropic_wire::build_request_body(
            self.wire_config(max_output_tokens),
            messages,
            tools,
            stream,
        )
    }

    fn build_count_tokens_body(
        &self,
        messages: &[Arc<LlmMessage>],
        tools: &[ToolDefinition],
    ) -> serde_json::Value {
        anthropic_wire::build_count_tokens_body(self.wire_config(None), messages, tools)
    }

    /// count_tokens（JSON）路径用的连接快照：基础头 + 重试策略，附 Anthropic 版本头。
    fn count_snapshot(&self) -> ConnectionSnapshot {
        let mut snapshot = ConnectionSnapshot::from_config(&self.config);
        ensure_header(
            &mut snapshot.headers,
            "anthropic-version",
            anthropic_wire::body::ANTHROPIC_API_VERSION,
        );
        snapshot
    }
}

#[async_trait::async_trait]
impl LlmProvider for AnthropicProvider {
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
            StrictToolProvider::Anthropic,
        )?;
        let request_body = self.build_request_body(&messages, &tools, max_output_tokens, true)?;
        let endpoint = self.endpoint();
        let client = self.client.clone();
        let snapshot = ConnectionSnapshot::from_config(&self.config);

        tokio::spawn(async move {
            let result = anthropic_wire::transport::stream_request(
                client,
                endpoint,
                snapshot,
                request_body,
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
        prepare_strict_tools(
            &mut tools,
            self.config.supports_strict_tool_use,
            StrictToolProvider::Anthropic,
        )?;
        let snapshot = self.count_snapshot();
        let value = HttpPostRequest {
            client: self.client.clone(),
            endpoint: self.count_tokens_endpoint(),
            headers: snapshot.headers,
            body: self.build_count_tokens_body(&messages, &tools),
            retry: snapshot.retry,
        }
        .json()
        .await?;
        let input_tokens = value
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                LlmError::stream_parse(format!(
                    "Anthropic count_tokens response missing input_tokens: {value}"
                ))
            })?;
        Ok(ProviderInputTokenCount::provider_count(input_tokens))
    }

    fn minimum_output_tokens(&self) -> usize {
        use astrcode_core::llm::thinking::ThinkingWireMapping;

        let uses_budget_thinking = self.config.thinking.enabled
            && self
                .config
                .thinking_capability
                .as_ref()
                .is_some_and(|capability| {
                    capability.wire_mapping == ThinkingWireMapping::AnthropicBudget
                });
        if uses_budget_thinking {
            return self
                .config
                .thinking
                .budget_tokens
                .map_or(1, |budget| budget as usize + 1);
        }
        1
    }

    fn model_limits(&self) -> ModelLimits {
        self.model_limits_val.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{ScriptedLlmServer, http_json_response};

    #[tokio::test]
    async fn generate_rejects_invalid_strict_schema_before_transport() {
        let provider = AnthropicProvider::new(
            LlmClientConfig {
                base_url: "https://api.anthropic.com/v1".into(),
                supports_strict_tool_use: true,
                ..LlmClientConfig::default()
            },
            "claude-test".into(),
            1024,
            8192,
        )
        .unwrap();
        let body = provider
            .build_request_body(&[Arc::new(LlmMessage::user("hi"))], &[], Some(123), true)
            .unwrap();
        assert_eq!(body["max_tokens"], 123);

        let invalid = ToolDefinition {
            name: "bounded".into(),
            description: String::new(),
            parameters: serde_json::json!({"type": "string"}),
            strict: true,
            origin: astrcode_core::tool::ToolOrigin::Bundled,
        };

        let result = provider
            .generate_request(LlmRequest::new(
                vec![Arc::new(LlmMessage::user("hi"))],
                vec![invalid],
            ))
            .await;

        assert!(matches!(
            result,
            Err(LlmError::Unsupported { message })
                if message.contains("strict tool `bounded` schema at `$`")
        ));
    }

    fn count_tokens_provider(base_url: String) -> AnthropicProvider {
        AnthropicProvider::new(
            LlmClientConfig {
                base_url,
                max_retries: 1,
                retry_base_delay_ms: 1,
                ..LlmClientConfig::default()
            },
            "claude-test".into(),
            1024,
            8192,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn count_input_tokens_parses_provider_response() {
        let server =
            ScriptedLlmServer::spawn(vec![http_json_response(r#"{"input_tokens":17}"#)]).await;
        let provider = count_tokens_provider(server.base_url().into());

        let count = provider
            .count_input_tokens(vec![Arc::new(LlmMessage::user("hi"))], vec![])
            .await
            .unwrap();

        assert_eq!(count.input_tokens, 17);
        server.assert_consumed();
    }

    #[tokio::test]
    async fn count_input_tokens_rejects_response_missing_input_tokens() {
        let server =
            ScriptedLlmServer::spawn(vec![http_json_response(r#"{"output_tokens":3}"#)]).await;
        let provider = count_tokens_provider(server.base_url().into());

        let result = provider
            .count_input_tokens(vec![Arc::new(LlmMessage::user("hi"))], vec![])
            .await;

        assert!(matches!(result, Err(LlmError::StreamParse { .. })));
        server.assert_consumed();
    }
}
