//! 空闲态会话 compact（手动命令 / HTTP），与 turn 内 auto/reactive 共用同一 compact 管线。

use std::sync::Arc;

use astrcode_context::{
    CompactSummaryRenderOptions, PostCompactEnrichInput, compaction::compact_messages_with_fallback,
};
use astrcode_core::compaction::{CompactStrategy, CompactTrigger};
use astrcode_extension_sdk::extension::ExtensionError;
use astrcode_storage::CompactSnapshotInput;

use crate::{
    Session,
    compact::{
        CompactHookContext, PersistCompactError, PersistCompactionOutcome,
        collect_compact_instructions, dispatch_post_compact, persist_compact_result,
        request_compact_summary,
    },
    projection_context::context_snapshot,
    session::SessionError,
};

/// 空闲态 compact 结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdleCompactionOutcome {
    Compacted { messages_removed: usize },
    Skipped { message: String },
}

#[derive(Debug, thiserror::Error)]
pub enum IdleCompactionError {
    #[error("{0}")]
    Session(#[from] SessionError),
    #[error("{0}")]
    Extension(#[from] ExtensionError),
    #[error("{0}")]
    Persist(#[from] PersistCompactError),
}

/// 在无 active turn 时压缩会话历史并持久化。
pub async fn compact_idle_session(
    session: &Session,
    keep_recent_turns: Option<usize>,
) -> Result<IdleCompactionOutcome, IdleCompactionError> {
    let runtime_services = session.runtime_services();
    let extension_runner = runtime_services.turn_hooks_arc();
    let context_assembler = runtime_services.context_assembler_arc();

    let state = session.read_model().await?;
    let pre_hook = CompactHookContext {
        session_id: session.id.as_str(),
        working_dir: &state.identity.working_dir,
        model_id: &state.identity.model_id,
        trigger: CompactTrigger::ManualCommand,
        message_count: state.transcript.messages.len(),
    };
    let custom_instructions =
        collect_compact_instructions(extension_runner.as_ref(), pre_hook).await?;

    let state = session.read_model().await?;
    let snapshot = context_snapshot(&state);
    let llm = runtime_services.llm();
    let post_hook = CompactHookContext {
        session_id: session.id.as_str(),
        working_dir: &state.identity.working_dir,
        model_id: &state.identity.model_id,
        trigger: CompactTrigger::ManualCommand,
        message_count: snapshot.messages.len(),
    };
    let tool_registry = session
        .tool_registry_snapshot(&state.identity.working_dir)
        .await?;
    let tools = tool_registry.list_definitions();
    let transcript_path = session
        .write_compact_snapshot(CompactSnapshotInput {
            trigger: CompactTrigger::ManualCommand.as_str().into(),
            model_id: state.identity.model_id.clone(),
            working_dir: state.identity.working_dir.clone(),
            system_prompt: Some(snapshot.system_prompt.clone()),
            provider_messages: snapshot.messages.clone(),
        })
        .await?;
    let render_options = CompactSummaryRenderOptions {
        transcript_path,
        custom_instructions: custom_instructions.clone(),
    };
    let compact_execution = compact_messages_with_fallback(
        &snapshot.messages,
        Some(&snapshot.system_prompt),
        context_assembler.settings(),
        &custom_instructions,
        &render_options,
        keep_recent_turns,
        |messages| request_compact_summary(Arc::clone(&llm), messages),
    )
    .await;

    let mut compaction = match compact_execution {
        Err(_) => {
            return Ok(IdleCompactionOutcome::Skipped {
                message: "Nothing to compact".into(),
            });
        },
        Ok(compaction) => compaction.result,
    };

    let session_store_dir = session.session_store_dir().await;
    session
        .runtime_services()
        .post_compact_enricher()
        .enrich(
            &mut compaction,
            PostCompactEnrichInput {
                session_id: session.id.as_str(),
                source_messages: &snapshot.messages,
                working_dir: &state.identity.working_dir,
                system_prompt: Some(&snapshot.system_prompt),
                tools: &tools,
                settings: context_assembler.settings(),
                session_store_dir,
            },
        )
        .await;

    let persisted = persist_compact_result(
        session,
        &compaction,
        CompactTrigger::ManualCommand.as_str(),
        snapshot.source_seq,
        CompactStrategy::Manual { keep_recent_turns },
    )
    .await?;
    match persisted {
        PersistCompactionOutcome::Committed => {},
        PersistCompactionOutcome::Stale => {
            return Ok(IdleCompactionOutcome::Skipped {
                message: "Context changed during compaction; retry".into(),
            });
        },
    };

    if let Err(error) =
        dispatch_post_compact(extension_runner.as_ref(), post_hook, &compaction).await
    {
        tracing::warn!(error = %error, "PostCompact extension dispatch failed");
    }
    Ok(IdleCompactionOutcome::Compacted {
        messages_removed: compaction.messages_removed,
    })
}
