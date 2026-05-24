//! Turn 管理 — Agent turn 任务启停、完成清理。

use std::sync::Arc;

use astrcode_core::types::*;
use tokio::sync::mpsc;

use super::{CommandHandler, CommandMessage, HandlerError};
use crate::turn_scheduler::TurnScheduler;

/// Turn 完成结果，通过 oneshot 通道发送。
#[derive(Debug, Clone)]
pub(crate) enum TurnCompletion {
    Completed { finish_reason: String },
    Failed { error: String },
    Aborted,
}

impl CommandHandler {
    /// 启动新 Turn：委托给 scheduler.submit()，spawn completion watcher。
    pub(in crate::handler) async fn start_turn_for_session(
        &self,
        sid: SessionId,
        user_text: String,
        completion_tx: Option<tokio::sync::oneshot::Sender<TurnCompletion>>,
    ) -> Result<TurnId, HandlerError> {
        tracing::info!(session_id = %sid, text_len = user_text.len(), "start_turn");
        let (turn_id, handle) = self
            .scheduler
            .submit(sid.clone(), user_text)
            .await
            .map_err(|e| match e {
                crate::turn_scheduler::TurnError::TurnAlreadyRunning => {
                    self.send_error(40900, "A turn is already running");
                    HandlerError::TurnAlreadyRunning
                },
                crate::turn_scheduler::TurnError::SessionNotFound(msg) => {
                    HandlerError::SessionNotFound(msg)
                },
                crate::turn_scheduler::TurnError::Session(e) => {
                    HandlerError::Session(e)
                },
                crate::turn_scheduler::TurnError::Turn(e) => {
                    HandlerError::Turn(e)
                },
                crate::turn_scheduler::TurnError::EventEmit(e) => {
                    HandlerError::Session(e)
                },
                crate::turn_scheduler::TurnError::SessionManager(e) => {
                    HandlerError::SessionManager(e)
                },
                other => HandlerError::InvalidRequest(other.to_string()),
            })?;

        let scheduler = Arc::clone(&self.scheduler);
        let actor_tx = self.actor_tx.clone();
        let sid_for_watcher = sid.clone();
        let turn_id_for_watcher = turn_id.clone();
        tokio::spawn(async move {
            run_completion_watcher(
                handle,
                scheduler,
                actor_tx,
                sid_for_watcher,
                turn_id_for_watcher,
                completion_tx,
            )
            .await;
        });

        Ok(turn_id)
    }

    /// 中止指定会话的活跃 Turn。
    pub(in crate::handler) async fn abort_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), HandlerError> {
        match self.scheduler.abort(session_id).await {
            Ok(()) => Ok(()),
            Err(crate::turn_scheduler::TurnError::NoActiveTurn) => {
                self.send_error(40400, "No active turn");
                Err(HandlerError::NoActiveTurn)
            },
            Err(e) => Err(HandlerError::InvalidRequest(e.to_string())),
        }
    }

    /// 中止当前活跃会话的 Turn。
    pub(in crate::handler) async fn abort_active_turn(&self) -> Result<(), HandlerError> {
        let Some(sid) = self.active_session_id.clone() else {
            self.send_error(40400, "No active turn");
            return Ok(());
        };
        self.abort_session(&sid).await
    }

    /// 修复遗留状态。
    pub(in crate::handler) async fn repair_stale_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), HandlerError> {
        self.scheduler
            .repair_stale(session_id)
            .await
            .map_err(|e| match e {
                crate::turn_scheduler::TurnError::SessionNotFound(msg) => {
                    HandlerError::SessionNotFound(msg)
                },
                crate::turn_scheduler::TurnError::NoActiveTurn => HandlerError::NoActiveTurn,
                crate::turn_scheduler::TurnError::SessionManager(err) => {
                    HandlerError::SessionManager(err)
                },
                crate::turn_scheduler::TurnError::Session(e) => {
                    HandlerError::Session(e)
                },
                crate::turn_scheduler::TurnError::Turn(e) => {
                    HandlerError::Turn(e)
                },
                crate::turn_scheduler::TurnError::EventEmit(e) => {
                    HandlerError::Session(e)
                },
                other => HandlerError::InvalidRequest(other.to_string()),
            })
    }

    /// 提交提示词并返回完成通知接收器。
    pub(in crate::handler) async fn submit_input_with_completion(
        &self,
        sid: SessionId,
        text: String,
    ) -> Result<(TurnId, tokio::sync::oneshot::Receiver<TurnCompletion>), HandlerError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let turn_id = self.start_turn_for_session(sid, text, Some(tx)).await?;
        Ok((turn_id, rx))
    }
}

/// Completion watcher：等待 TurnHandle 完成，通知 actor 清理。
///
/// Turn 的终态事件（TurnCompleted / AgentRunCompleted）由 `Session::submit` 内部发射。
/// 这里只负责 registry 清理、sync durable events、通知 actor 触发 queued input dispatch。
async fn run_completion_watcher(
    handle: astrcode_session::turn_handle::TurnHandle,
    scheduler: Arc<TurnScheduler>,
    actor_tx: mpsc::UnboundedSender<CommandMessage>,
    sid: SessionId,
    turn_id: TurnId,
    completion_tx: Option<tokio::sync::oneshot::Sender<TurnCompletion>>,
) {
    let completion = match handle.wait().await {
        Some(result) => match result.output {
            Ok(output) => {
                scheduler.sync_durable_events(&sid).await;
                TurnCompletion::Completed {
                    finish_reason: output.finish_reason,
                }
            },
            Err(error) => {
                scheduler.sync_durable_events(&sid).await;
                TurnCompletion::Failed {
                    error: error.to_string(),
                }
            },
        },
        None => TurnCompletion::Aborted,
    };

    scheduler.registry().remove_if_matches(&sid, &turn_id);

    if let Some(tx) = completion_tx {
        let _ = tx.send(completion.clone());
    }

    let _ = actor_tx.send(CommandMessage::AgentTurnCleanup {
        session_id: sid,
        turn_id,
        completion,
    });
}
