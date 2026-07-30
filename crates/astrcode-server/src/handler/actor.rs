//! 交互式 CommandHandler 的异步消息驱动封装。
//!
//! `CommandHandle` 保留 focus、UI 选择等有状态交互的串行语义；HTTP/ACP
//! 这类显式携带 session ID 的请求直接使用 `SessionCommandService`。

use std::sync::Arc;

#[cfg(test)]
use astrcode_core::types::TurnId;
use astrcode_core::{tool::SessionToolSelection, types::SessionId, user_input::UserInput};
use astrcode_extension_sdk::extension::CommandCompletions;
use astrcode_protocol::commands::ClientCommand;
use tokio::sync::{mpsc, oneshot};

use super::{
    CommandHandler, CommandInvocation, HandlerError, ManualCompactOutcome, PromptSubmission,
    TurnCompletion, session_command::CommandList,
};
use crate::{
    bootstrap::ServerRuntime,
    turn_scheduler::{TurnCompletionEvent, TurnScheduler},
};

/// Command actor 队列容量；满时 `send().await` 对调用方施加背压。
pub(in crate::handler) const COMMAND_ACTOR_CAPACITY: usize = 256;

/// 外部访问 CommandHandler 的句柄，通过消息通道发送命令。
#[derive(Clone)]
pub struct CommandHandle {
    pub(super) tx: mpsc::Sender<CommandMessage>,
    task: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl CommandHandle {
    async fn post(&self, message: CommandMessage) -> Result<(), HandlerError> {
        self.tx
            .send(message)
            .await
            .map_err(|_| HandlerError::ActorUnavailable)
    }

    async fn request<T>(
        &self,
        message: impl FnOnce(oneshot::Sender<Result<T, HandlerError>>) -> CommandMessage,
    ) -> Result<T, HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.post(message(reply)).await?;
        rx.await.map_err(|_| HandlerError::ActorUnavailable)?
    }

    /// 发送客户端命令，等待执行完成。
    pub async fn handle(&self, command: ClientCommand) -> Result<(), HandlerError> {
        self.request(|reply| CommandMessage::ClientCommand { command, reply })
            .await
    }

    /// 创建新会话，返回会话 ID。
    pub async fn create_session(&self, working_dir: String) -> Result<SessionId, HandlerError> {
        self.create_session_with_tool_selection(working_dir, None)
            .await
    }

    pub async fn create_session_with_tool_selection(
        &self,
        working_dir: String,
        tool_selection: Option<SessionToolSelection>,
    ) -> Result<SessionId, HandlerError> {
        self.request(|reply| CommandMessage::CreateSession {
            working_dir,
            tool_selection,
            reply,
        })
        .await
    }

    /// 提交提示词，返回 Turn ID 和完成通知接收器。
    #[cfg(test)]
    pub(crate) async fn submit_prompt_with_completion(
        &self,
        session_id: SessionId,
        input: UserInput,
    ) -> Result<(TurnId, oneshot::Receiver<TurnCompletion>), HandlerError> {
        self.request(|reply| CommandMessage::SubmitInputWithCompletion {
            session_id,
            input,
            reply,
        })
        .await
    }

    /// 向指定会话提交输入。
    pub async fn submit_input_for_session(
        &self,
        session_id: SessionId,
        input: UserInput,
    ) -> Result<PromptSubmission, HandlerError> {
        self.request(|reply| CommandMessage::SubmitInputForSession {
            session_id,
            input,
            reply,
        })
        .await
    }

    /// 向活跃 turn 注入 mid-turn 消息（steer）。
    pub async fn inject_input_for_session(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<PromptSubmission, HandlerError> {
        self.request(|reply| CommandMessage::InjectInputForSession {
            session_id,
            text,
            reply,
        })
        .await
    }

    /// 手动压缩指定会话。
    pub async fn compact_session(
        &self,
        session_id: SessionId,
        keep_recent_turns: Option<usize>,
    ) -> Result<ManualCompactOutcome, HandlerError> {
        self.request(|reply| CommandMessage::CompactSession {
            session_id,
            keep_recent_turns,
            reply,
        })
        .await
    }

    /// 中止指定会话的活跃 Turn。
    pub async fn abort_session(&self, session_id: SessionId) -> Result<(), HandlerError> {
        self.request(|reply| CommandMessage::AbortSession { session_id, reply })
            .await
    }

    /// 获取指定会话的完整命令列表与诊断。
    pub async fn command_list_for_session(
        &self,
        session_id: SessionId,
    ) -> Result<CommandList, HandlerError> {
        self.request(|reply| CommandMessage::ListCommandCatalogForSession { session_id, reply })
            .await
    }

    /// 执行指定会话的一等 command。
    pub async fn invoke_command_for_session(
        &self,
        session_id: SessionId,
        command_name: String,
        arguments: String,
    ) -> Result<CommandInvocation, HandlerError> {
        self.request(|reply| CommandMessage::InvokeCommandForSession {
            session_id,
            command_name,
            arguments,
            reply,
        })
        .await
    }

    /// 请求指定 command 的参数补全。
    pub async fn complete_command_for_session(
        &self,
        session_id: SessionId,
        command_name: String,
        argument: String,
        cursor: Option<usize>,
    ) -> Result<CommandCompletions, HandlerError> {
        self.request(|reply| CommandMessage::CompleteCommandForSession {
            session_id,
            command_name,
            argument,
            cursor,
            reply,
        })
        .await
    }

    /// 修复进程重启后残留的过期 turn phase。
    ///
    /// 如果 session phase 为非 Idle 且无活跃 Turn，写入 `TurnCompleted(interrupted)`
    /// 将 session 恢复为 Idle。session 已经是 Idle/Error 时静默返回 Ok。
    pub async fn repair_stale_turn(&self, session_id: SessionId) -> Result<(), HandlerError> {
        self.request(|reply| CommandMessage::RepairStaleTurn { session_id, reply })
            .await
    }

    /// Fork 源会话，返回新 session ID。
    pub async fn fork_session(
        &self,
        source_id: SessionId,
        at_cursor: Option<String>,
    ) -> Result<SessionId, HandlerError> {
        self.request(|reply| CommandMessage::ForkSession {
            source_id,
            at_cursor,
            reply,
        })
        .await
    }

    /// 停止 actor 主循环并等待任务退出。
    pub async fn shutdown(&self) {
        let graceful = async {
            let (reply, rx) = oneshot::channel();
            self.post(CommandMessage::Shutdown { reply }).await?;
            rx.await.map_err(|_| HandlerError::ActorUnavailable)
        };
        let graceful = tokio::time::timeout(std::time::Duration::from_secs(1), graceful)
            .await
            .is_ok_and(|result| result.is_ok());

        let task = self.task.lock().await.take();
        if let Some(task) = task {
            if !graceful {
                tracing::warn!("command actor did not stop gracefully; aborting task");
                task.abort();
            }
            if let Err(error) = task.await {
                if error.is_panic() {
                    tracing::error!("command actor task panicked");
                } else if !error.is_cancelled() {
                    tracing::warn!(%error, "command actor task failed");
                }
            }
        }
    }

    /// 删除指定工作目录下的所有会话，返回删除数量。
    pub async fn delete_project(&self, working_dir: String) -> Result<usize, HandlerError> {
        self.request(|reply| CommandMessage::DeleteProject { working_dir, reply })
            .await
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
        tool_selection: Option<SessionToolSelection>,
        reply: oneshot::Sender<Result<SessionId, HandlerError>>,
    },
    /// 提交输入
    SubmitInputForSession {
        session_id: SessionId,
        input: UserInput,
        reply: oneshot::Sender<Result<PromptSubmission, HandlerError>>,
    },
    /// Mid-turn 注入（steer）
    InjectInputForSession {
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
    /// 列出命令及诊断
    ListCommandCatalogForSession {
        session_id: SessionId,
        reply: oneshot::Sender<Result<CommandList, HandlerError>>,
    },
    /// 执行一等 command
    InvokeCommandForSession {
        session_id: SessionId,
        command_name: String,
        arguments: String,
        reply: oneshot::Sender<Result<CommandInvocation, HandlerError>>,
    },
    /// command 参数补全
    CompleteCommandForSession {
        session_id: SessionId,
        command_name: String,
        argument: String,
        cursor: Option<usize>,
        reply: oneshot::Sender<Result<CommandCompletions, HandlerError>>,
    },
    /// 提交提示词并等待完成通知
    #[cfg(test)]
    SubmitInputWithCompletion {
        session_id: SessionId,
        input: UserInput,
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
    /// 创建新的 Handler 实例。
    pub(super) fn new(
        runtime: Arc<ServerRuntime>,
        scheduler: Arc<TurnScheduler>,
        event_bus: Arc<crate::server_event_bus::ServerEventBus>,
    ) -> Self {
        let model_selection =
            super::model_selection::ModelSelectionController::new(runtime.config_manager().clone());
        let session_commands = crate::session_command_service::SessionCommandService::new(
            Arc::clone(&runtime),
            Arc::clone(&scheduler),
            Arc::clone(&event_bus),
        );
        Self {
            runtime,
            focused_session_id: None,
            scheduler,
            event_bus,
            session_commands,
            model_selection,
        }
    }

    /// 启动 Actor 任务，返回外部访问句柄。
    pub(crate) fn spawn_actor(
        runtime: Arc<ServerRuntime>,
        scheduler: Arc<TurnScheduler>,
        event_bus: Arc<crate::server_event_bus::ServerEventBus>,
    ) -> CommandHandle {
        let (tx, rx) = mpsc::channel(COMMAND_ACTOR_CAPACITY);
        let mut handler = Self::new(runtime, scheduler, event_bus);
        let task = tokio::spawn(async move {
            handler.run(rx).await;
        });
        CommandHandle {
            tx,
            task: Arc::new(tokio::sync::Mutex::new(Some(task))),
        }
    }

    /// Actor 主循环：接收并处理消息直到通道关闭。
    ///
    /// 内置空闲 recap 机制：turn 完成后若 5 分钟内无新 prompt 提交，
    /// 自动生成 recap 摘要推送给所有客户端。
    async fn run(&mut self, mut rx: mpsc::Receiver<CommandMessage>) {
        use std::time::Duration;

        use tokio::{
            sync::broadcast,
            time::{Instant, sleep_until},
        };

        const IDLE_RECAP_DELAY: Duration = Duration::from_secs(300); // 5 分钟

        enum Wake {
            Message(Option<CommandMessage>),
            Completion(Result<TurnCompletionEvent, broadcast::error::RecvError>),
            Recap,
        }

        let mut completion_rx = self.scheduler.subscribe_completions();
        let mut recap_deadline: Option<Instant> = None;

        loop {
            let wake = if let Some(deadline) = recap_deadline {
                tokio::select! {
                    message = rx.recv() => Wake::Message(message),
                    completion = completion_rx.recv() => Wake::Completion(completion),
                    _ = sleep_until(deadline) => Wake::Recap,
                }
            } else {
                tokio::select! {
                    message = rx.recv() => Wake::Message(message),
                    completion = completion_rx.recv() => Wake::Completion(completion),
                }
            };

            match wake {
                Wake::Message(Some(message)) => {
                    if matches!(
                        &message,
                        CommandMessage::ClientCommand {
                            command: ClientCommand::SubmitPrompt { .. },
                            ..
                        } | CommandMessage::SubmitInputForSession { .. }
                    ) {
                        recap_deadline = None;
                    }
                    let shutting_down = matches!(&message, CommandMessage::Shutdown { .. });
                    self.handle_message(message).await;
                    if shutting_down {
                        break;
                    }
                },
                Wake::Message(None) => break,
                Wake::Completion(Ok(event)) => {
                    tracing::debug!(
                        session_id = %event.session_id,
                        turn_id = %event.turn_id,
                        completion = ?event.completion,
                        "turn completion observed by command actor"
                    );
                    if matches!(event.completion, TurnCompletion::Completed { .. })
                        && self.focused_session_id.as_ref() == Some(&event.session_id)
                        && !self.scheduler.registry().has_active(&event.session_id)
                    {
                        recap_deadline = Some(Instant::now() + IDLE_RECAP_DELAY);
                    }
                },
                Wake::Completion(Err(broadcast::error::RecvError::Lagged(skipped))) => {
                    tracing::warn!(skipped, "command actor lagged behind turn completions");
                },
                Wake::Completion(Err(broadcast::error::RecvError::Closed)) => {
                    tracing::warn!("turn completion channel closed");
                    break;
                },
                Wake::Recap => {
                    recap_deadline = None;
                    if self
                        .focused_session_id
                        .as_ref()
                        .is_some_and(|sid| !self.scheduler.registry().has_active(sid))
                    {
                        if let Err(error) = self.recap_session().await {
                            tracing::debug!(%error, "auto-recap skipped");
                        }
                    }
                },
            }
        }
    }

    /// 分发消息到对应处理方法。
    async fn handle_message(&mut self, message: CommandMessage) {
        match message {
            CommandMessage::ClientCommand { command, reply } => {
                let _ = reply.send(self.handle(command).await);
            },
            CommandMessage::CreateSession {
                working_dir,
                tool_selection,
                reply,
            } => {
                let _ = reply.send(
                    self.create_session_with_tool_selection(working_dir, tool_selection)
                        .await,
                );
            },
            CommandMessage::SubmitInputForSession {
                session_id,
                input,
                reply,
            } => {
                let _ = reply.send(self.submit_input_for_session(session_id, input).await);
            },
            CommandMessage::InjectInputForSession {
                session_id,
                text,
                reply,
            } => {
                let _ = reply.send(self.inject_input_for_session(session_id, text).await);
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
            CommandMessage::ListCommandCatalogForSession { session_id, reply } => {
                let _ = reply.send(self.command_list_for_session(&session_id).await);
            },
            CommandMessage::InvokeCommandForSession {
                session_id,
                command_name,
                arguments,
                reply,
            } => {
                let command = super::slash::ParsedSlashCommand {
                    name: command_name,
                    arguments,
                };
                let _ = reply.send(self.invoke_command_for_session(session_id, command).await);
            },
            CommandMessage::CompleteCommandForSession {
                session_id,
                command_name,
                argument,
                cursor,
                reply,
            } => {
                let _ = reply.send(
                    self.complete_command_for_session(session_id, command_name, argument, cursor)
                        .await,
                );
            },
            #[cfg(test)]
            CommandMessage::SubmitInputWithCompletion {
                session_id,
                input,
                reply,
            } => {
                let _ = reply.send(self.submit_input_with_completion(session_id, input).await);
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
