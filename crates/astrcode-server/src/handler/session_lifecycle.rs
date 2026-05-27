//! 会话创建、恢复与 fork。

use astrcode_core::types::SessionId;
use astrcode_protocol::events::ClientNotification;

use super::{CommandHandler, HandlerError, snapshot::session_snapshot};

impl CommandHandler {
    pub(super) async fn send_current_state(&mut self) {
        let Some(session_id) = self.active_session_id.as_ref() else {
            self.send_error(40400, "No active session");
            return;
        };
        match self
            .runtime
            .event_store()
            .session_read_model(session_id)
            .await
        {
            Ok(state) => {
                let snapshot = session_snapshot(&state);
                self.event_bus
                    .send_notification(ClientNotification::SessionResumed {
                        session_id: session_id.to_string(),
                        snapshot,
                    });
            },
            Err(e) => self.send_error(40401, &format!("Session not found: {e}")),
        }
    }

    pub async fn create_session(&mut self, working_dir: String) -> Result<SessionId, HandlerError> {
        tracing::info!(working_dir = %working_dir, "creating session");
        let created = match self.runtime.session_manager().create(&working_dir).await {
            Ok(created) => created,
            Err(error) => {
                tracing::error!(working_dir = %working_dir, error = %error, "create session failed");
                self.send_error(-32603, &error.to_string());
                return Err(HandlerError::SessionManager(error));
            },
        };
        let sid = created.session.id().clone();
        self.active_session_id = Some(sid.clone());

        tracing::info!(session_id = %sid, "session created, dispatching SessionStart");
        self.broadcast_event(created.start_event);

        match created.session.ensure_runtime_ready(true).await {
            Ok(()) => {
                tracing::info!(session_id = %sid, "session fully initialized");
                Ok(sid)
            },
            Err(e) => {
                tracing::error!(session_id = %sid, error = %e, "session prompt init failed");
                self.send_error(-32603, &e.to_string());
                Err(HandlerError::Session(e))
            },
        }
    }

    pub(super) async fn active_session_working_dir(&self) -> Result<String, String> {
        let Some(sid) = self.active_session_id.as_ref() else {
            return Ok(std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned());
        };
        self.runtime
            .session_manager()
            .read_model(sid)
            .await
            .map(|state| state.working_dir)
            .map_err(|e| format!("read session {sid}: {e}"))
    }

    pub(super) async fn resume_session(&mut self, session_id: SessionId) {
        match self
            .runtime
            .session_manager()
            .open(session_id.clone())
            .await
        {
            Ok(session) => {
                if let Err(e) = self.repair_stale_session(&session_id).await {
                    self.send_error(-32603, &e.to_string());
                    return;
                }
                let state = match self.runtime.session_manager().read_model(&session_id).await {
                    Ok(state) => state,
                    Err(e) => {
                        self.send_error(40401, &format!("Session not found: {e}"));
                        return;
                    },
                };
                let snapshot = session_snapshot(&state);

                if let Err(e) = session.ensure_runtime_ready(false).await {
                    self.send_error(-32603, &e.to_string());
                    return;
                }
                self.active_session_id = Some(session_id.clone());
                self.event_bus
                    .send_notification(ClientNotification::SessionResumed {
                        session_id: session_id.into_string(),
                        snapshot,
                    });
            },
            Err(e) => self.send_error(40401, &format!("Session not found: {e}")),
        }
    }

    pub(super) async fn ensure_session(&mut self) -> Result<SessionId, HandlerError> {
        if let Some(ref sid) = self.active_session_id {
            return Ok(sid.clone());
        }

        let wd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".into());
        self.create_session(wd).await
    }

    pub(in crate::handler) async fn delete_session_by_id(
        &mut self,
        session_id: SessionId,
    ) -> Result<(), HandlerError> {
        match self
            .runtime
            .session_manager()
            .delete_with_turn_teardown(self.scheduler.as_ref(), &session_id)
            .await
        {
            Ok(()) => {
                if self.active_session_id.as_ref() == Some(&session_id) {
                    self.active_session_id = None;
                }
                Ok(())
            },
            Err(e) => {
                self.send_error(40401, &format!("Session not found: {e}"));
                Err(HandlerError::SessionManager(e))
            },
        }
    }
}
