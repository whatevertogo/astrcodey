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
    anthropic::AnthropicProvider, openai::StandardProvider as OpenAiStandardProvider,
};

pub(crate) fn build_provider(
    provider_kind: &str,
    wire_format: ProviderWireFormat,
    config: LlmClientConfig,
    model_id: String,
    max_tokens: u32,
    context_limit: usize,
) -> Result<Arc<dyn LlmProvider>, LlmError> {
    tracing::debug!(
        provider_kind,
        ?wire_format,
        "resolved LLM provider wire format"
    );
    let provider: Arc<dyn LlmProvider> = match wire_format {
        ProviderWireFormat::AnthropicMessages => Arc::new(AnthropicProvider::new(
            config,
            model_id,
            max_tokens,
            context_limit,
        )?),
        ProviderWireFormat::OpenAiChatCompletions | ProviderWireFormat::OpenAiResponses => {
            let api_mode = wire_format
                .openai_api_mode()
                .ok_or_else(|| LlmError::Unsupported {
                    message: format!(
                        "provider '{}' does not use an OpenAI wire format",
                        provider_kind
                    ),
                })?;
            Arc::new(OpenAiStandardProvider::new(
                config,
                api_mode,
                model_id,
                max_tokens,
                context_limit,
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
