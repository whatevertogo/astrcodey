//! 从 durable projection 派生运行时 context。

use astrcode_context::ContextSnapshot;
use astrcode_core::llm::TranscriptMessage;
use astrcode_session_projection::SessionReadModel;

pub(crate) fn context_snapshot(model: &SessionReadModel) -> ContextSnapshot {
    let transcript = model
        .model_context
        .messages
        .iter()
        .map(|entry| TranscriptMessage {
            message: entry.message.clone(),
            origin: entry.origin,
        })
        .collect();
    let Some(usage) = &model.model_context.usage else {
        return ContextSnapshot::from_transcript(
            model.stats.last_seq,
            model.system_prompt.text.clone(),
            transcript,
        );
    };
    let snapshot = ContextSnapshot::from_transcript(
        model.stats.last_seq,
        model.system_prompt.text.clone(),
        transcript,
    );
    snapshot.with_input_token_anchor(
        usage.context_tokens,
        usage.model_context_window,
        usage.covered_message_count,
    )
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        llm::{LlmContent, LlmMessage, LlmRole, TranscriptMessageOrigin},
        types::new_session_id,
    };
    use astrcode_session_projection::{SequencedLlmMessage, SessionReadModel};

    use super::*;
    use crate::test_support::read_model;

    fn sample_model() -> SessionReadModel {
        let mut model = read_model(new_session_id());
        model.model_context.messages.push(SequencedLlmMessage {
            message: LlmMessage::user("hello"),
            updated_seq: 1,
            origin: Some(TranscriptMessageOrigin::TurnAborted),
        });
        model.model_context.messages.push(SequencedLlmMessage {
            message: LlmMessage::system("stale system in store"),
            updated_seq: 2,
            origin: None,
        });
        model.model_context.messages.push(SequencedLlmMessage {
            message: LlmMessage::assistant("ctx"),
            updated_seq: 3,
            origin: None,
        });
        model
    }

    #[test]
    fn context_snapshot_excludes_system_and_includes_transcript() {
        let model = sample_model();
        let snapshot = context_snapshot(&model);
        assert_eq!(snapshot.messages.len(), 2);
        assert!(
            snapshot
                .messages
                .iter()
                .all(|message| message.role != LlmRole::System)
        );
        assert_eq!(
            snapshot
                .retained_transcript_messages(&snapshot.messages)
                .unwrap()[0]
                .origin,
            Some(TranscriptMessageOrigin::TurnAborted)
        );
    }

    #[test]
    fn context_snapshot_builds_request_with_its_system_prompt() {
        let mut model = sample_model();
        model.system_prompt.text = "fresh system".into();
        let snapshot = context_snapshot(&model);
        let messages = snapshot.request_messages(snapshot.messages.clone());
        assert!(messages.iter().any(|m| {
            m.role == LlmRole::System
                && m.content.iter().any(|c| {
                    matches!(
                        c,
                        LlmContent::Text { text } if text.contains("fresh")
                    )
                })
        }));
        assert!(!messages.iter().any(|m| {
            m.role == LlmRole::System
                && m.content.iter().any(|c| {
                    matches!(
                        c,
                        LlmContent::Text { text } if text == "stale system in store"
                    )
                })
        }));
    }
}
