//! Server-side AgentSessionControl 实现。
//!
//! 使用 `CommandHandle` 提供的 submit/abort 能力，
//! 和 `EventStore` 提供的读模型查询能力。

use std::sync::Arc;

use astrcode_core::{
    llm::{LlmContent, LlmRole},
    storage::EventStore,
    tool::{AgentSessionControl, AgentSessionInfo, TurnResult},
    types::SessionId,
};
use astrcode_session::Session;
use parking_lot::RwLock;

use crate::handler::{CommandHandle, TurnCompletion};

/// Server-side AgentSessionControl 实现。
///
/// 持有 `CommandHandle`（通过共享槽延迟注入）和 `EventStore`。
/// 不存储中间状态，`send_and_wait` 内部直接 await completion receiver。
pub struct ServerAgentSessionControl {
    store: Arc<dyn EventStore>,
    command_handle: Arc<RwLock<Option<CommandHandle>>>,
}

impl ServerAgentSessionControl {
    pub fn new(
        store: Arc<dyn EventStore>,
        command_handle: Arc<RwLock<Option<CommandHandle>>>,
    ) -> Self {
        Self {
            store,
            command_handle,
        }
    }

    /// 读取 session 最后一条 assistant 消息的文本内容。
    async fn read_last_output(&self, session_id: &str) -> Option<String> {
        let session = Session::open(self.store.clone(), SessionId::from(session_id))
            .await
            .ok()?;
        let model = session.read_model().await.ok()?;

        model.messages.iter().rev().find_map(|msg| {
            if matches!(msg.role, LlmRole::Assistant) {
                msg.content.iter().find_map(|c| match c {
                    LlmContent::Text { text } => Some(text.clone()),
                    _ => None,
                })
            } else {
                None
            }
        })
    }
}

#[async_trait::async_trait]
impl AgentSessionControl for ServerAgentSessionControl {
    async fn send_and_wait(
        &self,
        child_session_id: &str,
        message: String,
    ) -> Result<TurnResult, String> {
        let handle = self
            .command_handle
            .read()
            .clone()
            .ok_or_else(|| "command handle not bound yet".to_string())?;

        let sid = SessionId::from(child_session_id);
        let (_turn_id, rx) = handle
            .submit_prompt_with_completion(sid, message)
            .await
            .map_err(|e| format!("submit prompt: {e}"))?;

        let completion = rx
            .await
            .map_err(|_| String::from("turn channel closed unexpectedly"))?;

        match completion {
            TurnCompletion::Completed { .. } => {
                let output = self
                    .read_last_output(child_session_id)
                    .await
                    .unwrap_or_default();
                Ok(TurnResult::Completed { output })
            },
            TurnCompletion::Failed { error } => Ok(TurnResult::Failed { error }),
            TurnCompletion::Aborted => Ok(TurnResult::Aborted),
        }
    }

    async fn abort_session(&self, session_id: &str) -> Result<(), String> {
        let handle = self
            .command_handle
            .read()
            .clone()
            .ok_or_else(|| "command handle not bound yet".to_string())?;

        handle
            .abort_session(SessionId::from(session_id))
            .await
            .map_err(|e| format!("abort session: {e}"))
    }

    async fn list_children(&self, session_id: &str) -> Result<Vec<AgentSessionInfo>, String> {
        let session = Session::open(self.store.clone(), SessionId::from(session_id))
            .await
            .map_err(|e| format!("open session: {e}"))?;
        let model = session
            .read_model()
            .await
            .map_err(|e| format!("read session: {e}"))?;

        Ok(model
            .agent_sessions
            .iter()
            .map(|link| AgentSessionInfo {
                session_id: link.child_session_id.to_string(),
                agent_name: link.agent_name.clone(),
                task: link.task.clone(),
                status: link.status,
            })
            .collect())
    }
}
