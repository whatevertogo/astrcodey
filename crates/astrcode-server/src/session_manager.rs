use std::{collections::HashMap, sync::Arc};

use astrcode_core::{
    event::{Event, EventPayload},
    extension::ExtensionEvent,
    storage::{EventStore, SessionReadModel, SessionSummary, StorageError},
    types::{Cursor, SessionId},
};
use astrcode_session::{Session, SessionError, SessionRuntimeServices, SessionRuntimeState};
use parking_lot::Mutex;

use crate::config_manager::ConfigManager;

/// 会话创建后的回调类型，用于在 session 注册到 manager 时自动执行副作用（如 attach 到 event_bus）。
pub type SessionAttachHook = Arc<dyn Fn(&Session) + Send + Sync>;

pub(crate) struct CreatedSession {
    pub(crate) session: Session,
    pub(crate) start_event: Event,
}

pub(crate) struct ForkedSession {
    pub(crate) session: Session,
    #[allow(dead_code)]
    pub(crate) start_event: Event,
    #[allow(dead_code)]
    pub(crate) fork_event: Event,
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
}

/// Server 侧的 session 生命周期门面。
///
/// durable session 仍由 [`Session`] / [`EventStore`] 负责；这里集中管理
/// 与 session 同生灭的进程内资源，避免 handler 逐项记忆清理细节。
///
/// 后台任务（`BackgroundTaskManager`）现在由 `SessionRuntimeState` 持有，
/// 跟着 session 走；SessionManager 不再持有全局副本。删除 session 时通过
/// `runtime_states` 找到对应 runtime 并清理它的 bg_tasks。
pub struct SessionManager {
    event_store: Arc<dyn EventStore>,
    config: Arc<ConfigManager>,
    runtime_states: Mutex<HashMap<SessionId, Arc<SessionRuntimeState>>>,
    capabilities: Arc<SessionRuntimeServices>,
    /// 可选的 attach 回调：子会话注册 runtime 后自动把 session 接入 event_bus 广播。
    attach_hook: Mutex<Option<SessionAttachHook>>,
}

impl SessionManager {
    // ─── 生命周期 ─────────────────────────────────────────────────────

    pub fn new(
        event_store: Arc<dyn EventStore>,
        config: Arc<ConfigManager>,
        capabilities: Arc<SessionRuntimeServices>,
    ) -> Self {
        Self {
            event_store,
            config,
            runtime_states: Mutex::new(HashMap::new()),
            capabilities,
            attach_hook: Mutex::new(None),
        }
    }

    fn get_or_create_runtime(&self, session_id: &SessionId) -> Arc<SessionRuntimeState> {
        self.runtime_states
            .lock()
            .entry(session_id.clone())
            .or_insert_with(|| Arc::new(SessionRuntimeState::default()))
            .clone()
    }

    pub(crate) fn config(&self) -> &Arc<ConfigManager> {
        &self.config
    }

    /// 设置 attach 回调。event_bus 创建后由调用方注入，子会话注册 runtime 时自动触发。
    pub fn set_attach_hook(&self, hook: SessionAttachHook) {
        *self.attach_hook.lock() = Some(hook);
    }

    /// 把子会话的 runtime 注册到 manager，并自动 attach 到 event_bus（如果 hook 已设置）。
    ///
    /// 子会话由 `Session::spawn_child` 创建，其 runtime 不经过 `get_or_create_runtime`，
    /// 必须手动注册才能让后续 `open(child_sid)` 拿到同一个 runtime（共享广播通道）。
    pub(crate) fn register_child_session(&self, session: &Session) {
        let sid = session.id().clone();
        let runtime = session.runtime().clone();
        self.runtime_states.lock().insert(sid, runtime);
        if let Some(hook) = self.attach_hook.lock().as_ref() {
            hook(session);
        }
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
        let runtime = self.get_or_create_runtime(&session_id);
        let session = Session::open(
            Arc::clone(&self.event_store),
            session_id,
            runtime,
            Arc::clone(&self.capabilities),
        )
        .await?;
        Ok(session)
    }

    pub(crate) async fn delete(&self, session_id: &SessionId) -> Result<(), SessionManagerError> {
        let model = self.event_store.session_read_model(session_id).await?;
        Session::emit_lifecycle_for_read_model(
            &self.capabilities,
            session_id,
            &model,
            ExtensionEvent::SessionShutdown,
        )
        .await?;
        self.event_store.delete_session(session_id).await?;
        // 清理本 session 的 runtime（含 bg_tasks）后从 registry 移除。
        if let Some(runtime) = self.runtime_states.lock().remove(session_id) {
            runtime
                .background_tasks()
                .lock()
                .cleanup_session(session_id);
        }
        Ok(())
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

    pub(crate) async fn list_summaries(&self) -> Result<Vec<SessionSummary>, SessionManagerError> {
        self.event_store
            .list_session_summaries()
            .await
            .map_err(SessionManagerError::from)
    }

    pub(crate) async fn replay_from(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<Vec<Event>, SessionManagerError> {
        self.event_store
            .replay_from(session_id, cursor)
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

    pub(crate) async fn recycle_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), SessionManagerError> {
        self.event_store
            .recycle_session(session_id)
            .await
            .map_err(SessionManagerError::from)?;
        // ephemeral 子会话回收后清理 runtime 占位，避免 HashMap 无限膨胀。
        if let Some(runtime) = self.runtime_states.lock().remove(session_id) {
            runtime
                .background_tasks()
                .lock()
                .cleanup_session(session_id);
        }
        Ok(())
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
    ) -> Result<ForkedSession, SessionManagerError> {
        // 1. 读源 session 的 read model
        let source_model = self.event_store.session_read_model(source_id).await?;

        // 2. 确定 fork 点 cursor
        let fork_cursor = match at_cursor {
            Some(cursor) => cursor.clone(),
            None => source_model.cursor(),
        };

        // 3. 计算 fork 点之前的 provider 消息 如果 at_cursor 为 None（从末尾 fork），直接用 read
        //    model 的消息。 如果指定了 cursor，需要从事件日志重放到指定点来获取消息。
        let (context_messages, retained_messages) = if at_cursor.is_some() {
            // 重放到指定 cursor 获取消息快照
            let events = self.event_store.replay_events(source_id).await?;
            let truncated_seq: u64 = fork_cursor.parse().map_err(|_| {
                SessionManagerError::Session(SessionError::Other(format!(
                    "invalid cursor: {fork_cursor}"
                )))
            })?;
            let truncated_events: Vec<_> = events
                .into_iter()
                .filter(|e| e.seq.unwrap_or(0) <= truncated_seq)
                .collect();
            let truncated_model =
                astrcode_storage::projection::replay(source_id.clone(), &truncated_events);
            (truncated_model.context_messages, truncated_model.messages)
        } else {
            (
                source_model.context_messages.clone(),
                source_model.messages.clone(),
            )
        };

        // 4. 创建新 session
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

        // 5. 写入 SessionForked 事件
        let fork_event = session
            .append_event(Event::new(
                new_sid.clone(),
                None,
                EventPayload::SessionForked {
                    source_session_id: source_id.clone(),
                    source_cursor: fork_cursor,
                    context_messages,
                    retained_messages,
                },
            ))
            .await?;

        // 6. 复制源 session 的 system prompt 配置到新 session（保证 KV 前缀一致）
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

        // 7. 读第一个事件作为 start_event 返回
        let start_event = self
            .event_store
            .replay_events(&new_sid)
            .await?
            .into_iter()
            .next()
            .ok_or(SessionManagerError::MissingStartEvent)?;

        Ok(ForkedSession {
            session,
            start_event,
            fork_event,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc, time::Duration};

    use astrcode_context::prompt_engine::load_system_prompt_files;
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
        let prompt_files = load_system_prompt_files(".").await;
        let (system_prompt, fingerprint) =
            build_system_prompt_snapshot(SystemPromptSnapshotInput {
                extension_runner: &runner,
                session_id: "session-1",
                working_dir: ".",
                model_id: "mock",
                tools: &[],
                extra_system_prompt: Some("child body"),
                tool_prompt_metadata: HashMap::new(),
                prompt_files,
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
            .await;
        runner
            .register(Arc::new(StaticToolExtension {
                id: "second",
                tool_name: "shell",
                description: "second extension shell",
            }))
            .await;

        let registry = build_tool_registry_snapshot(&runner, ".", 1, None).await;
        let shell = registry.find_definition("shell").unwrap();

        assert_eq!(shell.origin, ToolOrigin::Extension);
        assert_eq!(shell.description, "first extension shell");
    }
}
