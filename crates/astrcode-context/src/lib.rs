//! astrcode-context：LLM 上下文窗口管理。
//!
//! 负责 system prompt 组装和 provider-ready 上下文构建：
//! - `prompt_engine`：system prompt 组装（稳定内容在前，动态内容在后）
//! - `context_assembler`：上下文窗口裁剪
//! - `compaction`：LLM 驱动的摘要压缩
//! - `token_budget`：token 估算

pub use astrcode_core::{
    compaction::{
        COMPACT_SUMMARY_MARKER, POST_COMPACT_CONTEXT_MARKER, is_compact_summary_message,
        is_compact_summary_text, is_synthetic_context_message,
    },
    config::ContextSettings,
};
pub use context::{
    CompactError, CompactResult, CompactRetainedContext, CompactSkipReason,
    CompactSummaryRenderOptions, ContextSnapshot, is_prompt_too_long_message,
};
pub use context_assembler::{ContextAssembler, ContextPrepareInput, PreparedContext};

pub mod compaction;
mod context;
pub mod context_assembler;
pub mod prompt_engine;
pub mod token_budget;
pub mod token_estimate;
