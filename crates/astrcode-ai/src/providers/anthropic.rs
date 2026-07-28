//! Anthropic Messages API provider.
//!
//! 厂商 wire DTO、SSE 事件状态机与字节流传输都封装在 [`crate::wire::anthropic`] 内部，
//! 本模块只暴露 provider 并连接配置/模型状态——结构与 [`crate::providers::openai`] 对称。

use astrcode_core::{llm::*, tool::ToolDefinition};
use tokio::sync::mpsc;

use crate::{
    common::{
        HttpPostRequest, apply_auth_header, build_client, ensure_header, report_stream_error,
        retry_policy_from_config,
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
            supports_strict_tool_use: self.config.supports_strict_tool_use,
            thinking: &self.config.thinking,
            thinking_capability: self
                .config
                .thinking_configured
                .then_some(self.config.thinking_capability.as_ref())
                .flatten(),
        }
    }

    fn build_request_body(
        &self,
        messages: &[LlmMessage],
        tools: &[ToolDefinition],
        stream: bool,
    ) -> Result<serde_json::Value, LlmError> {
        anthropic_wire::build_request_body(self.wire_config(), messages, tools, stream)
    }

    fn build_count_tokens_body(
        &self,
        messages: &[LlmMessage],
        tools: &[ToolDefinition],
    ) -> serde_json::Value {
        anthropic_wire::build_count_tokens_body(self.wire_config(), messages, tools)
    }

    /// count_tokens（JSON）路径用的基础请求头：用户自定义头 + 鉴权 + Anthropic 版本。
    fn headers(&self) -> Vec<(String, String)> {
        let mut headers: Vec<(String, String)> = self
            .config
            .extra_headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        apply_auth_header(&mut headers, self.config.auth_scheme, &self.config.api_key);
        ensure_header(
            &mut headers,
            "anthropic-version",
            anthropic_wire::body::ANTHROPIC_API_VERSION,
        );
        headers
    }
}

#[async_trait::async_trait]
impl LlmProvider for AnthropicProvider {
    async fn generate(
        &self,
        messages: Vec<LlmMessage>,
        mut tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        prepare_strict_tools(
            &mut tools,
            self.config.supports_strict_tool_use,
            StrictToolProvider::Anthropic,
        )?;
        let request_body = self.build_request_body(&messages, &tools, true)?;
        let endpoint = self.endpoint();
        let api_key = self.config.api_key.clone();
        let auth_scheme = self.config.auth_scheme;
        let extra_headers = self.config.extra_headers.clone();
        let client = self.client.clone();
        let retry = retry_policy_from_config(&self.config);

        tokio::spawn(async move {
            let result = anthropic_wire::transport::stream_request(
                client,
                endpoint,
                api_key,
                auth_scheme,
                extra_headers,
                request_body,
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
        mut tools: Vec<ToolDefinition>,
    ) -> Result<ProviderInputTokenCount, LlmError> {
        prepare_strict_tools(
            &mut tools,
            self.config.supports_strict_tool_use,
            StrictToolProvider::Anthropic,
        )?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn generate_rejects_invalid_strict_schema_before_transport() {
        let provider = AnthropicProvider::new(
            LlmClientConfig {
                base_url: "https://api.anthropic.com/v1".into(),
                supports_strict_tool_use: true,
                ..LlmClientConfig::default()
            },
            "claude-test".into(),
            None,
            None,
        )
        .unwrap();
        let invalid = ToolDefinition {
            name: "bounded".into(),
            description: String::new(),
            parameters: serde_json::json!({"type": "string"}),
            strict: true,
            origin: astrcode_core::tool::ToolOrigin::Builtin,
            execution_mode: astrcode_core::tool::ExecutionMode::Parallel,
        };

        let result = provider
            .generate(vec![LlmMessage::user("hi")], vec![invalid])
            .await;

        assert!(matches!(
            result,
            Err(LlmError::Unsupported(message))
                if message.contains("strict tool `bounded` schema at `$`")
        ));
    }
}
