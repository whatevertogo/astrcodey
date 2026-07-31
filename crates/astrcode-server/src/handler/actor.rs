//! 交互式 CommandHandler 的异步消息驱动封装。
//!
//! `CommandHandle` 保留 focus、UI 选择等有状态交互的串行语义；HTTP/ACP
//! 这类显式携带 session ID 的请求直接使用 `SessionCommandService`。

use std::sync::Arc;

use astrcode_protocol::commands::ClientCommand;
use tokio::sync::{mpsc, oneshot};

use super::{CommandHandler, HandlerError};
use crate::{
    bootstrap::ServerRuntime,
    session_command_service::SessionCommandService,
    turn_scheduler::{TurnCompletion, TurnCompletionEvent, TurnScheduler},
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

    /// 发送客户端命令，等待执行完成。
    pub async fn handle(&self, command: ClientCommand) -> Result<(), HandlerError> {
        let (reply, rx) = oneshot::channel();
        self.post(CommandMessage::ClientCommand { command, reply })
            .await?;
        rx.await.map_err(|_| HandlerError::ActorUnavailable)?
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
}

/// Actor 内部消息只承载需要串行访问交互状态的操作。
pub(in crate::handler) enum CommandMessage {
    ClientCommand {
        command: ClientCommand,
        reply: oneshot::Sender<Result<(), HandlerError>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

impl CommandHandler {
    /// 创建新的 Handler 实例。
    pub(super) fn new(
        runtime: Arc<ServerRuntime>,
        scheduler: Arc<TurnScheduler>,
        event_bus: Arc<crate::server_event_bus::ServerEventBus>,
        session_commands: SessionCommandService,
    ) -> Self {
        let model_selection =
            super::model_selection::ModelSelectionController::new(runtime.config_manager().clone());
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
        session_commands: SessionCommandService,
    ) -> CommandHandle {
        let (tx, rx) = mpsc::channel(COMMAND_ACTOR_CAPACITY);
        let mut handler = Self::new(runtime, scheduler, event_bus, session_commands);
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
                        }
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
            CommandMessage::Shutdown { reply } => {
                let _ = reply.send(());
            },
        }
    }
}
