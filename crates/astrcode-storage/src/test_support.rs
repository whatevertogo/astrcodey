use astrcode_core::{
    event::{
        DurableEvent, DurableEventPayload, PersistedSystemPrompt, SessionStarted,
        SystemPromptSource,
    },
    tool::SessionToolSelection,
    types::{SessionId, new_message_id},
};

pub(crate) fn started_event(session_id: &SessionId) -> DurableEvent {
    DurableEvent::session(
        session_id.clone(),
        DurableEventPayload::SessionStarted(SessionStarted {
            working_dir: "/workspace".into(),
            model_id: "model-a".into(),
            parent: None,
            tool_selection: SessionToolSelection::default(),
            source_extension: None,
            initial_system_prompt: PersistedSystemPrompt {
                text: "system".into(),
                fingerprint: "fingerprint".into(),
                extra_system_prompt: None,
                source: SystemPromptSource::Native,
            },
        }),
    )
}

pub(crate) fn user_event(session_id: &SessionId, text: &str) -> DurableEvent {
    DurableEvent::session(
        session_id.clone(),
        DurableEventPayload::UserMessage {
            message_id: new_message_id(),
            text: text.into(),
            attachments: vec![],
            accepted_seq: None,
        },
    )
}
