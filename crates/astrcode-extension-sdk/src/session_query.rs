use std::{path::PathBuf, sync::Arc};

use crate::{llm::LlmMessage, types::SessionId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub parent_session_id: Option<SessionId>,
    pub source_extension: Option<String>,
    pub working_dir: String,
    pub model_id: String,
    pub created_at: String,
    pub updated_at: String,
    pub latest_cursor: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionTranscriptMessage {
    pub message: LlmMessage,
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionTranscript {
    pub session_id: SessionId,
    pub messages: Vec<SessionTranscriptMessage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionTokenUsage {
    pub total_tokens: u64,
    pub model_context_window: Option<usize>,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionQueryError {
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("session query is unsupported: {0}")]
    Unsupported(String),
    #[error("session query failed: {0}")]
    Query(String),
}

#[async_trait::async_trait]
pub trait SessionQuery: Send + Sync {
    async fn list_summaries(&self) -> Result<Vec<SessionSummary>, SessionQueryError>;

    async fn transcript(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionTranscript, SessionQueryError>;

    async fn token_usage(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionTokenUsage>, SessionQueryError>;

    async fn extension_data_dir(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<PathBuf>, SessionQueryError>;
}

pub trait SessionQueryFactory: Send + Sync {
    fn for_extension(&self, extension_id: &str) -> Arc<dyn SessionQuery>;
}
