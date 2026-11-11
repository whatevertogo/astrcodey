//! 从 durable projection 派生运行时 context。

use std::sync::Arc;

use astrcode_context::ContextSnapshot;
use astrcode_core::llm::{SharedTranscriptMessage, provider_transcript_shared_messages};
use astrcode_session_projection::SessionReadModel;

pub(crate) fn context_snapshot(model: &SessionReadModel) -> ContextSnapshot {
    let transcript: Vec<_> = model
        .model_context
        .messages
        .iter()
        .map(|entry| SharedTranscriptMessage {
            message: Arc::clone(&entry.message),
            origin: entry.origin,
        })
        .collect();
    let anchor = model.model_context.usage.as_ref().and_then(|usage| {
        transcript
            .get(..usage.covered_message_count)
            .map(|prefix| (usage, provider_transcript_shared_messages(prefix.to_vec())))
    });
    let snapshot = ContextSnapshot::from_shared_transcript(
        model.stats.last_seq,
        model.system_prompt.text.clone(),
        transcript,
    );
    let Some((usage, covered_prefix)) = anchor else {
        return snapshot;
    };
    // 归一化可能过滤或跨边界合并消息；只复用仍匹配的前缀，余下消息保守重估。
    let covered_message_count = covered_prefix
        .iter()
        .zip(snapshot.messages.iter().map(Arc::as_ref))
        .take_while(|(entry, message)| entry.message.as_ref() == *message)
        .count();
    snapshot.with_input_token_anchor(
        usage.context_tokens,
        usage.model_context_window,
        covered_message_count,
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
            message: Arc::new(LlmMessage::user("hello")),
            updated_seq: 1,
            origin: Some(TranscriptMessageOrigin::TurnAborted),
        });
        model.model_context.messages.push(SequencedLlmMessage {
            message: Arc::new(LlmMessage::system("stale system in store")),
            updated_seq: 2,
            origin: None,
        });
        model.model_context.messages.push(SequencedLlmMessage {
            message: Arc::new(LlmMessage::assistant("ctx")),
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
        let owned_messages = snapshot
            .messages
            .iter()
            .map(|message| (**message).clone())
            .collect::<Vec<_>>();
        assert_eq!(
            snapshot
                .retained_transcript_messages(&owned_messages)
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

    #[test]
    fn usage_anchor_follows_the_normalized_transcript_prefix() {
        for invisible in [LlmMessage::system("stale"), LlmMessage::assistant("")] {
            let mut model = sample_model();
            model.model_context.messages[1].message = Arc::new(invisible);
            model.model_context.usage = Some(astrcode_session_projection::ContextUsageView {
                context_tokens: 180_000,
                model_context_window: 200_000,
                covered_message_count: 3,
            });
            let covered = context_snapshot(&model);
            assert_eq!(covered.estimate_own_input_tokens(0, 200_000), 180_000);
            model.model_context.messages.push(SequencedLlmMessage {
                message: Arc::new(LlmMessage::user("tail ".repeat(400))),
                updated_seq: 4,
                origin: None,
            });
            let with_tail = context_snapshot(&model);
            assert!(with_tail.estimate_own_input_tokens(0, 200_000) > 180_400);
        }
    }
}
