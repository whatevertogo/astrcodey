//! LLM 流消费 — 从 LLM provider 接收事件流，发射 live 事件，解析文本/工具调用。

use astrcode_context::is_prompt_too_long_message;
use astrcode_core::{
    event::LiveEventPayload,
    llm::{LlmError, LlmEvent, LlmTokenUsage},
    types::*,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{tool_types::StreamedToolCall, turn_context::TurnError, turn_publish::TurnEvents};

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
pub(crate) async fn consume_llm_stream(
    mut rx: mpsc::UnboundedReceiver<LlmEvent>,
    publisher: &TurnEvents,
    message_id: MessageId,
    cancellation_token: &CancellationToken,
) -> Result<StreamOutcome, TurnError> {
    let mut consumer = StreamConsumer::new(publisher, message_id);

    loop {
        let event = tokio::select! {
            _ = cancellation_token.cancelled() => {
                return Err(TurnError::Aborted);
            }
            event = rx.recv() => event,
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
            } => {
                consumer.reset_for_retry();
                consumer.publisher.live(LiveEventPayload::LlmRetrying {
                    status,
                    attempt,
                    max_retries,
                    delay_ms,
                });
            },
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
                return consumer.finish(finish_reason);
            },
            LlmEvent::Error { message } => return consumer.handle_error(message).await,
            LlmEvent::ToolCallCompleted { .. } => {},
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
    captured_usage: Option<LlmTokenUsage>,
}

impl<'a> StreamConsumer<'a> {
    fn new(publisher: &'a TurnEvents, message_id: MessageId) -> Self {
        Self {
            publisher,
            message_id,
            current_text: String::new(),
            reasoning_content: String::new(),
            tool_calls: Vec::new(),
            message_started: false,
            captured_usage: None,
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

    fn reset_for_retry(&mut self) {
        self.current_text.clear();
        self.reasoning_content.clear();
        self.tool_calls.clear();
        self.captured_usage = None;
        if self.message_started {
            self.publisher
                .live(LiveEventPayload::AssistantMessageReset {
                    message_id: self.message_id.clone(),
                });
            self.message_started = false;
        }
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

    fn finish(mut self, finish_reason: String) -> Result<StreamOutcome, TurnError> {
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
        let text = if self.current_text.is_empty() {
            None
        } else {
            Some(self.current_text)
        };
        Ok(StreamOutcome::ToolCalls {
            text,
            reasoning_content: std::mem::take(&mut self.reasoning_content),
            tool_calls: self.tool_calls,
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
    use std::sync::Arc;

    use astrcode_core::event::EventPayload;
    use astrcode_storage::in_memory::InMemoryEventStore;

    use super::*;
    use crate::{
        session::{Session, SessionCreateParams},
        session_event_sink::SessionEventSink,
        session_runtime::SessionRuntimeState,
        test_support::{ChannelObserver, test_runtime_services},
    };

    #[tokio::test]
    async fn retry_discards_partial_response_and_emits_reset() {
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let store = Arc::new(InMemoryEventStore::new());
        let runtime = Arc::new(SessionRuntimeState::new_with_event_sink(
            new_session_id(),
            store.clone(),
            Arc::new(SessionEventSink::new(ChannelObserver::new(events_tx))),
        ));
        let session = Session::create_with_params(SessionCreateParams {
            working_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            model_id: "mock-model".into(),
            parent_session_id: None,
            tool_selection: None,
            source_extension: None,
            extra_system_prompt: None,
            initial_system_prompt: None,
            runtime,
            runtime_services: test_runtime_services(),
        })
        .await
        .unwrap();
        while events_rx.try_recv().is_ok() {}

        let publisher = TurnEvents::new(session.clone(), new_turn_id());
        let message_id = new_message_id();
        let (stream_tx, stream_rx) = mpsc::unbounded_channel();
        for event in [
            LlmEvent::ThinkingDelta {
                delta: "stale reasoning".into(),
            },
            LlmEvent::ContentDelta {
                delta: "stale".into(),
            },
            LlmEvent::Retrying {
                status: None,
                attempt: 1,
                max_retries: 2,
                delay_ms: 1,
            },
            LlmEvent::ThinkingDelta {
                delta: "fresh reasoning".into(),
            },
            LlmEvent::ContentDelta {
                delta: "fresh".into(),
            },
            LlmEvent::Done {
                finish_reason: "stop".into(),
            },
        ] {
            stream_tx.send(event).unwrap();
        }

        let outcome = consume_llm_stream(
            stream_rx,
            &publisher,
            message_id.clone(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            StreamOutcome::Complete {
                text,
                reasoning_content,
                message_id: outcome_message_id,
                ..
            } if text == "fresh"
                && reasoning_content == "fresh reasoning"
                && outcome_message_id == message_id
        ));
        session
            .runtime
            .event_sink()
            .sync(store, session.id())
            .await
            .unwrap();
        assert!(
            std::iter::from_fn(|| events_rx.try_recv().ok()).any(|event| matches!(
                event.payload,
                EventPayload::Live(LiveEventPayload::AssistantMessageReset { .. })
            ))
        );
    }

    #[test]
    fn reasoning_content_is_present_only_when_nonempty() {
        assert_eq!(
            non_empty_reasoning_content("thinking...".into()),
            Some("thinking...".into())
        );
        assert_eq!(non_empty_reasoning_content(String::new()), None);
    }
}
