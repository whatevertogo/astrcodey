//! astrcode-context：LLM 上下文窗口管理。
//!
//! 负责 system prompt 组装和 provider-ready 上下文构建：
//! - `prompt`：system prompt 组装（静态内容在前，动态内容在后）
//! - `manager`：上下文窗口裁剪
//! - `compaction`：LLM 驱动的摘要压缩
//! - `token_usage`：token 估算

pub use astrcode_core::config::ContextSettings;

pub mod compaction;
pub mod manager;
pub mod prompt;
pub mod token_usage;
