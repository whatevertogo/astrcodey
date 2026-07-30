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
    event::{DurableEventPayload, EventPayload, LiveEventPayload},
    types::TurnId,
};
use astrcode_session_projection::SessionReadModel;
use astrcode_storage::StorageError;
use tokio::sync::{mpsc, oneshot};

use crate::{
    session::{Session, SessionError},
    session_event_sink::SessionEventPublishError,
    turn_context::{TurnError, TurnEventTx},
};

const DURABLE_PUBLISH_MAX_ATTEMPTS: u32 = 3;
const DURABLE_PUBLISH_RETRY_BASE_MS: u64 = 50;

/// Turn 内统一的事件发布入口。
pub(crate) struct TurnEvents {
    session: Session,
    turn_id: TurnId,
    emitted_durable_error: AtomicBool,
}

impl TurnEvents {
    pub(crate) fn new(session: Session, turn_id: TurnId) -> Self {
        Self {
            session,
            turn_id,
            emitted_durable_error: AtomicBool::new(false),
        }
    }

    pub(crate) fn emitted_error(&self) -> bool {
        self.emitted_durable_error.load(Ordering::Relaxed)
    }

    /// 返回 storage 当前 projection 的快照。
    pub(crate) async fn snapshot_model(&self) -> Result<Arc<SessionReadModel>, TurnError> {
        self.session.read_model().await.map_err(TurnError::from)
    }

    pub(crate) async fn durable(&self, payload: DurableEventPayload) -> Result<(), TurnError> {
        match self.persist_durable(payload).await {
            Ok(()) => Ok(()),
            Err(error) => {
                self.live_error(-32603, error.to_string(), false);
                Err(error)
            },
        }
    }

    async fn persist_durable(&self, payload: DurableEventPayload) -> Result<(), TurnError> {
        self.session
            .emit_durable(Some(&self.turn_id), payload)
            .await?;
        Ok(())
    }

    pub(crate) fn live(&self, payload: LiveEventPayload) {
        self.session.emit_live(Some(&self.turn_id), payload);
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

async fn durable_with_retry(publisher: &TurnEvents, payload: DurableEventPayload) {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match publisher.persist_durable(payload.clone()).await {
            Ok(()) => break,
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
                publisher.live_error(-32603, error.to_string(), false);
                break;
            },
        }
    }
}

fn durable_publish_error_is_retryable(error: &TurnError) -> bool {
    let TurnError::Session(SessionError::EventPublish(SessionEventPublishError::Storage(
        StorageError::Io(error),
    ))) = error
    else {
        return false;
    };
    matches!(
        error.kind(),
        std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::NotConnected
    )
}

async fn dispatch_payload(publisher: &TurnEvents, payload: EventPayload) {
    match payload {
        EventPayload::Durable(payload) => durable_with_retry(publisher, payload).await,
        EventPayload::Live(payload) => publisher.live(payload),
    }
}

/// Turn 内 hook / 工具侧的事件入口：clone 后非阻塞 `send`，需要落盘时 `flush`。
#[derive(Clone)]
pub(crate) struct TurnEventSender {
    publish_tx: TurnEventTx,
    flush_tx: mpsc::UnboundedSender<oneshot::Sender<()>>,
}

impl TurnEventSender {
    pub(crate) fn event_tx(&self) -> TurnEventTx {
        self.publish_tx.clone()
    }

    /// 等待 ingress 队列中、本调用之前入队的 publish 全部处理完毕。
    pub(crate) async fn flush(&self) {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.flush_tx.send(ack_tx).is_err() {
            return;
        }
        let _ = ack_rx.await;
    }
}

/// 单 FIFO worker：turn 内唯一的 hook/工具事件 ingress。
pub(crate) struct TurnEventIngress {
    shutdown_tx: oneshot::Sender<()>,
    worker: tokio::task::JoinHandle<()>,
}

impl TurnEventIngress {
    pub(crate) fn start(publisher: Arc<TurnEvents>) -> (TurnEventSender, Self) {
        let (publish_tx, mut publish_rx) = mpsc::unbounded_channel::<EventPayload>();
        let (flush_tx, mut flush_rx) = mpsc::unbounded_channel::<oneshot::Sender<()>>();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let sender = TurnEventSender {
            publish_tx,
            flush_tx,
        };
        let worker = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        publish_rx.close();
                        flush_rx.close();
                        while let Some(payload) = publish_rx.recv().await {
                            dispatch_payload(&publisher, payload).await;
                        }
                        while let Some(ack) = flush_rx.recv().await {
                            let _ = ack.send(());
                        }
                        break;
                    }
                    Some(payload) = publish_rx.recv() => {
                        dispatch_payload(&publisher, payload).await;
                    }
                    Some(ack) = flush_rx.recv() => {
                        while let Ok(payload) = publish_rx.try_recv() {
                            dispatch_payload(&publisher, payload).await;
                        }
                        let _ = ack.send(());
                    }
                    else => break,
                }
            }
        });
        (
            sender,
            Self {
                shutdown_tx,
                worker,
            },
        )
    }

    pub(crate) async fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
        if let Err(error) = self.worker.await {
            tracing::error!(panic = %error, "turn event ingress worker panicked");
        }
    }
}

/// Turn 级扩展事件 ingress：在 `process_prompt` 期间为 hook / 工具提供 `event_tx`。
pub(crate) struct ExtensionEvents {
    ingress: TurnEventIngress,
}

impl ExtensionEvents {
    pub(crate) fn start(
        publisher: Arc<TurnEvents>,
        shared: &mut crate::turn_context::SharedTurnContext,
    ) -> Self {
        let (sender, ingress) = TurnEventIngress::start(publisher);
        shared.turn_event_sender = Some(sender);
        Self { ingress }
    }

    pub(crate) async fn shutdown(self, shared: &mut crate::turn_context::SharedTurnContext) {
        shared.turn_event_sender = None;
        self.ingress.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use astrcode_context::{
        ContextAssembler, ContextPrepareInput, NoopPostCompactEnricher, PreparedContext,
        context_assembler::LlmContextAssembler,
    };
    use astrcode_core::{
        config::{
            ContextSettings, EffectiveConfig, ExtensionSettings, LlmSettings, ProviderAuthScheme,
            ProviderWireFormat,
        },
        event::{DurableEventPayload, Event, EventPayload, ExtensionEventData, LiveEventPayload},
        llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
        tool::ToolDefinition,
        types::{new_session_id, new_turn_id},
    };
    use astrcode_extension_sdk::{
        extension::{
            CompactContext, CompactEvent, CompactResult, ContinueAfterStopContext,
            ContinueAfterStopResult, ExtensionError, ExtensionEvent, LifecycleContext,
            PostToolUseContext, PostToolUseResult, PreToolUseContext, PreToolUseResult,
            ProviderContext, ProviderEvent, ProviderResult, UserMessageEnvelopeContext,
            UserMessageEnvelopeResult,
        },
        runtime_ports::{NoopRuntimePorts, TurnHooks},
    };
    use astrcode_storage::in_memory::InMemoryEventStore;
    use tokio::sync::mpsc;

    use super::*;
    use crate::{
        SessionExtensionPorts,
        session::{Session, SessionCreateParams},
        session_event_sink::{SessionEventObserver, SessionEventSink},
        session_runtime::SessionRuntimeState,
        session_runtime_services::SessionRuntimeServices,
    };

    struct ChannelObserver(mpsc::UnboundedSender<Arc<Event>>);

    impl SessionEventObserver for ChannelObserver {
        fn publish(&self, event: Arc<Event>) {
            let _ = self.0.send(event);
        }
    }

    struct UnusedLlm;

    #[async_trait::async_trait]
    impl LlmProvider for UnusedLlm {
        async fn generate(
            &self,
            _messages: Vec<LlmMessage>,
            _tools: Vec<ToolDefinition>,
        ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
            unreachable!()
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: 1024,
                max_output_tokens: 1024,
            }
        }
    }

    struct TestContextAssembler {
        settings: ContextSettings,
    }

    impl ContextAssembler for TestContextAssembler {
        fn settings(&self) -> &ContextSettings {
            &self.settings
        }

        fn should_auto_compact(&self, _input: &ContextPrepareInput<'_>) -> bool {
            false
        }

        fn prepare_messages(&self, input: ContextPrepareInput<'_>) -> PreparedContext {
            LlmContextAssembler::new(self.settings.clone()).prepare_messages(input)
        }
    }

    fn test_runtime_services() -> Arc<SessionRuntimeServices> {
        test_runtime_services_with_hooks(Arc::new(NoopRuntimePorts))
    }

    fn test_runtime_services_with_hooks(
        turn_hooks: Arc<dyn TurnHooks>,
    ) -> Arc<SessionRuntimeServices> {
        let llm: Arc<dyn LlmProvider> = Arc::new(UnusedLlm);
        let context_assembler = Arc::new(TestContextAssembler {
            settings: ContextSettings::default(),
        });
        let effective = EffectiveConfig {
            llm: LlmSettings {
                provider_kind: "mock".into(),
                base_url: String::new(),
                api_key: String::new(),
                wire_format: ProviderWireFormat::OpenAiChatCompletions,
                auth_scheme: ProviderAuthScheme::Bearer,
                model_id: "mock-model".into(),
                max_tokens: 1024,
                context_limit: 1024,
                connect_timeout_secs: 1,
                read_timeout_secs: 1,
                max_retries: 0,
                retry_base_delay_ms: 0,
                supports_prompt_cache_key: false,
                supports_stream_usage: false,
                supports_strict_tool_use: false,
                prompt_cache_retention: None,
                reasoning: false,
                thinking_level: None,
                thinking: Default::default(),
                thinking_capability: None,
                thinking_configured: false,
            },
            small_llm: LlmSettings {
                provider_kind: "mock".into(),
                base_url: String::new(),
                api_key: String::new(),
                wire_format: ProviderWireFormat::OpenAiChatCompletions,
                auth_scheme: ProviderAuthScheme::Bearer,
                model_id: "mock-model".into(),
                max_tokens: 1024,
                context_limit: 1024,
                connect_timeout_secs: 1,
                read_timeout_secs: 1,
                max_retries: 0,
                retry_base_delay_ms: 0,
                supports_prompt_cache_key: false,
                supports_stream_usage: false,
                supports_strict_tool_use: false,
                prompt_cache_retention: None,
                reasoning: false,
                thinking_level: None,
                thinking: Default::default(),
                thinking_capability: None,
                thinking_configured: false,
            },
            context: ContextSettings::default(),
            agent: astrcode_core::config::AgentSettings::default(),
            permissions: Default::default(),
            extensions: ExtensionSettings::default(),
        };
        Arc::new(SessionRuntimeServices::new(
            llm.clone(),
            llm,
            effective,
            SessionExtensionPorts::with_turn_hooks(turn_hooks),
            context_assembler,
            Arc::new(NoopPostCompactEnricher),
            Arc::new(NoopRuntimePorts),
        ))
    }

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
        let session = Session::create_with_params(SessionCreateParams {
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
        .unwrap();
        session
    }

    #[tokio::test]
    async fn durable_ingress_notifies_once_for_non_retryable_failure() {
        let retryable_error = TurnError::Session(SessionError::EventPublish(
            SessionEventPublishError::Storage(StorageError::Io(std::io::Error::new(
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
            Arc::new(SessionEventSink::new(Arc::new(ChannelObserver(events_tx)))),
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
        let publisher = TurnEvents::new(session.clone(), new_turn_id());
        durable_with_retry(&publisher, duplicate_session_started).await;
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
        assert!(model.transcript.messages.iter().any(|message| {
            message.message.role == astrcode_core::llm::LlmRole::User
                && message.message.content.iter().any(|content| {
                    matches!(
                        content,
                        astrcode_core::llm::LlmContent::Text { text } if text == "injected"
                    )
                })
        }));
    }

    struct EmitEventRuntime;

    #[async_trait::async_trait]
    impl TurnHooks for EmitEventRuntime {
        async fn emit_pre_tool_use(
            &self,
            ctx: PreToolUseContext,
        ) -> Result<PreToolUseResult, ExtensionError> {
            let tx = ctx
                .event_tx
                .ok_or_else(|| ExtensionError::Internal("no turn event sender".into()))?;
            tx.send(EventPayload::Durable(DurableEventPayload::ExtensionEvent(
                ExtensionEventData {
                    extension_id: "emit-probe".into(),
                    event_type: "emit.probe".into(),
                    schema_version: 1,
                    payload: serde_json::json!({ "probe": true }),
                },
            )))
            .map_err(|_| ExtensionError::Internal("turn event sender closed".into()))?;
            tx.send(EventPayload::Live(LiveEventPayload::ExtensionEvent(
                ExtensionEventData {
                    extension_id: "emit-probe".into(),
                    event_type: "emit.live".into(),
                    schema_version: 1,
                    payload: serde_json::json!({ "probe": true }),
                },
            )))
            .map_err(|_| ExtensionError::Internal("turn event sender closed".into()))?;
            Ok(PreToolUseResult::Allow)
        }

        async fn emit_post_tool_use(
            &self,
            _ctx: PostToolUseContext,
        ) -> Result<PostToolUseResult, ExtensionError> {
            Ok(PostToolUseResult::Allow)
        }

        async fn emit_provider(
            &self,
            _event: ProviderEvent,
            _ctx: ProviderContext,
        ) -> Result<ProviderResult, ExtensionError> {
            Ok(ProviderResult::Allow)
        }

        async fn emit_compact(
            &self,
            _event: CompactEvent,
            _ctx: CompactContext,
        ) -> Result<CompactResult, ExtensionError> {
            Ok(CompactResult::Allow)
        }

        async fn emit_continue_after_stop(
            &self,
            _ctx: ContinueAfterStopContext,
        ) -> Result<ContinueAfterStopResult, ExtensionError> {
            Ok(ContinueAfterStopResult::EndTurn)
        }

        async fn emit_user_message_envelope(
            &self,
            _ctx: UserMessageEnvelopeContext,
        ) -> Result<UserMessageEnvelopeResult, ExtensionError> {
            Ok(UserMessageEnvelopeResult::Allow)
        }

        async fn emit_lifecycle(
            &self,
            _event: ExtensionEvent,
            _ctx: LifecycleContext,
        ) -> Result<(), ExtensionError> {
            Ok(())
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
        let mut shared =
            crate::turn_context::SharedTurnContext::from_read_model(session.id(), &model);
        let bridge = ExtensionEvents::start(Arc::clone(&publisher), &mut shared);

        let ctx = PreToolUseContext {
            session_id: session.id().to_string(),
            working_dir: shared.working_dir.clone(),
            model: shared.model_selection(),
            call_id: "call-1".into(),
            tool_name: "any".into(),
            tool_input: serde_json::json!({}),
            approval_mode: shared.approval_mode,
            available_tools: vec![],
            event_tx: shared.turn_event_tx(),
            extension_event_sink: None,
            session_store_dir: None,
        };
        runtime_services
            .turn_hooks()
            .emit_pre_tool_use(ctx)
            .await
            .unwrap();

        bridge.shutdown(&mut shared).await;

        let events = session
            .runtime
            .store()
            .replay_events(session.id())
            .await
            .unwrap();
        assert!(events.iter().any(|e| matches!(
            &e.payload,
            DurableEventPayload::ExtensionEvent(ExtensionEventData {
                extension_id,
                event_type,
                ..
            }) if extension_id == "emit-probe" && event_type == "emit.probe"
        )));
        assert!(!events.iter().any(|e| matches!(
            &e.payload,
            DurableEventPayload::ExtensionEvent(ExtensionEventData { event_type, .. })
                if event_type == "emit.live"
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
                sender.flush().await;
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
        drop(sender);
        tokio::time::timeout(Duration::from_secs(5), ingress.shutdown())
            .await
            .expect("parallel ingress flush timed out");
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
