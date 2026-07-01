//! Provider catalog: translate profile-level provider choices into concrete wire formats.
//!
//! `provider_kind` describes the user's provider family for display/logging, while
//! `ProviderWireFormat` describes the actual protocol shape. Keeping construction here makes
//! `lib.rs` a thin boundary instead of another provider switchboard.

use std::sync::Arc;

use astrcode_core::{
    config::ProviderWireFormat,
    llm::{LlmClientConfig, LlmError, LlmProvider},
};

use crate::providers::{
    anthropic::AnthropicProvider, google_genai::GeminiProvider,
    openai::StandardProvider as OpenAiStandardProvider,
};

pub(crate) struct ProviderInstance {
    provider_kind: String,
    wire_format: ProviderWireFormat,
    config: LlmClientConfig,
    model_id: String,
    max_tokens: Option<u32>,
    context_limit: Option<usize>,
}

impl ProviderInstance {
    pub(crate) fn resolve(
        provider_kind: &str,
        wire_format: ProviderWireFormat,
        config: LlmClientConfig,
        model_id: String,
        max_tokens: Option<u32>,
        context_limit: Option<usize>,
    ) -> Self {
        tracing::debug!(
            provider_kind,
            ?wire_format,
            "resolved LLM provider wire format"
        );
        Self {
            provider_kind: provider_kind.to_string(),
            wire_format,
            config,
            model_id,
            max_tokens,
            context_limit,
        }
    }
}

pub(crate) fn build_provider(instance: ProviderInstance) -> Result<Arc<dyn LlmProvider>, LlmError> {
    let provider: Arc<dyn LlmProvider> = match instance.wire_format {
        ProviderWireFormat::AnthropicMessages => Arc::new(AnthropicProvider::new(
            instance.config,
            instance.model_id,
            instance.max_tokens,
            instance.context_limit,
        )?),
        ProviderWireFormat::GoogleGenAi => Arc::new(GeminiProvider::new(
            instance.config,
            instance.model_id,
            instance.max_tokens,
            instance.context_limit,
        )?),
        ProviderWireFormat::OpenAiChatCompletions | ProviderWireFormat::OpenAiResponses => {
            let api_mode = instance.wire_format.openai_api_mode().ok_or_else(|| {
                LlmError::Unsupported(format!(
                    "provider '{}' does not use an OpenAI wire format",
                    instance.provider_kind
                ))
            })?;
            Arc::new(OpenAiStandardProvider::new(
                instance.config,
                api_mode,
                instance.model_id,
                instance.max_tokens,
                instance.context_limit,
            )?)
        },
    };
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_wire_formats_map_to_internal_api_mode() {
        assert!(
            ProviderWireFormat::OpenAiChatCompletions
                .openai_api_mode()
                .is_some()
        );
        assert!(
            ProviderWireFormat::OpenAiResponses
                .openai_api_mode()
                .is_some()
        );
        assert!(
            ProviderWireFormat::AnthropicMessages
                .openai_api_mode()
                .is_none()
        );
    }
}
