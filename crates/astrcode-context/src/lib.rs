//! astrcode-context：LLM 上下文窗口管理。
//!
//! 负责 system prompt 组装和 provider-ready 上下文构建：
//! - `prompt_engine`：system prompt 组装（稳定内容在前，动态内容在后）
//! - `context_assembler`：上下文窗口裁剪
//! - `compaction`：LLM 驱动的摘要压缩
//! - `token_budget`：token 估算
//! - `contribution`：扩展动态贡献类型

pub use astrcode_core::config::ContextSettings;
pub use context::{
    COMPACT_SUMMARY_MARKER, CompactError, CompactResult, CompactSkipReason,
    CompactSummaryRenderOptions, ContextSnapshot, NoopPostCompactEnricher,
    POST_COMPACT_CONTEXT_MARKER, PostCompactEnrichInput, PostCompactEnricher,
    is_compact_summary_message, is_compact_summary_text, is_prompt_too_long_message,
    is_synthetic_context_message,
};
pub use context_assembler::{ContextAssembler, ContextPrepareInput, PreparedContext};

pub mod compaction;
mod context;
pub mod context_assembler;
pub mod contribution;
pub mod post_compact_enricher;
pub mod prompt_engine;
pub mod token_budget;
