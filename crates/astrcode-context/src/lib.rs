//! astrcode-context：LLM 上下文组装 crate。
//!
//! 提供 system prompt 组装、token 估算和 LLM 驱动的摘要压缩，
//! 生成 provider-ready 的完整 LLM 上下文。

pub mod compaction;
pub mod manager;
pub mod prompt;
pub mod settings;
pub mod token_usage;
