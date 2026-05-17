//! Turn 管理 — 回合生命周期、Agent 任务启停、后台任务清理。
//!
//! per-session SessionActor 是写入者；router 的 submit/abort 都已转发到这里。

use std::sync::Arc;

use astrcode_core::{
    event::{EventPayload, Phase},
    extension::{ExtensionEvent, LifecycleContext},
    llm::{LlmContent, LlmRole},
    storage::SessionReadModel,
    tool::ToolResult,
    types::*,
};
use astrcode_session::{
    EventBus, Session, SessionServices, TurnRunner, agent_turn_completed_payloads,
    agent_turn_failed_payloads, agent_turn_started_payloads, background::BackgroundTaskCompletion,
    run_turn,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use super::{SessionActor, actor::SessionCommand};
use crate::{bootstrap::ServerRuntime, router::HandlerError};

/// 让 turn worker 只能把事件回投给所属 SessionActor。
struct ActorEventBus {
    actor_tx: mpsc::UnboundedSender<SessionCommand>,
}

#[async_trait::async_trait]
impl EventBus for ActorEventBus {
    async fn emit(&self, session_id: &SessionId, turn_id: Option<&TurnId>, payload: EventPayload) {
        let (reply, rx) = oneshot::channel();
        if self
            .actor_tx
            .send(SessionCommand::EmitEvent {
                session_id: session_id.clone(),
                turn_id: turn_id.cloned(),
                payload,
                reply,
            })
            .is_ok()
        {
            let _ = rx.await;
        }
    }
}

/// Agent Turn 的输入参数。
pub(crate) struct AgentTurnInput {
    pub turn_id: TurnId,
    pub session: Arc<Session>,
    pub session_state: SessionReadModel,
    pub text: String,
    pub actor_tx: mpsc::UnboundedSender<SessionCommand>,
}

/// 待处理的工具调用请求。
pub(crate) struct PendingRequestedToolCall {
    pub call_id: String,
    pub tool_name: String,
}

/// Turn 完成结果，通过 oneshot 通道发送。
#[derive(Debug)]
pub(crate) enum TurnCompletion {
    Completed { finish_reason: String },
    Failed { error: String },
    Aborted,
}

/// 正在执行的回合信息，持有 tokio 任务句柄。
pub(crate) struct ActiveTurn {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub handle: JoinHandle<()>,
    pub session: Arc<Session>,
    /// Turn 完成时通知等待者的通道
    pub completion_tx: Option<oneshot::Sender<TurnCompletion>>,
}

impl ActiveTurn {
    pub fn resolve_completion(&mut self, completion: TurnCompletion) {
        if let Some(tx) = self.completion_tx.take() {
            let _ = tx.send(completion);
        }
    }
}

impl SessionActor {
    pub(crate) async fn enqueue_runtime_message(&mut self, text: String) {
        if self.active_turn.is_some() {
            self.mailbox.push_back(text);
            return;
        }
        let session_id = self.session_id.clone();
        if let Err(error) = self
            .start_turn_for_session(session_id, text.clone(), text, None)
            .await
        {
            tracing::warn!(%error, "failed to start mailbox turn");
        }
    }

    /// 提交输入：斜杠命令派发，否则启动新 Turn。
    pub(crate) async fn submit_input_for_session(
        &mut self,
        sid: SessionId,
        text: String,
    ) -> Result<crate::router::PromptSubmission, HandlerError> {
        if let Some(command) = super::slash::parse_slash_command(&text) {
            return self
                .execute_slash_command_for_session(sid, command, text)
                .await;
        }
        self.start_turn_for_session(sid, text.clone(), text, None)
            .await
            .map(|turn_id| crate::router::PromptSubmission::Accepted { turn_id })
    }

    /// 提交提示词并返回完成通知接收器，用于测试或同步等待 Turn 结束。
    pub(crate) async fn submit_input_with_completion(
        &mut self,
        sid: SessionId,
        text: String,
    ) -> Result<(TurnId, oneshot::Receiver<TurnCompletion>), HandlerError> {
        let (tx, rx) = oneshot::channel();
        let turn_id = self
            .start_turn_for_session(sid, text.clone(), text, Some(tx))
            .await?;
        Ok((turn_id, rx))
    }

    /// 启动新 Turn：校验无冲突、恢复会话、创建 Agent 任务。
    pub(crate) async fn start_turn_for_session(
        &mut self,
        sid: SessionId,
        visible_text: String,
        user_text: String,
        completion_tx: Option<oneshot::Sender<TurnCompletion>>,
    ) -> Result<TurnId, HandlerError> {
        tracing::info!(session_id = %sid, text_len = user_text.len(), "start_turn");
        if self.active_turn.is_some() {
            self.send_error(40900, "A turn is already running");
            return Err(HandlerError::TurnAlreadyRunning);
        }

        let session = self
            .runtime
            .session_directory
            .open(sid.clone())
            .await
            .map_err(|e| HandlerError::SessionNotFound(format!("Session {sid} not found: {e}")))?;
        self.repair_stale_pending_tool_calls(&sid)
            .await
            .map_err(HandlerError::Other)?;
        let session_state = session
            .read_model()
            .await
            .map_err(|e| HandlerError::Other(format!("read session {sid}: {e}")))?;

        let turn_id = new_turn_id();
        let session_arc = Arc::new(session);

        for payload in agent_turn_started_payloads(new_message_id(), visible_text) {
            self.emit_session_event(&sid, Some(&turn_id), payload).await;
        }

        let handle = self.spawn_agent_turn(AgentTurnInput {
            turn_id: turn_id.clone(),
            session: Arc::clone(&session_arc),
            session_state,
            text: user_text,
            actor_tx: self.actor_tx.clone(),
        });
        self.active_turn = Some(ActiveTurn {
            session_id: sid,
            turn_id: turn_id.clone(),
            handle,
            session: session_arc,
            completion_tx,
        });
        Ok(turn_id)
    }

    pub(crate) fn spawn_agent_turn(&self, input: AgentTurnInput) -> JoinHandle<()> {
        let runtime = self.runtime.clone();
        tokio::spawn(run_agent_turn_task(runtime, input))
    }

    pub(crate) fn cleanup_agent_turn(
        &mut self,
        session_id: SessionId,
        turn_id: TurnId,
        completion: TurnCompletion,
    ) {
        if !self.active_turn_matches(&session_id, &turn_id) {
            return;
        }
        let Some(mut turn) = self.active_turn.take() else {
            return;
        };
        turn.resolve_completion(completion);
        if let Some(next) = self.mailbox.pop_front() {
            let _ = self
                .actor_tx
                .send(SessionCommand::EnqueueMessage { text: next });
        }
    }

    pub(crate) async fn abort_session(
        &mut self,
        session_id: &SessionId,
    ) -> Result<(), HandlerError> {
        let Some(mut active_turn) = self.active_turn.take() else {
            self.send_error(40400, "No active turn");
            return Err(HandlerError::NoActiveTurn);
        };
        debug_assert_eq!(&active_turn.session_id, session_id);

        let session_state = active_turn
            .session
            .read_model()
            .await
            .map_err(|e| HandlerError::Other(format!("read session for abort: {e}")))?;
        let lifecycle_ctx = LifecycleContext {
            session_id: active_turn.session_id.to_string(),
            working_dir: session_state.working_dir,
            model: astrcode_core::config::ModelSelection::simple(session_state.model_id),
        };
        if let Err(e) = self
            .runtime
            .extension_runner
            .emit_lifecycle(ExtensionEvent::TurnAborted, lifecycle_ctx)
            .await
        {
            tracing::warn!(error = %e, "TurnAborted extension dispatch failed");
        }

        if !active_turn.handle.is_finished() {
            active_turn.handle.abort();
        }
        self.runtime
            .session_directory
            .cleanup_background_tasks(&active_turn.session_id);

        for payload in agent_turn_completed_payloads("aborted".into()) {
            self.emit_session_event(&active_turn.session_id, Some(&active_turn.turn_id), payload)
                .await;
        }
        self.sync_durable_events(&active_turn.session_id).await;

        active_turn.resolve_completion(TurnCompletion::Aborted);
        Ok(())
    }

    pub(crate) fn active_turn_matches(&self, session_id: &SessionId, turn_id: &TurnId) -> bool {
        self.active_turn
            .as_ref()
            .is_some_and(|t| &t.session_id == session_id && &t.turn_id == turn_id)
    }

    pub(crate) async fn repair_stale_pending_tool_calls(
        &self,
        session_id: &SessionId,
    ) -> Result<(), String> {
        if self.active_turn.is_some() {
            return Ok(());
        }

        let state = self
            .runtime
            .session_directory
            .read_model(session_id)
            .await
            .map_err(|e| format!("read session {session_id}: {e}"))?;
        if state.phase != Phase::CallingTool || state.pending_tool_calls.is_empty() {
            return Ok(());
        }

        for pending in pending_requested_tool_calls(&state) {
            self.emit_session_event(
                session_id,
                None,
                EventPayload::ToolCallCompleted {
                    call_id: pending.call_id.clone().into(),
                    tool_name: pending.tool_name,
                    result: interrupted_tool_result(&pending.call_id),
                },
            )
            .await;
        }
        for payload in agent_turn_completed_payloads("interrupted".into()) {
            self.emit_session_event(session_id, None, payload).await;
        }
        self.sync_durable_events(session_id).await;
        Ok(())
    }
}

/// 从会话状态中提取待处理的工具调用请求。
fn pending_requested_tool_calls(state: &SessionReadModel) -> Vec<PendingRequestedToolCall> {
    let mut remaining = state.pending_tool_calls.clone();
    let mut pending = Vec::new();

    for message in &state.messages {
        if message.role != LlmRole::Assistant {
            continue;
        }
        for content in &message.content {
            let LlmContent::ToolCall { call_id, name, .. } = content else {
                continue;
            };
            if remaining.remove(&ToolCallId::from(call_id.clone())) {
                pending.push(PendingRequestedToolCall {
                    call_id: call_id.clone(),
                    tool_name: name.clone(),
                });
            }
        }
    }

    pending
}

fn interrupted_tool_result(call_id: &str) -> ToolResult {
    let content = "Tool execution interrupted before completion".to_string();
    ToolResult {
        call_id: call_id.to_string(),
        content: content.clone(),
        is_error: true,
        error: Some(content),
        metadata: Default::default(),
        duration_ms: None,
    }
}

/// Agent Turn 后台任务：组装 TurnRunner 并驱动 LLM ↔ 工具循环。
async fn run_agent_turn_task(runtime: Arc<ServerRuntime>, input: AgentTurnInput) {
    let AgentTurnInput {
        turn_id,
        session,
        session_state,
        text,
        actor_tx,
    } = input;
    let sid = session.id().clone();

    let (background_result_tx, mut background_result_rx) =
        mpsc::unbounded_channel::<BackgroundTaskCompletion>();
    {
        let bg_actor_tx = actor_tx.clone();
        let handle = tokio::spawn(async move {
            while let Some(completion) = background_result_rx.recv().await {
                let session_id = completion.session_id.clone();
                let (tool_call_event, bg_event) = completion.into_events();
                emit_actor_event(&bg_actor_tx, &session_id, None, tool_call_event).await;
                emit_actor_event(&bg_actor_tx, &session_id, None, bg_event).await;
            }
        });
        tokio::spawn(async move {
            if let Err(e) = handle.await {
                tracing::error!("background result forwarder panicked: {e}");
            }
        });
    }

    let model_id = runtime.config_manager.read_effective().llm.model_id.clone();
    if let Err(e) = session.update_model_id(&model_id).await {
        tracing::warn!(session_id = %sid, error = %e, "failed to update session model_id");
    }

    let working_dir = session_state.working_dir.clone();
    let tool_registry = runtime
        .session_bootstrapper
        .ensure_tool_registry(&sid, &working_dir)
        .await;
    if session_state.system_prompt.is_none() {
        match runtime
            .session_bootstrapper
            .configure_system_prompt(&sid, &working_dir, &tool_registry, None)
            .await
        {
            Ok(payload) => emit_actor_event(&actor_tx, &sid, None, payload).await,
            Err(e) => {
                tracing::warn!(session_id = %sid, error = %e, "configure system prompt failed")
            },
        }
    }

    let mut services = SessionServices::new(
        runtime.config_manager.read_llm_provider(),
        tool_registry,
        runtime.extension_runner.clone(),
        runtime.context_assembler.clone(),
        session,
        runtime.background_tasks.clone(),
        runtime.session_bootstrapper.file_observation_store(&sid),
    )
    .with_background_result_tx(background_result_tx);
    if let Some(supervisor) = runtime.session_supervisor.read().clone() {
        services = services.with_session_messenger(Arc::new(
            crate::session::BoundSessionMessenger::new(sid.clone(), supervisor),
        ));
    }
    let mut agent = match TurnRunner::new(services, &session_state) {
        Ok(agent) => agent,
        Err(e) => {
            for payload in agent_turn_failed_payloads(Some(e.to_string()), "error".into()) {
                emit_actor_event(&actor_tx, &sid, Some(&turn_id), payload).await;
            }
            let _ = actor_tx.send(SessionCommand::AgentTurnCleanup {
                session_id: sid,
                turn_id,
                completion: TurnCompletion::Failed {
                    error: e.to_string(),
                },
            });
            return;
        },
    };

    let actor_event_bus = ActorEventBus {
        actor_tx: actor_tx.clone(),
    };
    let result = run_turn(&mut agent, &text, &turn_id, &actor_event_bus).await;

    match result.output {
        Ok(output) => {
            for payload in agent_turn_completed_payloads(output.finish_reason.clone()) {
                emit_actor_event(&actor_tx, &sid, Some(&turn_id), payload).await;
            }
            let _ = actor_tx.send(SessionCommand::AgentTurnCleanup {
                session_id: sid,
                turn_id,
                completion: TurnCompletion::Completed {
                    finish_reason: output.finish_reason,
                },
            });
        },
        Err(error) => {
            for payload in agent_turn_failed_payloads(
                (!result.emitted_error).then(|| error.to_string()),
                "error".into(),
            ) {
                emit_actor_event(&actor_tx, &sid, Some(&turn_id), payload).await;
            }
            let _ = actor_tx.send(SessionCommand::AgentTurnCleanup {
                session_id: sid,
                turn_id,
                completion: TurnCompletion::Failed {
                    error: error.to_string(),
                },
            });
        },
    }
}

async fn emit_actor_event(
    actor_tx: &mpsc::UnboundedSender<SessionCommand>,
    session_id: &SessionId,
    turn_id: Option<&TurnId>,
    payload: EventPayload,
) {
    let (reply, rx) = oneshot::channel();
    if actor_tx
        .send(SessionCommand::EmitEvent {
            session_id: session_id.clone(),
            turn_id: turn_id.cloned(),
            payload,
            reply,
        })
        .is_ok()
    {
        let _ = rx.await;
    }
}
