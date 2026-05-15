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
    AgentSignal, Session, SessionServices, TurnOutput, TurnRunner, agent_turn_completed_payloads,
    agent_turn_failed_payloads, agent_turn_started_payloads, background::BackgroundTaskCompletion,
    drive_agent,
};
use astrcode_tools::registry::ToolRegistry;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use super::{CommandHandler, CommandMessage, HandlerError};
use crate::bootstrap::ServerRuntime;

/// Agent Turn 的输入参数，用于启动后台任务。
pub(in crate::handler) struct AgentTurnInput {
    pub sid: SessionId,
    pub turn_id: TurnId,
    pub working_dir: String,
    pub tool_registry: Arc<ToolRegistry>,
    pub system_prompt: String,
    pub history: Vec<astrcode_core::llm::LlmMessage>,
    pub text: String,
    /// 斜杠命令注入的一次性指令
    pub transient_instructions: Option<String>,
    pub actor_tx: mpsc::UnboundedSender<CommandMessage>,
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
    pub working_dir: String,
    pub model_id: String,
    pub system_prompt: String,
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
        if self.active_turns.contains_key(&sid) {
            self.send_error(40900, "A turn is already running");
            return Err(HandlerError::TurnAlreadyRunning);
        }

        // 恢复会话并修复可能的遗留状态
        {
            let store = self.runtime.event_store.clone();
            Session::open(store, sid.clone()).await.map_err(|e| {
                HandlerError::SessionNotFound(format!("Session {sid} not found: {e}"))
            })?;
        }
        self.repair_stale_pending_tool_calls(&sid)
            .await
            .map_err(HandlerError::Other)?;
        // 读取会话状态
        let state = self
            .runtime
            .event_store
            .session_read_model(&sid)
            .await
            .map_err(|e| HandlerError::Other(format!("read session {sid}: {e}")))?;
        let history = state.provider_messages();
        let working_dir = state.working_dir;
        let model_id = state.model_id;
        let system_prompt = state.system_prompt;
        let tool_registry = self.ensure_tool_registry(&sid, &working_dir).await;
        // 如未配置 system prompt，自动配置
        let system_prompt = match system_prompt {
            Some(system_prompt) => system_prompt,
            None => self
                .configure_session_prompt(&sid, &working_dir, &tool_registry, None)
                .await
                .map_err(HandlerError::Other)?,
        };
        let turn_id = new_turn_id();

        // 记录 Turn 开始事件
        self.record_turn_payloads(
            &sid,
            Some(&turn_id),
            agent_turn_started_payloads(new_message_id(), visible_text),
        )
        .await
        .map_err(HandlerError::Other)?;

        // 启动 Agent 后台任务
        let handle = self.spawn_agent_turn(AgentTurnInput {
            sid: sid.clone(),
            turn_id: turn_id.clone(),
            working_dir: working_dir.clone(),
            tool_registry: Arc::clone(&tool_registry),
            system_prompt: system_prompt.clone(),
            history,
            text: user_text,
            transient_instructions,
            actor_tx: self.actor_tx.clone(),
        });
        self.active_turns.insert(
            sid.clone(),
            ActiveTurn {
                session_id: sid,
                turn_id: turn_id.clone(),
                handle,
                working_dir,
                model_id,
                system_prompt,
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

    /// 处理 Agent Turn 成功完成。
    pub(in crate::handler) async fn finish_agent_turn(
        &mut self,
        session_id: SessionId,
        turn_id: TurnId,
        output: TurnOutput,
    ) {
        // 忽略：Turn 已被中止或替换
        if !self.active_turn_matches(&session_id, &turn_id) {
            return;
        }
        let Some(mut turn) = self.active_turns.remove(&session_id) else {
            return;
        };
        let finish_reason = output.finish_reason.clone();
        let _ = self
            .record_turn_payloads(
                &session_id,
                Some(&turn_id),
                agent_turn_completed_payloads(output.finish_reason),
            )
            .await;
        turn.resolve_completion(TurnCompletion::Completed { finish_reason });
    }

    /// 处理 Agent Turn 失败。
    pub(in crate::handler) async fn fail_agent_turn(
        &mut self,
        session_id: SessionId,
        turn_id: TurnId,
        error: astrcode_session::TurnError,
        emitted_error: bool,
    ) {
        // 忽略：Turn 已被中止或替换
        if !self.active_turn_matches(&session_id, &turn_id) {
            return;
        }
        let Some(mut turn) = self.active_turns.remove(&session_id) else {
            return;
        };
        let error_message = error.to_string();
        let _ = self
            .record_turn_payloads(
                &session_id,
                Some(&turn_id),
                agent_turn_failed_payloads(
                    // 如 agent 未发送错误事件，补充发送
                    (!emitted_error).then(|| error.to_string()),
                    "error".into(),
                ),
            )
            .await;
        turn.resolve_completion(TurnCompletion::Failed {
            error: error_message,
        });
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

        // 发送 TurnAborted 生命周期事件
        let lifecycle_ctx = LifecycleContext {
            session_id: active_turn.session_id.to_string(),
            working_dir: active_turn.working_dir.clone(),
            model: astrcode_core::config::ModelSelection::simple(active_turn.model_id.clone()),
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
        self.cleanup_background_tasks_for_session(&active_turn.session_id);

        // 记录中止完成事件
        self.record_turn_payloads(
            &active_turn.session_id,
            Some(&active_turn.turn_id),
            agent_turn_completed_payloads("aborted".into()),
        )
        .await
        .map_err(HandlerError::Other)?;

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

    /// 清理会话的所有后台任务。
    pub(in crate::handler) fn cleanup_background_tasks_for_session(&self, session_id: &SessionId) {
        self.runtime
            .background_tasks
            .lock()
            .cleanup_session(session_id);
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
            .event_store
            .session_read_model(session_id)
            .await
            .map_err(|e| format!("read session {session_id}: {e}"))?;
        // 仅处理 CallingTool 阶段且有待处理调用的会话
        if state.phase != Phase::CallingTool || state.pending_tool_calls.is_empty() {
            return Ok(());
        }

        // 标记所有待处理调用为中断
        for pending in pending_requested_tool_calls(&state) {
            self.record_and_broadcast(
                session_id,
                None,
                EventPayload::ToolCallCompleted {
                    call_id: pending.call_id.clone().into(),
                    tool_name: pending.tool_name,
                    result: interrupted_tool_result(&pending.call_id),
                },
            )
            .await?;
        }
        // 标记 Turn 为中断完成
        self.record_turn_payloads(
            session_id,
            None,
            agent_turn_completed_payloads("interrupted".into()),
        )
        .await?;
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
        sid,
        turn_id,
        working_dir,
        tool_registry,
        system_prompt,
        history,
        text,
        transient_instructions,
        actor_tx,
    } = input;

    // 后台子任务结果转发到 Actor
    let current_session_id = Arc::new(tokio::sync::Mutex::new(sid.clone()));
    let (background_result_tx, mut background_result_rx) =
        mpsc::unbounded_channel::<BackgroundTaskCompletion>();
    {
        let bg_actor_tx = actor_tx.clone();
        let handle = tokio::spawn(async move {
            while let Some(completion) = background_result_rx.recv().await {
                let _ = bg_actor_tx.send(CommandMessage::BackgroundTaskCompleted(completion));
            }
        });
        tokio::spawn(async move {
            if let Err(e) = handle.await {
                tracing::error!("background result forwarder panicked: {e}");
            }
        });
    }

    // 合并斜杠命令的一次性指令到 system prompt
    let model_id = runtime.read_effective().llm.model_id.clone();
    let system_prompt = transient_instructions
        .filter(|instructions| !instructions.trim().is_empty())
        .map(|instructions| {
            format!(
                "{system_prompt}\n\n[Slash Command Instructions]\n{}",
                instructions.trim()
            )
        })
        .unwrap_or(system_prompt);

    // 组装 TurnRunner
    let session = Session::open(runtime.event_store.clone(), sid.clone())
        .await
        .expect("session should be openable at agent turn start");
    let agent = TurnRunner::new(
        sid.clone(),
        working_dir,
        system_prompt,
        model_id,
        SessionServices::new(
            runtime.read_llm_provider(),
            tool_registry,
            runtime.extension_runner.clone(),
            runtime.context_assembler.clone(),
            Arc::new(session),
            runtime.auto_compact_failures.clone(),
            runtime.background_tasks.clone(),
        )
        .with_background_result_tx(background_result_tx)
        .with_agent_session_control(runtime.agent_session_control.read().clone()),
    );

    // 驱动 Agent 循环，通过回调转发事件到 Actor
    let noop_bus = astrcode_session::NoopEventBus;
    let (output, emitted_error) = drive_agent(&agent, &text, history, &noop_bus, |signal| {
        let actor_tx = actor_tx.clone();
        let current_session_id = Arc::clone(&current_session_id);
        let turn_id = turn_id.clone();
        async move {
            match signal {
                AgentSignal::Event(payload) => {
                    let session_id = current_session_id.lock().await.clone();
                    let _ = actor_tx.send(CommandMessage::AgentEvent {
                        session_id,
                        turn_id,
                        payload,
                    });
                },
                // 自动压缩触发：请求 Actor 执行压缩并返回新会话 ID
                AgentSignal::AutoCompact {
                    trigger,
                    compaction,
                    reply,
                } => {
                    let session_id = current_session_id.lock().await.clone();
                    let (actor_reply, actor_rx) = oneshot::channel();
                    let result: Result<SessionId, HandlerError> = if actor_tx
                        .send(CommandMessage::AgentAutoCompact {
                            session_id,
                            turn_id,
                            trigger,
                            compaction,
                            reply: actor_reply,
                        })
                        .is_err()
                    {
                        Err(HandlerError::Other("command actor is unavailable".into()))
                    } else {
                        match actor_rx.await {
                            Ok(result) => result,
                            Err(_) => Err(HandlerError::Other(
                                "command actor dropped auto compact response".into(),
                            )),
                        }
                    };
                    // 更新当前会话 ID（压缩后可能切换到子会话）
                    if let Ok(child_session_id) = &result {
                        *current_session_id.lock().await = child_session_id.clone();
                    }
                    let _ = reply.send(result.map_err(|e| e.to_string()));
                },
            }
        }
    })
    .await;
    let final_session_id = current_session_id.lock().await.clone();

    // 发送完成或失败结果到 Actor
    match output {
        Ok(output) => {
            let _ = actor_tx.send(CommandMessage::AgentTurnFinished {
                session_id: final_session_id,
                turn_id,
                output,
            });
        },
        Err(error) => {
            let _ = actor_tx.send(CommandMessage::AgentTurnFailed {
                session_id: final_session_id,
                turn_id,
                error,
                emitted_error,
            });
        },
    }
}
