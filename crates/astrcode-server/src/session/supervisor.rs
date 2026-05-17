use std::{collections::HashMap, sync::Arc};

use astrcode_core::{
    event::EventPayload,
    tool::SessionMessenger,
    types::{SessionId, TurnId},
};
use parking_lot::Mutex;

use super::{ManualCompactOutcome, SessionActor, SessionHandle};
use crate::{
    bootstrap::ServerRuntime,
    events::ClientEventPublisher,
    router::{HandlerError, PromptSubmission},
};

/// 负责按需创建并缓存每个 session 的 actor handle。
pub struct SessionSupervisor {
    runtime: Arc<ServerRuntime>,
    event_publisher: Arc<ClientEventPublisher>,
    handles: Mutex<HashMap<SessionId, SessionHandle>>,
}

impl SessionSupervisor {
    pub fn new(runtime: Arc<ServerRuntime>, event_publisher: Arc<ClientEventPublisher>) -> Self {
        Self {
            runtime,
            event_publisher,
            handles: Mutex::new(HashMap::new()),
        }
    }

    pub fn handle_for(&self, session_id: &SessionId) -> SessionHandle {
        if let Some(handle) = self.handles.lock().get(session_id).cloned() {
            return handle;
        }
        let handle = SessionActor::spawn(
            Arc::clone(&self.runtime),
            Arc::clone(&self.event_publisher),
            session_id.clone(),
        );
        self.handles
            .lock()
            .insert(session_id.clone(), handle.clone());
        handle
    }

    pub fn remove(&self, session_id: &SessionId) {
        self.handles.lock().remove(session_id);
    }

    pub async fn submit_input(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<PromptSubmission, HandlerError> {
        self.handle_for(&session_id)
            .submit_input(session_id, text)
            .await
    }

    pub async fn abort(&self, session_id: SessionId) -> Result<(), HandlerError> {
        self.handle_for(&session_id).abort(session_id).await
    }

    pub async fn compact(
        &self,
        session_id: SessionId,
    ) -> Result<ManualCompactOutcome, HandlerError> {
        self.handle_for(&session_id).compact(session_id).await
    }

    /// 通过 actor 写入一条事件并广播。
    pub async fn emit_event(
        &self,
        session_id: SessionId,
        turn_id: Option<TurnId>,
        payload: EventPayload,
    ) -> Result<(), HandlerError> {
        self.handle_for(&session_id)
            .emit_session_event(session_id, turn_id, payload)
            .await
    }

    /// 让 SessionActor 修复任何遗留的 pending tool calls。
    pub async fn repair(&self, session_id: SessionId) -> Result<(), String> {
        // 直接读 actor 的写入路径完成 repair（actor 内部已保证 active_turn 检查）
        // 通过一条专用命令进入 actor。
        let handle = self.handle_for(&session_id);
        handle.repair_stale_pending_tool_calls(session_id).await
    }

    pub async fn same_tree(&self, left: &SessionId, right: &SessionId) -> Result<bool, String> {
        Ok(self.root_id(left).await? == self.root_id(right).await?)
    }

    async fn root_id(&self, session_id: &SessionId) -> Result<SessionId, String> {
        let mut current = session_id.clone();
        loop {
            let state = self
                .runtime
                .session_directory
                .read_model(&current)
                .await
                .map_err(|e| format!("read session {current}: {e}"))?;
            match state.parent_session_id {
                Some(parent) => current = parent,
                None => return Ok(current),
            }
        }
    }
}

/// 绑定发送方 session 的工具侧 messenger。
pub struct BoundSessionMessenger {
    sender: SessionId,
    supervisor: Arc<SessionSupervisor>,
}

impl BoundSessionMessenger {
    pub fn new(sender: SessionId, supervisor: Arc<SessionSupervisor>) -> Self {
        Self { sender, supervisor }
    }
}

impl SessionMessenger for BoundSessionMessenger {
    fn send(&self, target: &SessionId, message: String) -> Result<(), String> {
        let sender = self.sender.clone();
        let target = target.clone();
        let supervisor = Arc::clone(&self.supervisor);
        tokio::spawn(async move {
            match supervisor.same_tree(&sender, &target).await {
                Ok(true) => {
                    supervisor.handle_for(&target).enqueue_message(message);
                },
                Ok(false) => {
                    tracing::warn!(sender_session_id = %sender, target_session_id = %target, "cross-tree session message rejected");
                },
                Err(error) => {
                    tracing::warn!(sender_session_id = %sender, target_session_id = %target, %error, "session tree lookup failed");
                },
            }
        });
        Ok(())
    }
}
