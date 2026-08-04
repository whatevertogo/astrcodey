//! LLM 流消费 — 从 LLM provider 接收事件流，发射 live 事件，解析文本/工具调用。

use std::collections::HashSet;

use astrcode_context::is_prompt_too_long_message;
use astrcode_core::{
    event::LiveEventPayload,
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
    TurnError::Llm(LlmError::stream_parse(message.into()))
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

pub(crate) enum StreamOutcome {
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
pub(crate) async fn consume_llm_stream(
    mut rx: mpsc::UnboundedReceiver<LlmEvent>,
    publisher: &TurnEvents,
    message_id: MessageId,
    cancellation_token: &CancellationToken,
    early_exec: Option<EarlyExecContext<'_>>,
) -> Result<StreamOutcome, TurnError> {
    let mut consumer = StreamConsumer::new(publisher, message_id, early_exec);

    loop {
        let event = match consumer.pending.take() {
            Some(event) => Some(event),
            None => {
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        if let Some(ref mut scheduler) = consumer.scheduler {
                            scheduler.abort_all();
                        }
                        return Err(TurnError::Aborted);
                    }
                    completed = poll_early_tool(&mut consumer.scheduler), if consumer.scheduler.as_ref().is_some_and(EarlyToolScheduler::has_pending) => {
                        if let Some((index, outcome)) = completed? {
                            if let Some(ref mut scheduler) = consumer.scheduler {
                                scheduler.record_outcome(index, outcome);
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
            LlmEvent::Retrying {
                status,
                attempt,
                max_retries,
                delay_ms,
            } => consumer.publisher.live(LiveEventPayload::LlmRetrying {
                status,
                attempt,
                max_retries,
                delay_ms,
            }),
            LlmEvent::RetryRecovered => {
                consumer.publisher.live(LiveEventPayload::LlmRetryRecovered)
            },
            LlmEvent::ContentDelta { delta } => consumer.handle_content_delta(delta),
            LlmEvent::ThinkingDelta { delta } => consumer.handle_thinking_delta(delta),
            LlmEvent::ToolCallStart {
                call_id,
                name,
                arguments,
            } => consumer.handle_tool_call_start(call_id, name, arguments)?,
            LlmEvent::ToolCallDelta { call_id, delta } => {
                consumer.handle_tool_call_delta(call_id, delta)?;
            },
            LlmEvent::Usage { usage } => consumer.handle_usage(usage),
            LlmEvent::Done { finish_reason } => {
                return consumer.finish(finish_reason).await;
            },
            LlmEvent::Error { message } => return consumer.handle_error(message).await,
            LlmEvent::ToolCallCompleted { call_id } => {
                consumer.handle_tool_call_completed(call_id).await?;
            },
        }
    }
}

/// 单次 LLM 响应流的消费状态与事件处理。
struct StreamConsumer<'a> {
    publisher: &'a TurnEvents,
    message_id: MessageId,
    current_text: String,
    reasoning_content: String,
    tool_calls: Vec<StreamedToolCall>,
    message_started: bool,
    pending: Option<LlmEvent>,
    captured_usage: Option<LlmTokenUsage>,
    early_exec: Option<EarlyExecContext<'a>>,
    scheduler: Option<EarlyToolScheduler>,
    /// 已处理过 `ToolCallCompleted` 的 call_id（防止重复调度）。
    handled_tool_call_ids: HashSet<String>,
}

impl<'a> StreamConsumer<'a> {
    fn new(
        publisher: &'a TurnEvents,
        message_id: MessageId,
        early_exec: Option<EarlyExecContext<'a>>,
    ) -> Self {
        // 流式执行调度器（仅当 early_exec 提供时创建）
        let scheduler = early_exec.as_ref().map(|ctx| {
            ctx.pipeline
                .create_early_scheduler(ctx.visible_tools.clone(), ctx.max_parallel)
        });
        Self {
            publisher,
            message_id,
            current_text: String::new(),
            reasoning_content: String::new(),
            tool_calls: Vec::new(),
            message_started: false,
            pending: None,
            captured_usage: None,
            early_exec,
            scheduler,
            handled_tool_call_ids: HashSet::new(),
        }
    }

    fn handle_content_delta(&mut self, delta: String) {
        ensure_assistant_message_started(
            self.publisher,
            &self.message_id,
            &mut self.message_started,
        );
        self.current_text.push_str(&delta);
        self.publisher.live(LiveEventPayload::AssistantTextDelta {
            message_id: self.message_id.clone(),
            delta,
        });
    }

    fn handle_thinking_delta(&mut self, delta: String) {
        ensure_assistant_message_started(
            self.publisher,
            &self.message_id,
            &mut self.message_started,
        );
        self.reasoning_content.push_str(&delta);
        self.publisher.live(LiveEventPayload::ThinkingDelta {
            message_id: self.message_id.clone(),
            delta,
        });
    }

    fn handle_tool_call_start(
        &mut self,
        call_id: String,
        name: String,
        arguments: String,
    ) -> Result<(), TurnError> {
        if let Some(existing) = self.tool_calls.iter_mut().find(|t| t.call_id == call_id) {
            tracing::warn!(
                call_id,
                name,
                "duplicate ToolCallStart with same call_id, replacing previous entry"
            );
            ensure_tool_call_args_limit(arguments.len())?;
            existing.name = name;
            existing.arguments = arguments;
        } else {
            if self.tool_calls.len() >= MAX_TOOL_CALLS_PER_RESPONSE {
                return Err(stream_parse_limit_error(format!(
                    "tool call count exceeds limit ({MAX_TOOL_CALLS_PER_RESPONSE})"
                )));
            }
            ensure_tool_call_args_limit(arguments.len())?;
            self.publisher.live(LiveEventPayload::ToolCallStarted {
                call_id: call_id.clone().into(),
                tool_name: name.clone(),
            });
            if !arguments.is_empty() {
                self.publisher
                    .live(LiveEventPayload::ToolCallArgumentsDelta {
                        call_id: call_id.clone().into(),
                        delta: arguments.clone(),
                    });
            }
            self.tool_calls.push(StreamedToolCall {
                call_id,
                name,
                arguments,
            });
        }
        Ok(())
    }

    fn handle_tool_call_delta(&mut self, call_id: String, delta: String) -> Result<(), TurnError> {
        if let Some(tc) = self.tool_calls.iter_mut().find(|t| t.call_id == call_id) {
            ensure_tool_call_args_limit(tc.arguments.len().saturating_add(delta.len()))?;
            tc.arguments.push_str(&delta);
        } else {
            // 畸形 provider 先发 delta 后发 start：live 侧已可见增量，结果里却没有该调用。
            tracing::warn!(
                call_id,
                "ToolCallDelta for unknown call_id; delta dropped from tool call result"
            );
        }
        self.publisher
            .live(LiveEventPayload::ToolCallArgumentsDelta {
                call_id: call_id.into(),
                delta,
            });
        Ok(())
    }

    fn handle_usage(&mut self, usage: LlmTokenUsage) {
        self.captured_usage = Some(usage);
    }

    async fn handle_tool_call_completed(&mut self, call_id: String) -> Result<(), TurnError> {
        if self.handled_tool_call_ids.contains(&call_id) {
            return Ok(());
        }
        // 同一结构体的两个字段分别可变借用是允许的（字段级借用）。
        let (early_exec, scheduler) = (self.early_exec.as_mut(), self.scheduler.as_mut());
        if let (Some(ctx), Some(scheduler)) = (early_exec, scheduler) {
            if let Some((index, tc)) = self
                .tool_calls
                .iter()
                .enumerate()
                .find(|(_, tc)| tc.call_id == call_id)
            {
                let prepared = ctx
                    .pipeline
                    .prepare_single_tool_call(tc, index, &ctx.visible_tools, ctx.deduplicator)
                    .await?;
                scheduler.schedule(prepared);
            }
        }
        self.handled_tool_call_ids.insert(call_id);
        Ok(())
    }

    async fn finish(mut self, finish_reason: String) -> Result<StreamOutcome, TurnError> {
        if self.tool_calls.is_empty() {
            return Ok(StreamOutcome::Complete {
                text: self.current_text,
                reasoning_content: std::mem::take(&mut self.reasoning_content),
                finish_reason,
                message_id: self.message_id,
                message_started: self.message_started,
                usage: self.captured_usage,
            });
        }
        // 在返回前 drain 流式执行的工具，收集 early results
        let early_results = if let Some(mut scheduler) = self.scheduler {
            scheduler.drain_all().await?;
            scheduler.into_entries()
        } else {
            Vec::new()
        };
        let text = if self.current_text.is_empty() {
            None
        } else {
            Some(self.current_text)
        };
        Ok(StreamOutcome::ToolCalls {
            text,
            reasoning_content: std::mem::take(&mut self.reasoning_content),
            tool_calls: self.tool_calls,
            early_results,
            message_id: self.message_id,
            message_started: self.message_started,
            usage: self.captured_usage,
        })
    }

    async fn handle_error(&self, message: String) -> Result<StreamOutcome, TurnError> {
        let recoverable = is_prompt_too_long_message(&message);
        if recoverable {
            self.publisher.live_error(
                crate::payload::JSON_RPC_INTERNAL_ERROR,
                message.clone(),
                true,
            );
            return Err(TurnError::Llm(LlmError::ContextWindowExceeded { message }));
        }
        self.publisher
            .durable_error(
                crate::payload::JSON_RPC_INTERNAL_ERROR,
                message.clone(),
                false,
            )
            .await?;
        Err(TurnError::Llm(LlmError::stream_parse(message)))
    }
}

async fn poll_early_tool(
    scheduler: &mut Option<EarlyToolScheduler>,
) -> Result<Option<(usize, crate::tool_types::ToolExecutionOutcome)>, TurnError> {
    let Some(scheduler) = scheduler else {
        return Ok(None);
    };
    scheduler.poll_completed().await
}

fn ensure_assistant_message_started(
    publisher: &TurnEvents,
    message_id: &MessageId,
    message_started: &mut bool,
) {
    if *message_started {
        return;
    }
    publisher.live(LiveEventPayload::AssistantMessageStarted {
        message_id: message_id.clone(),
    });
    *message_started = true;
}

pub(crate) fn non_empty_reasoning_content(reasoning_content: String) -> Option<String> {
    if reasoning_content.is_empty() {
        None
    } else {
        Some(reasoning_content)
    }
}

#[cfg(test)]
mod tests {
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
}
