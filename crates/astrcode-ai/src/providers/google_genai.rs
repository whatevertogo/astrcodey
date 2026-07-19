//! Google Gemini API provider.
//!
//! Implements [`LlmProvider`] for Google's generativelanguage API with SSE
//! streaming, function calling, and thinking support.

use astrcode_core::{llm::*, tool::ToolDefinition};
use tokio::sync::mpsc;

use crate::{
    common::{
        HttpPostRequest, SharedStreamSink, StreamEventSink, apply_auth_header, build_client,
        ensure_header, retry_policy_from_config, send_event, stream_with_retry,
        token_usage_has_value,
    },
    wire::google_genai as google_wire,
};

pub struct GeminiProvider {
    config: LlmClientConfig,
    model_id: String,
    model_limits_val: ModelLimits,
    client: reqwest::Client,
}

impl GeminiProvider {
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
                max_input_tokens: context_limit.unwrap_or(1_048_576),
                max_output_tokens: max_tokens.unwrap_or(8192) as usize,
            },
            client,
        })
    }

    fn endpoint(&self) -> String {
        google_wire::endpoint_url(&self.config.base_url, &self.model_id)
    }

    fn count_tokens_endpoint(&self) -> String {
        google_wire::count_tokens_endpoint(&self.config.base_url, &self.model_id)
    }

    fn wire_config(&self) -> google_wire::GoogleGenAiRequestConfig {
        google_wire::GoogleGenAiRequestConfig {
            max_output_tokens: self.model_limits_val.max_output_tokens,
        }
    }

    fn build_request_body(
        &self,
        messages: &[LlmMessage],
        tools: &[ToolDefinition],
    ) -> serde_json::Value {
        google_wire::build_request_body(self.wire_config(), messages, tools)
    }

    fn build_count_tokens_body(
        &self,
        messages: &[LlmMessage],
        tools: &[ToolDefinition],
    ) -> serde_json::Value {
        google_wire::build_count_tokens_body(self.wire_config(), messages, tools)
    }

    fn headers(&self) -> Vec<(String, String)> {
        let mut headers: Vec<(String, String)> = self
            .config
            .extra_headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        apply_auth_header(&mut headers, self.config.auth_scheme, &self.config.api_key);
        headers
    }
}

#[async_trait::async_trait]
impl LlmProvider for GeminiProvider {
    async fn generate(
        &self,
        messages: Vec<LlmMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let body = self.build_request_body(&messages, &tools);

        let endpoint = self.endpoint();
        let mut headers = self.headers();
        ensure_header(&mut headers, "Accept", "text/event-stream");
        let client = self.client.clone();
        let retry = retry_policy_from_config(&self.config);

        tokio::spawn(async move {
            let sink = SharedStreamSink::new();
            let result = stream_with_retry(
                client,
                endpoint,
                headers,
                body,
                retry,
                tx.clone(),
                sink.wrap(|sink, _event_type, event, tx| process_gemini_chunk(event, tx, sink)),
            )
            .await;
            sink.finalize(result, &tx);
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
            .get("totalTokens")
            .or_else(|| value.get("total_tokens"))
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                LlmError::StreamParse(format!(
                    "Gemini countTokens response missing totalTokens: {value}"
                ))
            })?;
        Ok(ProviderInputTokenCount::provider_count(input_tokens))
    }

    fn model_limits(&self) -> ModelLimits {
        self.model_limits_val.clone()
    }
}

fn process_gemini_chunk(
    event: &serde_json::Value,
    tx: &mpsc::UnboundedSender<LlmEvent>,
    sink: &mut StreamEventSink,
) -> bool {
    if !sink.usage_reported() {
        if let Some(usage) = extract_gemini_token_usage(event) {
            if !send_event(tx, LlmEvent::Usage { usage }) {
                return false;
            }
            sink.mark_usage_reported();
        }
    }

    let Some(candidates) = event.get("candidates").and_then(|v| v.as_array()) else {
        return true;
    };

    for candidate in candidates {
        let Some(parts) = candidate
            .pointer("/content/parts")
            .and_then(|v| v.as_array())
        else {
            continue;
        };

        for part in parts {
            if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                let event = if part
                    .get("thought")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    LlmEvent::ThinkingDelta {
                        delta: text.to_string(),
                    }
                } else {
                    LlmEvent::ContentDelta {
                        delta: text.to_string(),
                    }
                };
                if !send_event(tx, event) {
                    return false;
                }
            }

            if let Some(fc) = part.get("functionCall") {
                let name = fc.get("name").and_then(|v| v.as_str()).unwrap_or_default();
                let call_id = sink.tool_call_id(fc.get("id").and_then(|v| v.as_str()));
                let args = fc.get("args").cloned().unwrap_or(serde_json::json!({}));
                let arguments_str = serde_json::to_string(&args).unwrap_or_default();
                if !send_event(
                    tx,
                    LlmEvent::ToolCallStart {
                        call_id: call_id.clone(),
                        name: name.to_string(),
                        arguments: arguments_str,
                    },
                ) {
                    return false;
                }
                if !send_event(tx, LlmEvent::ToolCallCompleted { call_id }) {
                    return false;
                }
            }
        }

        if let Some(finish) = candidate.get("finishReason").and_then(|v| v.as_str()) {
            if !finish.is_empty() && !sink.emit_done(tx, finish) {
                return false;
            }
        }
    }

    true
}

fn extract_gemini_token_usage(event: &serde_json::Value) -> Option<LlmTokenUsage> {
    let usage = event.get("usageMetadata")?;
    let token_usage = LlmTokenUsage {
        input_tokens: usage.get("promptTokenCount").and_then(|v| v.as_u64()),
        cached_input_tokens: usage
            .get("cachedContentTokenCount")
            .and_then(|v| v.as_u64()),
        cache_creation_input_tokens: None,
        output_tokens: usage.get("candidatesTokenCount").and_then(|v| v.as_u64()),
        reasoning_output_tokens: usage.get("thoughtsTokenCount").and_then(|v| v.as_u64()),
        total_tokens: usage.get("totalTokenCount").and_then(|v| v.as_u64()),
        source: Some(LlmTokenUsageSource::ProviderUsage),
    };
    token_usage_has_value(&token_usage).then_some(token_usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_preserves_multiple_system_messages() {
        let provider = GeminiProvider::new(
            LlmClientConfig::default(),
            "gemini-test".into(),
            Some(1024),
            Some(8192),
        )
        .unwrap();
        let body = provider.build_request_body(
            &[
                LlmMessage::system("static instructions"),
                LlmMessage::system("project instructions"),
                LlmMessage::user("hi"),
            ],
            &[],
        );

        assert_eq!(
            body["systemInstruction"]["parts"][0]["text"],
            "static instructions\n\nproject instructions"
        );
    }

    #[test]
    fn count_tokens_request_reuses_generate_content_shape_without_generation_config() {
        let provider = GeminiProvider::new(
            LlmClientConfig {
                base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
                ..LlmClientConfig::default()
            },
            "gemini-test".into(),
            Some(1024),
            Some(8192),
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
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-test:countTokens"
        );
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "s");
        assert!(body["contents"].is_array());
        assert!(body["tools"].is_array());
        assert!(body.get("generationConfig").is_none());
    }

    #[test]
    fn endpoint_includes_model_and_key() {
        let provider = GeminiProvider::new(
            LlmClientConfig {
                base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
                api_key: "test-key".into(),
                ..LlmClientConfig::default()
            },
            "gemini-2.5-pro".into(),
            None,
            None,
        )
        .unwrap();
        let endpoint = provider.endpoint();
        assert!(endpoint.contains("gemini-2.5-pro"));
        assert!(endpoint.contains("streamGenerateContent"));
        assert!(!endpoint.contains("test-key"));
    }

    #[test]
    fn gemini_done_event_is_emitted_once() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut sink = StreamEventSink::new();
        let event = serde_json::json!({
            "candidates": [{
                "content": {"parts": [{"text": "hi"}]},
                "finishReason": "STOP"
            }]
        });
        assert!(process_gemini_chunk(&event, &tx, &mut sink));
        assert!(process_gemini_chunk(&event, &tx, &mut sink));
        let done_count = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|event| matches!(event, LlmEvent::Done { .. }))
            .count();
        assert_eq!(done_count, 1);
        assert!(sink.done_sent());
    }

    #[test]
    fn gemini_usage_metadata_emits_token_usage() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut sink = StreamEventSink::new();
        let event = serde_json::json!({
            "usageMetadata": {
                "promptTokenCount": 100,
                "cachedContentTokenCount": 64,
                "candidatesTokenCount": 20,
                "thoughtsTokenCount": 5,
                "totalTokenCount": 120
            },
            "candidates": []
        });

        assert!(process_gemini_chunk(&event, &tx, &mut sink));

        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
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
        assert!(sink.usage_reported());
    }

    #[test]
    fn gemini_fallback_call_ids_are_unique_without_provider_id() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut sink = StreamEventSink::new();
        let event = serde_json::json!({
            "candidates": [{
                "content": {"parts": [
                    {"functionCall": {"name": "read", "args": {}}},
                    {"functionCall": {"name": "read", "args": {}}}
                ]}
            }]
        });
        assert!(process_gemini_chunk(&event, &tx, &mut sink));

        let call_ids: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|event| match event {
                LlmEvent::ToolCallStart { call_id, .. } => Some(call_id),
                _ => None,
            })
            .collect();
        assert_eq!(call_ids, vec!["call_1", "call_2"]);
    }
}
