//! 下一 turn 输入队列（FIFO），由 [`crate::turn_scheduler`] 引用。

use astrcode_core::types::{SessionId, TurnId};
use astrcode_session::turn_handle::TurnHandle;

use super::{SubmitOutcome, TurnScheduleError, TurnScheduler};

impl TurnScheduler {
    /// 通知需要处理，在**下一 turn** 触发。
    pub async fn notify_turn(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<SubmitOutcome, TurnScheduleError> {
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

    /// turn 结束后的收尾：子 agent 回收等（排队输入由 completion watcher 单独启动）。
    pub async fn on_turn_completed(&self, session_id: &SessionId) {
        self.process_child_completions(session_id).await;
    }

    /// 若队列非空且当前无活跃 turn，弹出一条并 `submit`（每次 completion 最多一条）。
    pub async fn start_next_queued_turn(
        &self,
        session_id: &SessionId,
    ) -> Option<(TurnId, TurnHandle)> {
        if self.registry.has_active(session_id) {
            return None;
        }
        let text = self.dequeue_next_pending(session_id)?;
        tracing::info!(session_id = %session_id, "auto-submitting next queued message for new turn");
        match self.submit(session_id.clone(), text).await {
            Ok(pair) => Some(pair),
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
                    error = %e,
                    "failed to auto-submit queued message"
                );
                None
            },
        }
    }
}
