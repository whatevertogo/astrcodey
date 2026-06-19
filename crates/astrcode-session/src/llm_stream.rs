//! LLM 流消费 — 从 LLM provider 接收事件流，发射 live 事件，解析文本/工具调用。

use astrcode_core::{
    context::is_prompt_too_long_message,
    event::EventPayload,
    llm::{LlmError, LlmEvent, LlmTokenUsage},
    types::*,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{tool_types::PendingToolCall, turn_context::TurnError, turn_publish::TurnEvents};

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
        tool_calls: Vec<PendingToolCall>,
        message_id: MessageId,
        message_started: bool,
        usage: Option<LlmTokenUsage>,
    },
}

/// 消费 LLM 事件流直到完成或积累工具调用。
///
/// 返回 `StreamOutcome::Complete` 表示回复完成（无工具调用），
/// 返回 `StreamOutcome::ToolCalls` 表示需要执行工具后继续循环。
/// `AssistantMessageCompleted` 由 turn_runner 在 outcome 分支 durable 写入。
pub async fn consume_llm_stream(
    mut rx: mpsc::UnboundedReceiver<LlmEvent>,
    publisher: &TurnEvents,
    message_id: MessageId,
    cancellation_token: &CancellationToken,
) -> Result<StreamOutcome, TurnError> {
    let mut current_text = String::new();
    let mut reasoning_content = String::new();
    let mut tool_calls: Vec<PendingToolCall> = Vec::new();
    let mut message_started = false;
    let mut pending: Option<LlmEvent> = None;
    let mut captured_usage: Option<LlmTokenUsage> = None;

    loop {
        let event = match pending.take() {
            Some(event) => Some(event),
            None => {
                tokio::select! {
                    _ = cancellation_token.cancelled() => return Err(TurnError::Aborted),
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
                    tool_calls.push(PendingToolCall {
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
                let text = if current_text.is_empty() {
                    None
                } else {
                    Some(current_text)
                };
                return Ok(StreamOutcome::ToolCalls {
                    text,
                    reasoning_content: std::mem::take(&mut reasoning_content),
                    tool_calls,
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
                    .durable(EventPayload::ErrorOccurred {
                        code: -32603,
                        message: message.clone(),
                        recoverable: false,
                    })
                    .await?;
                return Err(TurnError::Llm(LlmError::StreamParse(message)));
            },
        }
    }
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

    fn assistant_message_with_thinking(
        text: &str,
        reasoning_content: Option<String>,
    ) -> LlmMessage {
        let mut message = LlmMessage::assistant(text);
        message.reasoning_content = reasoning_content;
        message
    }

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
    fn assistant_message_with_thinking_sets_reasoning() {
        let msg = assistant_message_with_thinking("hi", Some("reason".into()));
        assert_eq!(msg.reasoning_content.as_deref(), Some("reason"));
        assert!(msg.content.iter().any(|c| matches!(
            c,
            astrcode_core::llm::LlmContent::Text { text } if text == "hi"
        )));
    }

    #[test]
    fn assistant_message_without_thinking() {
        let msg = assistant_message_with_thinking("hi", None);
        assert!(msg.reasoning_content.is_none());
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
