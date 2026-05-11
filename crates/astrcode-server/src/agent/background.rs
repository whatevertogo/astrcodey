//! 后台任务管理器与占位结果构造。
//!
//! 管理被自动后台化的工具调用（主要是长时间运行的 shell 命令）。
//! 提供注册、取消、查询和清理能力。

use std::{collections::HashMap, sync::Arc};

use parking_lot::Mutex;

use astrcode_core::{
    tool::{BackgroundTaskReader, ToolResult},
    types::{BackgroundTaskId, SessionId},
};

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
