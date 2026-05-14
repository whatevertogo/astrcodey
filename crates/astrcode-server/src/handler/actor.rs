use std::{collections::HashMap, sync::Arc};

use astrcode_context::compaction::CompactResult;
use astrcode_core::{
    event::EventPayload,
    extension::CompactTrigger,
    types::{SessionId, TurnId},
};
use astrcode_protocol::{commands::ClientCommand, events::ClientNotification};
use tokio::sync::{broadcast, mpsc, oneshot};

use super::{CommandHandler, HandlerError, ManualCompactOutcome, PromptSubmission, TurnCompletion};
use crate::{
    agent::{AgentError, AgentTurnOutput, tool_types::BackgroundTaskCompletion},
    bootstrap::ServerRuntime,
};

#[derive(Clone)]
pub struct CommandHandle {
    pub(super) tx: mpsc::UnboundedSender<CommandMessage>,
}

impl CommandHandle {
    /// Spawn a command handler actor and return a handle to it.
    pub fn spawn(
        runtime: Arc<ServerRuntime>,
        event_tx: broadcast::Sender<ClientNotification>,
    ) -> Self {
        CommandHandler::spawn_actor(runtime, event_tx)
    }

    pub async fn handle(&self, command: ClientCommand) -> Result<(), HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CommandMessage::ClientCommand { command, reply })
            .map_err(|_| HandlerError::Other("command actor is unavailable".into()))?;
        rx.await
            .map_err(|_| HandlerError::Other("command actor dropped response".into()))?
    }

    pub async fn create_session(&self, working_dir: String) -> Result<SessionId, HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CommandMessage::CreateSession { working_dir, reply })
            .map_err(|_| HandlerError::Other("command actor is unavailable".into()))?;
        rx.await
            .map_err(|_| HandlerError::Other("command actor dropped response".into()))?
    }

    pub async fn submit_prompt_for_session(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<TurnId, HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CommandMessage::SubmitPromptForSession {
                session_id,
                text,
                reply,
            })
            .map_err(|_| HandlerError::Other("command actor is unavailable".into()))?;
        rx.await
            .map_err(|_| HandlerError::Other("command actor dropped response".into()))?
    }

    pub(crate) async fn submit_prompt_with_completion(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<(TurnId, oneshot::Receiver<TurnCompletion>), HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CommandMessage::SubmitPromptWithCompletion {
                session_id,
                text,
                reply,
            })
            .map_err(|_| HandlerError::Other("command actor is unavailable".into()))?;
        rx.await
            .map_err(|_| HandlerError::Other("command actor dropped response".into()))?
    }

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

    pub async fn abort_session(&self, session_id: SessionId) -> Result<(), HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CommandMessage::AbortSession { session_id, reply })
            .map_err(|_| HandlerError::Other("command actor is unavailable".into()))?;
        rx.await
            .map_err(|_| HandlerError::Other("command actor dropped response".into()))?
    }

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

pub(super) enum CommandMessage {
    ClientCommand {
        command: ClientCommand,
        reply: oneshot::Sender<Result<(), HandlerError>>,
    },
    CreateSession {
        working_dir: String,
        reply: oneshot::Sender<Result<SessionId, HandlerError>>,
    },
    SubmitPromptForSession {
        session_id: SessionId,
        text: String,
        reply: oneshot::Sender<Result<TurnId, HandlerError>>,
    },
    SubmitInputForSession {
        session_id: SessionId,
        text: String,
        reply: oneshot::Sender<Result<PromptSubmission, HandlerError>>,
    },
    CompactSession {
        session_id: SessionId,
        reply: oneshot::Sender<Result<ManualCompactOutcome, HandlerError>>,
    },
    AbortSession {
        session_id: SessionId,
        reply: oneshot::Sender<Result<(), HandlerError>>,
    },
    ListCommandsForSession {
        session_id: SessionId,
        reply: oneshot::Sender<
            Result<Vec<astrcode_protocol::events::ExtensionCommandInfo>, HandlerError>,
        >,
    },
    AgentEvent {
        session_id: SessionId,
        turn_id: TurnId,
        payload: EventPayload,
    },
    AgentTurnFinished {
        session_id: SessionId,
        turn_id: TurnId,
        output: AgentTurnOutput,
    },
    AgentTurnFailed {
        session_id: SessionId,
        turn_id: TurnId,
        error: AgentError,
        emitted_error: bool,
    },
    AgentAutoCompact {
        session_id: SessionId,
        turn_id: TurnId,
        trigger: CompactTrigger,
        compaction: CompactResult,
        reply: oneshot::Sender<Result<SessionId, HandlerError>>,
    },
    /// 后台任务完成，需要持久化事件并广播给客户端。
    BackgroundTaskCompleted(BackgroundTaskCompletion),
    SubmitPromptWithCompletion {
        session_id: SessionId,
        text: String,
        reply: oneshot::Sender<Result<(TurnId, oneshot::Receiver<TurnCompletion>), HandlerError>>,
    },
}

impl CommandHandler {
    /// 创建新的命令处理器。
    ///
    /// # 参数
    /// - `runtime`: 服务器运行时服务集合
    /// - `event_tx`: 事件广播发送端
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

    pub fn spawn_actor(
        runtime: Arc<ServerRuntime>,
        event_tx: broadcast::Sender<ClientNotification>,
    ) -> CommandHandle {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut handler = Self::new(runtime, event_tx, tx.clone());
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

    async fn run(&mut self, mut rx: mpsc::UnboundedReceiver<CommandMessage>) {
        while let Some(message) = rx.recv().await {
            self.handle_message(message).await;
        }
    }

    async fn handle_message(&mut self, message: CommandMessage) {
        match message {
            CommandMessage::ClientCommand { command, reply } => {
                let _ = reply.send(self.handle(command).await);
            },
            CommandMessage::CreateSession { working_dir, reply } => {
                let _ = reply.send(self.create_session(working_dir).await);
            },
            CommandMessage::SubmitPromptForSession {
                session_id,
                text,
                reply,
            } => {
                let _ = reply.send(self.submit_prompt_for_session(session_id, text).await);
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
            CommandMessage::AgentTurnFinished {
                session_id,
                turn_id,
                output,
            } => {
                self.finish_agent_turn(session_id, turn_id, output).await;
            },
            CommandMessage::AgentTurnFailed {
                session_id,
                turn_id,
                error,
                emitted_error,
            } => {
                self.fail_agent_turn(session_id, turn_id, error, emitted_error)
                    .await;
            },
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
            CommandMessage::BackgroundTaskCompleted(completion) => {
                // 持久化后台任务完成事件并广播给客户端
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
            CommandMessage::SubmitPromptWithCompletion {
                session_id,
                text,
                reply,
            } => {
                let _ = reply.send(
                    self.submit_prompt_for_session_with_completion(session_id, text)
                        .await,
                );
            },
        }
    }
}
