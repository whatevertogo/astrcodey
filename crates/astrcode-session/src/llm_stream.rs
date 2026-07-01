//! LLM 流消费 — 从 LLM provider 接收事件流，发射 live 事件，解析文本/工具调用。

use std::collections::HashSet;

use astrcode_core::{
    context::is_prompt_too_long_message,
    event::EventPayload,
    llm::{LlmError, LlmEvent, LlmTokenUsage},
    types::*,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    early_tool_scheduler::{EarlyExecutionEntry, EarlyToolScheduler},
    tool_deduplicator::ToolCallDeduplicator,
    tool_pipeline::ToolCalls,
    tool_types::StreamedToolCall,
    turn_context::TurnError,
    turn_publish::TurnEvents,
};

/// 单次 LLM 响应允许的最大 tool call 数量。
const MAX_TOOL_CALLS_PER_RESPONSE: usize = 64;
/// 单个 tool call 参数字节上限（4 MiB）。
const MAX_TOOL_CALL_ARGUMENTS_BYTES: usize = 4 * 1024 * 1024;

fn stream_parse_limit_error(message: impl Into<String>) -> TurnError {
    TurnError::Llm(LlmError::StreamParse(message.into()))
}

fn ensure_tool_call_args_limit(size: usize) -> Result<(), TurnError> {
    if size > MAX_TOOL_CALL_ARGUMENTS_BYTES {
        return Err(stream_parse_limit_error(format!(
            "tool call arguments exceed limit ({MAX_TOOL_CALL_ARGUMENTS_BYTES} bytes)"
        )));
    }
    Ok(())
}

// ─── StreamOutcome ───────────────────────────────────────────────────────

pub enum StreamOutcome {
    Complete {
        text: String,
        reasoning_content: String,
        finish_reason: String,
        message_id: MessageId,
        message_started: bool,
        usage: Option<LlmTokenUsage>,
    },
    ToolCalls {
        text: Option<String>,
        reasoning_content: String,
        tool_calls: Vec<StreamedToolCall>,
        /// 流式执行阶段的结果（已准备 + 已执行的工具）。
        /// 为空表示未启用流式执行，tools_stage 需走完整 prepare + execute 路径。
        early_results: Vec<EarlyExecutionEntry>,
        message_id: MessageId,
        message_started: bool,
        usage: Option<LlmTokenUsage>,
    },
}

/// 流式工具执行的上下文。
///
/// 提供 `consume_llm_stream` 在流式过程中准备和调度工具执行所需的依赖。
pub(crate) struct EarlyExecContext<'a> {
    pub pipeline: &'a ToolCalls,
    pub visible_tools: Vec<astrcode_core::tool::ToolDefinition>,
    pub deduplicator: &'a mut ToolCallDeduplicator,
    pub max_parallel: usize,
}

/// 消费 LLM 事件流直到完成或积累工具调用。
///
/// 返回 `StreamOutcome::Complete` 表示回复完成（无工具调用），
/// 返回 `StreamOutcome::ToolCalls` 表示需要执行工具后继续循环。
/// `AssistantMessageCompleted` 由 turn_runner 在 outcome 分支 durable 写入。
///
/// 当 `early_exec` 为 `Some` 时，在 `ToolCallCompleted` 事件到达时即准备和
/// 调度工具执行，不等整个 LLM 响应流结束。
pub async fn consume_llm_stream(
    mut rx: mpsc::UnboundedReceiver<LlmEvent>,
    publisher: &TurnEvents,
    message_id: MessageId,
    cancellation_token: &CancellationToken,
    early_exec: Option<EarlyExecContext<'_>>,
) -> Result<StreamOutcome, TurnError> {
    let mut current_text = String::new();
    let mut reasoning_content = String::new();
    let mut tool_calls: Vec<StreamedToolCall> = Vec::new();
    let mut message_started = false;
    let mut pending: Option<LlmEvent> = None;
    let mut captured_usage: Option<LlmTokenUsage> = None;
    let mut early_exec = early_exec;
    let mut scheduled_tool_call_ids: HashSet<String> = HashSet::new();

    // 流式执行调度器（仅当 early_exec 提供时创建）
    let mut scheduler: Option<EarlyToolScheduler> = early_exec.as_ref().map(|ctx| {
        ctx.pipeline
            .create_early_scheduler(ctx.visible_tools.clone(), ctx.max_parallel)
    });

    loop {
        let event = match pending.take() {
            Some(event) => Some(event),
            None => {
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        if let Some(ref mut scheduler) = scheduler {
                            scheduler.abort_all();
                        }
                        return Err(TurnError::Aborted);
                    }
                    completed = poll_early_tool(&mut scheduler), if scheduler.as_ref().is_some_and(EarlyToolScheduler::has_pending) => {
                        if let Some((index, result)) = completed? {
                            if let Some(ref mut scheduler) = scheduler {
                                scheduler.record_result(index, result);
                            }
                        }
                        continue;
                    }
                    event = rx.recv() => event,
                }
            },
        };
        let Some(event) = event else {
            return Err(TurnError::StreamEndedUnexpectedly);
        };
        match event {
            LlmEvent::ContentDelta { delta } => {
                ensure_assistant_message_started(publisher, &message_id, &mut message_started)
                    .await;
                let mut batch = delta;
                current_text.push_str(&batch);
                while let Ok(next) = rx.try_recv() {
                    match next {
                        LlmEvent::ContentDelta { delta } => {
                            current_text.push_str(&delta);
                            batch.push_str(&delta);
                        },
                        other => {
                            pending = Some(other);
                            break;
                        },
                    }
                }
                publisher
                    .live(EventPayload::AssistantTextDelta {
                        message_id: message_id.clone(),
                        delta: batch,
                    })
                    .await;
            },
            LlmEvent::ThinkingDelta { delta } => {
                ensure_assistant_message_started(publisher, &message_id, &mut message_started)
                    .await;
                let mut batch = delta;
                reasoning_content.push_str(&batch);
                while let Ok(next) = rx.try_recv() {
                    match next {
                        LlmEvent::ThinkingDelta { delta } => {
                            reasoning_content.push_str(&delta);
                            batch.push_str(&delta);
                        },
                        other => {
                            pending = Some(other);
                            break;
                        },
                    }
                }
                publisher
                    .live(EventPayload::ThinkingDelta {
                        message_id: message_id.clone(),
                        delta: batch,
                    })
                    .await;
            },
            LlmEvent::ToolCallStart {
                call_id,
                name,
                arguments,
            } => {
                if let Some(existing) = tool_calls.iter_mut().find(|t| t.call_id == call_id) {
                    tracing::warn!(
                        call_id,
                        name,
                        "duplicate ToolCallStart with same call_id, replacing previous entry"
                    );
                    ensure_tool_call_args_limit(arguments.len())?;
                    existing.name = name;
                    existing.arguments = arguments;
                } else {
                    if tool_calls.len() >= MAX_TOOL_CALLS_PER_RESPONSE {
                        return Err(stream_parse_limit_error(format!(
                            "tool call count exceeds limit ({MAX_TOOL_CALLS_PER_RESPONSE})"
                        )));
                    }
                    ensure_tool_call_args_limit(arguments.len())?;
                    publisher
                        .live(EventPayload::ToolCallStarted {
                            call_id: call_id.clone().into(),
                            tool_name: name.clone(),
                        })
                        .await;
                    if !arguments.is_empty() {
                        publisher
                            .live(EventPayload::ToolCallArgumentsDelta {
                                call_id: call_id.clone().into(),
                                delta: arguments.clone(),
                            })
                            .await;
                    }
                    tool_calls.push(StreamedToolCall {
                        call_id,
                        name,
                        arguments,
                    });
                }
            },
            LlmEvent::ToolCallDelta { call_id, delta } => {
                if let Some(tc) = tool_calls.iter_mut().find(|t| t.call_id == call_id) {
                    ensure_tool_call_args_limit(tc.arguments.len().saturating_add(delta.len()))?;
                    tc.arguments.push_str(&delta);
                }
                publisher
                    .live(EventPayload::ToolCallArgumentsDelta {
                        call_id: call_id.into(),
                        delta,
                    })
                    .await;
            },
            LlmEvent::Usage { usage } => {
                captured_usage = Some(usage);
            },
            LlmEvent::Done { finish_reason } => {
                if tool_calls.is_empty() {
                    return Ok(StreamOutcome::Complete {
                        text: current_text,
                        reasoning_content: std::mem::take(&mut reasoning_content),
                        finish_reason,
                        message_id,
                        message_started,
                        usage: captured_usage,
                    });
                }
                // 在返回前 drain 流式执行的工具，收集 early results
                let early_results = if let Some(mut scheduler) = scheduler {
                    scheduler.drain_all().await?;
                    scheduler.into_entries()
                } else {
                    Vec::new()
                };
                let text = if current_text.is_empty() {
                    None
                } else {
                    Some(current_text)
                };
                return Ok(StreamOutcome::ToolCalls {
                    text,
                    reasoning_content: std::mem::take(&mut reasoning_content),
                    tool_calls,
                    early_results,
                    message_id,
                    message_started,
                    usage: captured_usage,
                });
            },
            LlmEvent::Error { message } => {
                let recoverable = is_prompt_too_long_message(&message);
                if recoverable {
                    publisher.live_error(-32603, message.clone(), true).await;
                    return Err(TurnError::Llm(LlmError::PromptTooLong(message)));
                }
                publisher
                    .durable_error(-32603, message.clone(), false)
                    .await?;
                return Err(TurnError::Llm(LlmError::StreamParse(message)));
            },
            LlmEvent::ToolCallCompleted { call_id } => {
                if scheduled_tool_call_ids.contains(&call_id) {
                    continue;
                }
                if let (Some(ref mut ctx), Some(ref mut scheduler)) =
                    (early_exec.as_mut(), scheduler.as_mut())
                {
                    if let Some((index, tc)) = tool_calls
                        .iter()
                        .enumerate()
                        .find(|(_, tc)| tc.call_id == call_id)
                    {
                        let prepared = ctx
                            .pipeline
                            .prepare_single_tool_call(
                                tc,
                                index,
                                &ctx.visible_tools,
                                ctx.deduplicator,
                            )
                            .await?;
                        scheduler.schedule(prepared);
                    }
                }
                scheduled_tool_call_ids.insert(call_id);
            },
        }
    }
}

async fn poll_early_tool(
    scheduler: &mut Option<EarlyToolScheduler>,
) -> Result<Option<(usize, astrcode_core::tool::ToolResult)>, TurnError> {
    let Some(scheduler) = scheduler else {
        return Ok(None);
    };
    scheduler.poll_completed().await
}

async fn ensure_assistant_message_started(
    publisher: &TurnEvents,
    message_id: &MessageId,
    message_started: &mut bool,
) {
    if *message_started {
        return;
    }
    publisher
        .live(EventPayload::AssistantMessageStarted {
            message_id: message_id.clone(),
        })
        .await;
    *message_started = true;
}

pub fn non_empty_reasoning_content(reasoning_content: String) -> Option<String> {
    if reasoning_content.is_empty() {
        None
    } else {
        Some(reasoning_content)
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::llm::{LlmMessage, provider_visible_messages};

    use super::*;

    #[test]
    fn non_empty_reasoning_returns_some() {
        assert_eq!(
            non_empty_reasoning_content("thinking...".into()),
            Some("thinking...".into())
        );
    }

    #[test]
    fn non_empty_reasoning_empty_returns_none() {
        assert_eq!(non_empty_reasoning_content(String::new()), None);
    }

    #[test]
    fn provider_visible_filters_empty_system_messages() {
        let messages = vec![LlmMessage::user("hello"), LlmMessage::system("")];
        let visible = provider_visible_messages(messages);
        assert_eq!(visible.len(), 1);
        assert!(matches!(visible[0].role, astrcode_core::llm::LlmRole::User));
    }

    #[test]
    fn provider_visible_keeps_non_empty() {
        let messages = vec![LlmMessage::user("hello"), LlmMessage::assistant("world")];
        let visible = provider_visible_messages(messages);
        assert_eq!(visible.len(), 2);
    }
}
