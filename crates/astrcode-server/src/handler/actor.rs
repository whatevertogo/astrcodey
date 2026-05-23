//! Actor 框架 — CommandHandler 的异步消息驱动封装。
//!
//! 提供 `CommandHandle` 作为外部访问入口，所有操作通过消息通道异步执行，
//! 避免并发冲突并简化状态管理。

use std::{collections::HashMap, sync::Arc};

use astrcode_core::types::{SessionId, TurnId};
use astrcode_protocol::commands::ClientCommand;
use tokio::sync::{mpsc, oneshot};

use super::{CommandHandler, HandlerError, ManualCompactOutcome, PromptSubmission, TurnCompletion};
use crate::{bootstrap::ServerRuntime, turn_scheduler::TurnScheduler};

/// 外部访问 CommandHandler 的句柄，通过消息通道发送命令。
#[derive(Clone)]
pub struct CommandHandle {
    pub(super) tx: mpsc::UnboundedSender<CommandMessage>,
}

impl CommandHandle {
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
        keep_recent_turns: Option<usize>,
    ) -> Result<ManualCompactOutcome, HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CommandMessage::CompactSession {
                session_id,
                keep_recent_turns,
                reply,
            })
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

    /// 修复进程重启后残留的过期 turn phase。
    ///
    /// 如果 session phase 为非 Idle 且无活跃 Turn，写入 `TurnCompleted(interrupted)`
    /// 将 session 恢复为 Idle。session 已经是 Idle/Error 时静默返回 Ok。
    pub async fn repair_stale_turn(&self, session_id: SessionId) -> Result<(), HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CommandMessage::RepairStaleTurn { session_id, reply })
            .map_err(|_| HandlerError::Other("command actor is unavailable".into()))?;
        rx.await
            .map_err(|_| HandlerError::Other("command actor dropped response".into()))?
    }

    /// Fork 源会话，返回新 session ID。
    pub async fn fork_session(
        &self,
        source_id: SessionId,
        at_cursor: Option<String>,
    ) -> Result<SessionId, HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CommandMessage::ForkSession {
                source_id,
                at_cursor,
                reply,
            })
            .map_err(|_| HandlerError::Other("command actor is unavailable".into()))?;
        rx.await
            .map_err(|_| HandlerError::Other("command actor dropped response".into()))?
    }

    /// 删除指定工作目录下的所有会话，返回删除数量。
    pub async fn delete_project(&self, working_dir: String) -> Result<usize, HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CommandMessage::DeleteProject { working_dir, reply })
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
    /// Agent Turn 完成/失败后的清理（事件已由 turn task 直接广播）
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
}

impl CommandHandler {
    fn enqueue_input_for_next_turn(&mut self, session_id: SessionId, text: String) {
        self.queued_inputs
            .entry(session_id)
            .or_default()
            .push_back(text);
    }

    async fn maybe_start_queued_turn(&mut self, session_id: &SessionId) {
        if self.scheduler.registry().has_active(session_id) {
            return;
        }
        let next_text = self
            .queued_inputs
            .get_mut(session_id)
            .and_then(|queue| queue.pop_front());
        let Some(text) = next_text else {
            return;
        };
        if self
            .queued_inputs
            .get(session_id)
            .is_some_and(|queue| queue.is_empty())
        {
            self.queued_inputs.remove(session_id);
        }
        if let Err(error) = self
            .start_turn_for_session(session_id.clone(), text, None)
            .await
        {
            tracing::error!(%session_id, error = %error, "failed to start queued turn");
            self.send_error(super::slash::command_error_code(&error), &error.to_string());
        }
    }

    /// 创建新的 Handler 实例。
    pub(super) fn new(
        runtime: Arc<ServerRuntime>,
        scheduler: Arc<TurnScheduler>,
        event_bus: Arc<crate::server_event_bus::ServerEventBus>,
        actor_tx: mpsc::UnboundedSender<CommandMessage>,
    ) -> Self {
        let model_selection =
            super::model_selection::ModelSelectionController::new(runtime.config_manager.clone());
        Self {
            runtime,
            active_session_id: None,
            scheduler,
            event_bus,
            queued_inputs: HashMap::new(),
            compacting_sessions: std::collections::HashSet::new(),
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
        let (tx, rx) = mpsc::unbounded_channel();
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
    async fn run(&mut self, mut rx: mpsc::UnboundedReceiver<CommandMessage>) {
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

            // Turn 完成 → 启动/重置 recap 计时
            let starts_timer = matches!(&message, CommandMessage::AgentTurnCleanup { .. });

            self.handle_message(message).await;

            if starts_timer
                && !self
                    .active_session_id
                    .as_ref()
                    .is_some_and(|sid| self.scheduler.registry().has_active(sid))
            {
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
                if self.scheduler.registry().has_active(&session_id)
                    || self.compacting_sessions.contains(&session_id)
                {
                    self.enqueue_input_for_next_turn(session_id, text);
                    let _ = reply.send(Ok(PromptSubmission::Handled {
                        message: "queued for next turn".into(),
                    }));
                } else {
                    let _ = reply.send(self.submit_input_for_session(session_id, text).await);
                }
            },
            CommandMessage::CompactSession {
                session_id,
                keep_recent_turns,
                reply,
            } => {
                let sid = session_id.clone();
                let result = self.compact_session(&session_id, keep_recent_turns).await;
                // 无论 compact 成功、跳过还是失败，都必须移除 compacting 标记，
                // 否则该 session 后续输入会被永久排队。
                self.compacting_sessions.remove(&sid);
                if matches!(result, Ok(ManualCompactOutcome::Compacted { .. })) {
                    self.maybe_start_queued_turn(&sid).await;
                }
                let _ = reply.send(result);
            },
            CommandMessage::AbortSession { session_id, reply } => {
                let result = self.abort_session(&session_id).await;
                if result.is_ok() {
                    self.maybe_start_queued_turn(&session_id).await;
                }
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
                let sid = session_id.clone();
                let _ = turn_id;
                // abort 完成时不 dispatch，已由 AbortSession handler 处理。
                if !matches!(completion, TurnCompletion::Aborted) {
                    self.maybe_start_queued_turn(&sid).await;
                }
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
        }
    }
}
