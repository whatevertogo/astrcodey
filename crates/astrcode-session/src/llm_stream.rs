//! LLM 流消费 — 从 LLM provider 接收事件流，发射到 EventBus/mpsc，解析文本/工具调用。

use astrcode_context::compaction::is_prompt_too_long_message;
use astrcode_core::{
    event::EventPayload,
    llm::{LlmError, LlmEvent, LlmMessage},
    types::*,
};
use tokio::sync::mpsc;

use crate::{
    tool_types::PendingToolCall,
    turn_context::{AgentSignal, TurnError, send_event},
};

// ─── StreamOutcome ───────────────────────────────────────────────────────

pub enum StreamOutcome {
    Complete {
        text: String,
        reasoning_content: String,
        finish_reason: String,
        message_id: MessageId,
        message_started: bool,
    },
    ToolCalls {
        text: Option<String>,
        reasoning_content: String,
        tool_calls: Vec<PendingToolCall>,
        message_id: MessageId,
        message_started: bool,
    },
}

/// 消费 LLM 事件流直到完成或积累工具调用。
///
/// 返回 `StreamOutcome::Complete` 表示回复完成（无工具调用），
/// 返回 `StreamOutcome::ToolCalls` 表示需要执行工具后继续循环。
pub async fn consume_llm_stream(
    mut rx: mpsc::UnboundedReceiver<LlmEvent>,
    event_tx: &Option<mpsc::UnboundedSender<AgentSignal>>,
    message_id: MessageId,
) -> Result<StreamOutcome, TurnError> {
    let mut current_text = String::new();
    let mut reasoning_content = String::new();
    let mut tool_calls: Vec<PendingToolCall> = Vec::new();
    let mut message_started = false;

    while let Some(event) = rx.recv().await {
        match event {
            LlmEvent::ContentDelta { delta } => {
                ensure_assistant_message_started(event_tx, &message_id, &mut message_started);
                send_event(
                    event_tx.as_ref(),
                    EventPayload::AssistantTextDelta {
                        message_id: message_id.clone(),
                        delta: delta.clone(),
                    },
                );
                current_text.push_str(&delta);
            },
            LlmEvent::ThinkingDelta { delta } => {
                ensure_assistant_message_started(event_tx, &message_id, &mut message_started);
                send_event(
                    event_tx.as_ref(),
                    EventPayload::ThinkingDelta {
                        message_id: message_id.clone(),
                        delta: delta.clone(),
                    },
                );
                reasoning_content.push_str(&delta);
            },
            LlmEvent::ToolCallStart {
                call_id,
                name,
                arguments,
            } => {
                // Replace duplicate call_id entries from buggy providers
                // instead of silently merging arguments.
                if let Some(existing) = tool_calls.iter_mut().find(|t| t.call_id == call_id) {
                    tracing::warn!(
                        call_id,
                        name,
                        "duplicate ToolCallStart with same call_id, replacing previous entry"
                    );
                    existing.name = name.clone();
                    existing.arguments = arguments.clone();
                } else {
                    send_event(
                        event_tx.as_ref(),
                        EventPayload::ToolCallStarted {
                            call_id: call_id.clone().into(),
                            tool_name: name.clone(),
                        },
                    );
                    if !arguments.is_empty() {
                        send_event(
                            event_tx.as_ref(),
                            EventPayload::ToolCallArgumentsDelta {
                                call_id: call_id.clone().into(),
                                delta: arguments.clone(),
                            },
                        );
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
                    tc.arguments.push_str(&delta);
                }
                send_event(
                    event_tx.as_ref(),
                    EventPayload::ToolCallArgumentsDelta {
                        call_id: call_id.into(),
                        delta,
                    },
                );
            },
            LlmEvent::Done { finish_reason } => {
                if tool_calls.is_empty() {
                    return Ok(StreamOutcome::Complete {
                        text: current_text,
                        reasoning_content: std::mem::take(&mut reasoning_content),
                        finish_reason,
                        message_id,
                        message_started,
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
                });
            },
            LlmEvent::Error { message } => {
                let recoverable = is_prompt_too_long_message(&message);
                send_event(
                    event_tx.as_ref(),
                    EventPayload::ErrorOccurred {
                        code: -32603,
                        message: message.clone(),
                        recoverable,
                    },
                );
                if recoverable {
                    return Err(TurnError::Llm(LlmError::PromptTooLong(message)));
                }
                return Err(TurnError::Llm(LlmError::StreamParse(message)));
            },
        }
    }

    Err(TurnError::Internal("LLM stream ended unexpectedly".into()))
}

pub fn ensure_assistant_message_started(
    event_tx: &Option<mpsc::UnboundedSender<AgentSignal>>,
    message_id: &MessageId,
    message_started: &mut bool,
) {
    if *message_started {
        return;
    }
    send_event(
        event_tx.as_ref(),
        EventPayload::AssistantMessageStarted {
            message_id: message_id.clone(),
        },
    );
    *message_started = true;
}

pub fn non_empty_reasoning_content(reasoning_content: String) -> Option<String> {
    if reasoning_content.is_empty() {
        None
    } else {
        Some(reasoning_content)
    }
}

pub fn assistant_message_with_thinking(
    text: &str,
    reasoning_content: Option<String>,
) -> LlmMessage {
    let mut message = LlmMessage::assistant(text);
    message.reasoning_content = reasoning_content;
    message
}

pub fn provider_visible_messages(messages: Vec<LlmMessage>) -> Vec<LlmMessage> {
    messages
        .into_iter()
        .map(LlmMessage::provider_visible)
        .filter(LlmMessage::has_provider_visible_content)
        .collect()
}

#[cfg(test)]
mod tests {
    use astrcode_core::llm::LlmMessage;

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
