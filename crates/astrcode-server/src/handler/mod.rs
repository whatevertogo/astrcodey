//! 命令处理器 — 使用 ServerRuntime 处理客户端命令。
//!
//! 传输层无关：同时被 stdio 二进制和进程内 CLI 使用。
//! 负责将 `ClientCommand` 路由到对应的服务方法，并通过广播通道发送通知。

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use astrcode_core::{
    event::{Event, EventPayload},
    types::*,
};
use astrcode_protocol::{
    commands::{ClientCommand, UiResponseValue},
    events::{ClientNotification, SessionListItem},
};
use tokio::sync::mpsc;

use crate::{
    bootstrap::ServerRuntime, server_event_bus::ServerEventBus,
    session_manager::SessionManagerError,
};

mod actor;
mod compact;
mod model_selection;
mod recap;
pub(crate) mod slash;
pub(crate) mod snapshot;
pub(in crate::handler) mod turn;

pub use actor::CommandHandle;
use actor::CommandMessage;
pub use compact::ManualCompactOutcome;
use model_selection::ModelSelectionController;
#[cfg(test)]
use snapshot::message_to_dto;
use snapshot::session_snapshot;
use turn::ActiveTurn;

/// 用户输入提交结果：被接受进入 Turn，或被斜杠命令处理。
#[derive(Debug)]
pub enum PromptSubmission {
    Accepted { turn_id: TurnId },
    Handled { message: String },
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
    #[error("{0}")]
    Other(String),
}

pub(crate) use turn::TurnCompletion;

/// 命令处理器，处理客户端命令并通过广播通道发送通知。
///
/// 维护当前活跃会话和活跃回合的状态，确保同一时间只有一个回合在运行。
pub struct CommandHandler {
    runtime: Arc<ServerRuntime>,
    /// 事件总线，统一处理持久化和广播
    event_bus: Arc<ServerEventBus>,
    /// 当前活跃的会话 ID
    active_session_id: Option<SessionId>,
    /// 当前正在执行的回合，按 session 隔离
    active_turns: HashMap<SessionId, ActiveTurn>,
    /// 输入排队队列：当 session 正在执行 turn 时，后续输入排队到下一 turn。
    queued_inputs: HashMap<SessionId, VecDeque<String>>,
    /// Actor 消息通道发送端，用于在后台任务中发送消息回 Handler
    actor_tx: mpsc::UnboundedSender<CommandMessage>,
    /// 模型选择流程。
    model_selection: ModelSelectionController,
}

impl CommandHandler {
    // ─── 命令路由 ────────────────────────────────────────────────────────

    /// 处理客户端命令，路由到对应处理方法。
    pub async fn handle(&mut self, cmd: ClientCommand) -> Result<(), HandlerError> {
        match cmd {
            ClientCommand::CreateSession { working_dir } => {
                self.create_session(working_dir).await?;
            },

            ClientCommand::SubmitPrompt { text, .. } => {
                self.submit_prompt(text).await?;
            },

            ClientCommand::InjectMessage { text } => {
                self.inject_mid_turn_message(text).await?;
            },

            ClientCommand::Recap => {
                self.recap_session().await?;
            },

            ClientCommand::ListSessions => {
                let items: Vec<_> = self
                    .runtime
                    .session_manager
                    .list_summaries()
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|summary| SessionListItem {
                        session_id: summary.session_id.into_string(),
                        created_at: summary.created_at,
                        last_active_at: summary.updated_at,
                        working_dir: summary.working_dir.clone(),
                        parent_session_id: summary.parent_session_id.map(SessionId::into_string),
                        title: summary.first_user_message.clone(),
                    })
                    .collect();
                let _ = self
                    .event_bus
                    .broadcast_sender()
                    .send(ClientNotification::SessionList { sessions: items });
            },

            ClientCommand::Abort => {
                self.abort_active_turn().await?;
            },

            ClientCommand::Compact => {
                self.compact_active_session().await?;
            },

            ClientCommand::GetState => {
                self.send_current_state().await;
            },

            ClientCommand::ResumeSession { session_id }
            | ClientCommand::SwitchSession { session_id } => {
                self.resume_session(session_id.into()).await;
            },

            ClientCommand::DeleteSession { session_id } => {
                let session_id = SessionId::from(session_id);
                match self.runtime.session_manager.delete(&session_id).await {
                    Ok(()) => {
                        // 中止该会话的活跃回合并清理资源
                        if let Some(mut turn) = self.active_turns.remove(&session_id) {
                            if !turn.handle.is_finished() {
                                turn.handle.abort();
                            }
                            turn.resolve_completion(turn::TurnCompletion::Aborted);
                        }
                        if self.active_session_id.as_ref() == Some(&session_id) {
                            self.active_session_id = None;
                        }
                        // session 已被销毁，释放 forwarder 占位（同 sid 重建时能重新 attach）
                        self.event_bus.detach(&session_id);
                    },
                    Err(e) => self.send_error(40401, &format!("Session not found: {e}")),
                }
            },

            ClientCommand::ListExtensionCommands => {
                let working_dir = match self.active_session_working_dir().await {
                    Ok(working_dir) => working_dir,
                    Err(error) => {
                        self.send_error(40400, &error);
                        return Ok(());
                    },
                };
                let infos = self.command_infos_for_working_dir(&working_dir).await;
                let keybindings: Vec<astrcode_protocol::events::KeybindingInfoDto> = self
                    .runtime
                    .extension_runner
                    .collect_keybindings()
                    .into_iter()
                    .map(|kb| astrcode_protocol::events::KeybindingInfoDto {
                        key: kb.key,
                        command: kb.command,
                        arguments: kb.arguments,
                        description: kb.description,
                    })
                    .collect();
                let status_items: Vec<astrcode_protocol::events::StatusItemInfoDto> = self
                    .runtime
                    .extension_runner
                    .collect_status_items()
                    .into_iter()
                    .map(|item| astrcode_protocol::events::StatusItemInfoDto {
                        id: item.id,
                        text: item.text,
                        priority: item.priority,
                    })
                    .collect();
                let _ = self.event_bus.broadcast_sender().send(
                    ClientNotification::ExtensionCommandList {
                        commands: infos,
                        keybindings,
                        status_items,
                    },
                );
            },

            ClientCommand::ExecuteExtensionCommand {
                command_name,
                arguments,
            } => {
                let sid = self.ensure_session().await?;
                let visible_text = if arguments.trim().is_empty() {
                    format!("/{command_name}")
                } else {
                    format!("/{command_name} {}", arguments.trim())
                };
                if let Err(error) = self
                    .execute_slash_command_for_session(
                        sid,
                        slash::ParsedSlashCommand {
                            name: command_name,
                            arguments,
                        },
                        visible_text,
                    )
                    .await
                {
                    self.send_error(slash::command_error_code(&error), &error.to_string());
                }
            },

            ClientCommand::ForkSession {
                session_id,
                at_cursor,
            } => {
                self.fork_session(session_id.into(), at_cursor).await?;
            },

            ClientCommand::SetModel { model_id } => {
                self.set_model(model_id).await?;
            },

            ClientCommand::UiResponse { request_id, value } => {
                self.handle_ui_response(request_id, value).await?;
            },

            _ => {
                return Err(HandlerError::Other("Not implemented".into()));
            },
        }
        Ok(())
    }

    // ─── 会话生命周期 ──────────────────────────────────────────────────

    /// 发送当前会话快照，用于客户端初次同步或恢复。
    async fn send_current_state(&mut self) {
        let Some(session_id) = self.active_session_id.as_ref() else {
            self.send_error(40400, "No active session");
            return;
        };
        match self
            .runtime
            .event_store
            .session_read_model(session_id)
            .await
        {
            Ok(state) => {
                let snapshot = session_snapshot(&state);
                self.event_bus
                    .send_notification(ClientNotification::SessionResumed {
                        session_id: session_id.to_string(),
                        snapshot,
                    });
            },
            Err(e) => self.send_error(40401, &format!("Session not found: {e}")),
        }
    }

    /// 创建新会话，分发 SessionStart 事件，初始化工具表和 system prompt。
    pub async fn create_session(&mut self, working_dir: String) -> Result<SessionId, HandlerError> {
        tracing::info!(working_dir = %working_dir, "creating session");
        let created = match self.runtime.session_manager.create(&working_dir).await {
            Ok(created) => created,
            Err(error) => {
                tracing::error!(working_dir = %working_dir, error = %error, "create session failed");
                self.send_error(-32603, &error.to_string());
                return Err(error.into());
            },
        };
        let sid = created.session.id().clone();
        self.event_bus.attach(&created.session);
        self.active_session_id = Some(sid.clone());

        tracing::info!(session_id = %sid, "session created, dispatching SessionStart");
        self.broadcast_event(created.start_event);

        match created.session.initialize_runtime(&working_dir).await {
            Ok(()) => {
                tracing::info!(session_id = %sid, "session fully initialized");
                Ok(sid)
            },
            Err(e) => {
                tracing::error!(session_id = %sid, error = %e, "session prompt init failed");
                self.send_error(-32603, &e.to_string());
                Err(HandlerError::Other(e.to_string()))
            },
        }
    }

    /// 提交用户输入，如有已有 Turn 运行则路由为中途消息注入。
    async fn submit_prompt(&mut self, text: String) -> Result<(), HandlerError> {
        let sid = self.ensure_session().await?;
        match self
            .submit_input_for_session(sid.clone(), text.clone())
            .await
        {
            Ok(_) => Ok(()),
            Err(HandlerError::TurnAlreadyRunning) => {
                // 已有 active turn → 视为中途消息注入（兼容未升级到 InjectMessage 的客户端）
                self.inject_mid_turn_message_for_session(&sid, text).await
            },
            Err(error) => {
                self.send_error(slash::command_error_code(&error), &error.to_string());
                Err(error)
            },
        }
    }

    /// 向正在执行的 turn 注入中途消息。
    async fn inject_mid_turn_message(&mut self, text: String) -> Result<(), HandlerError> {
        let sid = self.ensure_session().await?;
        self.inject_mid_turn_message_for_session(&sid, text).await
    }

    /// 向指定 session 的 active turn 注入中途消息。
    async fn inject_mid_turn_message_for_session(
        &mut self,
        sid: &SessionId,
        text: String,
    ) -> Result<(), HandlerError> {
        let active_turn = self
            .active_turns
            .get(sid)
            .ok_or(HandlerError::NoActiveTurn)?;
        let turn_id = active_turn.turn_id.clone();
        let message_id = new_message_id();
        active_turn
            .session
            .emit_durable(
                Some(&turn_id),
                EventPayload::UserMessage { message_id, text },
            )
            .await
            .map_err(|e| HandlerError::Other(format!("inject message: {e}")))?;
        Ok(())
    }

    /// 向指定会话提交输入。斜杠命令在此被拦截并派发，普通输入启动新 Turn。
    ///
    /// 以 `/` 开头的输入会尝试解析为斜杠命令。如果命令不存在（`UnknownCommand`），
    /// 则 fallback 为普通 prompt 提交——因为 `/` 开头不一定是命令（如路径 `/usr/bin`）。
    pub async fn submit_input_for_session(
        &mut self,
        sid: SessionId,
        text: String,
    ) -> Result<PromptSubmission, HandlerError> {
        if let Some(command) = slash::parse_slash_command(&text) {
            match self
                .execute_slash_command_for_session(sid.clone(), command, text.clone())
                .await
            {
                Err(HandlerError::UnknownCommand(_)) => {
                    // 不是已知命令，当作普通 prompt 处理
                },
                other => return other,
            }
        }

        self.start_turn_for_session(sid, text.clone(), text, None)
            .await
            .map(|turn_id| PromptSubmission::Accepted { turn_id })
    }

    /// 获取指定会话的可用命令列表。
    pub async fn command_infos_for_session(
        &self,
        sid: &SessionId,
    ) -> Result<Vec<astrcode_protocol::events::ExtensionCommandInfo>, HandlerError> {
        let state = self.runtime.session_manager.read_model(sid).await?;
        Ok(self.command_infos_for_working_dir(&state.working_dir).await)
    }

    /// 获取当前会话的工作目录，无活跃会话则返回当前目录。
    async fn active_session_working_dir(&self) -> Result<String, String> {
        let Some(sid) = self.active_session_id.as_ref() else {
            return Ok(std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned());
        };
        self.runtime
            .session_manager
            .read_model(sid)
            .await
            .map(|state| state.working_dir)
            .map_err(|e| format!("read session {sid}: {e}"))
    }

    /// 恢复或切换到指定会话，修复可能的遗留状态后发送快照。
    async fn resume_session(&mut self, session_id: SessionId) {
        match self.runtime.session_manager.open(session_id.clone()).await {
            Ok(session) => {
                self.event_bus.attach(&session);
                if let Err(e) = self.repair_stale_phase(&session_id).await {
                    if !matches!(e, HandlerError::NoActiveTurn) {
                        self.send_error(-32603, &e.to_string());
                        return;
                    }
                }
                let state = match self.runtime.session_manager.read_model(&session_id).await {
                    Ok(state) => state,
                    Err(e) => {
                        self.send_error(40401, &format!("Session not found: {e}"));
                        return;
                    },
                };
                let snapshot = session_snapshot(&state);

                if let Err(e) = session.ensure_runtime_ready().await {
                    self.send_error(-32603, &e.to_string());
                    return;
                }
                self.active_session_id = Some(session_id.clone());
                self.event_bus
                    .send_notification(ClientNotification::SessionResumed {
                        session_id: session_id.into_string(),
                        snapshot,
                    });
            },
            Err(e) => self.send_error(40401, &format!("Session not found: {e}")),
        }
    }

    // ─── 工具表与提示词配置 ────────────────────────────────────────────

    /// 确保存在活跃会话，无则自动创建。
    async fn ensure_session(&mut self) -> Result<SessionId, HandlerError> {
        if let Some(ref sid) = self.active_session_id {
            return Ok(sid.clone());
        }

        let wd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".into());
        self.create_session(wd).await
    }

    // ─── 事件记录与内部辅助 ──────────────────────────────────────────

    fn broadcast_event(&self, event: Event) {
        self.event_bus
            .send_notification(ClientNotification::Event(event));
    }

    /// 发送错误通知给客户端。
    fn send_error(&self, code: i32, message: &str) {
        self.event_bus.send_notification(ClientNotification::Error {
            code,
            message: message.into(),
        });
    }

    // ─── Fork ─────────────────────────────────────────────────────────

    /// Fork 源会话，创建新 session 并切换到新 session。
    ///
    /// 新 session 继承源 session fork 点之前的完整消息前缀和 system prompt，
    /// 保证 provider 侧 KV 缓存命中。
    pub async fn fork_session(
        &mut self,
        source_id: SessionId,
        at_cursor: Option<String>,
    ) -> Result<SessionId, HandlerError> {
        let forked = self
            .runtime
            .session_manager
            .fork(&source_id, at_cursor.as_ref())
            .await
            .map_err(|e| HandlerError::Other(format!("fork session: {e}")))?;

        let new_sid = forked.session.id().clone();
        self.event_bus.attach(&forked.session);
        self.active_session_id = Some(new_sid.clone());

        // 初始化 runtime（工具表在新 session 上需要重建）
        let working_dir = self
            .runtime
            .session_manager
            .read_model(&new_sid)
            .await
            .map(|m| m.working_dir)
            .unwrap_or_else(|_| ".".into());
        if let Err(e) = forked.session.initialize_runtime(&working_dir).await {
            tracing::warn!(session_id = %new_sid, error = %e, "fork: runtime init failed");
        }

        // 通知客户端
        let state = self
            .runtime
            .session_manager
            .read_model(&new_sid)
            .await
            .map_err(|e| HandlerError::Other(e.to_string()))?;
        let snapshot = session_snapshot(&state);
        self.event_bus
            .send_notification(ClientNotification::SessionResumed {
                session_id: new_sid.clone().into_string(),
                snapshot,
            });

        tracing::info!(
            source_session_id = %source_id,
            new_session_id = %new_sid,
            "session forked"
        );
        Ok(new_sid)
    }

    // ─── 模型选择 ───────────────────────────────────────────────────────

    /// 设置当前会话使用的主模型，格式为 `profile/model`。
    async fn set_model(&mut self, model_id: String) -> Result<(), HandlerError> {
        let notification = match self.model_selection.set_main_model(&model_id).await {
            Ok(notification) => notification,
            Err(HandlerError::Other(message))
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
        self.event_bus.send_notification(notification);

        Ok(())
    }

    /// 启动交互式模型选择流程。
    pub(in crate::handler) async fn start_model_selection(&mut self) -> Result<(), HandlerError> {
        let notification = self.model_selection.start();
        self.event_bus.send_notification(notification);
        Ok(())
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
        self.event_bus.send_notification(notification);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
