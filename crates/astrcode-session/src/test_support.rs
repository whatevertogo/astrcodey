use astrcode_core::{
    event::{
        DurableEvent, DurableEventPayload, PersistedSystemPrompt, SessionStarted, StoredEvent,
        SystemPromptSource,
    },
    tool::SessionToolSelection,
    types::SessionId,
};
use astrcode_session_projection::{SessionReadModel, replay};

pub(crate) fn read_model(session_id: SessionId) -> SessionReadModel {
    let started = DurableEvent::session(
        session_id.clone(),
        DurableEventPayload::SessionStarted(SessionStarted {
            working_dir: "/workspace".into(),
            model_id: "model".into(),
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
    );

    replay(session_id, &[StoredEvent::new(0, started)]).unwrap()
}
