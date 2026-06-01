//! Turn 内事件发布 — durable 同步写入 store，live 直发 fanout。
//!
//! 已打开 session 的 `read_model()` 克隆内存投影（非全量 replay）。本模块在 turn 内
//! 用 [`astrcode_storage::projection::reduce`] 增量更新缓存，避免每步 tool commit / prepare
//! 重复克隆整份 [`SessionReadModel`]。
//!
//! ## 事件 ingress
//!
//! Hook / 工具侧通过 [`TurnEventSender`] 非阻塞 `send`；单 FIFO worker 串行处理 durable，
//! 避免并行工具各自 `spawn` bridge 时争用 [`TurnEvents::model_cache`] 或在 sender/await
//! 上自挂。工具执行结束后 [`TurnEventSender::flush`] 即可保证此前入队事件已落盘/广播，
//! 无需 per-tool bridge 与 drop 顺序协议。

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use astrcode_core::{event::EventPayload, storage::SessionReadModel, types::TurnId};
use astrcode_storage::projection;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::{
    session::Session,
    turn_context::{TurnError, TurnEventTx, send_event},
};

const DURABLE_PUBLISH_MAX_ATTEMPTS: u32 = 3;
const DURABLE_PUBLISH_RETRY_BASE_MS: u64 = 50;

/// Turn 内统一的事件发布入口。
pub(crate) struct TurnEvents {
    session: Session,
    turn_id: TurnId,
    live_tx: Option<TurnEventTx>,
    emitted_error: Arc<AtomicBool>,
    model_cache: Mutex<Option<SessionReadModel>>,
}

impl TurnEvents {
    pub(crate) fn new(session: Session, turn_id: TurnId, live_tx: Option<TurnEventTx>) -> Self {
        Self {
            session,
            turn_id,
            live_tx,
            emitted_error: Arc::new(AtomicBool::new(false)),
            model_cache: Mutex::new(None),
        }
    }

    pub(crate) fn emitted_error(&self) -> bool {
        self.emitted_error.load(Ordering::Relaxed)
    }

    /// 丢弃 turn 内缓存（每轮 agent step 开始时调用，以吸收 mid-turn inject 等外部 durable）。
    pub(crate) async fn invalidate_model_cache(&self) {
        *self.model_cache.lock().await = None;
    }

    /// 在 bypass 本 publisher 的 durable 写入后（如 compaction persist）从 store 重载投影。
    pub(crate) async fn reload_model_cache(&self) -> Result<(), TurnError> {
        let model = self.session.read_model().await?;
        *self.model_cache.lock().await = Some(model);
        Ok(())
    }

    /// 返回当前投影快照；优先使用 turn 内缓存，否则从 store 加载一次。
    pub(crate) async fn snapshot_model(&self) -> Result<SessionReadModel, TurnError> {
        let cached = {
            let guard = self.model_cache.lock().await;
            guard.as_ref().cloned()
        };
        if let Some(model) = cached {
            return Ok(model);
        }
        let model = self.session.read_model().await?;
        let mut cache = self.model_cache.lock().await;
        if let Some(cached) = cache.as_ref() {
            return Ok(cached.clone());
        }
        *cache = Some(model.clone());
        Ok(model)
    }

    pub(crate) async fn durable(&self, payload: EventPayload) -> Result<(), TurnError> {
        let stored = match self
            .session
            .emit_durable(Some(&self.turn_id), payload)
            .await
        {
            Ok(event) => event,
            Err(error) => {
                self.live_error(-32603, error.to_string(), false).await;
                return Err(error.into());
            },
        };
        let should_reduce = {
            let cache = self.model_cache.lock().await;
            cache.is_some()
        };
        if should_reduce {
            let mut cache = self.model_cache.lock().await;
            if let Some(model) = cache.as_mut() {
                projection::reduce(&stored, model);
            }
            return Ok(());
        }
        let model = self.session.read_model().await?;
        let mut cache = self.model_cache.lock().await;
        match cache.as_mut() {
            Some(cached) => projection::reduce(&stored, cached),
            None => *cache = Some(model),
        }
        Ok(())
    }

    pub(crate) async fn live(&self, payload: EventPayload) {
        if let Some(tx) = self.live_tx.as_ref() {
            send_event(Some(tx), payload);
        } else {
            self.session.emit_live(Some(&self.turn_id), payload).await;
        }
    }

    /// 发送 live 错误事件，并标记 `emitted_error`（供 `drive_agent` 避免重复持久化）。
    pub(crate) async fn live_error(&self, code: i32, message: String, recoverable: bool) {
        self.emitted_error.store(true, Ordering::Relaxed);
        self.live(EventPayload::ErrorOccurred {
            code,
            message,
            recoverable,
        })
        .await;
    }
}

async fn durable_with_retry(publisher: &TurnEvents, payload: EventPayload) {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match publisher.durable(payload.clone()).await {
            Ok(()) => break,
            Err(error) if attempt < DURABLE_PUBLISH_MAX_ATTEMPTS => {
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
                    "turn event ingress durable publish failed after retries"
                );
                if let Err(reload_error) = publisher.reload_model_cache().await {
                    tracing::warn!(
                        error = %reload_error,
                        "failed to reload model cache after durable publish failure"
                    );
                }
                break;
            },
        }
    }
}

async fn dispatch_payload(publisher: &TurnEvents, payload: EventPayload) {
    if payload.is_durable() {
        durable_with_retry(publisher, payload).await;
    } else {
        publisher.live(payload).await;
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
    worker: tokio::task::JoinHandle<()>,
}

impl TurnEventIngress {
    pub(crate) fn start(publisher: Arc<TurnEvents>) -> (TurnEventSender, Self) {
        let (publish_tx, mut publish_rx) = mpsc::unbounded_channel::<EventPayload>();
        let (flush_tx, mut flush_rx) = mpsc::unbounded_channel::<oneshot::Sender<()>>();
        let sender = TurnEventSender {
            publish_tx,
            flush_tx,
        };
        let worker = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
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
        (sender, Self { worker })
    }

    pub(crate) async fn shutdown(self) {
        if let Err(error) = self.worker.await {
            tracing::error!(panic = %error, "turn event ingress worker panicked");
        }
    }
}

/// Turn 级扩展事件 ingress：在 `process_prompt` 期间为 hook / 工具提供 `event_tx`。
pub(crate) struct ExtensionEvents {
    ingress: TurnEventIngress,
    sender: Arc<TurnEventSender>,
}

impl ExtensionEvents {
    pub(crate) fn start(
        publisher: Arc<TurnEvents>,
        shared: &mut crate::turn_context::SharedTurnContext,
    ) -> Self {
        let (sender, ingress) = TurnEventIngress::start(publisher);
        let sender = Arc::new(sender);
        shared.turn_event_sender = Some(Arc::clone(&sender));
        Self { ingress, sender }
    }

    pub(crate) async fn shutdown(self, shared: &mut crate::turn_context::SharedTurnContext) {
        shared.turn_event_sender = None;
        drop(self.sender);
        self.ingress.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use astrcode_core::{
        config::{ContextSettings, EffectiveConfig, ExtensionSettings, LlmSettings, OpenAiApiMode},
        event::EventPayload,
        llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
        tool::ToolDefinition,
        types::{new_session_id, new_turn_id},
    };
    use astrcode_extensions::runner::ExtensionRunner;
    use astrcode_storage::in_memory::InMemoryEventStore;
    use tokio::sync::mpsc;

    use super::*;
    use crate::{
        session::{Session, SessionCreateParams},
        session_runtime::SessionRuntimeState,
        session_runtime_services::SessionRuntimeServices,
    };

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

    fn test_caps() -> Arc<SessionRuntimeServices> {
        let llm: Arc<dyn LlmProvider> = Arc::new(UnusedLlm);
        let extension_runner = Arc::new(ExtensionRunner::new(std::time::Duration::from_secs(1)));
        let context_assembler = Arc::new(
            astrcode_context::context_assembler::LlmContextAssembler::new(
                ContextSettings::default(),
            ),
        );
        let effective = EffectiveConfig {
            llm: LlmSettings {
                provider_kind: "mock".into(),
                base_url: String::new(),
                api_key: String::new(),
                api_mode: OpenAiApiMode::ChatCompletions,
                model_id: "mock-model".into(),
                max_tokens: 1024,
                context_limit: 1024,
                connect_timeout_secs: 1,
                read_timeout_secs: 1,
                max_retries: 0,
                retry_base_delay_ms: 0,
                supports_prompt_cache_key: false,
                prompt_cache_retention: None,
                reasoning: false,
                thinking_level: None,
            },
            small_llm: LlmSettings {
                provider_kind: "mock".into(),
                base_url: String::new(),
                api_key: String::new(),
                api_mode: OpenAiApiMode::ChatCompletions,
                model_id: "mock-model".into(),
                max_tokens: 1024,
                context_limit: 1024,
                connect_timeout_secs: 1,
                read_timeout_secs: 1,
                max_retries: 0,
                retry_base_delay_ms: 0,
                supports_prompt_cache_key: false,
                prompt_cache_retention: None,
                reasoning: false,
                thinking_level: None,
            },
            context: ContextSettings::default(),
            agent: astrcode_core::config::AgentSettings::default(),
            permissions: Default::default(),
            extensions: ExtensionSettings::default(),
        };
        Arc::new(SessionRuntimeServices::new(
            llm.clone(),
            llm,
            extension_runner,
            context_assembler,
            effective,
        ))
    }

    async fn test_session() -> Session {
        let store: Arc<dyn astrcode_core::storage::EventStore> =
            Arc::new(InMemoryEventStore::new());
        let caps = test_caps();
        let sid = new_session_id();
        let runtime = Arc::new(SessionRuntimeState::new(
            caps.llm(),
            caps.small_llm(),
            "mock-model".into(),
        ));
        let session = Session::create_with_params(SessionCreateParams {
            store,
            sid,
            working_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            model_id: "mock-model".into(),
            parent: None,
            tool_policy: None,
            source_extension: None,
            runtime,
            caps,
        })
        .await
        .unwrap();
        session.refresh_tools(".").await;
        session
    }

    #[tokio::test]
    async fn snapshot_model_uses_incremental_cache_after_durable() {
        let session = test_session().await;
        let turn_id = new_turn_id();
        let publisher = TurnEvents::new(session.clone(), turn_id, None);
        publisher
            .durable(EventPayload::UserMessage {
                message_id: astrcode_core::types::new_message_id(),
                text: "first".into(),
            })
            .await
            .unwrap();
        let snap = publisher.snapshot_model().await.unwrap();
        assert_eq!(snap.messages.len(), 1);

        publisher
            .durable(EventPayload::UserMessage {
                message_id: astrcode_core::types::new_message_id(),
                text: "second".into(),
            })
            .await
            .unwrap();
        let snap = publisher.snapshot_model().await.unwrap();
        assert_eq!(snap.messages.len(), 2);
    }

    #[tokio::test]
    async fn durable_emit_updates_read_model() {
        let session = test_session().await;
        let turn_id = new_turn_id();
        let publisher = TurnEvents::new(session.clone(), turn_id.clone(), None);
        publisher
            .durable(EventPayload::UserMessage {
                message_id: astrcode_core::types::new_message_id(),
                text: "injected".into(),
            })
            .await
            .unwrap();

        let model = session.read_model().await.unwrap();
        assert!(model.messages.iter().any(|message| {
            message.message.role == astrcode_core::llm::LlmRole::User
                && message.message.content.iter().any(|content| {
                    matches!(
                        content,
                        astrcode_core::llm::LlmContent::Text { text } if text == "injected"
                    )
                })
        }));
    }

    use astrcode_core::extension::{
        Extension, ExtensionCapability, HookMode, PreToolUseContext, PreToolUseHandler,
        PreToolUseResult, Registrar,
    };

    struct EmitProbeExtension;

    impl Extension for EmitProbeExtension {
        fn id(&self) -> &str {
            "emit-probe"
        }

        fn capabilities(&self) -> &[ExtensionCapability] {
            &[ExtensionCapability::EmitEvents]
        }

        fn register(&self, reg: &mut Registrar) {
            reg.extension_event("emit.probe").register();
            reg.on_pre_tool_use(HookMode::Blocking, 0, Arc::new(EmitOnPreToolUse));
        }
    }

    struct EmitOnPreToolUse;

    #[async_trait::async_trait]
    impl PreToolUseHandler for EmitOnPreToolUse {
        async fn handle(
            &self,
            ctx: PreToolUseContext,
        ) -> Result<PreToolUseResult, astrcode_core::extension::ExtensionError> {
            let sink = ctx.extension_event_sink.as_ref().ok_or_else(|| {
                astrcode_core::extension::ExtensionError::Internal("no extension_event_sink".into())
            })?;
            sink.emit("emit.probe", 1, serde_json::json!({ "probe": true }))
                .await?;
            Ok(PreToolUseResult::Allow)
        }
    }

    #[tokio::test]
    async fn extension_event_bridge_delivers_hook_emit_to_store() {
        let session = test_session().await;
        let caps = session.caps();
        caps.extension_runner()
            .register(Arc::new(EmitProbeExtension))
            .await
            .unwrap();

        let turn_id = new_turn_id();
        let publisher = Arc::new(TurnEvents::new(session.clone(), turn_id.clone(), None));
        let model = session.read_model().await.unwrap();
        let mut shared =
            crate::turn_context::SharedTurnContext::from_read_model(session.id(), &model);
        let bridge = ExtensionEvents::start(Arc::clone(&publisher), &mut shared);

        let ctx = PreToolUseContext {
            session_id: session.id().to_string(),
            working_dir: shared.working_dir.clone(),
            model: shared.model_selection(),
            tool_name: "any".into(),
            tool_input: serde_json::json!({}),
            approval_mode: shared.approval_mode,
            available_tools: vec![],
            event_tx: shared.turn_event_tx(),
            extension_event_sink: None,
            session_store_dir: None,
        };
        caps.extension_runner()
            .emit_pre_tool_use(ctx)
            .await
            .unwrap();

        bridge.shutdown(&mut shared).await;

        let events = session.store.replay_events(session.id()).await.unwrap();
        assert!(events.iter().any(|e| matches!(
            &e.payload,
            EventPayload::ExtensionEvent {
                extension_id,
                event_type,
                ..
            } if extension_id == "emit-probe" && event_type == "emit.probe"
        )));
    }

    /// 模拟并行工具经同一 ingress 发 durable 并 flush。
    #[tokio::test]
    async fn parallel_tool_senders_flush_through_single_ingress_without_deadlock() {
        use std::time::Duration;

        use astrcode_core::types::new_message_id;

        let session = test_session().await;
        let publisher = Arc::new(TurnEvents::new(
            session.clone(),
            new_turn_id(),
            None,
        ));
        publisher.invalidate_model_cache().await;

        let (sender, ingress) = TurnEventIngress::start(Arc::clone(&publisher));
        let sender = Arc::new(sender);

        let mut workers = Vec::new();
        for index in 0..8 {
            let sender = Arc::clone(&sender);
            workers.push(tokio::spawn(async move {
                let tx = sender.event_tx();
                tx.send(EventPayload::UserMessage {
                    message_id: new_message_id(),
                    text: format!("parallel-{index}"),
                })
                .unwrap();
                sender.flush().await;
            }));
        }

        for worker in workers {
            worker.await.unwrap();
        }
        drop(sender);
        tokio::time::timeout(Duration::from_secs(5), ingress.shutdown())
            .await
            .expect("parallel ingress flush timed out (possible model_cache deadlock)");

        let events = session.store.replay_events(session.id()).await.unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.payload, EventPayload::UserMessage { .. }))
                .count(),
            8
        );
    }
}
