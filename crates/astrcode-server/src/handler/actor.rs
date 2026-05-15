//! Actor 框架 — CommandHandler 的异步消息驱动封装。
//!
//! 提供 `CommandHandle` 作为外部访问入口，所有操作通过消息通道异步执行，
//! 避免并发冲突并简化状态管理。

use std::{collections::HashMap, sync::Arc};

use astrcode_context::compaction::CompactResult;
use astrcode_core::{
    event::EventPayload,
    extension::CompactTrigger,
    types::{SessionId, TurnId},
};
use astrcode_protocol::{commands::ClientCommand, events::ClientNotification};
use astrcode_session::{TurnError, TurnOutput, background::BackgroundTaskCompletion};
use tokio::sync::{broadcast, mpsc, oneshot};

use super::{CommandHandler, HandlerError, ManualCompactOutcome, PromptSubmission, TurnCompletion};
use crate::bootstrap::ServerRuntime;

/// 外部访问 CommandHandler 的句柄，通过消息通道发送命令。
#[derive(Clone)]
pub struct CommandHandle {
    pub(super) tx: mpsc::UnboundedSender<CommandMessage>,
}

impl CommandHandle {
    /// 启动 CommandHandler Actor，返回可克隆的句柄。
    pub fn spawn(
        runtime: Arc<ServerRuntime>,
        event_tx: broadcast::Sender<ClientNotification>,
    ) -> Self {
        CommandHandler::spawn_actor(runtime, event_tx)
    }

    /// 发送客户端命令，等待执行完成。
    pub async fn handle(&self, command: ClientCommand) -> Result<(), HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CommandMessage::ClientCommand { command, reply })
            .map_err(|_| HandlerError::Other("command actor is unavailable".into()))?;
        rx.await
            .map_err(|_| HandlerError::Other("command actor dropped response".into()))?
    }

    /// 创建新会话，返回会话 ID。
    pub async fn create_session(&self, working_dir: String) -> Result<SessionId, HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CommandMessage::CreateSession { working_dir, reply })
            .map_err(|_| HandlerError::Other("command actor is unavailable".into()))?;
        rx.await
            .map_err(|_| HandlerError::Other("command actor dropped response".into()))?
    }

    /// 提交提示词，返回 Turn ID 和完成通知接收器。
    pub(crate) async fn submit_prompt_with_completion(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<(TurnId, oneshot::Receiver<TurnCompletion>), HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CommandMessage::SubmitInputWithCompletion {
                session_id,
                text,
                reply,
            })
            .map_err(|_| HandlerError::Other("command actor is unavailable".into()))?;
        rx.await
            .map_err(|_| HandlerError::Other("command actor dropped response".into()))?
    }

    /// 向指定会话提交输入。
    pub async fn submit_input_for_session(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<PromptSubmission, HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CommandMessage::SubmitInputForSession {
                session_id,
                text,
                reply,
            })
            .map_err(|_| HandlerError::Other("command actor is unavailable".into()))?;
        rx.await
            .map_err(|_| HandlerError::Other("command actor dropped response".into()))?
    }

    /// 手动压缩指定会话。
    pub async fn compact_session(
        &self,
        session_id: SessionId,
    ) -> Result<ManualCompactOutcome, HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CommandMessage::CompactSession { session_id, reply })
            .map_err(|_| HandlerError::Other("command actor is unavailable".into()))?;
        rx.await
            .map_err(|_| HandlerError::Other("command actor dropped response".into()))?
    }

    /// 中止指定会话的活跃 Turn。
    pub async fn abort_session(&self, session_id: SessionId) -> Result<(), HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CommandMessage::AbortSession { session_id, reply })
            .map_err(|_| HandlerError::Other("command actor is unavailable".into()))?;
        rx.await
            .map_err(|_| HandlerError::Other("command actor dropped response".into()))?
    }

    /// 获取指定会话的可用命令列表。
    pub async fn command_infos_for_session(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<astrcode_protocol::events::ExtensionCommandInfo>, HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CommandMessage::ListCommandsForSession { session_id, reply })
            .map_err(|_| HandlerError::Other("command actor is unavailable".into()))?;
        rx.await
            .map_err(|_| HandlerError::Other("command actor dropped response".into()))?
    }
}

/// Actor 内部消息类型，涵盖所有需要异步处理的操作。
pub(in crate::handler) enum CommandMessage {
    /// 客户端命令
    ClientCommand {
        command: ClientCommand,
        reply: oneshot::Sender<Result<(), HandlerError>>,
    },
    /// 创建会话
    CreateSession {
        working_dir: String,
        reply: oneshot::Sender<Result<SessionId, HandlerError>>,
    },
    /// 提交输入
    SubmitInputForSession {
        session_id: SessionId,
        text: String,
        reply: oneshot::Sender<Result<PromptSubmission, HandlerError>>,
    },
    /// 手动压缩
    CompactSession {
        session_id: SessionId,
        reply: oneshot::Sender<Result<ManualCompactOutcome, HandlerError>>,
    },
    /// 中止 Turn
    AbortSession {
        session_id: SessionId,
        reply: oneshot::Sender<Result<(), HandlerError>>,
    },
    /// 列出命令
    ListCommandsForSession {
        session_id: SessionId,
        reply: oneshot::Sender<
            Result<Vec<astrcode_protocol::events::ExtensionCommandInfo>, HandlerError>,
        >,
    },
    /// Agent 事件（来自后台任务）
    AgentEvent {
        session_id: SessionId,
        turn_id: TurnId,
        payload: EventPayload,
    },
    /// Agent Turn 完成
    AgentTurnFinished {
        session_id: SessionId,
        turn_id: TurnId,
        output: TurnOutput,
    },
    /// Agent Turn 失败
    AgentTurnFailed {
        session_id: SessionId,
        turn_id: TurnId,
        error: TurnError,
        emitted_error: bool,
    },
    /// 自动压缩完成，需要继续 Turn
    AgentAutoCompact {
        session_id: SessionId,
        turn_id: TurnId,
        trigger: CompactTrigger,
        compaction: CompactResult,
        reply: oneshot::Sender<Result<SessionId, HandlerError>>,
    },
    /// 后台任务完成
    BackgroundTaskCompleted(BackgroundTaskCompletion),
    /// 提交提示词并等待完成通知
    SubmitInputWithCompletion {
        session_id: SessionId,
        text: String,
        reply: oneshot::Sender<Result<(TurnId, oneshot::Receiver<TurnCompletion>), HandlerError>>,
    },
}

impl CommandHandler {
    /// 创建新的 Handler 实例。
    pub(super) fn new(
        runtime: Arc<ServerRuntime>,
        event_tx: broadcast::Sender<ClientNotification>,
        actor_tx: mpsc::UnboundedSender<CommandMessage>,
    ) -> Self {
        Self {
            runtime,
            event_tx,
            active_session_id: None,
            session_tool_registries: HashMap::new(),
            active_turns: HashMap::new(),
            actor_tx,
        }
    }

    /// 启动 Actor 任务，返回外部访问句柄。
    pub fn spawn_actor(
        runtime: Arc<ServerRuntime>,
        event_tx: broadcast::Sender<ClientNotification>,
    ) -> CommandHandle {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut handler = Self::new(runtime, event_tx, tx.clone());
        let handle = tokio::spawn(async move {
            handler.run(rx).await;
        });
        // 监控 Actor 任务，记录 panic
        tokio::spawn(async move {
            if let Err(e) = handle.await {
                tracing::error!("command handler actor panicked: {e}");
            }
        });
        CommandHandle { tx }
    }

    /// Actor 主循环：接收并处理消息直到通道关闭。
    async fn run(&mut self, mut rx: mpsc::UnboundedReceiver<CommandMessage>) {
        while let Some(message) = rx.recv().await {
            self.handle_message(message).await;
        }
    }

    /// 分发消息到对应处理方法。
    async fn handle_message(&mut self, message: CommandMessage) {
        match message {
            CommandMessage::ClientCommand { command, reply } => {
                let _ = reply.send(self.handle(command).await);
            },
            CommandMessage::CreateSession { working_dir, reply } => {
                let _ = reply.send(self.create_session(working_dir).await);
            },
            CommandMessage::SubmitInputForSession {
                session_id,
                text,
                reply,
            } => {
                let _ = reply.send(self.submit_input_for_session(session_id, text).await);
            },
            CommandMessage::CompactSession { session_id, reply } => {
                let _ = reply.send(self.compact_session(&session_id).await);
            },
            CommandMessage::AbortSession { session_id, reply } => {
                let _ = reply.send(self.abort_session(&session_id).await);
            },
            CommandMessage::ListCommandsForSession { session_id, reply } => {
                let _ = reply.send(self.command_infos_for_session(&session_id).await);
            },
            // Agent 事件：校验 Turn 有效性后记录并广播
            CommandMessage::AgentEvent {
                session_id,
                turn_id,
                payload,
            } => {
                if self.active_turn_matches(&session_id, &turn_id) {
                    if let Err(e) = self
                        .record_and_broadcast(&session_id, Some(&turn_id), payload)
                        .await
                    {
                        tracing::warn!(
                            session_id = %session_id,
                            turn_id = %turn_id,
                            error = %e,
                            "failed to persist/broadcast agent event"
                        );
                    }
                }
            },
            // Agent Turn 完成
            CommandMessage::AgentTurnFinished {
                session_id,
                turn_id,
                output,
            } => {
                self.finish_agent_turn(session_id, turn_id, output).await;
            },
            // Agent Turn 失败
            CommandMessage::AgentTurnFailed {
                session_id,
                turn_id,
                error,
                emitted_error,
            } => {
                self.fail_agent_turn(session_id, turn_id, error, emitted_error)
                    .await;
            },
            // 自动压缩后继续 Turn
            CommandMessage::AgentAutoCompact {
                session_id,
                turn_id,
                trigger,
                compaction,
                reply,
            } => {
                let result = self
                    .continue_active_turn_from_compaction(session_id, turn_id, trigger, compaction)
                    .await;
                let _ = reply.send(result);
            },
            // 后台任务完成：持久化 ToolCallCompleted 和 BackgroundTaskCompleted 事件
            CommandMessage::BackgroundTaskCompleted(completion) => {
                if let Err(e) = self
                    .record_and_broadcast(
                        &completion.session_id,
                        None,
                        completion.to_tool_call_completed(),
                    )
                    .await
                {
                    tracing::warn!(
                        session_id = %completion.session_id,
                        error = %e,
                        "failed to persist ToolCallCompleted for background task"
                    );
                }
                if let Err(e) = self
                    .record_and_broadcast(
                        &completion.session_id,
                        None,
                        completion.to_background_task_completed(),
                    )
                    .await
                {
                    tracing::warn!(
                        session_id = %completion.session_id,
                        error = %e,
                        "failed to persist BackgroundTaskCompleted"
                    );
                }
            },
            CommandMessage::SubmitInputWithCompletion {
                session_id,
                text,
                reply,
            } => {
                let _ = reply.send(self.submit_input_with_completion(session_id, text).await);
            },
        }
    }
}
