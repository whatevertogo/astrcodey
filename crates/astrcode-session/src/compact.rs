//! Compact pipeline — hook 桥接与 LLM 请求构造。

use std::{future::Future, pin::Pin, sync::Arc};

use astrcode_context::compaction::{CompactError, CompactResult};
use astrcode_core::{
    config::ModelSelection,
    event::Event,
    extension::{
        CompactContext, CompactEvent, CompactResult as TypedCompactResult, CompactStrategy,
        CompactTrigger, ExtensionError,
    },
    llm::{LlmError, LlmEvent, LlmMessage, LlmProvider},
};
use astrcode_extensions::runner::ExtensionRunner;
use tokio::sync::mpsc;

use crate::{Session, session::SessionError};

type CompactRequestFn = Box<
    dyn FnMut(Vec<LlmMessage>) -> Pin<Box<dyn Future<Output = Result<String, CompactError>> + Send>>
        + Send,
>;

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
        CompactTrigger::ReactivePromptTooLong => "reactive_prompt_too_long",
    }
}

/// 从 LLM stream 收集纯文本输出，忽略 tool call 事件。
async fn collect_stream_text(
    mut rx: mpsc::UnboundedReceiver<LlmEvent>,
) -> Result<String, LlmError> {
    let mut text = String::new();
    while let Some(event) = rx.recv().await {
        match event {
            LlmEvent::ContentDelta { delta } => text.push_str(&delta),
            LlmEvent::Done { .. } => break,
            LlmEvent::Error { message } => return Err(LlmError::StreamParse(message)),
            _ => {},
        }
    }
    Ok(text)
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
            collect_stream_text(rx).await.map_err(CompactError::Llm)
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

/// compare-and-append 冲突：compact LLM 调用期间有新事件写入。
#[derive(Debug, thiserror::Error)]
#[error("compaction conflict: expected seq {expected}, actual {actual}")]
pub struct CompactionConflict {
    pub expected: u64,
    pub actual: u64,
}

/// persist_compact_result 的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum PersistCompactError {
    #[error("{0}")]
    Conflict(#[from] CompactionConflict),
    #[error("{0}")]
    Session(#[from] SessionError),
}

/// 纯持久化：校验 expected seq → append compact boundary events。
///
/// 不发 live event。`base_event_seq` 由调用方在 prepare 阶段产生并传入。
/// 若当前 cursor 与 `base_event_seq` 不一致则返回 conflict（调用方可选择放弃持久化）。
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
    // compare-and-append: 校验当前 cursor 与预期一致
    let current_seq = session
        .latest_cursor()
        .await?
        .and_then(|c| c.parse::<u64>().ok())
        .unwrap_or(0);
    if current_seq != base_event_seq {
        return Err(PersistCompactError::Conflict(CompactionConflict {
            expected: base_event_seq,
            actual: current_seq,
        }));
    }

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
