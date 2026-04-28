//! astrcode-ai：LLM 提供商抽象层。
//!
//! 提供 OpenAI 兼容的 API 客户端，支持 SSE 流式响应、指数退避重试、
//! Prompt 缓存感知以及多字节安全的 UTF-8 解码。

pub mod cache;
pub mod openai;
pub mod retry;
