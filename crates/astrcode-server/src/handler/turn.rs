//! Turn 管理 — Agent turn 任务启停、完成清理。

use std::sync::Arc;

use astrcode_core::types::*;
use tokio::sync::mpsc;

use super::{CommandHandler, CommandMessage, HandlerError, errors::turn_schedule_error_for_client};
use crate::turn_scheduler::{CompletionParams, StartedExecution, TurnScheduleError, TurnScheduler};

/// Turn 完成结果，通过 oneshot 通道发送。
#[derive(Debug, Clone)]
pub enum TurnCompletion {
    Completed {
        finish_reason: String,
    },
    Failed {
        error: String,
    },
    /// completion 通道关闭或 task 异常，未拿到 turn 结果。
    Dropped,
}

impl CommandHandler {
    /// 启动新 Turn：经 scheduler 统一启动并 spawn completion watcher。
    pub(in crate::handler) async fn start_turn_for_session(
        &self,
        sid: SessionId,
        user_text: String,
        completion_tx: Option<tokio::sync::oneshot::Sender<TurnCompletion>>,
    ) -> Result<TurnId, HandlerError> {
        tracing::info!(session_id = %sid, text_len = user_text.len(), "start_turn");
        let crate::turn_scheduler::StartedExecution { turn_id, handle } = self
            .scheduler
            .start_with_completion(sid.clone(), user_text)
            .await
            .map_err(|e| {
                let (code, err) = turn_schedule_error_for_client(e);
                if code == 40900 {
                    self.send_error(code, "A turn is already running");
                }
                err
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

    pub(in crate::handler) async fn abort_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), HandlerError> {
        match self.scheduler.abort(session_id).await {
            Ok(()) => Ok(()),
            Err(TurnScheduleError::NoActiveTurn) => {
                self.send_error(40400, "No active turn");
                Err(HandlerError::NoActiveTurn)
            },
            Err(e) => Err(HandlerError::from(e)),
        }
    }

    pub(in crate::handler) async fn abort_active_turn(&self) -> Result<(), HandlerError> {
        let Some(sid) = self.focused_session_id.as_ref() else {
            self.send_error(40400, "No active turn");
            return Ok(());
        };
        self.abort_session(sid).await
    }

    pub(in crate::handler) async fn repair_stale_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), HandlerError> {
        self.scheduler
            .repair_stale(session_id)
            .await
            .map_err(HandlerError::from)
    }

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

/// Completion watcher：等待 TurnHandle 完成，交给 scheduler 统一收尾。
async fn run_completion_watcher(
    mut handle: astrcode_session::turn_handle::TurnHandle,
    scheduler: Arc<TurnScheduler>,
    actor_tx: mpsc::Sender<CommandMessage>,
    sid: SessionId,
    mut turn_id: TurnId,
    mut completion_tx: Option<tokio::sync::oneshot::Sender<TurnCompletion>>,
) {
    loop {
        let completion = match handle.wait().await {
            Some(result) => match result.output {
                Ok(output) => TurnCompletion::Completed {
                    finish_reason: output.finish_reason,
                },
                Err(error) => TurnCompletion::Failed {
                    error: error.to_string(),
                },
            },
            None => TurnCompletion::Dropped,
        };

        let next = scheduler
            .finish_and_maybe_start_next(CompletionParams {
                session_id: sid.clone(),
                turn_id: turn_id.clone(),
            })
            .await;

        let actor_ok = send_turn_completion(
            &mut completion_tx,
            &actor_tx,
            sid.clone(),
            turn_id.clone(),
            completion,
        )
        .await;
        if !actor_ok {
            break;
        }

        let Some(StartedExecution {
            turn_id: next_turn_id,
            handle: next_handle,
        }) = next
        else {
            break;
        };
        turn_id = next_turn_id;
        handle = next_handle;
    }
}

/// 向 completion oneshot 与 actor 发送本轮结果；actor 通道关闭时返回 false。
async fn send_turn_completion(
    completion_tx: &mut Option<tokio::sync::oneshot::Sender<TurnCompletion>>,
    actor_tx: &mpsc::Sender<CommandMessage>,
    session_id: SessionId,
    turn_id: TurnId,
    completion: TurnCompletion,
) -> bool {
    if let Some(tx) = completion_tx.take() {
        let _ = tx.send(completion.clone());
    }
    if actor_tx
        .send(CommandMessage::AgentTurnCleanup {
            session_id,
            turn_id: turn_id.clone(),
            completion,
        })
        .await
        .is_err()
    {
        tracing::warn!(
            turn_id = %turn_id,
            "command actor queue closed; skipping turn cleanup message"
        );
        return false;
    }
    true
}
