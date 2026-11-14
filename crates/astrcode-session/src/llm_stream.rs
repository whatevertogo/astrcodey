//! LLM 流消费 — 从 LLM provider 接收事件流，发射 live 事件，解析文本/工具调用。

use astrcode_context::is_prompt_too_long_message;
use astrcode_core::{
    event::{AssistantAttemptOutcome, DurableEventPayload, LiveEventPayload, TurnCompletionReason},
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

#[derive(Debug)]
pub(crate) struct StreamResponse {
    pub(crate) message_id: MessageId,
    pub(crate) text: String,
    pub(crate) reasoning_content: String,
    pub(crate) tool_calls: Vec<StreamedToolCall>,
    pub(crate) usage: Option<LlmTokenUsage>,
}

impl StreamResponse {
    pub(crate) fn empty(message_id: MessageId) -> Self {
        Self {
            message_id,
            text: String::new(),
            reasoning_content: String::new(),
            tool_calls: Vec::new(),
            usage: None,
        }
    }

    pub(crate) async fn settle(
        &self,
        publisher: &TurnEvents,
        outcome: AssistantAttemptOutcome,
        model_context_window: usize,
    ) -> Result<(), TurnError> {
        let reasoning_content = non_empty_reasoning_content(self.reasoning_content.clone());
        let payload = if self.text.is_empty()
            && reasoning_content.is_none()
            && (self.tool_calls.is_empty() || outcome != AssistantAttemptOutcome::Completed)
        {
            DurableEventPayload::AssistantAttemptSettled {
                message_id: self.message_id.clone(),
                outcome,
            }
        } else {
            DurableEventPayload::AssistantMessage {
                message_id: self.message_id.clone(),
                text: self.text.clone(),
                reasoning_content,
                outcome,
            }
        };
        publisher.durable(payload).await?;
        if let Some(usage) = &self.usage {
            publisher
                .durable(DurableEventPayload::TokenUsageRecorded {
                    usage: usage.clone(),
                    model_context_window,
                })
                .await?;
        }
        Ok(())
    }
}

pub(crate) struct StreamOutcome {
    pub(crate) response: StreamResponse,
    pub(crate) finish_reason: TurnCompletionReason,
}

impl StreamOutcome {
    pub(crate) fn needs_context_recovery(&self) -> bool {
        self.finish_reason == TurnCompletionReason::Length
            && self.response.text.is_empty()
            && self.response.tool_calls.is_empty()
    }
}

#[derive(Debug)]
pub(crate) struct StreamFailure {
    pub(crate) response: StreamResponse,
    pub(crate) error: TurnError,
}

/// 消费 LLM 事件流直到完成或积累工具调用。
///
/// 消费结果保留部分正文、工具预览与用量，由执行循环统一结算。
pub(crate) async fn consume_llm_stream(
    mut rx: mpsc::UnboundedReceiver<LlmEvent>,
    publisher: &TurnEvents,
    response: StreamResponse,
    cancellation_token: &CancellationToken,
    model_context_window: usize,
) -> Result<StreamOutcome, Box<StreamFailure>> {
    let mut consumer = StreamConsumer {
        publisher,
        response,
        message_started: false,
    };

    let result = async {
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
                    let failed = AssistantAttemptOutcome::Failed {
                        error: format!("provider request retried (status: {status:?})"),
                    };
                    if let Err(error) = consumer
                        .response
                        .settle(publisher, failed, model_context_window)
                        .await
                    {
                        tracing::warn!(%error, "could not settle retried model attempt");
                    }
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
                    return match finish_reason.as_str() {
                        "error" => Err(TurnError::Llm(LlmError::stream_parse(
                            "provider returned an error finish reason",
                        ))),
                        "cancelled" | "aborted" | "interrupted" => Err(TurnError::Aborted),
                        _ => TurnCompletionReason::from_provider(&finish_reason).ok_or_else(|| {
                            TurnError::Llm(LlmError::stream_parse(format!(
                                "unknown provider finish reason: {finish_reason}"
                            )))
                        }),
                    };
                },
                LlmEvent::Error { message } => return Err(consumer.handle_error(message)),
                LlmEvent::ToolCallArgumentsCompleted { .. } => {},
            }
        }
    }
    .await;
    match result {
        Ok(finish_reason) => Ok(StreamOutcome {
            response: consumer.response,
            finish_reason,
        }),
        Err(error) => {
            consumer.discard_tool_previews();
            Err(Box::new(StreamFailure {
                response: consumer.response,
                error,
            }))
        },
    }
}

struct StreamConsumer<'a> {
    publisher: &'a TurnEvents,
    response: StreamResponse,
    message_started: bool,
}

impl StreamConsumer<'_> {
    fn handle_content_delta(&mut self, delta: String) {
        ensure_assistant_message_started(
            self.publisher,
            &self.response.message_id,
            &mut self.message_started,
        );
        self.response.text.push_str(&delta);
        self.publisher
            .live(LiveEventPayload::AssistantMessageDelta {
                message_id: self.response.message_id.clone(),
                delta: astrcode_core::event::AssistantDelta::Text(delta),
            });
    }

    fn reset_for_retry(&mut self) {
        self.discard_tool_previews();

        self.message_started = false;
        self.response = StreamResponse::empty(new_message_id());
    }

    fn handle_thinking_delta(&mut self, delta: String) {
        ensure_assistant_message_started(
            self.publisher,
            &self.response.message_id,
            &mut self.message_started,
        );
        self.response.reasoning_content.push_str(&delta);
        self.publisher
            .live(LiveEventPayload::AssistantMessageDelta {
                message_id: self.response.message_id.clone(),
                delta: astrcode_core::event::AssistantDelta::Reasoning(delta),
            });
    }

    fn handle_tool_call_start(
        &mut self,
        call_id: String,
        name: String,
        arguments: String,
    ) -> Result<(), TurnError> {
        if let Some(existing) = self
            .response
            .tool_calls
            .iter_mut()
            .find(|t| t.provider_call_id == call_id)
        {
            tracing::warn!(
                call_id,
                name,
                "duplicate ToolCallStart with same call_id, replacing previous entry"
            );
            ensure_tool_call_args_limit(arguments.len())?;
            existing.name = name;
            existing.arguments = arguments;
        } else {
            if self.response.tool_calls.len() >= MAX_TOOL_CALLS_PER_RESPONSE {
                return Err(stream_parse_limit_error(format!(
                    "tool call count exceeds limit ({MAX_TOOL_CALLS_PER_RESPONSE})"
                )));
            }
            ensure_tool_call_args_limit(arguments.len())?;
            let runtime_call_id = new_tool_call_id();
            self.publisher
                .live(LiveEventPayload::ToolCallPreviewStarted {
                    message_id: self.response.message_id.clone(),
                    call_id: runtime_call_id.clone(),
                    tool_name: name.clone(),
                });
            if !arguments.is_empty() {
                self.publisher
                    .live(LiveEventPayload::ToolCallArgumentsDelta {
                        message_id: self.response.message_id.clone(),
                        call_id: runtime_call_id.clone(),
                        delta: arguments.clone(),
                    });
            }
            self.response.tool_calls.push(StreamedToolCall {
                call_id: runtime_call_id.to_string(),
                provider_call_id: call_id,
                name,
                arguments,
            });
        }
        Ok(())
    }

    fn handle_tool_call_delta(&mut self, call_id: String, delta: String) -> Result<(), TurnError> {
        if let Some(tc) = self
            .response
            .tool_calls
            .iter_mut()
            .find(|t| t.provider_call_id == call_id)
        {
            ensure_tool_call_args_limit(tc.arguments.len().saturating_add(delta.len()))?;
            tc.arguments.push_str(&delta);
            self.publisher
                .live(LiveEventPayload::ToolCallArgumentsDelta {
                    message_id: self.response.message_id.clone(),
                    call_id: tc.call_id.clone().into(),
                    delta,
                });
        } else {
            // 畸形 provider 先发 delta 后发 start：live 侧已可见增量，结果里却没有该调用。
            tracing::warn!(
                call_id,
                "ToolCallDelta for unknown call_id; delta dropped from tool call result"
            );
        }
        Ok(())
    }

    fn handle_usage(&mut self, usage: LlmTokenUsage) {
        self.response.usage = Some(usage);
    }

    fn discard_tool_previews(&self) {
        for call in &self.response.tool_calls {
            self.publisher
                .live(LiveEventPayload::ToolCallPreviewDiscarded {
                    call_id: call.call_id.clone().into(),
                });
        }
    }

    fn handle_error(&self, message: String) -> TurnError {
        if is_prompt_too_long_message(&message) {
            self.publisher.live_error(
                crate::payload::JSON_RPC_INTERNAL_ERROR,
                message.clone(),
                true,
            );
            TurnError::Llm(LlmError::ContextWindowExceeded { message })
        } else {
            TurnError::Llm(LlmError::stream_parse(message))
        }
    }
}

fn ensure_assistant_message_started(
    publisher: &TurnEvents,
    message_id: &MessageId,
    started: &mut bool,
) {
    if !*started {
        publisher.live(LiveEventPayload::AssistantMessageStarted {
            message_id: message_id.clone(),
        });
        *started = true;
    }
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

    async fn test_session() -> (
        Session,
        Arc<InMemoryEventStore>,
        mpsc::UnboundedReceiver<Arc<astrcode_core::event::Event>>,
    ) {
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

        (session, store, events_rx)
    }

    #[tokio::test]
    async fn failed_attempt_matrix_preserves_partial_output_usage_and_empty_settlement() {
        for (terminal, visible) in [
            ("error", true),
            ("eof", true),
            ("cancel", true),
            ("error", false),
        ] {
            let (session, store, mut events_rx) = test_session().await;
            let publisher = TurnEvents::new(session.clone(), new_turn_id());
            let message_id = new_message_id();
            let cancellation = CancellationToken::new();
            let (tx, rx) = mpsc::unbounded_channel();
            tx.send(LlmEvent::Usage {
                usage: LlmTokenUsage {
                    total_tokens: Some(42),
                    ..Default::default()
                },
            })
            .unwrap();
            if visible {
                tx.send(LlmEvent::ThinkingDelta {
                    delta: "partial reasoning".into(),
                })
                .unwrap();
                tx.send(LlmEvent::ContentDelta {
                    delta: "partial answer".into(),
                })
                .unwrap();
            }
            if terminal == "error" {
                tx.send(LlmEvent::Error {
                    message: "provider failed".into(),
                })
                .unwrap();
            }
            let held_sender = (terminal == "cancel").then_some(tx);
            let consume = consume_llm_stream(
                rx,
                &publisher,
                StreamResponse::empty(message_id.clone()),
                &cancellation,
                100_000,
            );
            let result = if terminal == "cancel" {
                let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    tokio::join!(consume, async {
                        while let Some(event) = events_rx.recv().await {
                            if matches!(
                                event.payload,
                                astrcode_core::event::EventPayload::Live(
                                    LiveEventPayload::AssistantMessageDelta {
                                        delta: astrcode_core::event::AssistantDelta::Text(_),
                                        ..
                                    }
                                )
                            ) {
                                cancellation.cancel();
                                break;
                            }
                        }
                    })
                })
                .await
                .unwrap();
                result
            } else {
                consume.await
            };
            drop(held_sender);
            let failure = result.err().expect("the matrix only emits failed streams");
            let outcome = if terminal == "cancel" {
                AssistantAttemptOutcome::Cancelled
            } else {
                AssistantAttemptOutcome::Failed {
                    error: failure.error.to_string(),
                }
            };
            failure
                .response
                .settle(&publisher, outcome, 100_000)
                .await
                .unwrap();
            session
                .runtime
                .event_sink()
                .sync(store, session.id())
                .await
                .unwrap();
            let expected = if terminal == "cancel" {
                "cancelled"
            } else {
                "failed"
            };
            let mut settlements = 0;
            let mut usage_records = 0;
            while let Ok(event) = events_rx.try_recv() {
                let Some(payload) = event.payload.as_durable() else {
                    continue;
                };
                match payload {
                    DurableEventPayload::AssistantMessage {
                        message_id: id,
                        text,
                        reasoning_content,
                        outcome,
                    } => {
                        assert!(visible);
                        assert_eq!(id, &message_id);
                        assert_eq!(text, "partial answer");
                        assert_eq!(reasoning_content.as_deref(), Some("partial reasoning"));
                        assert_eq!(serde_json::to_value(outcome).unwrap()["type"], expected);
                        settlements += 1;
                    },
                    DurableEventPayload::AssistantAttemptSettled {
                        message_id: id,
                        outcome,
                    } => {
                        assert!(!visible);
                        assert_eq!(id, &message_id);
                        assert_eq!(serde_json::to_value(outcome).unwrap()["type"], expected);
                        settlements += 1;
                    },
                    DurableEventPayload::TokenUsageRecorded {
                        usage,
                        model_context_window,
                    } => {
                        assert_eq!(usage.total_tokens, Some(42));
                        assert_eq!(*model_context_window, 100_000);
                        usage_records += 1;
                    },
                    _ => {},
                }
            }
            assert_eq!((settlements, usage_records), (1, 1), "{terminal}");
            let state = session.read_model().await.unwrap();
            assert_eq!(state.model_context.messages.len(), usize::from(visible));
            if visible {
                assert_eq!(
                    state.model_context.messages[0].origin,
                    Some(if terminal == "cancel" {
                        astrcode_core::llm::TranscriptMessageOrigin::AssistantCancelled
                    } else {
                        astrcode_core::llm::TranscriptMessageOrigin::AssistantFailed
                    })
                );
            }
        }
    }

    #[tokio::test]
    async fn retry_settles_partial_response_and_usage_before_a_fresh_attempt() {
        let (session, store, mut events_rx) = test_session().await;

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
            LlmEvent::Usage {
                usage: LlmTokenUsage {
                    input_tokens: Some(7),
                    output_tokens: Some(3),
                    ..Default::default()
                },
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
            StreamResponse::empty(message_id.clone()),
            &CancellationToken::new(),
            100_000,
        )
        .await
        .unwrap();

        assert_eq!(outcome.response.text, "fresh");
        assert_eq!(outcome.response.reasoning_content, "fresh reasoning");
        assert_ne!(outcome.response.message_id, message_id);
        assert!(outcome.response.usage.is_none());
        session
            .runtime
            .event_sink()
            .sync(store, session.id())
            .await
            .unwrap();
        let events = std::iter::from_fn(|| events_rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(events.iter().any(|event| matches!(
            event.payload,
            EventPayload::Durable(DurableEventPayload::AssistantMessage {
                outcome: AssistantAttemptOutcome::Failed { .. },
                ..
            })
        )));
        assert!(events.iter().any(|event| matches!(
            &event.payload,
            EventPayload::Durable(DurableEventPayload::TokenUsageRecorded {usage, model_context_window: 100_000}) if usage.input_tokens == Some(7) && usage.output_tokens == Some(3)
        )));
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
