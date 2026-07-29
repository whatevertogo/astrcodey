//! Read-only application port for durable session state.

use std::sync::Arc;

use astrcode_core::types::{Cursor, SessionId};
use astrcode_session_projection::SessionReadModel;
use astrcode_storage::{EventReader, SessionReader, SessionStore, StorageError};

#[derive(Clone)]
pub(crate) struct SessionStateSource {
    events: Arc<dyn EventReader>,
    sessions: Arc<dyn SessionReader>,
}

impl SessionStateSource {
    pub(crate) fn new(store: Arc<dyn SessionStore>) -> Self {
        Self {
            events: Arc::clone(&store) as Arc<dyn EventReader>,
            sessions: store,
        }
    }

    pub(crate) async fn read_model(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<SessionReadModel>, StorageError> {
        self.sessions.session_read_model(session_id).await
    }

    pub(crate) async fn latest_cursor(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Cursor>, StorageError> {
        self.events.latest_cursor(session_id).await
    }
}
