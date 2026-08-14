//! Idle session 的 manual compact 入口。

use std::sync::Arc;

use astrcode_core::compaction::CompactStrategy;

use super::pipeline::{CompactionPipeline, CompactionPipelineOutcome};
use crate::{
    SessionError, projection_context::context_snapshot, session::Session,
    turn_context::hook_call_context_for_read_model,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManualCompactionOutcome {
    Compacted { messages_removed: usize },
    Skipped { message: String },
}

/// 压缩空闲 session。调用方必须在整个 await 期间持有 session operation guard。
pub async fn compact_manual_session(
    session: &Session,
    keep_recent_turns: Option<usize>,
) -> Result<ManualCompactionOutcome, SessionError> {
    let runtime_services = session.runtime_services();
    let (runtime_generation, runtime_view) = runtime_services.pin_turn_generation().await?;
    let extension_runner = runtime_view.turn_hooks_arc();
    let state = session.read_model().await?;
    let hook_call =
        hook_call_context_for_read_model(session.id(), &state, session.session_store_dir().await);
    let pre_hook_messages = context_snapshot(&state).messages;
    let tool_registry = session
        .tool_registry_snapshot_for_view(&runtime_view, &state.identity.working_dir)
        .await?;
    let tools = tool_registry.list_definitions();

    let outcome = CompactionPipeline {
        session,
        llm: runtime_generation.llm(),
        context_assembler: Arc::clone(runtime_generation.context_assembler()),
        extension_runner: extension_runner.as_ref(),
        hook_call,
        pre_hook_messages,
        tools: &tools,
        strategy: CompactStrategy::Manual { keep_recent_turns },
        use_llm: true,
    }
    .run()
    .await;

    match outcome {
        CompactionPipelineOutcome::Compacted {
            messages_removed, ..
        } => Ok(ManualCompactionOutcome::Compacted { messages_removed }),
        CompactionPipelineOutcome::Skipped { message, .. } => {
            Ok(ManualCompactionOutcome::Skipped { message })
        },
        CompactionPipelineOutcome::Failed { error, .. } => Err(error),
    }
}
