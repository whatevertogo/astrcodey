//! ServerSessionOperations — 纯粹的会话原子操作实现。
//!
//! 只做基础动作，不附带 agent 特有的编排逻辑（父子事件、progress 转发、ephemeral 回收）。
//! 这些编排由调用方（插件）自行组合。

use std::sync::Arc;

use astrcode_core::{
    event::{EventPayload, Phase},
    tool::{
        CreateSessionRequest, SessionApiError, SessionHandle, SessionOperations, SessionStatus,
        SubmitTurnRequest, SubmitTurnResult,
    },
    types::{SessionId, new_message_id, new_turn_id},
};

use crate::session_manager::SessionManager;

/// 服务端 SessionOperations 实现。
///
/// 每个方法是一个原子操作，不包含 agent 编排逻辑。
pub struct ServerSessionOperations {
    pub session_manager: Arc<SessionManager>,
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

        let session = Arc::new(
            self.session_manager
                .open(target_sid.clone())
                .await
                .map_err(|e| SessionApiError::NotFound(e.to_string()))?,
        );

        // 确保子 session runtime 就绪
        if let Err(e) = session.ensure_runtime_ready().await {
            return Err(SessionApiError::Internal(format!("runtime init: {e}")));
        }

        let turn_id = new_turn_id();

        // 纯粹的 submit——Session::submit 内部会自己写 TurnStarted + UserMessage
        let handle = session
            .submit(request.user_prompt.clone(), turn_id.clone())
            .await
            .map_err(|e| SessionApiError::Internal(format!("submit: {e}")))?;

        if request.wait_for_result {
            // 同步等待
            let result = handle.wait().await;
            match result {
                Some(r) => match r.output {
                    Ok(out) => {
                        // 写入 AgentSessionCompleted 到父 session
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
            let session_manager = Arc::clone(&self.session_manager);
            let watcher_caller_sid = caller_sid.clone();
            let watcher_target_sid = target_sid.clone();

            tokio::spawn(async move {
                let result = handle.wait().await;
                let outcome = result.as_ref().and_then(|r| r.output.as_ref().ok());

                // 写入 AgentSessionCompleted/Failed 到父 session
                if let Ok(parent_session) = session_manager.open(watcher_caller_sid.clone()).await {
                    match outcome {
                        Some(out) => {
                            let _ = parent_session
                                .append_event(astrcode_core::event::Event::new(
                                    watcher_caller_sid.clone(),
                                    None,
                                    EventPayload::AgentSessionCompleted {
                                        child_session_id: watcher_target_sid.clone(),
                                        final_session_id: watcher_target_sid.clone(),
                                        summary: one_line_summary(&out.text),
                                    },
                                ))
                                .await;
                        },
                        None => {
                            let error_msg = result
                                .as_ref()
                                .and_then(|r| r.output.as_ref().err())
                                .map(|e| e.to_string())
                                .unwrap_or_else(|| "turn task panicked".into());
                            let _ = parent_session
                                .append_event(astrcode_core::event::Event::new(
                                    watcher_caller_sid.clone(),
                                    None,
                                    EventPayload::AgentSessionFailed {
                                        child_session_id: watcher_target_sid.clone(),
                                        final_session_id: watcher_target_sid.clone(),
                                        error: error_msg,
                                    },
                                ))
                                .await;
                        },
                    }

                    // 通知父 session：通过 manager 的 prompt 提交回调启动新 turn。
                    // 直接写 UserMessage 不会启动 turn，必须走 actor 的 SubmitInputForSession。
                    if let Some(notify_text) = notify_parent {
                        if let Err(e) = session_manager
                            .submit_prompt_to_session(watcher_caller_sid.clone(), notify_text)
                        {
                            tracing::warn!(
                                parent_session_id = %watcher_caller_sid,
                                error = %e,
                                "child agent completion notification dropped",
                            );
                        }
                    }
                }

                // 回收 ephemeral session
                if recycle_on_complete {
                    if let Err(e) = session_manager.recycle_session(&watcher_target_sid).await {
                        tracing::warn!(
                            session_id = %watcher_target_sid,
                            error = %e,
                            "failed to recycle session after async turn"
                        );
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

        self.session_manager
            .recycle_session(&target_sid)
            .await
            .map_err(|e| SessionApiError::Internal(e.to_string()))?;

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
            let _ = parent_session
                .append_event(astrcode_core::event::Event::new(
                    parent_sid.clone(),
                    None,
                    EventPayload::AgentSessionCompleted {
                        child_session_id: child_sid.clone(),
                        final_session_id: child_sid.clone(),
                        summary: one_line_summary(summary),
                    },
                ))
                .await;
        }
    }

    /// 向父 session 写入 AgentSessionFailed 事件。
    async fn emit_agent_failed(&self, parent_sid: &SessionId, child_sid: &SessionId, error: &str) {
        if let Ok(parent_session) = self.session_manager.open(parent_sid.clone()).await {
            let _ = parent_session
                .append_event(astrcode_core::event::Event::new(
                    parent_sid.clone(),
                    None,
                    EventPayload::AgentSessionFailed {
                        child_session_id: child_sid.clone(),
                        final_session_id: child_sid.clone(),
                        error: error.to_string(),
                    },
                ))
                .await;
        }
    }
}

fn one_line_summary(text: &str) -> String {
    astrcode_support::text::compact_inline(text, 159)
}
