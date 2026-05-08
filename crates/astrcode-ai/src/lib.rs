//! astrcode-ai：LLM 提供商抽象层。
//!
//! 提供 OpenAI 兼容、Anthropic、Google Gemini 的 API 客户端，
//! 支持 SSE 流式响应、指数退避重试以及多字节安全的 UTF-8 解码。

pub mod anthropic;
pub mod common;
pub mod google_genai;
pub mod openai;
pub mod retry;
pub mod serialization;
pub mod stream_decoder;

use astrcode_core::{
    config::OpenAiApiMode,
    llm::{LlmClientConfig, LlmProvider},
};
use std::sync::Arc;

/// 根据 `provider_kind` 创建对应的 LLM provider 实例。
pub fn create_provider(
    provider_kind: &str,
    config: LlmClientConfig,
    api_mode: OpenAiApiMode,
    model_id: String,
    max_tokens: Option<u32>,
    context_limit: Option<usize>,
) -> Arc<dyn LlmProvider> {
    match provider_kind {
        "anthropic" => Arc::new(anthropic::AnthropicProvider::new(
            config,
            model_id,
            max_tokens,
            context_limit,
        )),
        "google_genai" | "gemini" => Arc::new(google_genai::GeminiProvider::new(
            config,
            model_id,
            max_tokens,
            context_limit,
        )),
        _ => Arc::new(openai::OpenAiProvider::new(
            config,
            api_mode,
            model_id,
            max_tokens,
            context_limit,
        )),
    }
}
