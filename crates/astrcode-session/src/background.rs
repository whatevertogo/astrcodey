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
}

impl BackgroundTaskCompletion {
    /// 从完成通知派生 `ToolCallCompleted` 和 `BackgroundTaskCompleted` 事件载荷。
    ///
    /// 消费 `self`，避免两个方法重复 clone `result`、`tool_name` 等字段。
    pub fn into_events(self) -> (EventPayload, EventPayload) {
        let call_id = ToolCallId::from(self.result.call_id.clone());
        let tool_call_completed = EventPayload::ToolCallCompleted {
            call_id: call_id.clone(),
            tool_name: self.tool_name.clone(),
            result: self.result.clone(),
        };
        let background_task_completed = EventPayload::BackgroundTaskCompleted {
            task_id: self.task_id,
            call_id,
            tool_name: self.tool_name,
            result: self.result,
        };
        (tool_call_completed, background_task_completed)
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
/// `(ToolCallCompleted, BackgroundTaskCompleted)` 两个事件，先经可选 `sink`，
/// 再通过 `Session::emit` 写入（store + runtime 广播）。
/// 这个函数替代了 `handler/turn.rs` 与 `session_spawner.rs` 各写一份的转发循环。
pub fn spawn_background_forwarder(
    mut rx: mpsc::UnboundedReceiver<BackgroundTaskCompletion>,
    session: Arc<crate::session::Session>,
    sink: Option<Arc<dyn crate::turn_context::EventSink>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(completion) = rx.recv().await {
            let (tool_call_event, bg_event) = completion.into_events();
            if let Some(sink) = sink.as_deref() {
                let preview_a = astrcode_core::event::Event::new(
                    session.id().clone(),
                    None,
                    tool_call_event.clone(),
                );
                let preview_b =
                    astrcode_core::event::Event::new(session.id().clone(), None, bg_event.clone());
                sink.on_event(&preview_a).await;
                sink.on_event(&preview_b).await;
            }
            session.emit(None, tool_call_event).await;
            session.emit(None, bg_event).await;
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
    use super::*;

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
