use std::{path::PathBuf, sync::Arc};

use astrcode_core::{event::EventPayload, llm::LlmTokenUsage, types::SessionId};
use astrcode_extension_sdk::session_query::{
    SessionQuery, SessionQueryError, SessionQueryFactory, SessionSummary, SessionTokenUsage,
    SessionTranscript, SessionTranscriptMessage,
};
use astrcode_storage::SessionStore;

pub struct StorageSessionQueryFactory {
    store: Arc<dyn SessionStore>,
}

impl StorageSessionQueryFactory {
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self { store }
    }
}

impl SessionQueryFactory for StorageSessionQueryFactory {
    fn for_extension(&self, extension_id: &str) -> Arc<dyn SessionQuery> {
        Arc::new(StorageSessionQuery {
            store: Arc::clone(&self.store),
            extension_id: extension_id.to_owned(),
        })
    }
}

struct StorageSessionQuery {
    store: Arc<dyn SessionStore>,
    extension_id: String,
}

#[async_trait::async_trait]
impl SessionQuery for StorageSessionQuery {
    async fn list_summaries(&self) -> Result<Vec<SessionSummary>, SessionQueryError> {
        self.store
            .list_session_summaries()
            .await
            .map(|summaries| {
                summaries
                    .into_iter()
                    .map(|summary| SessionSummary {
                        session_id: summary.session_id,
                        parent_session_id: summary.parent_session_id,
                        source_extension: summary.source_extension,
                        working_dir: summary.working_dir,
                        model_id: summary.model_id,
                        created_at: summary.created_at,
                        updated_at: summary.updated_at,
                        latest_cursor: summary.latest_cursor,
                    })
                    .collect()
            })
            .map_err(query_error)
    }

    async fn transcript(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionTranscript, SessionQueryError> {
        let model = self
            .store
            .session_read_model(session_id)
            .await
            .map_err(query_error)?;
        Ok(SessionTranscript {
            session_id: model.session_id,
            messages: model
                .messages
                .into_iter()
                .map(|message| SessionTranscriptMessage {
                    message: message.message,
                    source: message.source,
                })
                .collect(),
        })
    }

    async fn token_usage(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionTokenUsage>, SessionQueryError> {
        let events = self
            .store
            .replay_events(session_id)
            .await
            .map_err(query_error)?;
        let mut total_tokens = 0u64;
        let mut saw_usage = false;
        let mut model_context_window = None;
        for event in events {
            if let EventPayload::TokenUsageRecorded {
                usage,
                model_context_window: window,
            } = event.payload
            {
                if let Some(tokens) = non_cached_token_count(&usage) {
                    total_tokens = total_tokens.saturating_add(tokens);
                    saw_usage = true;
                }
                model_context_window = Some(window);
            }
        }
        Ok(saw_usage.then_some(SessionTokenUsage {
            total_tokens,
            model_context_window,
        }))
    }

    async fn extension_data_dir(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<PathBuf>, SessionQueryError> {
        self.store
            .session_store_dir(session_id)
            .await
            .map(|dir| dir.map(|dir| dir.join("extension_data").join(self.extension_id.as_str())))
            .map_err(query_error)
    }
}

fn non_cached_token_count(usage: &LlmTokenUsage) -> Option<u64> {
    match (usage.input_tokens, usage.output_tokens) {
        (Some(input), Some(output)) => Some(
            input
                .saturating_sub(usage.cached_input_tokens.unwrap_or_default())
                .saturating_add(output),
        ),
        _ => usage
            .total_tokens
            .map(|total| total.saturating_sub(usage.reasoning_output_tokens.unwrap_or_default())),
    }
}

fn query_error(error: astrcode_storage::StorageError) -> SessionQueryError {
    match error {
        astrcode_storage::StorageError::NotFound(id) => SessionQueryError::NotFound(id.to_string()),
        astrcode_storage::StorageError::Unsupported(message) => {
            SessionQueryError::Unsupported(message)
        },
        other => SessionQueryError::Query(other.to_string()),
    }
}
