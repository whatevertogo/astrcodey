//! Per-session 写入者 actor 与命令枚举。
//!
//! `SessionActor` 是单个 session 的唯一可变写入者；`SessionHandle` 是 supervisor
//! 缓存的薄壳。所有会改变 session durable state 的命令都通过 `SessionCommand`
//! 进入 actor mailbox。

use std::{collections::VecDeque, sync::Arc};

use astrcode_core::{
    event::{Event, EventPayload},
    types::{SessionId, TurnId},
};
use tokio::sync::{mpsc, oneshot};

use super::turn::ActiveTurn;
use crate::{
    bootstrap::ServerRuntime,
    events::ClientEventPublisher,
    router::{HandlerError, ManualCompactOutcome, PromptSubmission, TurnCompletion},
};

/// per-session actor 命令枚举。
pub(crate) enum SessionCommand {
    SubmitInput {
        session_id: SessionId,
        text: String,
        reply: oneshot::Sender<Result<PromptSubmission, HandlerError>>,
    },
    SubmitInputWithCompletion {
        session_id: SessionId,
        text: String,
        reply: oneshot::Sender<Result<(TurnId, oneshot::Receiver<TurnCompletion>), HandlerError>>,
    },
    Abort {
        session_id: SessionId,
        reply: oneshot::Sender<Result<(), HandlerError>>,
    },
    Compact {
        session_id: SessionId,
        reply: oneshot::Sender<Result<ManualCompactOutcome, HandlerError>>,
    },
    EnqueueMessage {
        text: String,
    },
    EmitEvent {
        session_id: SessionId,
        turn_id: Option<TurnId>,
        payload: EventPayload,
        reply: oneshot::Sender<()>,
    },
    AgentTurnCleanup {
        session_id: SessionId,
        turn_id: TurnId,
        completion: TurnCompletion,
    },
    Repair {
        session_id: SessionId,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

/// 单个 session 的内部 actor 句柄。supervisor 缓存它；外部不直接持有。
#[derive(Clone)]
pub struct SessionHandle {
    pub(super) tx: mpsc::UnboundedSender<SessionCommand>,
}

impl SessionHandle {
    pub(crate) async fn submit_input(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<PromptSubmission, HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(SessionCommand::SubmitInput {
                session_id,
                text,
                reply,
            })
            .map_err(|_| HandlerError::Other("session actor is unavailable".into()))?;
        rx.await
            .map_err(|_| HandlerError::Other("session actor dropped response".into()))?
    }

    pub(crate) async fn submit_prompt_with_completion(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<(TurnId, oneshot::Receiver<TurnCompletion>), HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(SessionCommand::SubmitInputWithCompletion {
                session_id,
                text,
                reply,
            })
            .map_err(|_| HandlerError::Other("session actor is unavailable".into()))?;
        rx.await
            .map_err(|_| HandlerError::Other("session actor dropped response".into()))?
    }

    pub(crate) async fn abort(&self, session_id: SessionId) -> Result<(), HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(SessionCommand::Abort { session_id, reply })
            .map_err(|_| HandlerError::Other("session actor is unavailable".into()))?;
        rx.await
            .map_err(|_| HandlerError::Other("session actor dropped response".into()))?
    }

    pub(crate) async fn compact(
        &self,
        session_id: SessionId,
    ) -> Result<ManualCompactOutcome, HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(SessionCommand::Compact { session_id, reply })
            .map_err(|_| HandlerError::Other("session actor is unavailable".into()))?;
        rx.await
            .map_err(|_| HandlerError::Other("session actor dropped response".into()))?
    }

    pub(crate) fn enqueue_message(&self, text: String) {
        let _ = self.tx.send(SessionCommand::EnqueueMessage { text });
    }

    pub(crate) async fn emit_session_event(
        &self,
        session_id: SessionId,
        turn_id: Option<TurnId>,
        payload: EventPayload,
    ) -> Result<(), HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(SessionCommand::EmitEvent {
                session_id,
                turn_id,
                payload,
                reply,
            })
            .map_err(|_| HandlerError::Other("session actor is unavailable".into()))?;
        rx.await
            .map_err(|_| HandlerError::Other("session actor dropped response".into()))
    }

    #[cfg(test)]
    pub(crate) fn agent_turn_cleanup(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        completion: TurnCompletion,
    ) {
        let _ = self.tx.send(SessionCommand::AgentTurnCleanup {
            session_id,
            turn_id,
            completion,
        });
    }

    pub(crate) async fn repair_stale_pending_tool_calls(
        &self,
        session_id: SessionId,
    ) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(SessionCommand::Repair { session_id, reply })
            .map_err(|_| "session actor is unavailable".to_string())?;
        rx.await
            .map_err(|_| "session actor dropped response".to_string())?
    }
}

/// 单个 session 的写入者 actor。
pub struct SessionActor {
    pub(crate) session_id: SessionId,
    pub(crate) runtime: Arc<ServerRuntime>,
    pub(crate) event_publisher: Arc<ClientEventPublisher>,
    pub(crate) actor_tx: mpsc::UnboundedSender<SessionCommand>,
    pub(crate) active_turn: Option<ActiveTurn>,
    pub(crate) mailbox: VecDeque<String>,
}

impl SessionActor {
    fn new(
        session_id: SessionId,
        runtime: Arc<ServerRuntime>,
        event_publisher: Arc<ClientEventPublisher>,
        actor_tx: mpsc::UnboundedSender<SessionCommand>,
    ) -> Self {
        Self {
            session_id,
            runtime,
            event_publisher,
            actor_tx,
            active_turn: None,
            mailbox: VecDeque::new(),
        }
    }

    /// 启动一个只服务单个 session 的 actor。
    pub(crate) fn spawn(
        runtime: Arc<ServerRuntime>,
        event_publisher: Arc<ClientEventPublisher>,
        session_id: SessionId,
    ) -> SessionHandle {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut actor = Self::new(session_id, runtime, event_publisher, tx.clone());
        let handle = tokio::spawn(async move { actor.run(rx).await });
        tokio::spawn(async move {
            if let Err(e) = handle.await {
                tracing::error!("session actor panicked: {e}");
            }
        });
        SessionHandle { tx }
    }

    async fn run(&mut self, mut rx: mpsc::UnboundedReceiver<SessionCommand>) {
        while let Some(message) = rx.recv().await {
            self.handle_session_command(message).await;
        }
    }

    async fn handle_session_command(&mut self, message: SessionCommand) {
        match message {
            SessionCommand::SubmitInput {
                session_id,
                text,
                reply,
            } => {
                let _ = reply.send(self.submit_input_for_session(session_id, text).await);
            },
            SessionCommand::SubmitInputWithCompletion {
                session_id,
                text,
                reply,
            } => {
                let _ = reply.send(self.submit_input_with_completion(session_id, text).await);
            },
            SessionCommand::Abort { session_id, reply } => {
                let _ = reply.send(self.abort_session(&session_id).await);
            },
            SessionCommand::Compact { session_id, reply } => {
                let _ = reply.send(self.compact_session(&session_id).await);
            },
            SessionCommand::EnqueueMessage { text } => {
                self.enqueue_runtime_message(text).await;
            },
            SessionCommand::EmitEvent {
                session_id,
                turn_id,
                payload,
                reply,
            } => {
                self.emit_session_event(&session_id, turn_id.as_ref(), payload)
                    .await;
                let _ = reply.send(());
            },
            SessionCommand::AgentTurnCleanup {
                session_id,
                turn_id,
                completion,
            } => {
                self.cleanup_agent_turn(session_id, turn_id, completion);
            },
            SessionCommand::Repair { session_id, reply } => {
                let _ = reply.send(self.repair_stale_pending_tool_calls(&session_id).await);
            },
        }
    }

    pub(crate) fn broadcast_event(&self, event: Event) {
        self.event_publisher
            .publish(astrcode_protocol::events::ClientNotification::Event(event));
    }

    pub(crate) fn send_error(&self, code: i32, message: &str) {
        self.event_publisher
            .publish(astrcode_protocol::events::ClientNotification::Error {
                code,
                message: message.into(),
            });
    }

    pub(crate) async fn emit_session_event(
        &self,
        session_id: &SessionId,
        turn_id: Option<&TurnId>,
        payload: EventPayload,
    ) {
        let event = Event::new(session_id.clone(), turn_id.cloned(), payload);
        if event.payload.is_durable() {
            if let Err(error) = self.runtime.event_store.append_event(event.clone()).await {
                tracing::error!(session_id = %session_id, %error, "failed to persist actor event");
                return;
            }
        }
        self.event_publisher
            .publish(astrcode_protocol::events::ClientNotification::Event(event));
    }

    pub(crate) async fn sync_durable_events(&self, session_id: &SessionId) {
        if let Err(error) = self
            .runtime
            .event_store
            .sync_durable_events(session_id)
            .await
        {
            tracing::error!(session_id = %session_id, %error, "failed to sync durable events");
        }
    }
}
