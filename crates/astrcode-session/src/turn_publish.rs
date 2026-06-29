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

    /// 统计 provider 可见的非合成 user 消息条数；优先读 turn 内 cache，避免 clone 整份读模型。
    ///
    /// cache 命中时假定计数已与投影一致：要么 turn 入口时 cache 为空（走 store），
    /// 要么本 turn 内 durable 事件已通过 [`projection::reduce`] 同步到 cache。
    /// 外部 bypass 写入后须先 [`Self::invalidate_model_cache`] 或 [`Self::reload_model_cache`]。
    pub(crate) async fn visible_user_message_count(&self) -> Result<usize, TurnError> {
        let cached_count = {
            let guard = self.model_cache.lock().await;
            guard
                .as_ref()
                .map(SessionReadModel::visible_user_message_count)
        };
        if let Some(count) = cached_count {
            return Ok(count);
        }
        Ok(self.session.visible_user_message_count().await?)
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
    use std::{collections::HashMap, sync::Arc};

    use astrcode_core::{
        config::{ContextSettings, EffectiveConfig, ExtensionSettings, LlmSettings, OpenAiApiMode},
        context::{
            CompactIfNeededOutcome, CompactMessagesOptions, CompactRequestFn,
            CompactSummaryRenderOptions, ContextAssembler, ContextPrepareInput,
            NoopPostCompactEnricher,
        },
        event::EventPayload,
        extension::{
            AfterToolResultsContext, AfterToolResultsResult, CompactContext, CompactEvent,
            CompactResult, ContinueAfterStopContext, ContinueAfterStopResult, ExtensionError,
            ExtensionEvent, LifecycleContext, PostToolUseContext, PostToolUseFailureContext,
            PostToolUseResult, PreToolUseContext, PreToolUseResult, PromptBuildContext,
            PromptContributions, ProviderContext, ProviderEvent, ProviderResult,
            UserMessageEnvelopeContext, UserMessageEnvelopeResult,
        },
        llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
        prompt::{PromptFileProvider, PromptFiles, PromptPlan, PromptProvider, SystemPromptInput},
        tool::{SessionOperations, Tool, ToolDefinition, ToolPromptMetadata},
        types::{new_session_id, new_turn_id},
    };
    use astrcode_kernel::{ExtensionRuntime, extension_runtime::NoopExtensionRuntime};
    use astrcode_storage::in_memory::InMemoryEventStore;
    use tokio::sync::mpsc;

    use super::*;
    use crate::{
        session::{Session, SessionCreateParams},
        session_runtime::SessionRuntimeState,
        session_runtime_services::{SessionHostServices, SessionRuntimeServices},
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

    struct TestContextAssembler {
        settings: ContextSettings,
    }

    #[async_trait::async_trait]
    impl ContextAssembler for TestContextAssembler {
        fn settings(&self) -> &ContextSettings {
            &self.settings
        }

        fn should_auto_compact(&self, _input: &ContextPrepareInput<'_>) -> bool {
            false
        }

        async fn compact_if_needed(
            &self,
            messages: Vec<LlmMessage>,
            _system_prompt: Option<&str>,
            _custom_instructions: &[String],
            _render_options: CompactSummaryRenderOptions,
            _options: CompactMessagesOptions,
            _request_text: CompactRequestFn,
        ) -> CompactIfNeededOutcome {
            CompactIfNeededOutcome::NotRun { messages }
        }
    }

    struct TestPromptProvider;

    #[async_trait::async_trait]
    impl PromptProvider for TestPromptProvider {
        async fn assemble(&self, input: SystemPromptInput) -> PromptPlan {
            PromptPlan::from_system_prompt(format!(
                "[Identity]\n  test host\n\n[Environment]\n  Working directory: {}",
                input.working_dir
            ))
        }
    }

    struct TestPromptFileProvider;

    #[async_trait::async_trait]
    impl PromptFileProvider for TestPromptFileProvider {
        async fn load(&self, _working_dir: &str, _include_agents_rules: bool) -> PromptFiles {
            PromptFiles::default()
        }
    }

    fn test_caps() -> Arc<SessionRuntimeServices> {
        test_caps_with_runtime(Arc::new(NoopExtensionRuntime))
    }

    fn test_caps_with_runtime(
        extension_runner: Arc<dyn ExtensionRuntime>,
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
                api_mode: OpenAiApiMode::ChatCompletions,
                model_id: "mock-model".into(),
                max_tokens: 1024,
                context_limit: 1024,
                connect_timeout_secs: 1,
                read_timeout_secs: 1,
                max_retries: 0,
                retry_base_delay_ms: 0,
                supports_prompt_cache_key: false,
                supports_stream_usage: false,
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
                supports_stream_usage: false,
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
            effective,
            SessionHostServices {
                extension_runner,
                context_assembler,
                post_compact_enricher: Arc::new(NoopPostCompactEnricher),
                prompt_provider: Arc::new(TestPromptProvider),
                prompt_file_provider: Arc::new(TestPromptFileProvider),
                tool_packs: Vec::new(),
            },
        ))
    }

    async fn test_session() -> Session {
        test_session_with_caps(test_caps()).await
    }

    async fn test_session_with_caps(caps: Arc<SessionRuntimeServices>) -> Session {
        let store: Arc<dyn astrcode_core::storage::EventStore> =
            Arc::new(InMemoryEventStore::new());
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
                attachments: vec![],
            })
            .await
            .unwrap();
        let snap = publisher.snapshot_model().await.unwrap();
        assert_eq!(snap.messages.len(), 1);

        publisher
            .durable(EventPayload::UserMessage {
                message_id: astrcode_core::types::new_message_id(),
                text: "second".into(),
                attachments: vec![],
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
                attachments: vec![],
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

    struct EmitEventRuntime;

    #[async_trait::async_trait]
    impl ExtensionRuntime for EmitEventRuntime {
        async fn emit_pre_tool_use(
            &self,
            ctx: PreToolUseContext,
        ) -> Result<PreToolUseResult, astrcode_core::extension::ExtensionError> {
            let tx = ctx
                .event_tx
                .ok_or_else(|| ExtensionError::Internal("no turn event sender".into()))?;
            tx.send(EventPayload::ExtensionEvent {
                extension_id: "emit-probe".into(),
                event_type: "emit.probe".into(),
                schema_version: 1,
                payload: serde_json::json!({ "probe": true }),
            })
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

        async fn collect_prompt_contributions(
            &self,
            _ctx: PromptBuildContext,
        ) -> Result<PromptContributions, ExtensionError> {
            Ok(PromptContributions::default())
        }

        async fn emit_compact(
            &self,
            _event: CompactEvent,
            _ctx: CompactContext,
        ) -> Result<CompactResult, ExtensionError> {
            Ok(CompactResult::Allow)
        }

        async fn emit_post_tool_use_failure(&self, _ctx: PostToolUseFailureContext) {}

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

        async fn emit_after_tool_results(
            &self,
            _ctx: AfterToolResultsContext,
        ) -> Result<AfterToolResultsResult, ExtensionError> {
            Ok(AfterToolResultsResult::Continue)
        }

        async fn emit_lifecycle(
            &self,
            _event: ExtensionEvent,
            _ctx: LifecycleContext,
        ) -> Result<(), ExtensionError> {
            Ok(())
        }

        async fn collect_tool_adapters(&self, _working_dir: &str) -> Vec<Arc<dyn Tool>> {
            Vec::new()
        }

        async fn collect_tool_prompt_metadata(&self) -> HashMap<String, ToolPromptMetadata> {
            HashMap::new()
        }

        fn session_ops(&self) -> Option<Arc<dyn SessionOperations>> {
            None
        }
    }

    #[tokio::test]
    async fn extension_event_bridge_delivers_hook_emit_to_store() {
        let session =
            test_session_with_caps(test_caps_with_runtime(Arc::new(EmitEventRuntime))).await;
        let caps = session.caps();

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
        let publisher = Arc::new(TurnEvents::new(session.clone(), new_turn_id(), None));
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
                    attachments: vec![],
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
