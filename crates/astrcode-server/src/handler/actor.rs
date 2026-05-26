//! Actor 框架 — CommandHandler 的异步消息驱动封装。
//!
//! 提供 `CommandHandle` 作为外部访问入口，所有操作通过消息通道异步执行，
//! 避免并发冲突并简化状态管理。

use std::sync::Arc;

use astrcode_core::types::{SessionId, TurnId};
use astrcode_protocol::commands::ClientCommand;
use tokio::sync::{mpsc, oneshot};

use super::{CommandHandler, HandlerError, ManualCompactOutcome, PromptSubmission, TurnCompletion};
use crate::{
    bootstrap::ServerRuntime,
    turn_scheduler::{SubmitOutcome, TurnScheduler},
};

/// Command actor 队列容量；满时 `send().await` 对调用方施加背压。
pub(in crate::handler) const COMMAND_ACTOR_CAPACITY: usize = 256;

/// 外部访问 CommandHandler 的句柄，通过消息通道发送命令。
#[derive(Clone)]
pub struct CommandHandle {
    pub(super) tx: mpsc::Sender<CommandMessage>,
}

impl CommandHandle {
    async fn post(&self, message: CommandMessage) -> Result<(), HandlerError> {
        self.tx
            .send(message)
            .await
            .map_err(|_| HandlerError::ActorUnavailable)
    }
    /// 启动 CommandHandler Actor，返回可克隆的句柄。
    pub fn spawn(
        runtime: Arc<ServerRuntime>,
        scheduler: Arc<TurnScheduler>,
        event_bus: Arc<crate::server_event_bus::ServerEventBus>,
    ) -> Self {
        CommandHandler::spawn_actor(runtime, scheduler, event_bus)
    }

    /// 发送客户端命令，等待执行完成。
    pub async fn handle(&self, command: ClientCommand) -> Result<(), HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.post(CommandMessage::ClientCommand { command, reply })
            .await?;
        rx.await.map_err(|_| HandlerError::ActorUnavailable)?
    }

    /// 创建新会话，返回会话 ID。
    pub async fn create_session(&self, working_dir: String) -> Result<SessionId, HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.post(CommandMessage::CreateSession { working_dir, reply })
            .await?;
        rx.await.map_err(|_| HandlerError::ActorUnavailable)?
    }

    /// 提交提示词，返回 Turn ID 和完成通知接收器。
    pub(crate) async fn submit_prompt_with_completion(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<(TurnId, oneshot::Receiver<TurnCompletion>), HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.post(CommandMessage::SubmitInputWithCompletion {
            session_id,
            text,
            reply,
        })
        .await?;
        rx.await.map_err(|_| HandlerError::ActorUnavailable)?
    }

    /// 向指定会话提交输入。
    pub async fn submit_input_for_session(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<PromptSubmission, HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.post(CommandMessage::SubmitInputForSession {
            session_id,
            text,
            reply,
        })
        .await?;
        rx.await.map_err(|_| HandlerError::ActorUnavailable)?
    }

    /// 手动压缩指定会话。
    pub async fn compact_session(
        &self,
        session_id: SessionId,
        keep_recent_turns: Option<usize>,
    ) -> Result<ManualCompactOutcome, HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.post(CommandMessage::CompactSession {
            session_id,
            keep_recent_turns,
            reply,
        })
        .await?;
        rx.await.map_err(|_| HandlerError::ActorUnavailable)?
    }

    /// 中止指定会话的活跃 Turn。
    pub async fn abort_session(&self, session_id: SessionId) -> Result<(), HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.post(CommandMessage::AbortSession { session_id, reply })
            .await?;
        rx.await.map_err(|_| HandlerError::ActorUnavailable)?
    }

    /// 获取指定会话的可用命令列表。
    pub async fn command_infos_for_session(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<astrcode_protocol::events::ExtensionCommandInfo>, HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.post(CommandMessage::ListCommandsForSession { session_id, reply })
            .await?;
        rx.await.map_err(|_| HandlerError::ActorUnavailable)?
    }

    /// 修复进程重启后残留的过期 turn phase。
    ///
    /// 如果 session phase 为非 Idle 且无活跃 Turn，写入 `TurnCompleted(interrupted)`
    /// 将 session 恢复为 Idle。session 已经是 Idle/Error 时静默返回 Ok。
    pub async fn repair_stale_turn(&self, session_id: SessionId) -> Result<(), HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.post(CommandMessage::RepairStaleTurn { session_id, reply })
            .await?;
        rx.await.map_err(|_| HandlerError::ActorUnavailable)?
    }

    /// Fork 源会话，返回新 session ID。
    pub async fn fork_session(
        &self,
        source_id: SessionId,
        at_cursor: Option<String>,
    ) -> Result<SessionId, HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.post(CommandMessage::ForkSession {
            source_id,
            at_cursor,
            reply,
        })
        .await?;
        rx.await.map_err(|_| HandlerError::ActorUnavailable)?
    }

    /// 停止 actor 主循环并等待任务退出。
    pub async fn shutdown(&self) {
        let (reply, rx) = oneshot::channel();
        if self.post(CommandMessage::Shutdown { reply }).await.is_ok() {
            let _ = rx.await;
        }
    }

    /// 删除指定工作目录下的所有会话，返回删除数量。
    pub async fn delete_project(&self, working_dir: String) -> Result<usize, HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.post(CommandMessage::DeleteProject { working_dir, reply })
            .await?;
        rx.await.map_err(|_| HandlerError::ActorUnavailable)?
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
        keep_recent_turns: Option<usize>,
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
    /// Agent Turn 完成/失败后的清理（事件已由 turn task 直接广播）。
    ///
    /// `session_id` / `completion` 供 actor 侧空闲 recap 门控；
    /// `turn_id` 用于日志与后续 stale-cleanup 校验。
    AgentTurnCleanup {
        session_id: SessionId,
        turn_id: TurnId,
        completion: TurnCompletion,
    },
    /// 提交提示词并等待完成通知
    SubmitInputWithCompletion {
        session_id: SessionId,
        text: String,
        reply: oneshot::Sender<Result<(TurnId, oneshot::Receiver<TurnCompletion>), HandlerError>>,
    },
    /// 修复进程重启后残留的过期 turn phase
    RepairStaleTurn {
        session_id: SessionId,
        reply: oneshot::Sender<Result<(), HandlerError>>,
    },
    /// Fork 源会话
    ForkSession {
        source_id: SessionId,
        at_cursor: Option<String>,
        reply: oneshot::Sender<Result<SessionId, HandlerError>>,
    },
    /// 删除指定工作目录下的所有会话
    DeleteProject {
        working_dir: String,
        reply: oneshot::Sender<Result<usize, HandlerError>>,
    },
    /// 关闭 actor 主循环
    Shutdown { reply: oneshot::Sender<()> },
}

impl CommandHandler {
    async fn queue_input_for_next_turn(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<PromptSubmission, HandlerError> {
        match self.scheduler.notify_turn(session_id, text).await {
            Ok(SubmitOutcome::Queued) => Ok(PromptSubmission::Handled {
                message: "queued for next turn".into(),
            }),
            Ok(SubmitOutcome::Started { turn_id, .. }) => {
                Ok(PromptSubmission::Accepted { turn_id })
            },
            Ok(SubmitOutcome::Injected) => Ok(PromptSubmission::Handled {
                message: "injected into active turn".into(),
            }),
            Err(e) => Err(HandlerError::from(e)),
        }
    }

    /// 创建新的 Handler 实例。
    pub(super) fn new(
        runtime: Arc<ServerRuntime>,
        scheduler: Arc<TurnScheduler>,
        event_bus: Arc<crate::server_event_bus::ServerEventBus>,
        actor_tx: mpsc::Sender<CommandMessage>,
    ) -> Self {
        let model_selection =
            super::model_selection::ModelSelectionController::new(runtime.config_manager().clone());
        Self {
            runtime,
            active_session_id: None,
            scheduler,
            event_bus,
            actor_tx,
            model_selection,
        }
    }

    /// 启动 Actor 任务，返回外部访问句柄。
    pub fn spawn_actor(
        runtime: Arc<ServerRuntime>,
        scheduler: Arc<TurnScheduler>,
        event_bus: Arc<crate::server_event_bus::ServerEventBus>,
    ) -> CommandHandle {
        let (tx, rx) = mpsc::channel(COMMAND_ACTOR_CAPACITY);
        let mut handler = Self::new(runtime, scheduler, event_bus, tx.clone());
        let handle = tokio::spawn(async move {
            handler.run(rx).await;
        });
        tokio::spawn(async move {
            if let Err(e) = handle.await {
                tracing::error!("command handler actor panicked: {e}");
            }
        });
        CommandHandle { tx }
    }

    /// Actor 主循环：接收并处理消息直到通道关闭。
    ///
    /// 内置空闲 recap 机制：turn 完成后若 3 分钟内无新 prompt 提交，
    /// 自动生成 recap 摘要推送给所有客户端。
    async fn run(&mut self, mut rx: mpsc::Receiver<CommandMessage>) {
        use std::time::Duration;

        use tokio::time::{Instant, sleep_until};

        const IDLE_RECAP_DELAY: Duration = Duration::from_secs(300); // 5 分钟

        // None = 无计时；Some(deadline) = 等待中
        let mut recap_deadline: Option<Instant> = None;

        loop {
            let maybe_msg = if let Some(deadline) = recap_deadline {
                tokio::select! {
                    msg = rx.recv() => msg,
                    _ = sleep_until(deadline) => {
                        // 空闲超时，触发自动 recap
                        recap_deadline = None;
                        if self.active_session_id.as_ref().is_some_and(|sid| {
                            !self.scheduler.registry().has_active(sid)
                        }) {
                            if let Err(e) = self.recap_session().await {
                                tracing::debug!(error = %e, "auto-recap skipped");
                            }
                        }
                        continue;
                    }
                }
            } else {
                rx.recv().await
            };

            let Some(message) = maybe_msg else { break };

            // 用户提交 prompt → 取消 recap 计时
            let resets_timer = matches!(
                &message,
                CommandMessage::ClientCommand {
                    command: ClientCommand::SubmitPrompt { .. },
                    ..
                } | CommandMessage::SubmitInputForSession { .. }
                    | CommandMessage::SubmitInputWithCompletion { .. }
            );
            if resets_timer {
                recap_deadline = None;
            }

            // Turn 正常结束且仍是当前活跃 session → 启动空闲 recap 计时
            let starts_recap_timer = match &message {
                CommandMessage::AgentTurnCleanup {
                    session_id,
                    completion,
                    ..
                } => {
                    matches!(completion, TurnCompletion::Completed { .. })
                        && self.active_session_id.as_ref() == Some(session_id)
                        && !self.scheduler.registry().has_active(session_id)
                },
                _ => false,
            };

            if matches!(&message, CommandMessage::Shutdown { .. }) {
                let _ = self.handle_message(message).await;
                break;
            }

            self.handle_message(message).await;

            if starts_recap_timer {
                recap_deadline = Some(Instant::now() + IDLE_RECAP_DELAY);
            }
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
                let result = if self.scheduler.registry().has_active(&session_id) {
                    self.queue_input_for_next_turn(session_id, text).await
                } else {
                    self.submit_input_for_session(session_id, text).await
                };
                let _ = reply.send(result);
            },
            CommandMessage::CompactSession {
                session_id,
                keep_recent_turns,
                reply,
            } => {
                let result = self.compact_session(&session_id, keep_recent_turns).await;
                let _ = reply.send(result);
            },
            CommandMessage::AbortSession { session_id, reply } => {
                let result = self.abort_session(&session_id).await;
                let _ = reply.send(result);
            },
            CommandMessage::ListCommandsForSession { session_id, reply } => {
                let _ = reply.send(self.command_infos_for_session(&session_id).await);
            },
            // Agent Turn 清理（终态事件已由 turn task 直接广播）
            CommandMessage::AgentTurnCleanup {
                session_id,
                turn_id,
                completion,
            } => {
                tracing::debug!(
                    session_id = %session_id,
                    turn_id = %turn_id,
                    ?completion,
                    "agent turn cleanup"
                );
                // 排队输入由 TurnCompleted → TurnScheduler::on_turn_completed 统一出队。
            },
            CommandMessage::SubmitInputWithCompletion {
                session_id,
                text,
                reply,
            } => {
                let _ = reply.send(self.submit_input_with_completion(session_id, text).await);
            },
            CommandMessage::RepairStaleTurn { session_id, reply } => {
                let _ = reply.send(self.repair_stale_session(&session_id).await);
            },
            CommandMessage::ForkSession {
                source_id,
                at_cursor,
                reply,
            } => {
                let _ = reply.send(self.fork_session(source_id, at_cursor).await);
            },
            CommandMessage::DeleteProject { working_dir, reply } => {
                let _ = reply.send(self.delete_project(working_dir).await);
            },
            CommandMessage::Shutdown { reply } => {
                let _ = reply.send(());
            },
        }
    }
}
