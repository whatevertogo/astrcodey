//! Compact pipeline — hook 桥接与 LLM 请求构造。

use std::sync::Arc;

use astrcode_context::{CompactError, CompactResult};
use astrcode_core::{
    compaction::CompactStrategy,
    config::ModelSelection,
    llm::{self, LlmProvider},
};
use astrcode_extension_sdk::{
    extension::{
        CompactContext, CompactEvent, CompactResult as TypedCompactResult, ExtensionError,
    },
    runtime_ports::TurnHooks,
};
use astrcode_storage::StorageError;

use crate::{Session, session::SessionError};

#[derive(Clone, Copy)]
pub struct CompactHookContext<'a> {
    pub session_id: &'a str,
    pub working_dir: &'a str,
    pub model_id: &'a str,
    pub trigger: astrcode_core::compaction::CompactTrigger,
    pub message_count: usize,
}

impl<'a> CompactHookContext<'a> {
    fn build_compact_context(&self, compaction: Option<&CompactResult>) -> CompactContext {
        CompactContext {
            session_id: self.session_id.to_string(),
            working_dir: self.working_dir.to_string(),
            model: ModelSelection::simple(self.model_id),
            trigger: self.trigger,
            message_count: self.message_count,
            pre_tokens: compaction.map(|c| c.pre_tokens),
            post_tokens: compaction.map(|c| c.post_tokens),
            summary: compaction.map(|c| c.summary.clone()),
        }
    }
}

pub async fn collect_compact_instructions(
    extension_runner: &dyn TurnHooks,
    input: CompactHookContext<'_>,
) -> Result<Vec<String>, ExtensionError> {
    let ctx = input.build_compact_context(None);
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
    extension_runner: &dyn TurnHooks,
    input: CompactHookContext<'_>,
    compaction: &CompactResult,
) -> Result<(), ExtensionError> {
    let ctx = input.build_compact_context(Some(compaction));
    extension_runner
        .emit_compact(CompactEvent::PostCompact, ctx)
        .await?;
    Ok(())
}

/// 执行一次 compact 摘要请求。
pub async fn request_compact_summary(
    llm: Arc<dyn LlmProvider>,
    messages: Vec<astrcode_core::llm::LlmMessage>,
) -> Result<String, CompactError> {
    let rx = llm
        .generate(messages, vec![])
        .await
        .map_err(CompactError::Llm)?;
    llm::collect_stream_text(rx)
        .await
        .map_err(CompactError::Llm)
}

pub enum PersistCompactionOutcome {
    Committed,
    Stale,
}

#[derive(Debug, thiserror::Error)]
pub enum PersistCompactError {
    #[error("{0}")]
    Session(#[from] SessionError),
}

/// 仅当 projection 仍对应 compact 的输入快照时，原子重写 transcript。
pub async fn persist_compact_result(
    session: &Session,
    compaction: &CompactResult,
    trigger_name: &str,
    source_seq: u64,
    strategy: CompactStrategy,
) -> Result<PersistCompactionOutcome, PersistCompactError> {
    let rewrite = session
        .rewrite_transcript_for_compaction(
            trigger_name.to_owned(),
            compaction.clone(),
            source_seq,
            strategy,
        )
        .await;
    match rewrite {
        Ok(_) => Ok(PersistCompactionOutcome::Committed),
        Err(SessionError::Storage(StorageError::ConcurrentModification { .. })) => {
            Ok(PersistCompactionOutcome::Stale)
        },
        Err(error) => Err(error.into()),
    }
}
