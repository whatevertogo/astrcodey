//! 空闲态会话 compact（手动命令 / HTTP），与 turn 内 auto/reactive 共用同一 compact 管线。

use std::sync::Arc;

use astrcode_context::{
    compaction::CompactSummaryRenderOptions,
    context_assembler::{CompactIfNeededOutcome, CompactMessagesOptions, LlmContextAssembler},
};
use astrcode_core::{
    extension::{CompactStrategy, CompactTrigger},
    llm::LlmProvider,
    storage::SessionReadModel,
    tool::ToolDefinition,
};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_support::hash::hex_fingerprint;

use crate::{
    Session,
    compact::{
        CompactHookContext, PersistCompactError, collect_compact_instructions,
        compact_trigger_name, dispatch_post_compact, make_compact_request_fn,
        persist_compact_result,
    },
    post_compact::enrich_post_compact_context,
    session::SessionError,
};

/// 空闲态 compact 参数。
pub struct IdleCompactionParams {
    pub keep_recent_turns: Option<usize>,
    pub transcript_path: Option<String>,
}

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
    Extension(#[from] astrcode_core::extension::ExtensionError),
    #[error("{0}")]
    Persist(#[from] PersistCompactError),
    #[error("{0}")]
    InvalidRequest(String),
}

/// 在无 active turn 时压缩会话历史并持久化。
pub async fn compact_idle_session(
    session: &Session,
    extension_runner: &ExtensionRunner,
    context_assembler: &LlmContextAssembler,
    llm: Arc<dyn LlmProvider>,
    state: &SessionReadModel,
    tools: &[ToolDefinition],
    params: IdleCompactionParams,
) -> Result<IdleCompactionOutcome, IdleCompactionError> {
    let provider_messages = state.provider_messages();
    let hook_ctx = CompactHookContext {
        session_id: session.id.as_str(),
        working_dir: &state.working_dir,
        model_id: &state.model_id,
        trigger: CompactTrigger::ManualCommand,
        message_count: provider_messages.len(),
    };
    let custom_instructions = collect_compact_instructions(extension_runner, hook_ctx).await?;
    let base_event_seq = session
        .latest_cursor()
        .await?
        .and_then(|c| c.parse::<u64>().ok())
        .unwrap_or(0);
    let render_options = CompactSummaryRenderOptions {
        transcript_path: params.transcript_path,
        custom_instructions: custom_instructions.clone(),
    };
    let request_fn = make_compact_request_fn(llm);
    let compact_outcome = context_assembler
        .compact_if_needed(
            provider_messages.clone(),
            state.system_prompt.as_deref(),
            &custom_instructions,
            render_options,
            CompactMessagesOptions {
                run: true,
                use_llm: true,
                keep_recent_turns: params.keep_recent_turns,
            },
            request_fn,
        )
        .await;

    let mut compaction = match compact_outcome {
        CompactIfNeededOutcome::NotRun { .. } | CompactIfNeededOutcome::Skipped { .. } => {
            return Ok(IdleCompactionOutcome::Skipped {
                message: "Nothing to compact".into(),
            });
        },
        CompactIfNeededOutcome::Applied {
            compaction,
            messages: _,
        } => compaction.result,
    };

    let session_store_dir = session.session_store_dir().await;
    enrich_post_compact_context(
        &mut compaction,
        session.id.as_str(),
        &provider_messages,
        &state.working_dir,
        state.system_prompt.as_deref(),
        tools,
        context_assembler.settings(),
        session_store_dir,
    )
    .await;

    dispatch_post_compact(extension_runner, hook_ctx, &compaction).await?;

    let system_prompt = state.system_prompt.clone().ok_or_else(|| {
        IdleCompactionError::InvalidRequest("Cannot compact session without system prompt".into())
    })?;
    let fingerprint = hex_fingerprint(system_prompt.as_bytes());
    let persisted = match persist_compact_result(
        session,
        &compaction,
        compact_trigger_name(CompactTrigger::ManualCommand),
        &system_prompt,
        &fingerprint,
        state.extra_system_prompt.as_deref(),
        base_event_seq,
        CompactStrategy::Manual {
            keep_recent_turns: params.keep_recent_turns,
        },
    )
    .await
    {
        Ok(persisted) => persisted,
        Err(error) => return Err(error.into()),
    };

    Ok(IdleCompactionOutcome::Compacted {
        messages_removed: persisted.messages_removed,
    })
}
