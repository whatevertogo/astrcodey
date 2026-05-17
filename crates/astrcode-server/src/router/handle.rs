//! 协议路由 actor 的入口与命令枚举。
//!
//! `CommandRouterHandle` 是面向 transport / acp / http 的唯一句柄；router actor
//! 自身只解析 `ClientCommand`，session-mutation 全部转发给 SessionSupervisor。

use std::sync::Arc;

use astrcode_core::types::{SessionId, TurnId};
use astrcode_protocol::commands::ClientCommand;
use tokio::sync::{mpsc, oneshot};

use super::{CommandRouter, HandlerError, ManualCompactOutcome, PromptSubmission, TurnCompletion};
use crate::{bootstrap::ServerRuntime, events::ClientEventPublisher};

/// 协议层入口；外部通过此句柄发送 `ClientCommand`。
#[derive(Clone)]
pub struct CommandRouterHandle {
    pub(super) tx: mpsc::UnboundedSender<RouterCommand>,
}

impl CommandRouterHandle {
    /// 启动 CommandRouter Actor，返回可克隆的句柄。
    pub fn spawn(runtime: Arc<ServerRuntime>, event_publisher: Arc<ClientEventPublisher>) -> Self {
        CommandRouter::spawn_actor(runtime, event_publisher)
    }

    pub async fn handle(&self, command: ClientCommand) -> Result<(), HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(RouterCommand::ClientCommand { command, reply })
            .map_err(|_| HandlerError::Other("command actor is unavailable".into()))?;
        rx.await
            .map_err(|_| HandlerError::Other("command actor dropped response".into()))?
    }

    pub async fn create_session(&self, working_dir: String) -> Result<SessionId, HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(RouterCommand::CreateSession { working_dir, reply })
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
            .send(RouterCommand::SubmitInputWithCompletion {
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
            .send(RouterCommand::SubmitInputForSession {
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
            .send(RouterCommand::CompactSession { session_id, reply })
            .map_err(|_| HandlerError::Other("command actor is unavailable".into()))?;
        rx.await
            .map_err(|_| HandlerError::Other("command actor dropped response".into()))?
    }

    pub async fn abort_session(&self, session_id: SessionId) -> Result<(), HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(RouterCommand::AbortSession { session_id, reply })
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
            .send(RouterCommand::ListCommandsForSession { session_id, reply })
            .map_err(|_| HandlerError::Other("command actor is unavailable".into()))?;
        rx.await
            .map_err(|_| HandlerError::Other("command actor dropped response".into()))?
    }
}

/// CommandRouter 入口的命令枚举；session-mutation 命令最终路由到 SessionActor。
pub(crate) enum RouterCommand {
    ClientCommand {
        command: ClientCommand,
        reply: oneshot::Sender<Result<(), HandlerError>>,
    },
    CreateSession {
        working_dir: String,
        reply: oneshot::Sender<Result<SessionId, HandlerError>>,
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
    SubmitInputWithCompletion {
        session_id: SessionId,
        text: String,
        reply: oneshot::Sender<Result<(TurnId, oneshot::Receiver<TurnCompletion>), HandlerError>>,
    },
}

impl CommandRouter {
    pub(super) fn new(
        runtime: Arc<ServerRuntime>,
        event_publisher: Arc<ClientEventPublisher>,
    ) -> Self {
        Self {
            runtime,
            event_publisher,
            active_session_id: None,
        }
    }

    /// 启动 CommandRouter actor 与配套 SessionSupervisor。
    pub fn spawn_actor(
        runtime: Arc<ServerRuntime>,
        event_publisher: Arc<ClientEventPublisher>,
    ) -> CommandRouterHandle {
        let supervisor = Arc::new(crate::session::SessionSupervisor::new(
            Arc::clone(&runtime),
            Arc::clone(&event_publisher),
        ));
        *runtime.session_supervisor.write() = Some(supervisor);

        let (tx, rx) = mpsc::unbounded_channel();
        let mut router = Self::new(runtime, event_publisher);
        let handle = tokio::spawn(async move { router.run(rx).await });
        tokio::spawn(async move {
            if let Err(e) = handle.await {
                tracing::error!("command router actor panicked: {e}");
            }
        });
        CommandRouterHandle { tx }
    }

    async fn run(&mut self, mut rx: mpsc::UnboundedReceiver<RouterCommand>) {
        while let Some(message) = rx.recv().await {
            self.handle_router_command(message).await;
        }
    }

    async fn handle_router_command(&mut self, message: RouterCommand) {
        match message {
            RouterCommand::ClientCommand { command, reply } => {
                let _ = reply.send(self.handle(command).await);
            },
            RouterCommand::CreateSession { working_dir, reply } => {
                let _ = reply.send(self.create_session(working_dir).await);
            },
            RouterCommand::SubmitInputForSession {
                session_id,
                text,
                reply,
            } => {
                let _ = reply.send(self.submit_input_for_session(session_id, text).await);
            },
            RouterCommand::CompactSession { session_id, reply } => {
                let _ = reply.send(self.forward_compact(session_id).await);
            },
            RouterCommand::AbortSession { session_id, reply } => {
                let _ = reply.send(self.forward_abort(session_id).await);
            },
            RouterCommand::ListCommandsForSession { session_id, reply } => {
                let _ = reply.send(self.command_infos_for_session(&session_id).await);
            },
            RouterCommand::SubmitInputWithCompletion {
                session_id,
                text,
                reply,
            } => {
                let _ = reply.send(self.forward_submit_with_completion(session_id, text).await);
            },
        }
    }

    async fn forward_compact(
        &self,
        session_id: SessionId,
    ) -> Result<ManualCompactOutcome, HandlerError> {
        self.session_supervisor()?.compact(session_id).await
    }

    async fn forward_abort(&self, session_id: SessionId) -> Result<(), HandlerError> {
        self.session_supervisor()?.abort(session_id).await
    }

    async fn forward_submit_with_completion(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<(TurnId, oneshot::Receiver<TurnCompletion>), HandlerError> {
        self.session_supervisor()?
            .handle_for(&session_id)
            .submit_prompt_with_completion(session_id, text)
            .await
    }
}
