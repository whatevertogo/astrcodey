//! Turn 内事件发布 — durable 同步写入 store，live 直发 fanout。
//!
//! 已打开 session 的 `read_model()` 克隆内存投影（非全量 replay）。本模块在 turn 内
//! 用 [`astrcode_storage::projection::reduce`] 增量更新缓存，避免每步 tool commit / prepare
//! 重复克隆整份 [`SessionReadModel`]。

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use astrcode_core::{event::EventPayload, storage::SessionReadModel, types::TurnId};
use astrcode_storage::projection;
use tokio::sync::{Mutex, mpsc};

use crate::{
    session::Session,
    turn_context::{TurnError, TurnEventTx, send_event},
};

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
        *self.model_cache.lock().await = Some(self.session.read_model().await?);
        Ok(())
    }

    /// 返回当前投影快照；优先使用 turn 内缓存，否则从 store 加载一次。
    pub(crate) async fn snapshot_model(&self) -> Result<SessionReadModel, TurnError> {
        let mut cache = self.model_cache.lock().await;
        if cache.is_none() {
            *cache = Some(self.session.read_model().await?);
        }
        cache.clone().ok_or(TurnError::ModelCacheEmpty)
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
        let mut cache = self.model_cache.lock().await;
        match cache.as_mut() {
            Some(model) => projection::reduce(&stored, model),
            None => {
                *cache = Some(self.session.read_model().await?);
            },
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

/// 将扩展/钩子侧的 `event_tx.send` 转发到 [`TurnEvents`]（durable / live 由 payload 决定）。
pub(crate) fn spawn_event_bridge(
    publisher: Arc<TurnEvents>,
) -> (TurnEventTx, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<EventPayload>();
    let (durable_tx, mut durable_rx) = mpsc::unbounded_channel::<EventPayload>();
    let durable_publisher = Arc::clone(&publisher);
    let durable_worker = tokio::spawn(async move {
        while let Some(payload) = durable_rx.recv().await {
            if let Err(error) = durable_publisher.durable(payload).await {
                tracing::error!(error = %error, "event bridge durable publish failed");
            }
        }
    });
    let handle = tokio::spawn(async move {
        while let Some(payload) = rx.recv().await {
            // durable 写入（磁盘 I/O）不阻塞同 bridge 上的 live delta 转发，但保持顺序。
            if payload.is_durable() {
                if durable_tx.send(payload).is_err() {
                    tracing::error!("event bridge durable worker unavailable");
                }
            } else {
                publisher.live(payload).await;
            }
        }
        drop(durable_tx);
        let _ = durable_worker.await;
    });
    (tx, handle)
}

/// Turn 级扩展事件桥：在 `process_prompt` 期间为 hook 提供 `event_tx`。
pub(crate) struct ExtensionEvents {
    tx: TurnEventTx,
    handle: tokio::task::JoinHandle<()>,
}

impl ExtensionEvents {
    pub(crate) fn start(
        publisher: Arc<TurnEvents>,
        shared: &mut crate::turn_context::SharedTurnContext,
    ) -> Self {
        let (tx, handle) = spawn_event_bridge(publisher);
        shared.turn_event_tx = Some(tx.clone());
        Self { tx, handle }
    }

    pub(crate) async fn shutdown(self, shared: &mut crate::turn_context::SharedTurnContext) {
        shared.turn_event_tx = None;
        drop(self.tx);
        let _ = self.handle.await;
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

    #[tokio::test]
    async fn snapshot_model_uses_incremental_cache_after_durable() {
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
            store: Arc::clone(&store),
            sid: sid.clone(),
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
            store: Arc::clone(&store),
            sid: sid.clone(),
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
            store: Arc::clone(&store),
            sid: sid.clone(),
            working_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            model_id: "mock-model".into(),
            parent: None,
            tool_policy: None,
            source_extension: None,
            runtime,
            caps: Arc::clone(&caps),
        })
        .await
        .unwrap();
        session.refresh_tools(".").await;

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
            available_tools: vec![],
            event_tx: shared.turn_event_tx.clone(),
            extension_event_sink: None,
            session_store_dir: None,
        };
        caps.extension_runner()
            .emit_pre_tool_use(ctx)
            .await
            .unwrap();

        bridge.shutdown(&mut shared).await;

        let events = store.replay_events(session.id()).await.unwrap();
        assert!(events.iter().any(|e| matches!(
            &e.payload,
            EventPayload::ExtensionEvent {
                extension_id,
                event_type,
                ..
            } if extension_id == "emit-probe" && event_type == "emit.probe"
        )));
    }
}
