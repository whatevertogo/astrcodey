//! astrcode-ai：LLM 提供商抽象层。
//!
//! 支持 OpenAI 兼容、Anthropic、Google Gemini 的 API 客户端。
//! 提供 SSE 流式响应、指数退避重试、多字节安全 UTF-8 解码，
//! 以及可替换的内容累积器 trait（[`ChatAccumulator`]）。

mod common;
mod provider_catalog;
mod retry;
mod serialization;
mod stream_decoder;
mod tool_result_wire;
mod wire;

pub mod providers;

use std::sync::Arc;

use astrcode_core::{
    config::ProviderWireFormat,
    llm::{LlmClientConfig, LlmError, LlmProvider},
};
pub use providers::{
    anthropic::AnthropicProvider,
    google_genai::GeminiProvider,
    openai::{ChatAccumulator, StandardAccumulator, StandardProvider},
};
pub use retry::RetryPolicy;

/// 根据显式 wire format、连接配置和模型创建 LLM provider。
pub fn create_provider(
    provider_kind: &str,
    wire_format: ProviderWireFormat,
    config: LlmClientConfig,
    model_id: String,
    max_tokens: Option<u32>,
    context_limit: Option<usize>,
) -> Result<Arc<dyn LlmProvider>, LlmError> {
    let instance = provider_catalog::ProviderInstance::resolve(
        provider_kind,
        wire_format,
        config,
        model_id,
        max_tokens,
        context_limit,
    );
    provider_catalog::build_provider(instance)
}
