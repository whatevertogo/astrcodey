//! Turn 内事件组装与扩展 ingress。
//!
//! durable/live 最终都进入 session 级有序发布管线；projection 始终从 storage 的唯一
//! 实例读取，不在 turn 内维护第二份可变副本。
//!
//! ## 事件 ingress
//!
//! Hook / 工具侧通过 [`TurnEventSender`] 非阻塞 `send`；单 FIFO worker 串行桥接到 async
//! publisher。工具执行结束后 [`TurnEventSender::flush`] 保证此前入队事件已处理。

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use astrcode_core::{
    event::{
        DurableEventPayload, EventDeliveryReceipt, EventPayload, EventPublisher, EventSendError,
        LiveEventPayload, StoredEvent,
    },
    types::{EventId, TurnId},
};
use astrcode_session_projection::SessionReadModel;
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};

use crate::{
    payload::JSON_RPC_INTERNAL_ERROR,
    session::Session,
    turn_context::{TurnError, TurnEventTx},
};

const DURABLE_PUBLISH_MAX_ATTEMPTS: u32 = 3;
const DURABLE_PUBLISH_RETRY_BASE_MS: u64 = 50;
const TURN_EVENT_INGRESS_CAPACITY: usize = 256;

/// Turn 内统一的事件发布入口。
pub(crate) struct TurnEvents {
    session: Session,
    turn_id: TurnId,
    emitted_durable_error: AtomicBool,
    ingress_error: Mutex<Option<String>>,
}

impl TurnEvents {
    pub(crate) fn new(session: Session, turn_id: TurnId) -> Self {
        Self {
            session,
            turn_id,
            emitted_durable_error: AtomicBool::new(false),
            ingress_error: Mutex::new(None),
        }
    }

    pub(crate) fn emitted_error(&self) -> bool {
        self.emitted_durable_error.load(Ordering::Relaxed)
    }

    fn record_ingress_error(&self, error: &TurnError) {
        let mut ingress_error = self.ingress_error.lock();
        if ingress_error.is_none() {
            // first-wins：后续错误只记日志，避免覆盖首个根因。
            *ingress_error = Some(error.to_string());
        } else {
            tracing::warn!(error = %error, "turn event ingress failed again after first error");
        }
    }

    fn ingress_result(&self) -> Result<(), TurnError> {
        match self.ingress_error.lock().clone() {
            Some(error) => Err(TurnError::EventIngress(error)),
            None => Ok(()),
        }
    }

    /// 返回 storage 当前 projection 的快照。
    pub(crate) async fn snapshot_model(&self) -> Result<Arc<SessionReadModel>, TurnError> {
        self.session.read_model().await.map_err(TurnError::from)
    }

    /// 持久化失败统一收尾：发 live 错误事件后返回原始错误。
    fn durable_failed(publisher: &TurnEvents, error: TurnError) -> TurnError {
        publisher.live_error(JSON_RPC_INTERNAL_ERROR, error.to_string(), false);
        error
    }

    pub(crate) async fn durable(&self, payload: DurableEventPayload) -> Result<(), TurnError> {
        match self.persist_durable(payload).await {
            Ok(_) => Ok(()),
            Err(error) => Err(Self::durable_failed(self, error)),
        }
    }

    async fn persist_durable(
        &self,
        payload: DurableEventPayload,
    ) -> Result<StoredEvent, TurnError> {
        self.session
            .emit_durable(Some(&self.turn_id), payload)
            .await
            .map_err(TurnError::from)
    }

    pub(crate) fn live(&self, payload: LiveEventPayload) {
        self.session.emit_live(Some(&self.turn_id), payload);
    }

    async fn live_required(&self, payload: LiveEventPayload) -> Result<EventId, TurnError> {
        self.session
            .emit_live_required(Some(&self.turn_id), payload)
            .await
            .map_err(TurnError::from)
    }

    /// 持久化错误事件，并标记 `emitted_error`（供 `drive_agent` 避免重复持久化）。
    pub(crate) async fn durable_error(
        &self,
        code: i32,
        message: String,
        recoverable: bool,
    ) -> Result<(), TurnError> {
        self.durable(DurableEventPayload::ErrorOccurred {
            code,
            message,
            recoverable,
        })
        .await?;
        self.emitted_durable_error.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// 发送 live 错误事件。live 事件不会出现在后续 conversation snapshot 中，因此不能
    /// 标记 `emitted_error`，否则 turn finalizer 会跳过 durable ErrorOccurred。
    pub(crate) fn live_error(&self, code: i32, message: String, recoverable: bool) {
        self.live(LiveEventPayload::ErrorOccurred {
            code,
            message,
            recoverable,
        });
    }
}

async fn durable_with_retry(
    publisher: &TurnEvents,
    payload: DurableEventPayload,
) -> Result<StoredEvent, TurnError> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match publisher.persist_durable(payload.clone()).await {
            Ok(stored) => return Ok(stored),
            Err(error)
                if attempt < DURABLE_PUBLISH_MAX_ATTEMPTS
                    && durable_publish_error_is_retryable(&error) =>
            {
                tracing::warn!(
                    error = %error,
                    attempt,
                    max_attempts = DURABLE_PUBLISH_MAX_ATTEMPTS,
                    "turn event ingress durable publish failed, retrying"
                );
                tokio::time::sleep(std::time::Duration::from_millis(
                    DURABLE_PUBLISH_RETRY_BASE_MS * u64::from(attempt),
                ))
                .await;
            },
            Err(error) => {
                tracing::error!(
                    error = %error,
                    attempt,
                    "turn event ingress durable publish failed"
                );
                return Err(TurnEvents::durable_failed(publisher, error));
            },
        }
    }
}

fn durable_publish_error_is_retryable(error: &TurnError) -> bool {
    matches!(error, TurnError::Session(error) if error.is_retryable())
}

async fn dispatch_payload(
    publisher: &TurnEvents,
    payload: EventPayload,
) -> Result<EventDeliveryReceipt, TurnError> {
    match payload {
        EventPayload::Durable(payload) => {
            let stored = durable_with_retry(publisher, payload).await?;
            Ok(EventDeliveryReceipt::Persisted {
                event_id: stored.id.clone(),
                seq: stored.seq,
            })
        },
        // 自定义实时事件走 required 路径，确保生产扩展能观察到入队失败；
        // 其余 Live 变体默认 best-effort，丢事件是可接受的降级。
        EventPayload::Live(payload @ LiveEventPayload::CustomEvent(_)) => {
            let event_id = publisher.live_required(payload).await?;
            Ok(EventDeliveryReceipt::LivePublished { event_id })
        },
        EventPayload::Live(payload) => {
            publisher.live(payload);
            Ok(EventDeliveryReceipt::Accepted)
        },
    }
}

/// Turn 内 hook / 工具侧的事件入口：clone 后非阻塞 `send`，需要落盘时 `flush`。
#[derive(Clone)]
pub(crate) struct TurnEventSender {
    command_tx: mpsc::Sender<IngressCommand>,
    event_tx: TurnEventTx,
    publisher: Arc<TurnEvents>,
}

impl TurnEventSender {
    pub(crate) fn event_tx(&self) -> TurnEventTx {
        self.event_tx.clone()
    }

    /// 等待 ingress 队列中、本调用之前入队的 publish 全部处理完毕。
    pub(crate) async fn flush(&self) -> Result<(), TurnError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self
            .command_tx
            .send(IngressCommand::Flush(ack_tx))
            .await
            .is_err()
        {
            return Err(TurnError::EventIngress(
                "turn event ingress is closed".into(),
            ));
        }
        ack_rx
            .await
            .map_err(|_| TurnError::EventIngress("turn event flush was dropped".into()))?;
        self.publisher.ingress_result()
    }
}

enum IngressCommand {
    Publish {
        payload: Box<EventPayload>,
        reply: Option<oneshot::Sender<Result<EventDeliveryReceipt, EventSendError>>>,
    },
    Flush(oneshot::Sender<()>),
    Shutdown(oneshot::Sender<()>),
}

struct TurnEventPublisher {
    command_tx: mpsc::Sender<IngressCommand>,
}

#[async_trait::async_trait]
impl EventPublisher for TurnEventPublisher {
    // durable-in-try_send 与 session 路径（`crate::session_runtime` 的
    // `SessionScopedEventPublisher`，直接拒绝 durable）语义不同：这里接受并入队，
    // 由 ingress worker 经 `dispatch_payload` 持久化。
    fn try_send(&self, payload: EventPayload) -> Result<(), EventSendError> {
        self.command_tx
            .try_send(IngressCommand::Publish {
                payload: Box::new(payload),
                reply: None,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => EventSendError::Full,
                mpsc::error::TrySendError::Closed(_) => EventSendError::Closed,
            })
    }

    async fn send_confirmed(
        &self,
        payload: EventPayload,
    ) -> Result<EventDeliveryReceipt, EventSendError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(IngressCommand::Publish {
                payload: Box::new(payload),
                reply: Some(reply_tx),
            })
            .await
            .map_err(|_| EventSendError::Closed)?;
        reply_rx.await.map_err(|_| EventSendError::Closed)?
    }
}

/// 单 FIFO worker：turn 内唯一的 hook/工具事件 ingress。
pub(crate) struct TurnEventIngress {
    command_tx: mpsc::Sender<IngressCommand>,
    worker: tokio::task::JoinHandle<()>,
    publisher: Arc<TurnEvents>,
}

impl TurnEventIngress {
    pub(crate) fn start(publisher: Arc<TurnEvents>) -> (TurnEventSender, Self) {
        let (command_tx, mut command_rx) = mpsc::channel(TURN_EVENT_INGRESS_CAPACITY);
        let event_tx = TurnEventTx::from_publisher(Arc::new(TurnEventPublisher {
            command_tx: command_tx.clone(),
        }));
        let sender = TurnEventSender {
            command_tx: command_tx.clone(),
            event_tx,
            publisher: Arc::clone(&publisher),
        };
        let publisher_for_worker = Arc::clone(&publisher);
        let worker = tokio::spawn(async move {
            tracing::debug!("turn event ingress worker started");
            let mut shutdown_ack = None;
            while let Some(command) = command_rx.recv().await {
                match command {
                    IngressCommand::Publish { payload, reply } => {
                        let result = dispatch_payload(&publisher_for_worker, *payload).await;
                        if let Err(error) = &result {
                            publisher_for_worker.record_ingress_error(error);
                        }
                        if let Some(reply) = reply {
                            // 与 `session_runtime.rs` 的 `map_event_publish_error` 互指：
                            // `TurnError` 不带 Closed/Full 语义，统一折叠为 PublishFailed。
                            let result = result
                                .map_err(|error| EventSendError::PublishFailed(error.to_string()));
                            let _ = reply.send(result);
                        }
                    },
                    IngressCommand::Flush(ack) => {
                        let _ = ack.send(());
                    },
                    IngressCommand::Shutdown(ack) => {
                        command_rx.close();
                        shutdown_ack = Some(ack);
                    },
                }
            }
            if let Some(ack) = shutdown_ack {
                let _ = ack.send(());
            }
            tracing::debug!("turn event ingress worker stopped");
        });
        (
            sender,
            Self {
                command_tx,
                worker,
                publisher,
            },
        )
    }

    pub(crate) async fn shutdown(self) -> Result<(), TurnError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.command_tx
            .send(IngressCommand::Shutdown(ack_tx))
            .await
            .map_err(|_| TurnError::EventIngress("turn event ingress is closed".into()))?;
        ack_rx
            .await
            .map_err(|_| TurnError::EventIngress("turn event shutdown was dropped".into()))?;
        self.worker
            .await
            .map_err(|error| TurnError::EventIngress(format!("worker panicked: {error}")))?;
        self.publisher.ingress_result()
    }
}

/// Turn 级事件桥：在 `process_prompt` 期间为 hook / 工具提供 `event_tx`。
pub(crate) struct TurnEventBridge {
    ingress: TurnEventIngress,
}

impl TurnEventBridge {
    pub(crate) fn start(
        publisher: Arc<TurnEvents>,
        shared: &mut crate::turn_context::SharedTurnContext,
    ) -> Self {
        let (sender, ingress) = TurnEventIngress::start(publisher);
        shared.turn_event_sender = Some(sender);
        Self { ingress }
    }

    pub(crate) async fn shutdown(
        self,
        shared: &mut crate::turn_context::SharedTurnContext,
    ) -> Result<(), TurnError> {
        shared.turn_event_sender = None;
        self.ingress.shutdown().await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use astrcode_core::{
        event::{CustomEventData, DurableEventPayload, EventPayload, LiveEventPayload},
        types::{new_session_id, new_turn_id},
        user_input::UserInput,
    };
    use astrcode_extension_sdk::{
        extension::{
            CompactEvent, CompactResult, ContinueAfterStopResult, ExtensionError, LifecycleEvent,
            PostToolUseResult, PreToolUsePayload, PreToolUseResult, ProviderEvent, ProviderResult,
            RuntimeCompactContext, RuntimeContinueAfterStopContext, RuntimeLifecycleContext,
            RuntimePostToolUseContext, RuntimePreToolUseContext, RuntimeProviderContext,
            RuntimeUserMessageEnvelopeContext, UserMessageEnvelopeResult,
        },
        runtime_ports::TurnHooks,
    };
    use astrcode_storage::{StorageError, in_memory::InMemoryEventStore};
    use tokio::sync::mpsc;

    use super::*;
    use crate::{
        SessionError,
        session::{Session, SessionCreateParams},
        session_event_sink::{SessionEventPublishError, SessionEventSink},
        session_runtime::SessionRuntimeState,
        session_runtime_services::SessionRuntimeServices,
        test_support::{ChannelObserver, test_runtime_services, test_runtime_services_with_hooks},
    };

    async fn test_session() -> Session {
        test_session_with_runtime_services(test_runtime_services()).await
    }

    async fn test_session_with_runtime_services(
        runtime_services: Arc<SessionRuntimeServices>,
    ) -> Session {
        let store: Arc<dyn astrcode_storage::SessionStore> = Arc::new(InMemoryEventStore::new());
        let session_id = new_session_id();
        let runtime = Arc::new(SessionRuntimeState::new(session_id, store));
        test_session_with_runtime(runtime_services, runtime).await
    }

    async fn test_session_with_runtime(
        runtime_services: Arc<SessionRuntimeServices>,
        runtime: Arc<SessionRuntimeState>,
    ) -> Session {
        Session::create_with_params(SessionCreateParams {
            working_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            model_id: "mock-model".into(),
            parent_session_id: None,
            tool_selection: None,
            source_extension: None,
            extra_system_prompt: None,
            initial_system_prompt: None,
            runtime,
            runtime_services,
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn durable_ingress_notifies_once_for_non_retryable_failure() {
        let retryable_error = TurnError::Session(SessionError::EventPublish(
            SessionEventPublishError::from(StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "injected timeout",
            ))),
        ));
        assert!(durable_publish_error_is_retryable(&retryable_error));

        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let store = Arc::new(InMemoryEventStore::new());
        let runtime = Arc::new(SessionRuntimeState::new_with_event_sink(
            new_session_id(),
            store.clone(),
            Arc::new(SessionEventSink::new(ChannelObserver::new(events_tx))),
        ));
        let session = test_session_with_runtime(test_runtime_services(), runtime).await;
        while events_rx.try_recv().is_ok() {}

        let duplicate_session_started = session
            .runtime
            .store()
            .replay_events(session.id())
            .await
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .event
            .payload;
        let publisher = Arc::new(TurnEvents::new(session.clone(), new_turn_id()));
        let (sender, ingress) = TurnEventIngress::start(publisher);
        sender
            .event_tx()
            .send(EventPayload::Durable(duplicate_session_started))
            .unwrap();
        assert!(sender.flush().await.is_err());
        drop(sender);
        assert!(ingress.shutdown().await.is_err());
        session
            .runtime
            .event_sink()
            .sync(store, session.id())
            .await
            .unwrap();

        let mut live_error_count = 0;
        while let Ok(event) = events_rx.try_recv() {
            if matches!(
                event.payload,
                EventPayload::Live(LiveEventPayload::ErrorOccurred { .. })
            ) {
                live_error_count += 1;
            }
        }
        assert_eq!(live_error_count, 1);
    }

    #[tokio::test]
    async fn durable_emit_updates_read_model() {
        let session = test_session().await;
        let turn_id = new_turn_id();
        let publisher = TurnEvents::new(session.clone(), turn_id.clone());
        publisher
            .durable(DurableEventPayload::UserMessage {
                message_id: astrcode_core::types::new_message_id(),
                text: "injected".into(),
                attachments: vec![],
                accepted_seq: None,
            })
            .await
            .unwrap();

        let model = session.read_model().await.unwrap();
        assert!(model.model_context.messages.iter().any(|message| {
            message.message.role == astrcode_core::llm::LlmRole::User
                && message.message.content.iter().any(|content| {
                    matches!(
                        content,
                        astrcode_core::llm::LlmContent::Text { text } if text == "injected"
                    )
                })
        }));
    }

    #[tokio::test]
    async fn event_sender_reports_publication_and_admission_state() {
        let session = test_session().await;
        let publisher = Arc::new(TurnEvents::new(session.clone(), new_turn_id()));
        let (sender, ingress) = TurnEventIngress::start(publisher);
        let event = CustomEventData {
            extension_id: "receipt-probe".into(),
            event_type: "receipt.published".into(),
            schema_version: 1,
            causation_id: None,
            cascade_depth: 0,
            payload: serde_json::json!({ "status": "ok" }),
        };

        let receipt = sender
            .event_tx()
            .send_confirmed(EventPayload::Durable(DurableEventPayload::CustomEvent(
                event,
            )))
            .await
            .unwrap();
        let EventDeliveryReceipt::Persisted { event_id, seq } = receipt else {
            panic!("durable extension event must return a persisted receipt");
        };
        assert_eq!(seq, 1);

        let stored = session
            .runtime
            .store()
            .replay_events(session.id())
            .await
            .unwrap();
        assert!(stored.iter().any(|event| event.id == event_id));

        drop(sender);
        ingress.shutdown().await.unwrap();

        let (command_tx, mut command_rx) = mpsc::channel(1);
        let publisher = TurnEventPublisher {
            command_tx: command_tx.clone(),
        };
        publisher
            .try_send(EventPayload::Live(LiveEventPayload::AgentRunStarted))
            .unwrap();
        assert_eq!(
            publisher.try_send(EventPayload::Live(LiveEventPayload::AgentRunStarted)),
            Err(EventSendError::Full)
        );
        command_rx.close();
        assert_eq!(
            publisher.try_send(EventPayload::Live(LiveEventPayload::AgentRunStarted)),
            Err(EventSendError::Closed)
        );
    }

    #[tokio::test]
    async fn aborted_turn_does_not_complete_when_tool_settlement_fails() {
        let session = test_session().await;
        let turn_id = new_turn_id();
        session
            .emit_durable(Some(&turn_id), DurableEventPayload::TurnStarted)
            .await
            .unwrap();
        session
            .emit_durable(
                Some(&turn_id),
                DurableEventPayload::ToolCallRequested {
                    call_id: astrcode_core::types::ToolCallId::new("call-abort"),
                    tool_name: "read".into(),
                    arguments: serde_json::json!({"path": "README.md"}),
                    raw_arguments: None,
                },
            )
            .await
            .unwrap();

        let store = session.runtime.store().clone();
        session
            .runtime
            .event_sink()
            .release(store.as_ref(), session.id())
            .await
            .unwrap();
        assert!(
            crate::finalize_aborted_turn(&session, &turn_id)
                .await
                .is_err()
        );

        let events = store.replay_events(session.id()).await.unwrap();
        assert!(
            events.iter().any(|event| matches!(
                event.payload,
                DurableEventPayload::ToolCallRequested { .. }
            ))
        );
        assert!(!events.iter().any(|event| matches!(
            event.payload,
            DurableEventPayload::ToolCallCancelled { .. }
                | DurableEventPayload::TurnAbortedContext
                | DurableEventPayload::TurnCompleted { .. }
        )));
    }

    struct EmitEventRuntime;

    #[async_trait::async_trait]
    impl TurnHooks for EmitEventRuntime {
        async fn emit_pre_tool_use(
            &self,
            ctx: RuntimePreToolUseContext,
        ) -> Result<PreToolUseResult, ExtensionError> {
            let tx = ctx
                .call()
                .event_tx()
                .cloned()
                .ok_or_else(|| ExtensionError::Internal("no turn event sender".into()))?;
            tx.send(EventPayload::Durable(DurableEventPayload::CustomEvent(
                CustomEventData {
                    extension_id: "emit-probe".into(),
                    event_type: "emit.probe".into(),
                    schema_version: 1,
                    causation_id: None,
                    cascade_depth: 0,
                    payload: serde_json::json!({ "probe": true }),
                },
            )))
            .map_err(|_| ExtensionError::Internal("turn event sender closed".into()))?;
            tx.send(EventPayload::Live(LiveEventPayload::CustomEvent(
                CustomEventData {
                    extension_id: "emit-probe".into(),
                    event_type: "emit.live".into(),
                    schema_version: 1,
                    causation_id: None,
                    cascade_depth: 0,
                    payload: serde_json::json!({ "probe": true }),
                },
            )))
            .map_err(|_| ExtensionError::Internal("turn event sender closed".into()))?;
            Ok(PreToolUseResult::Allow)
        }

        async fn emit_post_tool_use(
            &self,
            _ctx: RuntimePostToolUseContext,
        ) -> Result<PostToolUseResult, ExtensionError> {
            Ok(PostToolUseResult::Allow)
        }

        async fn emit_provider(
            &self,
            _event: ProviderEvent,
            _ctx: RuntimeProviderContext,
        ) -> Result<ProviderResult, ExtensionError> {
            Ok(ProviderResult::Allow)
        }

        async fn emit_compact(
            &self,
            _event: CompactEvent,
            _ctx: RuntimeCompactContext,
        ) -> Result<CompactResult, ExtensionError> {
            Ok(CompactResult::Allow)
        }

        async fn emit_continue_after_stop(
            &self,
            _ctx: RuntimeContinueAfterStopContext,
        ) -> Result<ContinueAfterStopResult, ExtensionError> {
            Ok(ContinueAfterStopResult::EndTurn)
        }

        async fn emit_user_message_envelope(
            &self,
            _ctx: RuntimeUserMessageEnvelopeContext,
        ) -> Result<UserMessageEnvelopeResult, ExtensionError> {
            Ok(UserMessageEnvelopeResult::Allow)
        }

        async fn emit_lifecycle(
            &self,
            _event: LifecycleEvent,
            _ctx: RuntimeLifecycleContext,
        ) -> Result<(), ExtensionError> {
            Ok(())
        }
    }

    struct FailTurnStartAndEmitOnEnd;

    #[async_trait::async_trait]
    impl TurnHooks for FailTurnStartAndEmitOnEnd {
        async fn emit_lifecycle(
            &self,
            event: LifecycleEvent,
            ctx: RuntimeLifecycleContext,
        ) -> Result<(), ExtensionError> {
            match event {
                LifecycleEvent::TurnStart => Err(ExtensionError::Internal(
                    "injected turn start failure".into(),
                )),
                LifecycleEvent::TurnEnd => {
                    let tx =
                        ctx.call().event_tx().cloned().ok_or_else(|| {
                            ExtensionError::Internal("no turn event sender".into())
                        })?;
                    tx.send(EventPayload::Durable(DurableEventPayload::CustomEvent(
                        CustomEventData {
                            extension_id: "turn-end-probe".into(),
                            event_type: "turn.end.error".into(),
                            schema_version: 1,
                            causation_id: None,
                            cascade_depth: 0,
                            payload: serde_json::json!({}),
                        },
                    )))
                    .map_err(|_| ExtensionError::Internal("turn event sender closed".into()))
                },
                _ => Ok(()),
            }
        }
    }

    #[tokio::test]
    async fn extension_event_bridge_delivers_hook_emit_to_store() {
        let session = test_session_with_runtime_services(test_runtime_services_with_hooks(
            Arc::new(EmitEventRuntime),
        ))
        .await;
        let runtime_services = session.runtime_services();

        let turn_id = new_turn_id();
        let publisher = Arc::new(TurnEvents::new(session.clone(), turn_id.clone()));
        let model = session.read_model().await.unwrap();
        let mut tool_context = crate::tool_exec::TurnToolContext::for_turn(
            &session,
            &model,
            turn_id,
            model.identity.tool_selection.clone(),
            session.session_store_dir().await,
            tokio_util::sync::CancellationToken::new(),
        );
        let bridge = TurnEventBridge::start(Arc::clone(&publisher), &mut tool_context.shared);

        let call = tool_context.shared.hook_call_context();
        let ctx = RuntimePreToolUseContext::new(
            call,
            PreToolUsePayload::new(
                "call-1".into(),
                "any",
                serde_json::json!({}),
                tool_context.shared.approval_mode,
                vec![],
            ),
        );
        runtime_services
            .pin_extension_view()
            .await
            .unwrap()
            .turn_hooks()
            .emit_pre_tool_use(ctx)
            .await
            .unwrap();

        bridge.shutdown(&mut tool_context.shared).await.unwrap();

        let events = session
            .runtime
            .store()
            .replay_events(session.id())
            .await
            .unwrap();
        assert!(events.iter().any(|e| matches!(
            &e.payload,
            DurableEventPayload::CustomEvent(CustomEventData {
                extension_id,
                event_type,
                ..
            }) if extension_id == "emit-probe" && event_type == "emit.probe"
        )));
        assert!(!events.iter().any(|e| matches!(
            &e.payload,
            DurableEventPayload::CustomEvent(CustomEventData { event_type, .. })
                if event_type == "emit.live"
        )));
    }

    #[tokio::test]
    async fn failed_turn_end_hook_can_publish_before_event_bridge_shutdown() {
        let session = test_session_with_runtime_services(test_runtime_services_with_hooks(
            Arc::new(FailTurnStartAndEmitOnEnd),
        ))
        .await;
        let turn_id = new_turn_id();

        let handle = session
            .submit(UserInput::text_only("trigger failure"), turn_id, None)
            .await
            .unwrap();
        let result = handle.wait().await.unwrap();
        assert!(result.output.is_err());

        let events = session
            .runtime
            .store()
            .replay_events(session.id())
            .await
            .unwrap();
        assert!(events.iter().any(|event| matches!(
            &event.payload,
            DurableEventPayload::CustomEvent(CustomEventData {
                extension_id,
                event_type,
                ..
            }) if extension_id == "turn-end-probe" && event_type == "turn.end.error"
        )));
    }

    /// 模拟并行工具经同一 ingress 发 durable 并 flush。
    #[tokio::test]
    async fn parallel_tool_senders_flush_and_shutdown_with_retained_tx() {
        use std::time::Duration;

        use astrcode_core::types::new_message_id;

        let session = test_session().await;
        let publisher = Arc::new(TurnEvents::new(session.clone(), new_turn_id()));

        let (sender, ingress) = TurnEventIngress::start(Arc::clone(&publisher));
        let retained_tx = sender.event_tx();

        let mut workers = Vec::new();
        for index in 0..8 {
            let sender = sender.clone();
            workers.push(tokio::spawn(async move {
                let tx = sender.event_tx();
                tx.send(EventPayload::Durable(DurableEventPayload::UserMessage {
                    message_id: new_message_id(),
                    text: format!("parallel-{index}"),
                    attachments: vec![],
                    accepted_seq: None,
                }))
                .unwrap();
                sender.flush().await.unwrap();
            }));
        }

        for worker in workers {
            worker.await.unwrap();
        }
        retained_tx
            .send(EventPayload::Durable(DurableEventPayload::UserMessage {
                message_id: new_message_id(),
                text: "shutdown-drain".into(),
                attachments: vec![],
                accepted_seq: None,
            }))
            .unwrap();
        let (shutdown_ack_tx, shutdown_ack_rx) = oneshot::channel();
        ingress
            .command_tx
            .send(IngressCommand::Shutdown(shutdown_ack_tx))
            .await
            .unwrap();
        drop(sender);
        tokio::time::timeout(Duration::from_secs(5), shutdown_ack_rx)
            .await
            .expect("parallel ingress flush timed out")
            .unwrap();
        ingress.worker.await.unwrap();
        ingress.publisher.ingress_result().unwrap();
        assert!(
            retained_tx
                .send(EventPayload::Live(LiveEventPayload::AgentRunStarted))
                .is_err()
        );

        let events = session
            .runtime
            .store()
            .replay_events(session.id())
            .await
            .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    matches!(event.payload, DurableEventPayload::UserMessage { .. })
                })
                .count(),
            9
        );
    }
}
