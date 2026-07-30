//! 命令处理器 — 使用 ServerRuntime 处理客户端命令。
//!
//! 传输层无关：同时被 stdio 二进制和进程内 CLI 使用。
//! 负责将 `ClientCommand` 路由到对应的服务方法，并通过广播通道发送通知。
//!
//! 运行中输入的路由策略由 [`TurnScheduler::deliver_input`] 统一执行：
//! HTTP/ACP 连发 prompt 用 `QueueIfRunningElseStart`；显式 mid-turn 注入用
//! `InjectOnly`（见 `prompt::inject_input_for_session`）。本模块不再维护独立队列。

use std::sync::Arc;

use astrcode_core::types::*;
use astrcode_protocol::commands::UiResponseValue;

use crate::{
    bootstrap::ServerRuntime, session_command_service::SessionCommandService,
    session_manager::SessionManagerError, turn_scheduler::TurnScheduler,
};

mod actor;
mod compact;
mod errors;
mod model_selection;
mod notifications;
mod prompt;
mod recap;
mod router;
pub(crate) mod session_command;
mod session_lifecycle;
pub(crate) mod slash;
pub(crate) mod snapshot;
pub(in crate::handler) mod turn;

pub use actor::CommandHandle;
pub use compact::ManualCompactOutcome;
use model_selection::ModelSelectionController;
pub use session_command::CommandList;
/// 用户输入提交结果：被接受进入 Turn，或被斜杠命令处理。
#[derive(Debug)]
pub enum PromptSubmission {
    Accepted { turn_id: TurnId },
    Handled { message: String },
}

#[derive(Debug)]
pub enum CommandInvocation {
    Display { content: String, is_error: bool },
    Handled { message: String },
    Started { turn_id: TurnId },
}

impl CommandInvocation {
    pub(crate) fn into_prompt_submission(self) -> PromptSubmission {
        match self {
            Self::Display { content, is_error } => PromptSubmission::Handled {
                message: if is_error {
                    format!("Error: {content}")
                } else {
                    content
                },
            },
            Self::Handled { message } => PromptSubmission::Handled { message },
            Self::Started { turn_id } => PromptSubmission::Accepted { turn_id },
        }
    }
}

/// Handler 错误类型，替代原来的字符串匹配。
#[derive(Debug, thiserror::Error)]
pub enum HandlerError {
    #[error("A turn is already running")]
    TurnAlreadyRunning,
    #[error("No active turn")]
    NoActiveTurn,
    #[error("No active session")]
    NoActiveSession,
    #[error("Session not found: {0}")]
    SessionNotFound(String),
    #[error("Unknown command: /{0}")]
    UnknownCommand(String),
    #[error("Cannot compact while a turn is running")]
    CompactBlocked,
    #[error("Compaction skipped: {0}")]
    CompactionSkipped(String),
    #[error(transparent)]
    SessionManager(#[from] SessionManagerError),
    #[error(transparent)]
    Session(astrcode_session::SessionError),
    #[error(transparent)]
    Turn(astrcode_session::TurnError),
    #[error(transparent)]
    Compact(astrcode_context::CompactError),
    #[error("LLM error: {0}")]
    Llm(#[source] astrcode_core::llm::LlmError),
    #[error(transparent)]
    Extension(astrcode_extension_sdk::extension::ExtensionError),
    /// Command actor 通道已关闭，服务不可用。
    #[error("Command actor is unavailable")]
    ActorUnavailable,
    /// 验证失败或状态不满足前置条件。
    #[error("Invalid request: {0}")]
    InvalidRequest(String),
}

pub(crate) use crate::turn_scheduler::TurnCompletion;

/// 命令处理器，处理客户端命令并通过广播通道发送通知。
pub(crate) struct CommandHandler {
    runtime: Arc<ServerRuntime>,
    /// Handler 最近 focus 的 session（最近一次 create/fork），非「当前有 turn 在跑」。
    focused_session_id: Option<SessionId>,
    /// 统一的 turn 生命周期服务
    scheduler: Arc<TurnScheduler>,
    /// 事件总线，用于发送客户端通知
    event_bus: Arc<crate::server_event_bus::ServerEventBus>,
    /// 显式 session 命令的无状态实现。
    session_commands: SessionCommandService,
    /// 模型选择流程。
    model_selection: ModelSelectionController,
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use snapshot::message_to_dto;

impl CommandHandler {
    // ─── Fork ─────────────────────────────────────────────────────────

    /// Fork 源会话，创建新 session 并切换到新 session。
    ///
    /// 新 session 继承源 session fork 点之前的完整消息前缀和 system prompt，
    /// 保证 provider 侧 KV 缓存命中。
    pub(crate) async fn fork_session(
        &mut self,
        source_id: SessionId,
        at_cursor: Option<String>,
    ) -> Result<SessionId, HandlerError> {
        let session_id = self
            .session_commands
            .fork_session(source_id, at_cursor)
            .await?;
        self.focused_session_id = Some(session_id.clone());
        Ok(session_id)
    }

    /// 删除指定工作目录下的所有会话，返回删除数量。
    pub(crate) async fn delete_project(&self, working_dir: String) -> Result<usize, HandlerError> {
        self.session_commands.delete_project(&working_dir).await
    }

    // ─── 模型选择 ───────────────────────────────────────────────────────

    /// 将全局模型选择立即写入当前 session；其他顶层 session 在下次 turn 前同步。
    async fn configure_focused_session_model(&self) -> Result<(), HandlerError> {
        let Some(session_id) = self.focused_session_id.clone() else {
            return Ok(());
        };
        let model_id = self
            .runtime
            .runtime_services()
            .read_effective()
            .llm
            .model_id
            .clone();
        let session = self.runtime.session_manager().open(session_id).await?;
        session
            .configure_model(model_id)
            .await
            .map_err(HandlerError::Session)?;
        self.runtime
            .session_manager()
            .sync_durable_events(session.id())
            .await;
        Ok(())
    }

    /// 设置当前会话使用的主模型，格式为 `profile/model`。
    async fn set_model(&self, model_id: String) -> Result<(), HandlerError> {
        let notification = match self.model_selection.set_main_model(&model_id).await {
            Ok(notification) => notification,
            Err(HandlerError::InvalidRequest(message))
                if message.starts_with("Invalid model selection:") =>
            {
                self.send_error(
                    -32603,
                    "Invalid format. Use `profile/model` or `/model` for interactive selection.",
                );
                return Ok(());
            },
            Err(error) => return Err(error),
        };

        self.configure_focused_session_model().await?;

        self.event_bus.send_notification(notification);
        Ok(())
    }

    /// 启动交互式模型选择流程。
    pub(in crate::handler) fn start_model_selection(&mut self) {
        let notification = self.model_selection.start();
        self.event_bus.send_notification(notification);
    }

    /// 处理 UI 响应，推进模型选择流程。
    async fn handle_ui_response(
        &mut self,
        request_id: String,
        value: UiResponseValue,
    ) -> Result<(), HandlerError> {
        let notification = self
            .model_selection
            .handle_response(request_id, value)
            .await?;

        // 交互式选择完成时更新当前 session 的持久化模型。
        if self.model_selection.is_idle() {
            self.configure_focused_session_model().await?;
        }

        self.event_bus.send_notification(notification);
        Ok(())
    }
}
