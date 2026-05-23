//! ServerSessionOperations — 纯粹的会话原子操作实现。
//!
//! 只做基础动作，生命周期事件（TurnStarted/UserMessage/TurnCompleted 等）
//! 由 Session::submit 内部统一管理。

use std::sync::Arc;

use astrcode_core::{
    event::{EventPayload, Phase},
    tool::{
        CreateSessionRequest, SessionApiError, SessionHandle, SessionOperations, SessionStatus,
        SubmitTurnRequest, SubmitTurnResult,
    },
    types::{SessionId, new_message_id},
};

use crate::{session_manager::SessionManager, turn_scheduler::TurnScheduler};

/// 服务端 SessionOperations 实现。
///
/// 每个方法是一个原子操作，不包含 agent 编排逻辑。
pub struct ServerSessionOperations {
    pub session_manager: Arc<SessionManager>,
    pub scheduler: Arc<TurnScheduler>,
}

#[async_trait::async_trait]
impl SessionOperations for ServerSessionOperations {
    async fn create_session(
        &self,
        parent_session_id: &str,
        request: CreateSessionRequest,
    ) -> Result<SessionHandle, SessionApiError> {
        let parent_sid = SessionId::from(parent_session_id);
        let parent_session = self
            .session_manager
            .open(parent_sid.clone())
            .await
            .map_err(|e| SessionApiError::NotFound(format!("parent: {e}")))?;

        // 嵌套深度验证
        let depth = self.session_depth(&parent_sid).await?;
        let max_depth = self
            .session_manager
            .config()
            .read_effective()
            .agent
            .max_depth;
        if depth >= max_depth {
            return Err(SessionApiError::MaxDepthExceeded {
                current: depth,
                max: max_depth,
            });
        }

        let parent_model = parent_session
            .read_model()
            .await
            .map_err(|e| SessionApiError::Internal(e.to_string()))?;

        let working_dir = request
            .working_dir
            .unwrap_or_else(|| parent_model.working_dir.clone());
        let model_id = request
            .model_preference
            .unwrap_or_else(|| parent_model.model_id.clone());

        let child = parent_session
            .spawn_child(
                &working_dir,
                &model_id,
                request.name,
                String::new(),
                request.system_prompt,
                request.tool_policy,
                request.source_extension.as_deref(),
                request.tool_call_id.into(),
            )
            .await
            .map_err(|e| SessionApiError::Internal(format!("spawn child: {e}")))?;

        let child_sid = child.id().clone();
        self.session_manager.register_child_session(&child);

        Ok(SessionHandle {
            session_id: child_sid.into_string(),
        })
    }

    async fn inject_message(
        &self,
        caller_session_id: &str,
        target_session_id: &str,
        content: String,
    ) -> Result<(), SessionApiError> {
        let caller_sid = SessionId::from(caller_session_id);
        let target_sid = SessionId::from(target_session_id);

        self.verify_access(&caller_sid, &target_sid).await?;

        let session = self
            .session_manager
            .open(target_sid)
            .await
            .map_err(|e| SessionApiError::NotFound(e.to_string()))?;

        let message_id = new_message_id();
        session
            .emit_durable(
                None,
                EventPayload::UserMessage {
                    message_id,
                    text: content,
                },
            )
            .await
            .map_err(|e| SessionApiError::Internal(e.to_string()))?;

        Ok(())
    }

    async fn submit_turn(
        &self,
        caller_session_id: &str,
        request: SubmitTurnRequest,
    ) -> Result<SubmitTurnResult, SessionApiError> {
        let caller_sid = SessionId::from(caller_session_id);
        let target_sid = SessionId::from(request.target_session_id.as_str());

        self.verify_access(&caller_sid, &target_sid).await?;

        // 确保子 session runtime 就绪
        let session = self
            .session_manager
            .open(target_sid.clone())
            .await
            .map_err(|e| SessionApiError::NotFound(e.to_string()))?;
        if let Err(e) = session.ensure_runtime_ready().await {
            return Err(SessionApiError::Internal(format!("runtime init: {e}")));
        }

        // 通过 scheduler submit——统一走 TurnRegistry
        let (turn_id, handle) = self
            .scheduler
            .submit(target_sid.clone(), request.user_prompt.clone())
            .await
            .map_err(|e| SessionApiError::Internal(format!("submit: {e}")))?;

        // 统一 registry 清理：无论同步等待还是异步 watcher，都在 turn 完成后移除 registry entry
        let registry = Arc::clone(self.scheduler.registry());

        if request.wait_for_result {
            // 同步等待
            let result = handle.wait().await;
            self.scheduler.sync_durable_events(&target_sid).await;
            registry.remove_if_matches(&target_sid, &turn_id);
            match result {
                Some(r) => match r.output {
                    Ok(out) => {
                        self.emit_agent_completed(&caller_sid, &target_sid, &out.text)
                            .await;
                        Ok(SubmitTurnResult::Completed { content: out.text })
                    },
                    Err(e) => {
                        self.emit_agent_failed(&caller_sid, &target_sid, &e.to_string())
                            .await;
                        Err(SessionApiError::Internal(format!("turn error: {e}")))
                    },
                },
                None => {
                    self.emit_agent_failed(&caller_sid, &target_sid, "turn task panicked")
                        .await;
                    Err(SessionApiError::Internal("turn task panicked".into()))
                },
            }
        } else {
            // 异步：spawn watcher 处理完成后逻辑
            let notify_parent = request.notify_parent_on_complete.clone();
            let recycle_on_complete = request.recycle_on_complete;
            let scheduler = Arc::clone(&self.scheduler);
            let session_manager = Arc::clone(&self.session_manager);
            let watcher_caller_sid = caller_sid.clone();
            let watcher_target_sid = target_sid.clone();
            let watcher_turn_id = turn_id.clone();

            tokio::spawn(async move {
                let result = handle.wait().await;
                let outcome = result.as_ref().and_then(|r| r.output.as_ref().ok());
                scheduler.sync_durable_events(&watcher_target_sid).await;
                registry.remove_if_matches(&watcher_target_sid, &watcher_turn_id);

                // 写入 AgentSessionCompleted/Failed 到父 session
                if let Ok(parent_session) = session_manager.open(watcher_caller_sid.clone()).await {
                    match outcome {
                        Some(out) => {
                            if let Err(e) = parent_session
                                .append_event(astrcode_core::event::Event::new(
                                    watcher_caller_sid.clone(),
                                    None,
                                    EventPayload::AgentSessionCompleted {
                                        child_session_id: watcher_target_sid.clone(),
                                        final_session_id: watcher_target_sid.clone(),
                                        summary: one_line_summary(&out.text),
                                    },
                                ))
                                .await
                            {
                                tracing::warn!(
                                    parent_session_id = %watcher_caller_sid,
                                    child_session_id = %watcher_target_sid,
                                    error = %e,
                                    "failed to append AgentSessionCompleted event"
                                );
                            }
                        },
                        None => {
                            let error_msg = result
                                .as_ref()
                                .and_then(|r| r.output.as_ref().err())
                                .map(|e| e.to_string())
                                .unwrap_or_else(|| "turn task panicked".into());
                            if let Err(e) = parent_session
                                .append_event(astrcode_core::event::Event::new(
                                    watcher_caller_sid.clone(),
                                    None,
                                    EventPayload::AgentSessionFailed {
                                        child_session_id: watcher_target_sid.clone(),
                                        final_session_id: watcher_target_sid.clone(),
                                        error: error_msg,
                                    },
                                ))
                                .await
                            {
                                tracing::warn!(
                                    parent_session_id = %watcher_caller_sid,
                                    child_session_id = %watcher_target_sid,
                                    error = %e,
                                    "failed to append AgentSessionFailed event"
                                );
                            }
                        },
                    }
                    scheduler.sync_durable_events(&watcher_caller_sid).await;

                    // 回收 ephemeral session（在 notify 之前，确保 Recycled 事件紧跟
                    // Completed/Failed）
                    if recycle_on_complete {
                        Self::recycle_and_notify(
                            &session_manager,
                            &scheduler,
                            &watcher_caller_sid,
                            &watcher_target_sid,
                        )
                        .await;
                    }

                    // 通知父 session：通过 scheduler.submit_or_inject 启动新 turn
                    if let Some(notify_text) = notify_parent {
                        if let Err(e) = scheduler
                            .submit_or_inject(watcher_caller_sid.clone(), notify_text)
                            .await
                        {
                            tracing::warn!(
                                parent_session_id = %watcher_caller_sid,
                                error = %e,
                                "child agent completion notification dropped",
                            );
                        }
                    }
                }
            });

            Ok(SubmitTurnResult::Backgrounded {
                task_id: turn_id.into_string(),
                session_id: target_sid.into_string(),
            })
        }
    }

    async fn query_session(
        &self,
        caller_session_id: &str,
        target_session_id: &str,
    ) -> Result<SessionStatus, SessionApiError> {
        let caller_sid = SessionId::from(caller_session_id);
        let target_sid = SessionId::from(target_session_id);

        self.verify_access(&caller_sid, &target_sid).await?;

        let model = self
            .session_manager
            .read_model(&target_sid)
            .await
            .map_err(|e| SessionApiError::NotFound(e.to_string()))?;

        Ok(SessionStatus {
            alive: true,
            has_active_turn: !matches!(model.phase, Phase::Idle | Phase::Error),
            last_finish_reason: None,
            message_count: model.messages.len(),
        })
    }

    async fn recycle_session(
        &self,
        caller_session_id: &str,
        target_session_id: &str,
    ) -> Result<(), SessionApiError> {
        let caller_sid = SessionId::from(caller_session_id);
        let target_sid = SessionId::from(target_session_id);

        self.verify_access(&caller_sid, &target_sid).await?;

        Self::recycle_and_notify(
            &self.session_manager,
            &self.scheduler,
            &caller_sid,
            &target_sid,
        )
        .await;

        Ok(())
    }

    async fn delete_session(
        &self,
        caller_session_id: &str,
        target_session_id: &str,
    ) -> Result<(), SessionApiError> {
        let caller_sid = SessionId::from(caller_session_id);
        let target_sid = SessionId::from(target_session_id);

        self.verify_access(&caller_sid, &target_sid).await?;

        self.scheduler.cleanup(&target_sid).await;
        self.session_manager
            .delete(&target_sid)
            .await
            .map_err(|e| SessionApiError::Internal(e.to_string()))?;

        Ok(())
    }

    async fn restore_session(
        &self,
        caller_session_id: &str,
        target_session_id: &str,
    ) -> Result<(), SessionApiError> {
        let caller_sid = SessionId::from(caller_session_id);
        let target_sid = SessionId::from(target_session_id);

        self.verify_access(&caller_sid, &target_sid).await?;

        self.session_manager
            .restore_session(&target_sid)
            .await
            .map_err(|e| SessionApiError::Internal(e.to_string()))?;

        Ok(())
    }
}

impl ServerSessionOperations {
    async fn verify_access(
        &self,
        caller: &SessionId,
        target: &SessionId,
    ) -> Result<(), SessionApiError> {
        if caller == target {
            return Ok(());
        }
        let mut current = target.clone();
        loop {
            let model = self
                .session_manager
                .read_model(&current)
                .await
                .map_err(|e| SessionApiError::NotFound(e.to_string()))?;
            match model.parent_session_id {
                Some(parent) => {
                    if &parent == caller {
                        return Ok(());
                    }
                    current = parent;
                },
                None => {
                    return Err(SessionApiError::PermissionDenied(format!(
                        "session {target} is not a descendant of {caller}"
                    )));
                },
            }
        }
    }

    async fn session_depth(&self, session_id: &SessionId) -> Result<usize, SessionApiError> {
        let mut depth = 0;
        let mut current = session_id.clone();
        loop {
            let model = self
                .session_manager
                .read_model(&current)
                .await
                .map_err(|e| SessionApiError::Internal(format!("read session: {e}")))?;
            match model.parent_session_id {
                Some(parent) => {
                    depth += 1;
                    current = parent;
                },
                None => break,
            }
        }
        Ok(depth)
    }

    /// 向父 session 写入 AgentSessionCompleted 事件。
    async fn emit_agent_completed(
        &self,
        parent_sid: &SessionId,
        child_sid: &SessionId,
        summary: &str,
    ) {
        if let Ok(parent_session) = self.session_manager.open(parent_sid.clone()).await {
            if let Err(e) = parent_session
                .append_event(astrcode_core::event::Event::new(
                    parent_sid.clone(),
                    None,
                    EventPayload::AgentSessionCompleted {
                        child_session_id: child_sid.clone(),
                        final_session_id: child_sid.clone(),
                        summary: one_line_summary(summary),
                    },
                ))
                .await
            {
                tracing::warn!(
                    parent_session_id = %parent_sid,
                    child_session_id = %child_sid,
                    error = %e,
                    "failed to append AgentSessionCompleted event"
                );
            }
        }
    }

    /// 向父 session 写入 AgentSessionFailed 事件。
    async fn emit_agent_failed(&self, parent_sid: &SessionId, child_sid: &SessionId, error: &str) {
        if let Ok(parent_session) = self.session_manager.open(parent_sid.clone()).await {
            if let Err(e) = parent_session
                .append_event(astrcode_core::event::Event::new(
                    parent_sid.clone(),
                    None,
                    EventPayload::AgentSessionFailed {
                        child_session_id: child_sid.clone(),
                        final_session_id: child_sid.clone(),
                        error: error.to_string(),
                    },
                ))
                .await
            {
                tracing::warn!(
                    parent_session_id = %parent_sid,
                    child_session_id = %child_sid,
                    error = %e,
                    "failed to append AgentSessionFailed event"
                );
            }
        }
    }

    /// 回收子会话并向父会话写入 AgentSessionRecycled 事件。
    async fn recycle_and_notify(
        session_manager: &Arc<SessionManager>,
        scheduler: &Arc<TurnScheduler>,
        parent_sid: &SessionId,
        child_sid: &SessionId,
    ) {
        scheduler.cleanup(child_sid).await;
        if let Err(e) = session_manager.recycle_session(child_sid).await {
            tracing::warn!(
                session_id = %child_sid,
                error = %e,
                "failed to recycle session"
            );
            return;
        }
        if let Ok(parent_session) = session_manager.open(parent_sid.clone()).await {
            if let Err(e) = parent_session
                .append_event(astrcode_core::event::Event::new(
                    parent_sid.clone(),
                    None,
                    EventPayload::AgentSessionRecycled {
                        child_session_id: child_sid.clone(),
                    },
                ))
                .await
            {
                tracing::warn!(
                    parent_session_id = %parent_sid,
                    child_session_id = %child_sid,
                    error = %e,
                    "failed to append AgentSessionRecycled event"
                );
            }
            scheduler.sync_durable_events(parent_sid).await;
        }
    }
}

fn one_line_summary(text: &str) -> String {
    astrcode_support::text::compact_inline(text, 159)
}
