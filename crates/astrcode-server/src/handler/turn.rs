//! Turn 管理 — 回合生命周期、Agent 任务启停、后台任务清理。

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
use astrcode_tools::registry::ToolRegistry;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use super::{CommandHandler, CommandMessage, HandlerError};
use crate::{bootstrap::ServerRuntime, server_event_bus::ServerEventBus};

/// Agent Turn 的输入参数，用于启动后台任务。
pub(in crate::handler) struct AgentTurnInput {
    pub turn_id: TurnId,
    pub session: Arc<Session>,
    pub tool_registry: Arc<ToolRegistry>,
    pub text: String,
    /// 斜杠命令注入的一次性指令
    pub transient_instructions: Option<String>,
    pub actor_tx: mpsc::UnboundedSender<CommandMessage>,
    pub event_bus: Arc<ServerEventBus>,
}

/// 待处理的工具调用请求。
pub(in crate::handler) struct PendingRequestedToolCall {
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
pub(in crate::handler) struct ActiveTurn {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub handle: JoinHandle<()>,
    pub session: Arc<Session>,
    /// Turn 完成时通知等待者的通道
    pub completion_tx: Option<oneshot::Sender<TurnCompletion>>,
}

impl ActiveTurn {
    /// 发送完成通知（如果有等待者）。
    pub fn resolve_completion(&mut self, completion: TurnCompletion) {
        if let Some(tx) = self.completion_tx.take() {
            let _ = tx.send(completion);
        }
    }
}

impl CommandHandler {
    /// 提交提示词并返回完成通知接收器，用于测试等待 Turn 结束。
    pub(in crate::handler) async fn submit_input_with_completion(
        &mut self,
        sid: SessionId,
        text: String,
    ) -> Result<(TurnId, oneshot::Receiver<TurnCompletion>), HandlerError> {
        let (tx, rx) = oneshot::channel();
        let turn_id = self
            .start_turn_for_session(sid, text.clone(), text, None, Some(tx))
            .await?;
        Ok((turn_id, rx))
    }

    /// 启动新 Turn：校验无冲突、恢复会话、创建 Agent 任务。
    pub(in crate::handler) async fn start_turn_for_session(
        &mut self,
        sid: SessionId,
        visible_text: String,
        user_text: String,
        transient_instructions: Option<String>,
        completion_tx: Option<oneshot::Sender<TurnCompletion>>,
    ) -> Result<TurnId, HandlerError> {
        tracing::info!(session_id = %sid, text_len = user_text.len(), "start_turn");
        // 拒绝：已有 Turn 在运行
        // TODO: 支持排队
        if self.active_turns.contains_key(&sid) {
            self.send_error(40900, "A turn is already running");
            return Err(HandlerError::TurnAlreadyRunning);
        }

        // 恢复会话并修复可能的遗留状态
        let session = self
            .runtime
            .session_manager
            .open(sid.clone())
            .await
            .map_err(|e| HandlerError::SessionNotFound(format!("Session {sid} not found: {e}")))?;
        self.repair_stale_pending_tool_calls(&sid)
            .await
            .map_err(HandlerError::Other)?;
        // 读取会话状态
        let state = session
            .read_model()
            .await
            .map_err(|e| HandlerError::Other(format!("read session {sid}: {e}")))?;
        let working_dir = state.working_dir;
        let tool_registry = self.ensure_tool_registry(&sid, &working_dir).await;
        // 如未配置 system prompt，自动配置（写入 session 事件）
        if state.system_prompt.is_none() {
            self.configure_session_prompt(&sid, &working_dir, &tool_registry, None)
                .await
                .map_err(HandlerError::Other)?;
        }
        let turn_id = new_turn_id();
        let session_arc = Arc::new(session);

        // 记录 Turn 开始事件
        for payload in agent_turn_started_payloads(new_message_id(), visible_text) {
            self.event_bus.emit(&sid, Some(&turn_id), payload).await;
        }

        // 启动 Agent 后台任务
        let handle = self.spawn_agent_turn(AgentTurnInput {
            turn_id: turn_id.clone(),
            session: Arc::clone(&session_arc),
            tool_registry: Arc::clone(&tool_registry),
            text: user_text,
            transient_instructions,
            actor_tx: self.actor_tx.clone(),
            event_bus: Arc::clone(&self.event_bus),
        });
        self.active_turns.insert(
            sid.clone(),
            ActiveTurn {
                session_id: sid,
                turn_id: turn_id.clone(),
                handle,
                session: session_arc,
                completion_tx,
            },
        );
        Ok(turn_id)
    }

    /// 在后台启动 Agent Turn 任务。
    pub(in crate::handler) fn spawn_agent_turn(&self, input: AgentTurnInput) -> JoinHandle<()> {
        let runtime = self.runtime.clone();
        tokio::spawn(run_agent_turn_task(runtime, input))
    }

    /// 清理已完成的 Agent Turn（终态事件已由 turn task 广播，此处仅做 map 清理）。
    pub(in crate::handler) fn cleanup_agent_turn(
        &mut self,
        session_id: SessionId,
        turn_id: TurnId,
        completion: TurnCompletion,
    ) {
        if !self.active_turn_matches(&session_id, &turn_id) {
            return;
        }
        let Some(mut turn) = self.active_turns.remove(&session_id) else {
            return;
        };
        turn.resolve_completion(completion);
    }

    /// 中止指定会话的活跃 Turn。
    pub(in crate::handler) async fn abort_session(
        &mut self,
        session_id: &SessionId,
    ) -> Result<(), HandlerError> {
        let Some(mut active_turn) = self.active_turns.remove(session_id) else {
            self.send_error(40400, "No active turn");
            return Err(HandlerError::NoActiveTurn);
        };

        // 从 session 读取 working_dir 和 model_id 构建 lifecycle context
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

        // 中止后台任务并清理
        if !active_turn.handle.is_finished() {
            active_turn.handle.abort();
        }
        self.runtime
            .session_manager
            .cleanup_background_tasks(&active_turn.session_id);

        // 记录中止完成事件
        for payload in agent_turn_completed_payloads("aborted".into()) {
            self.event_bus
                .emit(&active_turn.session_id, Some(&active_turn.turn_id), payload)
                .await;
        }
        self.event_bus
            .sync_durable_events(&active_turn.session_id)
            .await;

        active_turn.resolve_completion(TurnCompletion::Aborted);
        Ok(())
    }

    /// 中止当前活跃会话的 Turn。
    pub(in crate::handler) async fn abort_active_turn(&mut self) -> Result<(), HandlerError> {
        let Some(sid) = self.active_session_id.clone() else {
            self.send_error(40400, "No active turn");
            return Ok(());
        };
        self.abort_session(&sid).await
    }

    /// 校验 Turn 是否仍有效（未被中止或替换）。
    pub(in crate::handler) fn active_turn_matches(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> bool {
        self.active_turns
            .get(session_id)
            .is_some_and(|active_turn| &active_turn.turn_id == turn_id)
    }

    /// 修复遗留的待处理工具调用状态（如服务重启后）。
    pub(in crate::handler) async fn repair_stale_pending_tool_calls(
        &self,
        session_id: &SessionId,
    ) -> Result<(), String> {
        // 如有活跃 Turn，不处理（正常流程中处理）
        if self.active_turns.contains_key(session_id) {
            return Ok(());
        }

        let state = self
            .runtime
            .session_manager
            .read_model(session_id)
            .await
            .map_err(|e| format!("read session {session_id}: {e}"))?;
        // 仅处理 CallingTool 阶段且有待处理调用的会话
        if state.phase != Phase::CallingTool || state.pending_tool_calls.is_empty() {
            return Ok(());
        }

        // 标记所有待处理调用为中断
        for pending in pending_requested_tool_calls(&state) {
            self.event_bus
                .emit(
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
        // 标记 Turn 为中断完成
        for payload in agent_turn_completed_payloads("interrupted".into()) {
            self.event_bus.emit(session_id, None, payload).await;
        }
        self.event_bus.sync_durable_events(session_id).await;
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

/// 创建中断状态的工具调用结果。
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
        tool_registry,
        text,
        transient_instructions,
        actor_tx,
        event_bus,
    } = input;
    let sid = session.id().clone();

    // 后台子任务结果通过 EventBus 直接持久化+广播，不经过 Actor
    let (background_result_tx, mut background_result_rx) =
        mpsc::unbounded_channel::<BackgroundTaskCompletion>();
    {
        let bg_event_bus = Arc::clone(&event_bus);
        let handle = tokio::spawn(async move {
            while let Some(completion) = background_result_rx.recv().await {
                let session_id = completion.session_id.clone();
                let (tool_call_event, bg_event) = completion.into_events();
                bg_event_bus.emit(&session_id, None, tool_call_event).await;
                bg_event_bus.emit(&session_id, None, bg_event).await;
                bg_event_bus.sync_durable_events(&session_id).await;
            }
        });
        tokio::spawn(async move {
            if let Err(e) = handle.await {
                tracing::error!("background result forwarder panicked: {e}");
            }
        });
    }

    // model_id 来自 runtime 配置（可被热更新覆盖 session 创建时的值）
    // 仅在 model_id 与 session 当前值不同时写入事件
    let model_id = runtime.config.read_effective().llm.model_id.clone();
    if let Err(e) = session.update_model_id(&model_id).await {
        tracing::warn!(session_id = %sid, error = %e, "failed to update session model_id");
    }

    // 组装 TurnRunner（从 session 读取所有事实）
    let agent_session_control = runtime.agent_session_control.read().clone();
    let agent = match TurnRunner::new(
        SessionServices::new(
            runtime.config.read_llm_provider(),
            tool_registry,
            runtime.extension_runner.clone(),
            runtime.context_assembler.clone(),
            session,
            runtime.background_tasks.clone(),
            runtime.session_manager.file_observation_store(&sid),
        )
        .with_background_result_tx(background_result_tx)
        .with_agent_session_control(agent_session_control),
    )
    .await
    {
        Ok(agent) => agent,
        Err(e) => {
            for payload in agent_turn_failed_payloads(Some(e.to_string()), "error".into()) {
                event_bus.emit(&sid, Some(&turn_id), payload).await;
            }
            let _ = actor_tx.send(CommandMessage::AgentTurnCleanup {
                session_id: sid,
                turn_id,
                completion: TurnCompletion::Failed {
                    error: e.to_string(),
                },
            });
            return;
        },
    };

    // 驱动 Agent 循环，事件通过 EventBus 直接持久化+广播
    let result = run_turn(
        &agent,
        &text,
        transient_instructions,
        &turn_id,
        event_bus.as_ref(),
    )
    .await;

    // 终态事件通过 EventBus 直接广播，避免绕 actor 通道导致的 SSE 延迟。
    match result.output {
        Ok(output) => {
            for payload in agent_turn_completed_payloads(output.finish_reason.clone()) {
                event_bus.emit(&sid, Some(&turn_id), payload).await;
            }
            event_bus.sync_durable_events(&sid).await;
            let _ = actor_tx.send(CommandMessage::AgentTurnCleanup {
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
                event_bus.emit(&sid, Some(&turn_id), payload).await;
            }
            event_bus.sync_durable_events(&sid).await;
            let _ = actor_tx.send(CommandMessage::AgentTurnCleanup {
                session_id: sid,
                turn_id,
                completion: TurnCompletion::Failed {
                    error: error.to_string(),
                },
            });
        },
    }
}
