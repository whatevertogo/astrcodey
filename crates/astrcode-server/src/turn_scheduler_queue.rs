//! 下一 turn 输入队列（FIFO），由 [`crate::turn_scheduler`] 引用。

use astrcode_core::types::SessionId;

use super::{SubmitOutcome, TurnError, TurnScheduler};

impl TurnScheduler {
    /// 通知需要处理，在**下一 turn** 触发。
    pub async fn notify_turn(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<SubmitOutcome, TurnError> {
        if !self.registry.has_active(&session_id) {
            let (turn_id, handle) = self.submit(session_id, text).await?;
            return Ok(SubmitOutcome::Started { turn_id, handle });
        }

        let mut queues = self.pending_queues.lock();
        let queue = queues.entry(session_id.clone()).or_default();
        queue.push_back(super::PendingMessage { text });

        let queue_len = queue.len();
        drop(queues);

        tracing::info!(
            session_id = %session_id,
            queue_len = queue_len,
            "message queued for next turn"
        );

        Ok(SubmitOutcome::Queued)
    }

    pub(super) fn dequeue_next_pending(&self, session_id: &SessionId) -> Option<String> {
        let mut queues = self.pending_queues.lock();
        let queue = queues.get_mut(session_id)?;
        let text = queue.pop_front()?.text;
        if queue.is_empty() {
            queues.remove(session_id);
        }
        if text.is_empty() { None } else { Some(text) }
    }

    /// 在 turn 结束且 registry 已清理后，按 FIFO 排空排队输入。
    ///
    /// 在 completion watcher 内同步调用，避免再 `spawn` 非 `Send` 的 `TurnHandle`。
    pub async fn on_turn_completed(&self, session_id: &SessionId) {
        self.process_child_completions(session_id).await;
        self.drain_pending_turns(session_id).await;
    }

    async fn drain_pending_turns(&self, session_id: &SessionId) {
        while !self.registry.has_active(session_id) {
            let Some(text) = self.dequeue_next_pending(session_id) else {
                return;
            };

            tracing::info!(session_id = %session_id, "auto-submitting next queued message for new turn");

            let (turn_id, handle) = match self.submit(session_id.clone(), text).await {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %e,
                        "failed to auto-submit queued message"
                    );
                    return;
                },
            };

            let _ = handle.wait().await;
            self.registry().remove_if_matches(session_id, &turn_id);
        }
    }
}
