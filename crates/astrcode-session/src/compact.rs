//! Compact pipeline — hook 桥接。
//!
//! 包含：
//! - [`CompactHookContext`]：在 compact 前后触发扩展钩子所需的上下文
//! - [`collect_compact_instructions`] / [`dispatch_post_compact`]：扩展钩子桥接
//!
//! 设计约束：这个模块不持有任何会话状态，所有参数通过调用方传入。

use astrcode_core::{
    config::ModelSelection,
    extension::{
        CompactContext, CompactEvent, CompactResult as TypedCompactResult, CompactTrigger,
        ExtensionError,
    },
};
use astrcode_extensions::runner::ExtensionRunner;

// ─── Hook 上下文 ─────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
pub struct CompactHookContext<'a> {
    pub session_id: &'a str,
    pub working_dir: &'a str,
    pub model_id: &'a str,
    pub trigger: CompactTrigger,
    pub message_count: usize,
}

pub async fn collect_compact_instructions(
    extension_runner: &ExtensionRunner,
    input: CompactHookContext<'_>,
) -> Result<Vec<String>, ExtensionError> {
    let ctx = CompactContext {
        session_id: input.session_id.to_string(),
        working_dir: input.working_dir.to_string(),
        model: ModelSelection::simple(input.model_id),
        trigger: input.trigger,
        message_count: input.message_count,
        pre_tokens: None,
        post_tokens: None,
        summary: None,
    };
    let result = extension_runner
        .emit_compact(CompactEvent::PreCompact, ctx)
        .await?;
    match result {
        TypedCompactResult::Contributions(c) => Ok(c
            .instructions
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()),
        TypedCompactResult::Block { reason } => Err(ExtensionError::Blocked { reason }),
        TypedCompactResult::Allow => Ok(Vec::new()),
    }
}

pub async fn dispatch_post_compact(
    extension_runner: &ExtensionRunner,
    input: CompactHookContext<'_>,
    compaction: &astrcode_context::compaction::CompactResult,
) -> Result<(), ExtensionError> {
    let ctx = CompactContext {
        session_id: input.session_id.to_string(),
        working_dir: input.working_dir.to_string(),
        model: ModelSelection::simple(input.model_id),
        trigger: input.trigger,
        message_count: input.message_count,
        pre_tokens: Some(compaction.pre_tokens),
        post_tokens: Some(compaction.post_tokens),
        summary: Some(compaction.summary.clone()),
    };
    extension_runner
        .emit_compact(CompactEvent::PostCompact, ctx)
        .await?;
    Ok(())
}

pub fn compact_trigger_name(trigger: CompactTrigger) -> &'static str {
    match trigger {
        CompactTrigger::AutoThreshold => "auto_threshold",
        CompactTrigger::ManualCommand => "manual_command",
    }
}

