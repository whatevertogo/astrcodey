//! Compact pipeline — hook 桥接与 LLM 请求构造。

use std::sync::Arc;

use astrcode_core::{
    config::ModelSelection,
    context::{CompactError, CompactRequestFn, CompactResult},
    event::Event,
    extension::{
        CompactContext, CompactEvent, CompactResult as TypedCompactResult, CompactStrategy,
        ExtensionError,
    },
    llm::{self, LlmProvider},
};
use astrcode_kernel::ExtensionRuntime;

use crate::{Session, session::SessionError};

#[derive(Clone, Copy)]
pub struct CompactHookContext<'a> {
    pub session_id: &'a str,
    pub working_dir: &'a str,
    pub model_id: &'a str,
    pub trigger: astrcode_core::extension::CompactTrigger,
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
    extension_runner: &dyn ExtensionRuntime,
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
    extension_runner: &dyn ExtensionRuntime,
    input: CompactHookContext<'_>,
    compaction: &CompactResult,
) -> Result<(), ExtensionError> {
    let ctx = input.build_compact_context(Some(compaction));
    extension_runner
        .emit_compact(CompactEvent::PostCompact, ctx)
        .await?;
    Ok(())
}

/// 从 LlmProvider 构造 compact 请求闭包。
///
/// 闭包调用 `llm.generate(messages, [])`，收集流式文本输出并返回。
/// 用于传入 `compact_messages_with_request` 或 `prepare_messages_with_llm`。
pub fn make_compact_request_fn(llm: Arc<dyn LlmProvider>) -> CompactRequestFn {
    Box::new(move |messages| {
        let llm = Arc::clone(&llm);
        Box::pin(async move {
            let rx = llm
                .generate(messages, vec![])
                .await
                .map_err(CompactError::Llm)?;
            llm::collect_stream_text(rx)
                .await
                .map_err(CompactError::Llm)
        })
    })
}

// ─── persist_compact_result ─────────────────────────────────────────

/// persist_compact_result 返回的持久化结果。
pub struct PersistedCompaction {
    pub events: Vec<Event>,
    pub base_event_seq: u64,
    pub messages_removed: usize,
}

/// persist_compact_result 的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum PersistCompactError {
    #[error("{0}")]
    Session(#[from] SessionError),
}

/// 纯持久化：append compact boundary events。
///
/// 不发 live event。`base_event_seq` 由调用方在 prepare 阶段产生并传入。
///
/// 注意：允许 compact LLM 调用期间有新事件写入。
/// replay 时会按 `base_event_seq` 将这些事件归类为 tail delta，保证不会被 compact 覆盖。
#[allow(clippy::too_many_arguments)]
pub async fn persist_compact_result(
    session: &Session,
    compaction: &CompactResult,
    trigger_name: &str,
    system_prompt: &str,
    fingerprint: &str,
    extra_system_prompt: Option<&str>,
    base_event_seq: u64,
    strategy: CompactStrategy,
) -> Result<PersistedCompaction, PersistCompactError> {
    let events = session
        .append_compact_boundary(
            system_prompt.to_owned(),
            fingerprint.to_owned(),
            extra_system_prompt.map(|s| s.to_owned()),
            trigger_name.to_owned(),
            compaction.clone(),
            base_event_seq,
            strategy,
        )
        .await?;

    Ok(PersistedCompaction {
        events,
        base_event_seq,
        messages_removed: compaction.messages_removed,
    })
}
