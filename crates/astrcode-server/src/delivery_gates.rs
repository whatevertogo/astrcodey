use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use astrcode_core::types::SessionId;
use parking_lot::Mutex;
use tokio::sync::{Mutex as AsyncMutex, Notify, OwnedMutexGuard};

use crate::turn_scheduler::TurnScheduleError;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionDeliveryState {
    Open,
    Closing,
}

pub(crate) struct SessionDeliveryGate {
    state: Arc<AsyncMutex<SessionDeliveryState>>,
    active_starts: AtomicUsize,
    starts_idle: Notify,
}

impl SessionDeliveryGate {
    fn new() -> Self {
        Self {
            state: Arc::new(AsyncMutex::new(SessionDeliveryState::Open)),
            active_starts: AtomicUsize::new(0),
            starts_idle: Notify::new(),
        }
    }

    pub(crate) async fn wait_for_starts(&self) {
        loop {
            let notified = self.starts_idle.notified();
            if self.active_starts.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

pub(crate) struct SessionStartLease {
    gate: Arc<SessionDeliveryGate>,
}

impl Drop for SessionStartLease {
    fn drop(&mut self) {
        if self.gate.active_starts.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.gate.starts_idle.notify_waiters();
        }
    }
}

pub(crate) struct SessionClosure {
    gates: Arc<SessionDeliveryGates>,
    session_id: SessionId,
    gate: Arc<SessionDeliveryGate>,
}

impl SessionClosure {
    pub(crate) async fn wait_for_starts(&self) {
        self.gate.wait_for_starts().await;
    }

    pub(crate) fn finish(self) {
        self.gates.remove_if_current(&self.session_id, &self.gate);
    }
}

/// 持有一个 session 的输入/命令决策权。
pub(crate) struct SessionOperationGuard {
    session_id: SessionId,
    gate: Arc<SessionDeliveryGate>,
    state: OwnedMutexGuard<SessionDeliveryState>,
}

impl SessionOperationGuard {
    pub(crate) fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub(crate) fn is_closing(&self) -> bool {
        *self.state == SessionDeliveryState::Closing
    }

    pub(crate) fn start_lease(&self) -> SessionStartLease {
        self.gate.active_starts.fetch_add(1, Ordering::AcqRel);
        SessionStartLease {
            gate: Arc::clone(&self.gate),
        }
    }

    fn mark_closing(mut self) -> Arc<SessionDeliveryGate> {
        *self.state = SessionDeliveryState::Closing;
        Arc::clone(&self.gate)
    }
}

#[derive(Default)]
pub(crate) struct SessionDeliveryGates {
    gates: Mutex<HashMap<SessionId, Arc<SessionDeliveryGate>>>,
}

impl SessionDeliveryGates {
    fn gate(&self, session_id: &SessionId) -> Arc<SessionDeliveryGate> {
        Arc::clone(
            self.gates
                .lock()
                .entry(session_id.clone())
                .or_insert_with(|| Arc::new(SessionDeliveryGate::new())),
        )
    }

    pub(crate) async fn lock(&self, session_id: &SessionId) -> SessionOperationGuard {
        let gate = self.gate(session_id);
        let state = Arc::clone(&gate.state).lock_owned().await;
        SessionOperationGuard {
            session_id: session_id.clone(),
            gate,
            state,
        }
    }

    pub(crate) async fn begin(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionOperationGuard, TurnScheduleError> {
        let operation = self.lock(session_id).await;
        if operation.is_closing() {
            return Err(TurnScheduleError::SessionNotFound(format!(
                "{session_id}: session is closing"
            )));
        }
        Ok(operation)
    }

    pub(crate) async fn close(
        self: &Arc<Self>,
        session_id: &SessionId,
    ) -> Result<SessionClosure, TurnScheduleError> {
        let operation = self.begin(session_id).await?;
        Ok(self.close_operation(operation))
    }

    pub(crate) fn close_operation(
        self: &Arc<Self>,
        operation: SessionOperationGuard,
    ) -> SessionClosure {
        SessionClosure {
            gates: Arc::clone(self),
            session_id: operation.session_id.clone(),
            gate: operation.mark_closing(),
        }
    }

    pub(crate) async fn mark_closing(&self, session_id: &SessionId) -> Arc<SessionDeliveryGate> {
        self.lock(session_id).await.mark_closing()
    }

    fn remove_if_current(&self, session_id: &SessionId, expected: &Arc<SessionDeliveryGate>) {
        let mut gates = self.gates.lock();
        if gates
            .get(session_id)
            .is_some_and(|gate| Arc::ptr_eq(gate, expected))
        {
            gates.remove(session_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn closing_waits_for_reserved_start_io() {
        let gates = Arc::new(SessionDeliveryGates::default());
        let session_id = SessionId::new("session-starting");
        let operation = gates.begin(&session_id).await.unwrap();
        let start_lease = operation.start_lease();
        drop(operation);

        let closing_gates = Arc::clone(&gates);
        let closing_session = session_id.clone();
        let closing = tokio::spawn(async move {
            let gate = closing_gates.close(&closing_session).await.unwrap();
            gate.wait_for_starts().await;
            gate
        });
        tokio::task::yield_now().await;
        assert!(!closing.is_finished());

        drop(start_lease);
        let closing = closing.await.unwrap();
        assert!(gates.begin(&session_id).await.is_err());
        closing.finish();

        let operation = gates.begin(&session_id).await.unwrap();
        let closing = gates.close_operation(operation);
        assert!(gates.begin(&session_id).await.is_err());
        closing.finish();
        assert!(gates.begin(&session_id).await.is_ok());

        let abandoned = gates.close(&session_id).await.unwrap();
        drop(abandoned);
        assert!(gates.begin(&session_id).await.is_err());
    }
}
