//! astrcode-ai: LLM provider abstraction.
//!
//! OpenAI-compatible API client with SSE streaming, exponential backoff retry,
//! prompt caching awareness, and multi-byte safe UTF-8 decoding.

pub mod cache;
pub mod openai;
pub mod retry;
