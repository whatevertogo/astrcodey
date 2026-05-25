//! EventQueryInner 的服务端实现。
//!
//! 将已有的 `EventReader` 适配为 `EventQueryInner` 能力接口。

use std::sync::Arc;

use astrcode_core::{
    capability::{ConversationView, EventQueryCap, EventQueryInner, PromptRole, SessionSummaryView, TurnView},
    llm::{LlmContent, LlmRole},
    storage::EventReader,
};

/// 将 `EventReader` 适配为 `EventQueryInner`。
pub struct ServerEventQuery {
    reader: Arc<dyn EventReader>,
}

impl ServerEventQuery {
    pub fn new(reader: Arc<dyn EventReader>) -> Self {
        Self { reader }
    }

    pub fn as_capability(self: &Arc<Self>) -> Arc<EventQueryCap> {
        Arc::new(EventQueryCap::new(self.clone()))
    }
}

#[async_trait::async_trait]
impl EventQueryInner for ServerEventQuery {
    async fn list_session_summaries(&self) -> Result<Vec<SessionSummaryView>, String> {
        let summaries = self
            .reader
            .list_session_summaries()
            .await
            .map_err(|e| e.to_string())?;

        Ok(summaries
            .iter()
            .map(|s| SessionSummaryView {
                session_id: s.session_id.as_ref().to_string(),
                working_dir: s.working_dir.clone(),
                model: s.model_id.clone(),
                first_user_message: s.first_user_message.clone(),
                parent_session_id: s.parent_session_id.as_ref().map(|id| id.as_ref().to_string()),
                source_extension: s.source_extension.clone(),
                updated_at: Some(s.updated_at.clone()),
            })
            .collect())
    }

    async fn read_conversation(&self, session_id: &str) -> Result<ConversationView, String> {
        let sid = astrcode_core::types::SessionId::from(session_id);
        let model = self
            .reader
            .session_read_model(&sid)
            .await
            .map_err(|e| e.to_string())?;

        let turns: Vec<TurnView> = model
            .messages
            .iter()
            .filter_map(|msg| {
                let role = match msg.role {
                    LlmRole::User => PromptRole::User,
                    LlmRole::Assistant => PromptRole::Assistant,
                    LlmRole::System => PromptRole::System,
                    LlmRole::Tool => return None,
                };
                let text: String = msg
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        LlmContent::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if text.is_empty() {
                    return None;
                }
                Some(TurnView { role, text })
            })
            .collect();

        Ok(ConversationView {
            session_id: session_id.to_string(),
            turns,
        })
    }
}
