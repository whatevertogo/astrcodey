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
    Session, agent_turn_completed_durable_payload, agent_turn_completed_live_payload,
    agent_turn_started_durable_payloads, agent_turn_started_live_payload,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::{AbortHandle, JoinHandle},
};

use super::{CommandHandler, CommandMessage, HandlerError};
use crate::server_event_bus::ServerEventBus;

/// Agent Turn 的输入参数，用于启动后台任务。
pub(in crate::handler) struct AgentTurnInput {
    pub turn_id: TurnId,
    pub session: Arc<Session>,
    pub text: String,
    pub actor_tx: mpsc::UnboundedSender<CommandMessage>,
    pub event_bus: Arc<ServerEventBus>,
    pub inner_abort_handle: Arc<parking_lot::Mutex<Option<AbortHandle>>>,
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
    pub inner_abort_handle: Arc<parking_lot::Mutex<Option<AbortHandle>>>,
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
            .start_turn_for_session(sid, text.clone(), text, Some(tx))
            .await?;
        Ok((turn_id, rx))
    }

    /// 启动新 Turn：校验无冲突、恢复会话、创建 Agent 任务。
    pub(in crate::handler) async fn start_turn_for_session(
        &mut self,
        sid: SessionId,
        visible_text: String,
        user_text: String,
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
        // attach 是幂等的；此处保证 session 已经接入 event_bus 的 broadcast 桥。
        // 测试或外部直连 event_store 创建的 session 在此首次接入。
        self.event_bus.attach(&session);
        if let Err(e) = self.repair_stale_phase(&sid).await {
            if !matches!(e, HandlerError::NoActiveTurn) {
                return Err(e);
            }
        }

        let turn_id = new_turn_id();
        let session_arc = Arc::new(session);

        // 记录 Turn 开始事件（durable）
        for payload in agent_turn_started_durable_payloads(new_message_id(), visible_text) {
            session_arc
                .emit_durable(Some(&turn_id), payload)
                .await
                .map_err(|e| HandlerError::Other(format!("persist turn start event: {e}")))?;
        }
        session_arc
            .emit_live(Some(&turn_id), agent_turn_started_live_payload())
            .await;

        // 启动 Agent 后台任务（Session::submit 内部刷新 tool registry / system prompt）
        let inner_abort_handle = Arc::new(parking_lot::Mutex::new(None));
        let handle = self.spawn_agent_turn(AgentTurnInput {
            turn_id: turn_id.clone(),
            session: Arc::clone(&session_arc),
            text: user_text,
            actor_tx: self.actor_tx.clone(),
            event_bus: Arc::clone(&self.event_bus),
            inner_abort_handle: Arc::clone(&inner_abort_handle),
        });
        self.active_turns.insert(
            sid.clone(),
            ActiveTurn {
                session_id: sid,
                turn_id: turn_id.clone(),
                handle,
                session: session_arc,
                inner_abort_handle,
                completion_tx,
            },
        );
        Ok(turn_id)
    }

    /// 在后台启动 Agent Turn 任务。
    pub(in crate::handler) fn spawn_agent_turn(&self, input: AgentTurnInput) -> JoinHandle<()> {
        tokio::spawn(run_agent_turn_task(input))
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
    ///
    /// 优先中止内存中的 `ActiveTurn`；若不存在（如进程重启后 phase 残留），
    /// 则检测并修复过期的非 Idle phase。
    pub(in crate::handler) async fn abort_session(
        &mut self,
        session_id: &SessionId,
    ) -> Result<(), HandlerError> {
        // 快路径：内存中有活跃 Turn
        if let Some(active_turn) = self.active_turns.remove(session_id) {
            self.abort_active_turn_inner(active_turn).await;
            return Ok(());
        }

        // 慢路径：无 ActiveTurn，尝试修复过期 phase
        let result = self.repair_stale_phase(session_id).await;
        if let Err(HandlerError::NoActiveTurn) = &result {
            self.send_error(40400, "No active turn");
        }
        result
    }

    /// 执行活跃 Turn 的中止逻辑：扩展通知、任务清理、事件写入。
    async fn abort_active_turn_inner(&self, mut active_turn: ActiveTurn) {
        // 从 session 读取 working_dir 和 model_id 构建 lifecycle context
        if let Ok(session_state) = active_turn.session.read_model().await {
            let lifecycle_ctx = LifecycleContext {
                session_id: active_turn.session_id.to_string(),
                working_dir: session_state.working_dir,
                model: astrcode_core::config::ModelSelection::simple(session_state.model_id),
                extension_event_sink: None,
                last_exchange: None,
            };
            if let Err(e) = self
                .runtime
                .extension_runner
                .emit_lifecycle(ExtensionEvent::TurnAborted, lifecycle_ctx)
                .await
            {
                tracing::warn!(error = %e, "TurnAborted extension dispatch failed");
            }
        } else {
            tracing::warn!(session_id = %active_turn.session_id, "read session for abort failed, skipping extension notification");
        }

        // 中止后台任务并清理：从 active_turn 的 session runtime 上找到 bg_tasks，
        // cleanup_session 会 abort 该 session 内所有挂着的工具/子 agent task。
        if !active_turn.handle.is_finished() {
            if let Some(handle) = active_turn.inner_abort_handle.lock().take() {
                handle.abort();
            }
            active_turn.handle.abort();
        }
        active_turn
            .session
            .runtime()
            .background_tasks()
            .lock()
            .cleanup_session(&active_turn.session_id);

        // 记录中止完成事件
        let _ = active_turn
            .session
            .emit_durable(
                Some(&active_turn.turn_id),
                agent_turn_completed_durable_payload("aborted".into()),
            )
            .await;
        active_turn
            .session
            .emit_live(
                Some(&active_turn.turn_id),
                agent_turn_completed_live_payload("aborted".into()),
            )
            .await;
        self.event_bus
            .sync_durable_events(&active_turn.session_id)
            .await;

        active_turn.resolve_completion(TurnCompletion::Aborted);
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

    /// 修复进程重启后残留的非 Idle phase。
    ///
    /// 覆盖所有非 Idle/Error phase（Thinking / Streaming / CallingTool / Compacting 等）。
    /// 当应用在操作中途被关闭时，内存中的 `ActiveTurn` 等瞬态丢失，但
    /// 持久化的 session phase 仍为非 Idle。本方法写入 `TurnCompleted(interrupted)`
    /// 将 phase 恢复为 Idle。
    ///
    /// 对于 `CallingTool` 阶段，还会先补写未完成工具调用的中断结果。
    ///
    /// 返回值：
    /// - `Ok(())`：有活跃 Turn 跳过，或成功修复了过期 phase
    /// - `Err(NoActiveTurn)`：session 已经是 Idle/Error，无需修复
    /// - `Err(SessionNotFound)` / `Err(Other)`：存储层错误
    pub(in crate::handler) async fn repair_stale_phase(
        &self,
        session_id: &SessionId,
    ) -> Result<(), HandlerError> {
        if self.active_turns.contains_key(session_id) {
            return Ok(());
        }

        let session = self
            .runtime
            .session_manager
            .open(session_id.clone())
            .await
            .map_err(|e| {
                HandlerError::SessionNotFound(format!("Session {session_id} not found: {e}"))
            })?;
        self.event_bus.attach(&session);
        let state = session
            .read_model()
            .await
            .map_err(|e| HandlerError::Other(format!("read session {session_id}: {e}")))?;

        if matches!(state.phase, Phase::Idle | Phase::Error) {
            return Err(HandlerError::NoActiveTurn);
        }

        tracing::info!(
            session_id = %session_id,
            phase = ?state.phase,
            "repairing stale turn phase after process restart"
        );

        // CallingTool 阶段可能有未完成的工具调用，需要先补写中断结果
        for pending in pending_requested_tool_calls(&state) {
            session
                .emit_durable(
                    None,
                    EventPayload::ToolCallCompleted {
                        call_id: pending.call_id.clone().into(),
                        tool_name: pending.tool_name,
                        result: interrupted_tool_result(&pending.call_id),
                    },
                )
                .await
                .map_err(|e| {
                    HandlerError::Other(format!("emit ToolCallCompleted during stale repair: {e}"))
                })?;
        }

        session
            .emit_durable(
                None,
                agent_turn_completed_durable_payload("interrupted".into()),
            )
            .await
            .map_err(|e| {
                HandlerError::Other(format!("emit TurnCompleted during stale repair: {e}"))
            })?;
        session
            .emit_live(
                None,
                agent_turn_completed_live_payload("interrupted".into()),
            )
            .await;
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

/// Agent Turn 后台任务：通过 `Session::submit` 启动，等待完成后写终态事件。
///
/// 历史上这里手动装配 `TurnRunner` 与 background forwarder，现在已搬到
/// `Session::submit` 内部。本函数只负责：
/// 1. 调用 `Session::submit` 启动 turn；
/// 2. 等待 `TurnHandle::wait` 拿到 `RunTurnResult`；
/// 3. 写 `TurnCompleted` / `TurnFailed` 事件并通知 actor 清理。
async fn run_agent_turn_task(input: AgentTurnInput) {
    let AgentTurnInput {
        turn_id,
        session,
        text,
        actor_tx,
        event_bus,
        inner_abort_handle,
    } = input;
    let sid = session.id().clone();

    let handle = match session.submit(text, turn_id.clone()).await {
        Ok(handle) => handle,
        Err(e) => {
            session
                .emit_durable(
                    Some(&turn_id),
                    EventPayload::ErrorOccurred {
                        code: -32603,
                        message: e.to_string(),
                        recoverable: false,
                    },
                )
                .await
                .ok();
            let _ = session
                .emit_durable(
                    Some(&turn_id),
                    agent_turn_completed_durable_payload("error".into()),
                )
                .await;
            session
                .emit_live(
                    Some(&turn_id),
                    agent_turn_completed_live_payload("error".into()),
                )
                .await;
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
    *inner_abort_handle.lock() = Some(handle.abort_handle());

    let Some(result) = handle.wait().await else {
        // task panicked or was aborted before completion
        let _ = actor_tx.send(CommandMessage::AgentTurnCleanup {
            session_id: sid,
            turn_id,
            completion: TurnCompletion::Aborted,
        });
        return;
    };

    match result.output {
        Ok(output) => {
            let _ = session
                .emit_durable(
                    Some(&turn_id),
                    agent_turn_completed_durable_payload(output.finish_reason.clone()),
                )
                .await;
            session
                .emit_live(
                    Some(&turn_id),
                    agent_turn_completed_live_payload(output.finish_reason.clone()),
                )
                .await;
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
            if !result.emitted_error {
                session
                    .emit_durable(
                        Some(&turn_id),
                        EventPayload::ErrorOccurred {
                            code: -32603,
                            message: error.to_string(),
                            recoverable: false,
                        },
                    )
                    .await
                    .ok();
            }
            let _ = session
                .emit_durable(
                    Some(&turn_id),
                    agent_turn_completed_durable_payload("error".into()),
                )
                .await;
            session
                .emit_live(
                    Some(&turn_id),
                    agent_turn_completed_live_payload("error".into()),
                )
                .await;
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
