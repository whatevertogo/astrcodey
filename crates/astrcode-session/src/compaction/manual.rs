//! Idle session 的 manual compact 入口。

use astrcode_core::compaction::{CompactStrategy, CompactTrigger};

use super::pipeline::{CompactionPipeline, CompactionPipelineOutcome};
use crate::{SessionError, session::Session, turn_context::SharedTurnContext};

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
    let runtime_view = runtime_services.turn_runtime_view().await?;
    let extension_runner = runtime_view.turn_hooks_arc();
    let state = session.read_model().await?;
    let session_store_dir = session.session_store_dir().await;
    let shared =
        SharedTurnContext::from_read_model(session.id(), &state, session_store_dir.clone());
    let tool_registry = session
        .tool_registry_snapshot_for_view(&runtime_view, &state.identity.working_dir)
        .await?;

    let outcome = CompactionPipeline {
        session,
        llm: runtime_services.llm(),
        extension_runner: extension_runner.as_ref(),
        hook_call: shared.hook_call_context(),
        pre_hook_message_count: state.model_context.messages.len(),
        tools: tool_registry.list_definitions(),
        working_dir: state.identity.working_dir.clone(),
        session_store_dir,
        turn_id: None,
        trigger: CompactTrigger::ManualCommand,
        strategy: CompactStrategy::Manual { keep_recent_turns },
        use_llm: true,
        keep_recent_turns,
        write_transcript_snapshot: true,
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
