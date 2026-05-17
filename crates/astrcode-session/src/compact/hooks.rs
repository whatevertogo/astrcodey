//! Compact pipeline — hook 桥接与 LLM 请求构造。

use std::{future::Future, pin::Pin, sync::Arc};

use astrcode_context::compaction::CompactError;
use astrcode_core::{
    config::ModelSelection,
    extension::{
        CompactContext, CompactEvent, CompactResult as TypedCompactResult, CompactTrigger,
        ExtensionError,
    },
    llm::{LlmError, LlmEvent, LlmMessage, LlmProvider},
};
use astrcode_extensions::runner::ExtensionRunner;
use tokio::sync::mpsc;

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
