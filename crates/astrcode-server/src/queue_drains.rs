use std::{collections::HashMap, sync::Arc, time::Duration};

use astrcode_core::types::SessionId;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(200);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const MAX_RETRY_EXPONENT: u32 = 8;

#[derive(Default)]
struct SessionQueueDrain {
    failed_starts: u32,
    scheduled_retry: Option<Arc<CancellationToken>>,
}

pub(crate) struct QueueDrainRetry {
    pub(crate) delay: Duration,
    pub(crate) cancel: Arc<CancellationToken>,
}

/// 协调每个 session 的 durable queue start 退避，避免瞬时故障遗留输入或生成并行重试链。
#[derive(Default)]
pub(crate) struct QueueDrainTracker {
    sessions: Mutex<HashMap<SessionId, SessionQueueDrain>>,
}

impl QueueDrainTracker {
    pub(crate) fn record_start_failure(&self, session_id: &SessionId) -> Option<QueueDrainRetry> {
        let mut sessions = self.sessions.lock();
        let state = sessions.entry(session_id.clone()).or_default();
        let exponent = state.failed_starts.min(MAX_RETRY_EXPONENT);
        state.failed_starts = state.failed_starts.saturating_add(1);
        if state.scheduled_retry.is_some() {
            return None;
        }
        let cancel = Arc::new(CancellationToken::new());
        state.scheduled_retry = Some(Arc::clone(&cancel));
        Some(QueueDrainRetry {
            delay: INITIAL_RETRY_DELAY
                .saturating_mul(1_u32 << exponent)
                .min(MAX_RETRY_DELAY),
            cancel,
        })
    }

    pub(crate) fn finish_retry_wait(
        &self,
        session_id: &SessionId,
        retry: &QueueDrainRetry,
    ) -> bool {
        let mut sessions = self.sessions.lock();
        let Some(state) = sessions.get_mut(session_id) else {
            return false;
        };
        if !state
            .scheduled_retry
            .as_ref()
            .is_some_and(|scheduled| Arc::ptr_eq(scheduled, &retry.cancel))
        {
            return false;
        }
        state.scheduled_retry = None;
        true
    }

    pub(crate) fn clear_session(&self, session_id: &SessionId) {
        let state = self.sessions.lock().remove(session_id);
        if let Some(state) = state
            && let Some(retry) = state.scheduled_retry
        {
            retry.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_is_single_flight_and_resets_after_success() {
        let tracker = QueueDrainTracker::default();
        let session_id = SessionId::new("session-queue-retry");

        let first = tracker.record_start_failure(&session_id).unwrap();
        assert_eq!(first.delay, INITIAL_RETRY_DELAY);
        assert!(tracker.record_start_failure(&session_id).is_none());
        assert!(tracker.finish_retry_wait(&session_id, &first));

        let second = tracker.record_start_failure(&session_id).unwrap();
        assert!(second.delay > first.delay);
        tracker.clear_session(&session_id);
        assert!(second.cancel.is_cancelled());

        let reset = tracker.record_start_failure(&session_id).unwrap();
        assert_eq!(reset.delay, INITIAL_RETRY_DELAY);
    }
}
