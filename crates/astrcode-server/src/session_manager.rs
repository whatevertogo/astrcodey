use std::{
    collections::HashMap,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use astrcode_core::{
    event::{Event, EventPayload},
    extension::ExtensionEvent,
    lifecycle::SessionResourceCleanup,
    storage::{EventStore, SessionReadModel, SessionSummary, StorageError},
    types::{Cursor, SessionId},
};
use astrcode_session::{
    Session, SessionError, SessionRuntimeServices, SessionRuntimeState,
    session::emit_lifecycle_for_read_model,
};
use parking_lot::Mutex;

use crate::{config_manager::ConfigManager, server_event_bus::ServerEventBus};

pub(crate) struct CreatedSession {
    pub(crate) session: Session,
    pub(crate) start_event: Event,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionManagerError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Extension(#[from] astrcode_core::extension::ExtensionError),
    #[error("session created but no events found")]
    MissingStartEvent,
    #[error("invalid fork cursor: {0}")]
    InvalidCursor(String),
}

/// Session durable 生命周期门面（create/open/delete/fork）与 per-session runtime 唯一性。
///
/// 不处理 active turn、输入队列或 child completion——那些由 [`crate::turn_scheduler`]
/// 与 [`crate::child_session`] 负责。
pub struct SessionManager {
    event_store: Arc<dyn EventStore>,
    config: Arc<ConfigManager>,
    runtime_registry: SessionRuntimeRegistry,
    capabilities: Arc<SessionRuntimeServices>,
    event_bus: OnceLock<Arc<ServerEventBus>>,
    resource_cleanups: Vec<Arc<dyn SessionResourceCleanup>>,
}

impl SessionManager {
    // ─── 生命周期 ─────────────────────────────────────────────────────

    pub fn new(
        event_store: Arc<dyn EventStore>,
        config: Arc<ConfigManager>,
        capabilities: Arc<SessionRuntimeServices>,
        resource_cleanups: Vec<Arc<dyn SessionResourceCleanup>>,
    ) -> Self {
        Self {
            event_store,
            config,
            runtime_registry: SessionRuntimeRegistry::default(),
            capabilities,
            event_bus: OnceLock::new(),
            resource_cleanups,
        }
    }

    /// 绑定事件总线（含 internal reactor）。create/fork/open 时 attach，delete/recycle 时 detach。
    pub fn bind_event_bus(&self, event_bus: Arc<ServerEventBus>) {
        let _ = self.event_bus.set(event_bus);
    }

    fn attach_session_subscribers(&self, session: &Session) {
        if let Some(bus) = self.event_bus.get() {
            bus.attach(session);
        }
    }

    fn detach_session_subscribers(&self, session_id: &SessionId) {
        if let Some(bus) = self.event_bus.get() {
            bus.detach(session_id);
        }
    }

    fn get_or_create_runtime(&self, session_id: &SessionId) -> Arc<SessionRuntimeState> {
        self.runtime_registry
            .get_or_create(session_id, || self.new_runtime_state())
    }

    fn runtime_for_open(&self, session_id: &SessionId) -> RuntimeForOpen {
        self.runtime_registry
            .runtime_for_open(session_id, || self.new_runtime_state())
    }

    fn new_runtime_state(&self) -> Arc<SessionRuntimeState> {
        let model_id = self.config.read_effective().llm.model_id.clone();
        Arc::new(SessionRuntimeState::new(
            self.capabilities.llm(),
            self.capabilities.small_llm(),
            model_id,
        ))
    }

    pub(crate) fn config(&self) -> &Arc<ConfigManager> {
        &self.config
    }

    /// 把子会话的 runtime 注册到 manager。
    ///
    /// 子会话由 `Session::spawn_child` 创建，其 runtime 不经过 `get_or_create_runtime`，
    /// 必须手动注册才能让后续 `open(child_sid)` 拿到同一个 runtime（共享广播通道）。
    /// event_bus 的 attach 由 TurnScheduler 在 submit 时统一处理。
    pub(crate) fn register_child_session(&self, session: &Session) {
        let sid = session.id().clone();
        let runtime = session.runtime_arc();
        self.runtime_registry.insert(sid, runtime);
    }

    /// 让所有已打开 session 的工具快照失效；下一次 turn 会按当前扩展集重建。
    pub(crate) fn invalidate_tool_registries(&self) {
        self.runtime_registry.invalidate_tool_registries();
    }

    pub(crate) async fn create(
        &self,
        working_dir: &str,
    ) -> Result<CreatedSession, SessionManagerError> {
        let model_id = self.config.read_effective().llm.model_id.clone();
        // 先在 registry 里登记 runtime，再创建 Session 让两者共享同一份。
        let sid = astrcode_core::types::new_session_id();
        let runtime = self.get_or_create_runtime(&sid);
        // SessionManager 调用 Session::create_with_id 而非 create_full：因为 sid 已生成。
        let session = Session::create_with_id(
            Arc::clone(&self.event_store),
            sid.clone(),
            working_dir,
            &model_id,
            None,
            None,
            None,
            runtime,
            Arc::clone(&self.capabilities),
        )
        .await?;

        self.attach_session_subscribers(&session);

        let start_event = self
            .event_store
            .replay_events(&sid)
            .await?
            .into_iter()
            .next()
            .ok_or(SessionManagerError::MissingStartEvent)?;

        session.emit_lifecycle(ExtensionEvent::SessionStart).await?;

        Ok(CreatedSession {
            session,
            start_event,
        })
    }

    pub(crate) async fn open(&self, session_id: SessionId) -> Result<Session, SessionManagerError> {
        loop {
            match self.runtime_for_open(&session_id) {
                RuntimeForOpen::Ready(runtime) => {
                    let session = Session::open(
                        Arc::clone(&self.event_store),
                        session_id.clone(),
                        runtime,
                        Arc::clone(&self.capabilities),
                    )
                    .await?;
                    self.attach_session_subscribers(&session);
                    return Ok(session);
                },
                RuntimeForOpen::Resuming(pending) => {
                    pending.wait().await;
                },
                RuntimeForOpen::Started(runtime) => {
                    let resume = SessionResumeGuard::new(
                        &self.runtime_registry,
                        session_id.clone(),
                        Arc::clone(&runtime),
                    );
                    let session = Session::open(
                        Arc::clone(&self.event_store),
                        session_id.clone(),
                        runtime,
                        Arc::clone(&self.capabilities),
                    )
                    .await?;
                    session
                        .emit_lifecycle(ExtensionEvent::SessionResume)
                        .await?;
                    resume.complete();
                    self.attach_session_subscribers(&session);
                    return Ok(session);
                },
            }
        }
    }

    pub(crate) async fn delete(&self, session_id: &SessionId) -> Result<(), SessionManagerError> {
        self.emit_session_shutdown(session_id).await?;
        self.event_store.delete_session(session_id).await?;
        self.cleanup_session_resources(session_id);
        // 清理本 session 关联的持久化终端。
        // 已通过 SessionResourceCleanup trait 注入，见 TerminalCleanup。
        Ok(())
    }

    async fn emit_session_shutdown(
        &self,
        session_id: &SessionId,
    ) -> Result<(), SessionManagerError> {
        let model = self.event_store.session_read_model(session_id).await?;
        emit_lifecycle_for_read_model(
            &self.capabilities,
            session_id,
            &model,
            ExtensionEvent::SessionShutdown,
        )
        .await
        .map_err(SessionManagerError::from)
    }

    /// 释放 session 占用的进程内资源。
    ///
    /// delete 和 recycle 共享同一套清理流程，确保两条路径不会出现遗漏。
    fn cleanup_session_resources(&self, session_id: &SessionId) {
        self.runtime_registry.cleanup_runtime(session_id);
        self.detach_session_subscribers(session_id);
        // 外部资源清理（trait 注入）。
        for cleanup in &self.resource_cleanups {
            cleanup.cleanup(session_id);
        }
    }

    // ─── 只读查询 ─────────────────────────────────────────────────────

    pub(crate) async fn read_model(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionReadModel, SessionManagerError> {
        self.event_store
            .session_read_model(session_id)
            .await
            .map_err(SessionManagerError::from)
    }

    pub(crate) async fn has_messages(
        &self,
        session_id: &SessionId,
    ) -> Result<bool, SessionManagerError> {
        self.event_store
            .session_has_messages(session_id)
            .await
            .map_err(SessionManagerError::from)
    }

    pub(crate) async fn agent_sessions(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<astrcode_core::storage::AgentSessionLinkView>, SessionManagerError> {
        self.event_store
            .session_agent_sessions(session_id)
            .await
            .map_err(SessionManagerError::from)
    }

    pub(crate) async fn list_summaries(&self) -> Result<Vec<SessionSummary>, SessionManagerError> {
        self.event_store
            .list_session_summaries()
            .await
            .map_err(SessionManagerError::from)
    }

    pub(crate) async fn replay_from_limited(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
        max_events: usize,
    ) -> Result<Vec<Event>, SessionManagerError> {
        self.event_store
            .replay_from_limited(session_id, cursor, max_events)
            .await
            .map_err(SessionManagerError::from)
    }

    pub(crate) async fn latest_cursor(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Cursor>, SessionManagerError> {
        self.event_store
            .latest_cursor(session_id)
            .await
            .map_err(SessionManagerError::from)
    }

    pub(crate) async fn session_store_dir(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<std::path::PathBuf>, SessionManagerError> {
        self.event_store
            .session_store_dir(session_id)
            .await
            .map_err(SessionManagerError::from)
    }

    /// 强制 fsync 指定会话的 durable event log。
    pub(crate) async fn sync_durable_events(&self, session_id: &SessionId) {
        if let Err(e) = self.event_store.sync_durable_events(session_id).await {
            tracing::error!(session_id = %session_id, error = %e, "failed to sync durable events");
        }
    }

    /// 将全局 caps 中的 provider / model_id 同步到本进程内所有已打开的 session runtime。
    ///
    /// 配置热更新只改 `SessionRuntimeServices`；调用方在 `apply_raw_config_and_rebuild`
    /// 之后必须调用此方法，否则非 active session 的 turn 仍会用旧的 per-session binding。
    pub(crate) fn sync_all_model_bindings_from_config(&self) {
        let effective = self.config.read_effective();
        self.runtime_registry.sync_model_bindings(
            self.capabilities.llm(),
            self.capabilities.small_llm(),
            effective.llm.model_id.clone(),
        );
    }

    pub(crate) async fn recycle_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), SessionManagerError> {
        self.emit_session_shutdown(session_id).await?;
        self.event_store
            .recycle_session(session_id)
            .await
            .map_err(SessionManagerError::from)?;
        self.cleanup_session_resources(session_id);
        Ok(())
    }

    /// 从 .recycled/ 恢复一个已回收的 session。
    pub(crate) async fn restore_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), SessionManagerError> {
        self.event_store
            .restore_session(session_id)
            .await
            .map_err(SessionManagerError::from)
    }

    /// Fork 一个已有会话，创建新 session 并复制 fork 点之前的消息前缀。
    ///
    /// fork 保证新 session 发送给 LLM 的 system prompt + 消息前缀与源 session 完全一致，
    /// 从而让 provider 侧的 KV 缓存（prompt cache）自动命中。
    ///
    /// - `source_id`: 源会话 ID
    /// - `at_cursor`: 可选 fork 点 cursor（event seq 的十进制字符串），为 None 则从末尾 fork
    ///
    /// 返回新 session 及其初始事件。
    pub(crate) async fn fork(
        &self,
        source_id: &SessionId,
        at_cursor: Option<&Cursor>,
    ) -> Result<Session, SessionManagerError> {
        let source_model = self.event_store.session_read_model(source_id).await?;

        let fork_cursor = at_cursor.cloned().unwrap_or_else(|| source_model.cursor());

        let (context_messages, retained_messages) = if at_cursor.is_some() {
            let events = self.event_store.replay_events(source_id).await?;
            let truncated_seq: u64 = fork_cursor
                .parse()
                .map_err(|_| SessionManagerError::InvalidCursor(fork_cursor.clone()))?;
            let truncated_events: Vec<_> = events
                .into_iter()
                .filter(|e| e.seq.unwrap_or(0) <= truncated_seq)
                .collect();
            let truncated_model =
                astrcode_storage::projection::replay(source_id.clone(), &truncated_events);
            (truncated_model.context_messages, truncated_model.messages)
        } else {
            (source_model.context_messages, source_model.messages)
        };

        let model_id = self.config.read_effective().llm.model_id.clone();
        let new_sid = astrcode_core::types::new_session_id();
        let runtime = self.get_or_create_runtime(&new_sid);
        let session = Session::create_with_id(
            Arc::clone(&self.event_store),
            new_sid.clone(),
            &source_model.working_dir,
            &model_id,
            None,
            None,
            None,
            runtime,
            Arc::clone(&self.capabilities),
        )
        .await?;

        self.attach_session_subscribers(&session);

        session
            .append_event(Event::new(
                new_sid.clone(),
                None,
                EventPayload::SessionForked {
                    source_session_id: source_id.clone(),
                    source_cursor: fork_cursor,
                    context_messages: context_messages.into_iter().map(|m| m.message).collect(),
                    retained_messages: retained_messages.into_iter().map(|m| m.message).collect(),
                },
            ))
            .await?;

        if let (Some(text), Some(fingerprint)) = (
            &source_model.system_prompt,
            &source_model.system_prompt_fingerprint,
        ) {
            session
                .append_event(Event::new(
                    new_sid.clone(),
                    None,
                    EventPayload::SystemPromptConfigured {
                        text: text.clone(),
                        fingerprint: fingerprint.clone(),
                        extra_system_prompt: source_model.extra_system_prompt.clone(),
                    },
                ))
                .await?;
        }

        Ok(session)
    }
}

/// 首次 cold open 的 SessionResume 完成前，后续 open 需在此 gate 上等待。
#[derive(Default)]
struct PendingSessionResume {
    done: AtomicBool,
    notify: tokio::sync::Notify,
}

impl PendingSessionResume {
    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.done.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    fn finish(&self) {
        self.done.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

enum SessionRuntimeEntry {
    Ready(Arc<SessionRuntimeState>),
    Resuming {
        runtime: Arc<SessionRuntimeState>,
        pending: Arc<PendingSessionResume>,
    },
}

impl SessionRuntimeEntry {
    fn runtime(&self) -> &Arc<SessionRuntimeState> {
        match self {
            Self::Ready(runtime) | Self::Resuming { runtime, .. } => runtime,
        }
    }
}

enum RuntimeForOpen {
    Ready(Arc<SessionRuntimeState>),
    Resuming(Arc<PendingSessionResume>),
    Started(Arc<SessionRuntimeState>),
}

/// 保证同一 `SessionId` 在当前进程里只有一份 local runtime state。
#[derive(Default)]
struct SessionRuntimeRegistry {
    states: Mutex<HashMap<SessionId, SessionRuntimeEntry>>,
}

impl SessionRuntimeRegistry {
    fn get_or_create(
        &self,
        session_id: &SessionId,
        create: impl FnOnce() -> Arc<SessionRuntimeState>,
    ) -> Arc<SessionRuntimeState> {
        let mut states = self.states.lock();
        if let Some(entry) = states.get(session_id) {
            return Arc::clone(entry.runtime());
        }
        let runtime = create();
        states.insert(
            session_id.clone(),
            SessionRuntimeEntry::Ready(Arc::clone(&runtime)),
        );
        runtime
    }

    fn runtime_for_open(
        &self,
        session_id: &SessionId,
        create: impl FnOnce() -> Arc<SessionRuntimeState>,
    ) -> RuntimeForOpen {
        let mut states = self.states.lock();
        match states.get(session_id) {
            Some(SessionRuntimeEntry::Ready(runtime)) => RuntimeForOpen::Ready(Arc::clone(runtime)),
            Some(SessionRuntimeEntry::Resuming { pending, .. }) => {
                RuntimeForOpen::Resuming(Arc::clone(pending))
            },
            None => {
                let runtime = create();
                states.insert(
                    session_id.clone(),
                    SessionRuntimeEntry::Resuming {
                        runtime: Arc::clone(&runtime),
                        pending: Arc::default(),
                    },
                );
                RuntimeForOpen::Started(runtime)
            },
        }
    }

    fn insert(&self, session_id: SessionId, runtime: Arc<SessionRuntimeState>) {
        self.states
            .lock()
            .insert(session_id, SessionRuntimeEntry::Ready(runtime));
    }

    fn complete_session_resume(&self, session_id: &SessionId, expected: &Arc<SessionRuntimeState>) {
        let mut states = self.states.lock();
        let transition = match states.get(session_id) {
            Some(SessionRuntimeEntry::Resuming { runtime, pending })
                if Arc::ptr_eq(runtime, expected) =>
            {
                Some((Arc::clone(runtime), Arc::clone(pending)))
            },
            _ => None,
        };
        if let Some((runtime, pending)) = transition {
            states.insert(session_id.clone(), SessionRuntimeEntry::Ready(runtime));
            drop(states);
            pending.finish();
        }
    }

    fn fail_session_resume(&self, session_id: &SessionId, expected: &Arc<SessionRuntimeState>) {
        let mut states = self.states.lock();
        let pending = match states.get(session_id) {
            Some(SessionRuntimeEntry::Resuming { runtime, pending })
                if Arc::ptr_eq(runtime, expected) =>
            {
                Some(Arc::clone(pending))
            },
            _ => None,
        };
        if let Some(pending) = pending {
            states.remove(session_id);
            drop(states);
            pending.finish();
        }
    }

    fn invalidate_tool_registries(&self) {
        for entry in self.states.lock().values() {
            entry.runtime().reset_tool_registry();
        }
    }

    fn sync_model_bindings(
        &self,
        llm: Arc<dyn astrcode_core::llm::LlmProvider>,
        small_llm: Arc<dyn astrcode_core::llm::LlmProvider>,
        model_id: String,
    ) {
        for entry in self.states.lock().values() {
            entry.runtime().replace_model_binding(
                Arc::clone(&llm),
                Arc::clone(&small_llm),
                model_id.clone(),
            );
        }
    }

    fn cleanup_runtime(&self, session_id: &SessionId) {
        let removed = self.states.lock().remove(session_id);
        if let Some(SessionRuntimeEntry::Resuming { pending, .. }) = removed {
            pending.finish();
        }
    }
}

struct SessionResumeGuard<'a> {
    registry: &'a SessionRuntimeRegistry,
    session_id: SessionId,
    runtime: Arc<SessionRuntimeState>,
    completed: bool,
}

impl<'a> SessionResumeGuard<'a> {
    fn new(
        registry: &'a SessionRuntimeRegistry,
        session_id: SessionId,
        runtime: Arc<SessionRuntimeState>,
    ) -> Self {
        Self {
            registry,
            session_id,
            runtime,
            completed: false,
        }
    }

    fn complete(mut self) {
        self.registry
            .complete_session_resume(&self.session_id, &self.runtime);
        self.completed = true;
    }
}

impl Drop for SessionResumeGuard<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.registry
                .fail_session_resume(&self.session_id, &self.runtime);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc, time::Duration};

    use astrcode_core::{
        extension::{Extension, Registrar, ToolHandler},
        tool::{ExecutionMode, ToolDefinition, ToolOrigin, ToolResult},
    };
    use astrcode_extensions::runner::ExtensionRunner;
    use astrcode_session::session_setup::{
        SystemPromptSnapshotInput, build_system_prompt_snapshot, build_tool_registry_snapshot,
    };

    struct StaticToolExtension {
        id: &'static str,
        tool_name: &'static str,
        description: &'static str,
    }

    #[async_trait::async_trait]
    impl Extension for StaticToolExtension {
        fn id(&self) -> &str {
            self.id
        }

        fn register(&self, reg: &mut Registrar) {
            reg.tool(
                ToolDefinition {
                    name: self.tool_name.into(),
                    description: self.description.into(),
                    parameters: serde_json::json!({"type": "object"}),
                    origin: ToolOrigin::Extension,
                    execution_mode: ExecutionMode::Sequential,
                },
                Arc::new(StaticToolHandler),
            );
        }
    }

    struct StaticToolHandler;

    #[async_trait::async_trait]
    impl ToolHandler for StaticToolHandler {
        async fn execute(
            &self,
            tool_name: &str,
            _arguments: serde_json::Value,
            _working_dir: &str,
            _ctx: &astrcode_core::tool::ToolExecutionContext,
        ) -> Result<ToolResult, astrcode_core::extension::ExtensionError> {
            Err(astrcode_core::extension::ExtensionError::NotFound(
                tool_name.into(),
            ))
        }
    }

    #[tokio::test]
    async fn child_extra_system_prompt_participates_in_snapshot_build() {
        let runner = ExtensionRunner::new(Duration::from_secs(1));
        let (system_prompt, fingerprint) =
            build_system_prompt_snapshot(SystemPromptSnapshotInput {
                extension_runner: &runner,
                prompt_provider: &astrcode_context::prompt_engine::DefaultPromptProvider,
                prompt_file_provider: &astrcode_context::prompt_engine::DefaultPromptFileProvider,
                session_id: "session-1",
                working_dir: ".",
                model_id: "mock",
                tools: &[],
                extra_system_prompt: Some("child body"),
                tool_prompt_metadata: HashMap::new(),
                include_agents_rules: true,
            })
            .await
            .unwrap();

        assert!(system_prompt.contains("child body"));
        assert!(!fingerprint.is_empty());
    }

    #[tokio::test]
    async fn tool_snapshot_precedence_is_explicit() {
        let runner = ExtensionRunner::new(Duration::from_secs(1));
        runner
            .register(Arc::new(StaticToolExtension {
                id: "first",
                tool_name: "shell",
                description: "first extension shell",
            }))
            .await
            .unwrap();
        runner
            .register(Arc::new(StaticToolExtension {
                id: "second",
                tool_name: "shell",
                description: "second extension shell",
            }))
            .await
            .unwrap();

        let default_tool_packs = astrcode_tools::registry::default_tool_packs();
        let registry = build_tool_registry_snapshot(&runner, &default_tool_packs, ".", None).await;
        let shell = registry.find_definition("shell").unwrap();

        assert_eq!(shell.origin, ToolOrigin::Extension);
        assert_eq!(shell.description, "first extension shell");
    }
}
