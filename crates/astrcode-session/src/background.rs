//! 后台任务管理器与占位结果构造。
//!
//! 管理被自动后台化的工具调用（主要是长时间运行的 shell 命令）。
//! 提供注册、取消、查询和清理能力。

use std::{collections::HashMap, sync::Arc};

use astrcode_core::{
    event::EventPayload,
    tool::{BackgroundTaskReader, ToolResult},
    types::{BackgroundTaskId, SessionId, ToolCallId},
};
use parking_lot::Mutex;
use tokio::sync::mpsc;

/// 后台任务完成通知的载荷。
pub struct BackgroundTaskCompletion {
    pub session_id: SessionId,
    pub task_id: BackgroundTaskId,
    pub tool_name: String,
    pub result: ToolResult,
    pub arguments: String,
    pub arguments_json: Option<serde_json::Value>,
}

impl BackgroundTaskCompletion {
    /// 从完成通知派生 `ToolCallCompleted` 和 `BackgroundTaskCompleted` 事件载荷。
    pub fn to_tool_call_completed_event(&self) -> EventPayload {
        let call_id = ToolCallId::from(self.result.call_id.clone());
        EventPayload::ToolCallCompleted {
            call_id,
            tool_name: self.tool_name.clone(),
            result: self.result.clone(),
            arguments: self.arguments.clone(),
            arguments_json: self.arguments_json.clone(),
        }
    }

    pub fn to_background_task_completed_event(&self) -> EventPayload {
        EventPayload::BackgroundTaskCompleted {
            task_id: self.task_id.clone(),
            call_id: ToolCallId::from(self.result.call_id.clone()),
            tool_name: self.tool_name.clone(),
            result: self.result.clone(),
        }
    }
}

struct RunningTask {
    session_id: SessionId,
    exec_handle: tokio::task::JoinHandle<()>,
    watcher_handle: tokio::task::JoinHandle<()>,
}

/// 管理所有 session 的后台任务。
///
/// 当工具执行超过阈值时，agent loop 将其转入后台，把 exec 和 watcher 的 JoinHandle 注册到这里。
/// cancel 会同时 abort 工具执行和 watcher。完成后 watcher 自行移除任务。
pub struct BackgroundTaskManager {
    tasks: HashMap<BackgroundTaskId, RunningTask>,
}

impl BackgroundTaskManager {
    pub fn new() -> Self {
        Self {
            tasks: HashMap::new(),
        }
    }

    /// 注册一个后台任务。
    ///
    /// `task_id` 由调用方提前生成，保证与 ToolCallBackgrounded 事件和占位结果中的 ID 一致。
    pub fn register(
        &mut self,
        task_id: BackgroundTaskId,
        session_id: SessionId,
        exec_handle: tokio::task::JoinHandle<()>,
        watcher_handle: tokio::task::JoinHandle<()>,
    ) {
        self.tasks.insert(
            task_id,
            RunningTask {
                session_id,
                exec_handle,
                watcher_handle,
            },
        );
    }

    /// 移除已完成的任务（由 watcher 在完成后调用）。
    pub fn remove(&mut self, task_id: &BackgroundTaskId) {
        self.tasks.remove(task_id);
    }

    /// 取消并移除一个后台任务。
    ///
    /// 同时 abort 工具执行 task 和 watcher。
    pub fn cancel(&mut self, task_id: &BackgroundTaskId) -> bool {
        if let Some(task) = self.tasks.remove(task_id) {
            task.exec_handle.abort();
            task.watcher_handle.abort();
            true
        } else {
            false
        }
    }

    /// 清理指定 session 的所有后台任务（session 结束或删除时调用）。
    pub fn cleanup_session(&mut self, session_id: &SessionId) {
        let to_remove: Vec<BackgroundTaskId> = self
            .tasks
            .iter()
            .filter(|(_, task)| &task.session_id == session_id)
            .map(|(id, _)| id.clone())
            .collect();
        for task_id in to_remove {
            if let Some(task) = self.tasks.remove(&task_id) {
                task.exec_handle.abort();
                task.watcher_handle.abort();
            }
        }
    }

    /// 列出指定会话的所有活跃后台任务 ID。
    pub fn list_active(&self, session_id: &SessionId) -> Vec<BackgroundTaskId> {
        self.tasks
            .iter()
            .filter(|(_, task)| &task.session_id == session_id)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

/// 将 `BackgroundTaskManager` 适配为 `BackgroundTaskReader` trait。
///
/// 这个薄包装器让 `TaskTool` 能通过 `ToolExecutionContext` 读取后台任务状态，
/// 而不暴露 `BackgroundTaskManager` 的内部方法（如 `register`、`cleanup_session`）。
pub struct BackgroundTaskReaderImpl {
    manager: Arc<Mutex<BackgroundTaskManager>>,
}

impl BackgroundTaskReaderImpl {
    pub fn new(manager: Arc<Mutex<BackgroundTaskManager>>) -> Self {
        Self { manager }
    }
}

impl BackgroundTaskReader for BackgroundTaskReaderImpl {
    fn list_active(&self, session_id: &SessionId) -> Vec<BackgroundTaskId> {
        self.manager.lock().list_active(session_id)
    }

    fn cancel(&self, session_id: &SessionId, task_id: &BackgroundTaskId) -> bool {
        let mut mgr = self.manager.lock();
        if mgr
            .tasks
            .get(task_id)
            .is_some_and(|t| &t.session_id == session_id)
        {
            mgr.cancel(task_id)
        } else {
            false
        }
    }
}

impl Default for BackgroundTaskManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 完成后从管理器中移除任务。
pub fn complete_background_task(
    manager: &Arc<Mutex<BackgroundTaskManager>>,
    task_id: &BackgroundTaskId,
) {
    manager.lock().remove(task_id);
}

/// 后台任务完成事件的统一转发器。
///
/// 监听 `rx`，把每个 `BackgroundTaskCompletion` 翻译成
/// `(ToolCallCompleted, BackgroundTaskCompleted)` 两个事件，
/// 通过 `Session::emit` 写入（store + runtime 广播）。
///
/// 后台任务完成后的事件由 TurnScheduler 监听处理，无需回调唤醒 agent。
pub fn spawn_background_forwarder(
    mut rx: mpsc::UnboundedReceiver<BackgroundTaskCompletion>,
    session: Arc<crate::session::Session>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(completion) = rx.recv().await {
            let tool_call_event = completion.to_tool_call_completed_event();
            let bg_event = completion.to_background_task_completed_event();
            if let Err(e) = session.emit_durable(None, tool_call_event.clone()).await {
                tracing::warn!(session_id = %session.id(), error = %e, "background forwarder: persist tool_call_completed failed; sending live fallback");
                session.emit_live(None, tool_call_event).await;
            }
            session.emit_live(None, bg_event).await;
        }
    })
}

/// 为后台化的工具调用构造占位 `ToolResult`。
///
/// LLM 收到这个结果后会知道任务已在后台运行，可以继续其他推理。
pub fn backgrounded_placeholder_result(
    call_id: &str,
    task_id: &BackgroundTaskId,
    command: Option<&str>,
) -> ToolResult {
    let mut content = format!(
        "Task moved to background (task: {task_id}). The result will be available in the next \
         turn."
    );
    if let Some(cmd) = command {
        content = format!("{content} Command: {cmd}");
    }

    let mut meta = std::collections::BTreeMap::new();
    meta.insert("backgrounded".into(), serde_json::json!(true));
    meta.insert("task_id".into(), serde_json::json!(task_id.to_string()));

    ToolResult {
        call_id: call_id.to_string(),
        content,
        is_error: false,
        error: None,
        metadata: meta,
        duration_ms: None,
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        config::{EffectiveConfig, ExtensionSettings, LlmSettings, OpenAiApiMode},
        event::Event,
        llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
        storage::{EventReader, EventStore, SessionReadModel, SessionSummary, StorageError},
        tool::{ToolDefinition, ToolResult},
        types::Cursor,
    };
    use astrcode_extensions::runner::ExtensionRunner;
    use astrcode_storage::in_memory::InMemoryEventStore;

    use super::*;

    struct NeverLlm;

    #[async_trait::async_trait]
    impl LlmProvider for NeverLlm {
        async fn generate(
            &self,
            _messages: Vec<LlmMessage>,
            _tools: Vec<ToolDefinition>,
        ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
            std::future::pending().await
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: 1024,
                max_output_tokens: 1024,
            }
        }
    }

    struct FailToolCompletionStore {
        inner: InMemoryEventStore,
    }

    impl FailToolCompletionStore {
        fn new() -> Self {
            Self {
                inner: InMemoryEventStore::new(),
            }
        }
    }

    #[async_trait::async_trait]
    impl EventReader for FailToolCompletionStore {
        async fn replay_events(&self, session_id: &SessionId) -> Result<Vec<Event>, StorageError> {
            self.inner.replay_events(session_id).await
        }

        async fn session_read_model(
            &self,
            session_id: &SessionId,
        ) -> Result<SessionReadModel, StorageError> {
            self.inner.session_read_model(session_id).await
        }

        async fn session_system_prompt(
            &self,
            session_id: &SessionId,
        ) -> Result<Option<String>, StorageError> {
            self.inner.session_system_prompt(session_id).await
        }

        async fn list_session_summaries(&self) -> Result<Vec<SessionSummary>, StorageError> {
            self.inner.list_session_summaries().await
        }

        async fn latest_cursor(
            &self,
            session_id: &SessionId,
        ) -> Result<Option<Cursor>, StorageError> {
            self.inner.latest_cursor(session_id).await
        }

        async fn replay_from(
            &self,
            session_id: &SessionId,
            cursor: &Cursor,
        ) -> Result<Vec<Event>, StorageError> {
            self.inner.replay_from(session_id, cursor).await
        }

        async fn list_sessions(&self) -> Result<Vec<SessionId>, StorageError> {
            self.inner.list_sessions().await
        }

        async fn read_tool_result_artifact_by_path(
            &self,
            session_id: &SessionId,
            path: &str,
            char_offset: usize,
            max_chars: usize,
        ) -> Result<astrcode_core::storage::ToolResultArtifactSlice, StorageError> {
            self.inner
                .read_tool_result_artifact_by_path(session_id, path, char_offset, max_chars)
                .await
        }

        async fn session_store_dir(
            &self,
            session_id: &SessionId,
        ) -> Result<Option<std::path::PathBuf>, StorageError> {
            self.inner.session_store_dir(session_id).await
        }
    }

    #[async_trait::async_trait]
    impl EventStore for FailToolCompletionStore {
        async fn create_session(
            &self,
            session_id: &SessionId,
            working_dir: &str,
            model_id: &str,
            parent_session_id: Option<&SessionId>,
            tool_policy: Option<&astrcode_core::extension::ChildToolPolicy>,
            source_extension: Option<&str>,
        ) -> Result<Event, StorageError> {
            self.inner
                .create_session(
                    session_id,
                    working_dir,
                    model_id,
                    parent_session_id,
                    tool_policy,
                    source_extension,
                )
                .await
        }

        async fn append_event(&self, event: Event) -> Result<Event, StorageError> {
            if matches!(event.payload, EventPayload::ToolCallCompleted { .. }) {
                return Err(StorageError::Unsupported("forced append failure".into()));
            }
            self.inner.append_event(event).await
        }

        async fn checkpoint(
            &self,
            session_id: &SessionId,
            cursor: &Cursor,
        ) -> Result<(), StorageError> {
            self.inner.checkpoint(session_id, cursor).await
        }

        async fn delete_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
            self.inner.delete_session(session_id).await
        }
    }

    fn test_caps() -> Arc<crate::session_runtime_services::SessionRuntimeServices> {
        let llm: Arc<dyn LlmProvider> = Arc::new(NeverLlm);
        let extension_runner = Arc::new(ExtensionRunner::new(std::time::Duration::from_secs(1)));
        let context_assembler = Arc::new(
            astrcode_context::context_assembler::LlmContextAssembler::new(Default::default()),
        );
        Arc::new(
            crate::session_runtime_services::SessionRuntimeServices::new(
                Arc::clone(&llm),
                llm,
                extension_runner,
                context_assembler,
                EffectiveConfig {
                    llm: LlmSettings {
                        provider_kind: "mock".into(),
                        base_url: String::new(),
                        api_key: String::new(),
                        api_mode: OpenAiApiMode::ChatCompletions,
                        model_id: "mock".into(),
                        max_tokens: 1024,
                        context_limit: 1024,
                        connect_timeout_secs: 1,
                        read_timeout_secs: 1,
                        max_retries: 0,
                        retry_base_delay_ms: 0,
                        supports_prompt_cache_key: false,
                        prompt_cache_retention: None,
                        reasoning: false,
                        reasoning_split: false,
                    },
                    small_llm: LlmSettings {
                        provider_kind: "mock".into(),
                        base_url: String::new(),
                        api_key: String::new(),
                        api_mode: OpenAiApiMode::ChatCompletions,
                        model_id: "mock".into(),
                        max_tokens: 1024,
                        context_limit: 1024,
                        connect_timeout_secs: 1,
                        read_timeout_secs: 1,
                        max_retries: 0,
                        retry_base_delay_ms: 0,
                        supports_prompt_cache_key: false,
                        prompt_cache_retention: None,
                        reasoning: false,
                        reasoning_split: false,
                    },
                    context: Default::default(),
                    agent: Default::default(),
                    extensions: ExtensionSettings::default(),
                },
            ),
        )
    }

    fn fake_handles() -> (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>) {
        let exec = tokio::spawn(async {});
        let watcher = tokio::spawn(async {});
        (exec, watcher)
    }

    #[tokio::test]
    async fn cancel_removes_task_and_returns_true() {
        let mut mgr = BackgroundTaskManager::new();
        let task_id = BackgroundTaskId::from("task-1");
        let session_id = SessionId::from("session-1");
        let (exec, watcher) = fake_handles();
        mgr.register(task_id.clone(), session_id.clone(), exec, watcher);

        assert!(mgr.cancel(&task_id));
        assert!(mgr.list_active(&session_id).is_empty());
    }

    #[test]
    fn cancel_returns_false_for_unknown_task() {
        let mut mgr = BackgroundTaskManager::new();
        let task_id = BackgroundTaskId::from("nonexistent");
        assert!(!mgr.cancel(&task_id));
    }

    #[tokio::test]
    async fn cleanup_session_removes_all_tasks_for_session() {
        let mut mgr = BackgroundTaskManager::new();
        let session_a = SessionId::from("session-a");
        let session_b = SessionId::from("session-b");

        for i in 0..3 {
            let (exec, watcher) = fake_handles();
            mgr.register(
                BackgroundTaskId::from(format!("task-a-{i}")),
                session_a.clone(),
                exec,
                watcher,
            );
        }
        let (exec, watcher) = fake_handles();
        mgr.register(
            BackgroundTaskId::from("task-b-0"),
            session_b.clone(),
            exec,
            watcher,
        );

        mgr.cleanup_session(&session_a);
        assert!(mgr.list_active(&session_a).is_empty());
        assert_eq!(mgr.list_active(&session_b).len(), 1);
    }

    #[tokio::test]
    async fn list_active_returns_only_matching_session_tasks() {
        let mut mgr = BackgroundTaskManager::new();
        let session_1 = SessionId::from("s1");
        let session_2 = SessionId::from("s2");

        let (exec1, watcher1) = fake_handles();
        let (exec2, watcher2) = fake_handles();
        mgr.register(
            BackgroundTaskId::from("t1"),
            session_1.clone(),
            exec1,
            watcher1,
        );
        mgr.register(
            BackgroundTaskId::from("t2"),
            session_2.clone(),
            exec2,
            watcher2,
        );

        let active = mgr.list_active(&session_1);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0], BackgroundTaskId::from("t1"));
    }

    #[tokio::test]
    async fn forwarder_sends_live_tool_completion_when_durable_write_fails() {
        let store: Arc<dyn EventStore> = Arc::new(FailToolCompletionStore::new());
        let session_id = SessionId::from("session-forwarder-fallback");
        let runtime = Arc::new(crate::session_runtime::SessionRuntimeState::new(
            Arc::new(NeverLlm),
            Arc::new(NeverLlm),
            "mock".into(),
        ));
        let session = Arc::new(
            crate::session::Session::create_with_id(
                Arc::clone(&store),
                session_id.clone(),
                ".",
                "mock",
                None,
                None,
                None,
                runtime,
                test_caps(),
            )
            .await
            .unwrap(),
        );
        let mut events = session.subscribe();
        let (tx, rx) = mpsc::unbounded_channel();
        let _forwarder = spawn_background_forwarder(rx, Arc::clone(&session));

        let mut metadata = std::collections::BTreeMap::new();
        metadata.insert("task_id".into(), serde_json::json!("task-1"));
        tx.send(BackgroundTaskCompletion {
            session_id,
            task_id: "task-1".into(),
            tool_name: "shell".into(),
            result: ToolResult {
                call_id: "call-1".into(),
                content: "done".into(),
                is_error: false,
                error: None,
                metadata,
                duration_ms: None,
            },
            arguments: "{}".into(),
            arguments_json: Some(serde_json::json!({})),
        })
        .unwrap();

        let mut saw_tool_completion = false;
        let mut saw_background_completion = false;
        for _ in 0..2 {
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
                .await
                .expect("fallback events should arrive")
                .expect("session event channel should remain open");
            match event.payload {
                EventPayload::ToolCallCompleted { call_id, .. } => {
                    saw_tool_completion = call_id.as_str() == "call-1";
                },
                EventPayload::BackgroundTaskCompleted { task_id, .. } => {
                    saw_background_completion = task_id.as_str() == "task-1";
                },
                _ => {},
            }
        }

        assert!(
            saw_tool_completion,
            "live ToolCallCompleted should finalize UI"
        );
        assert!(saw_background_completion);
        assert_eq!(
            store
                .session_read_model(session.id())
                .await
                .unwrap()
                .messages
                .len(),
            0,
            "fallback is live-only when durable write fails"
        );
    }

    #[tokio::test]
    async fn reader_cancel_rejects_wrong_session() {
        let manager = Arc::new(Mutex::new(BackgroundTaskManager::new()));
        let reader = BackgroundTaskReaderImpl::new(Arc::clone(&manager));

        let task_id = BackgroundTaskId::from("task-x");
        let session_correct = SessionId::from("correct");
        let session_wrong = SessionId::from("wrong");

        let (exec, watcher) = fake_handles();
        manager
            .lock()
            .register(task_id.clone(), session_correct.clone(), exec, watcher);

        assert!(!reader.cancel(&session_wrong, &task_id));
        // Correct session can cancel
        assert!(reader.cancel(&session_correct, &task_id));
    }
}
