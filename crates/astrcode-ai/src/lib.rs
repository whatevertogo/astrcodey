//! astrcode-ai：LLM 提供商抽象层。
//!
//! 支持 OpenAI 兼容、Anthropic、Google Gemini 的 API 客户端。
//! 提供 SSE 流式响应、指数退避重试、多字节安全 UTF-8 解码，
//! 以及可替换的内容累积器 trait（[`ChatAccumulator`]）。

mod common;
mod retry;
mod serialization;
mod stream_decoder;

pub mod providers;

use std::sync::Arc;

use astrcode_core::{
    config::OpenAiApiMode,
    llm::{LlmClientConfig, LlmError, LlmProvider},
};
use providers::{
    anthropic::AnthropicProvider as Anthropic, google_genai::GeminiProvider as Gemini,
    openai::StandardProvider as OpenAiStandardProvider,
};
pub use providers::{
    anthropic::AnthropicProvider,
    google_genai::GeminiProvider,
    openai::{ChatAccumulator, StandardAccumulator, StandardProvider},
};
pub use retry::RetryPolicy;

/// 根据 `provider_kind`、`base_url` 和 `model_id` 创建 LLM provider。
///
/// 未知 `provider_kind` 默认走 OpenAI 兼容路径。
pub fn create_provider(
    provider_kind: &str,
    config: LlmClientConfig,
    api_mode: OpenAiApiMode,
    model_id: String,
    max_tokens: Option<u32>,
    context_limit: Option<usize>,
) -> Result<Arc<dyn LlmProvider>, LlmError> {
    let provider: Arc<dyn LlmProvider> = match provider_kind {
        "anthropic" => Arc::new(Anthropic::new(config, model_id, max_tokens, context_limit)?),
        "google_genai" | "gemini" => {
            Arc::new(Gemini::new(config, model_id, max_tokens, context_limit)?)
        },
        _ => Arc::new(OpenAiStandardProvider::new(
            config,
            api_mode,
            model_id,
            max_tokens,
            context_limit,
        )?),
    };
    Ok(provider)
}
