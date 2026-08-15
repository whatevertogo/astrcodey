//! Compact 持久化与并发 tail 保留行为的集成测试。

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use astrcode_context::is_compact_summary_message;
use astrcode_core::{
    config::ContextSettings,
    event::DurableEventPayload,
    llm::{LlmContent, LlmError, LlmEvent, LlmMessage, LlmProvider, LlmRole, ModelLimits},
    types::{SessionId, new_message_id, new_turn_id},
};
use astrcode_session::Session;
use astrcode_session_projection::{SessionArtifactView, SessionReadModel};
use astrcode_storage::SessionStore;
use tokio::sync::mpsc;

mod common;

const VALID_COMPACT_SUMMARY: &str = r#"<summary>
1. Primary Request and Intent:
   integration compact summary

2. Key Technical Concepts:
   - compact

3. Files and Code Sections:
   - (none)

4. Errors and fixes:
   - (none)

5. Problem Solving:
   compacted

6. All user messages:
   - (none)

7. Pending Tasks:
   - (none)

8. Current Work:
   compact test

9. Optional Next Step:
   - (none)
</summary>"#;

fn is_compact_summary_request(messages: &[Arc<LlmMessage>]) -> bool {
    messages.last().is_some_and(|message| {
        message.role == LlmRole::User
            && message
                .content
                .iter()
                .any(|content| matches!(content, LlmContent::Text { text } if text.contains("Do not call tools")))
    })
}

async fn seed_history(session: &Session, pairs: usize) {
    for index in 0..pairs {
        session
            .emit_durable(
                None,
                DurableEventPayload::UserMessage {
                    message_id: new_message_id(),
                    text: format!("old user {index} {}", "x ".repeat(24)),
                    attachments: vec![],
                    accepted_seq: None,
                },
            )
            .await
            .unwrap();
        session
            .emit_durable(
                None,
                DurableEventPayload::AssistantMessageCompleted {
                    message_id: new_message_id(),
                    text: format!("old answer {index} {}", "y ".repeat(24)),
                    reasoning_content: None,
                },
            )
            .await
            .unwrap();
    }
}

async fn configure_system_prompt(session: &Session) {
    session
        .emit_durable(
            None,
            DurableEventPayload::SystemPromptConfigured {
                text: "integration system prompt".into(),
                fingerprint: "integration-system-prompt".into(),
                extra_system_prompt: None,
                source: Default::default(),
            },
        )
        .await
        .unwrap();
}

async fn compact_event_count(store: &dyn SessionStore, session_id: &SessionId) -> usize {
    store
        .replay_events(session_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| {
            matches!(
                event.payload,
                DurableEventPayload::TranscriptRewritten { .. }
            )
        })
        .count()
}

fn projected_provider_messages(model: &SessionReadModel) -> Vec<LlmMessage> {
    astrcode_core::llm::provider_visible_messages(
        model
            .model_context
            .messages
            .iter()
            .map(|message| (*message.message).clone())
            .collect(),
    )
}

/// 在 compact LLM 调用期间注入 durable 事件，使 `source_seq` 过期。
///
/// 事件在 mock 内部、LLM 返回前注入，避免测试侧与 mock 之间的 Notify/oneshot 竞态。
struct RaceOnCompactLlm {
    main_calls: AtomicUsize,
    main_requests: Arc<std::sync::Mutex<Vec<Vec<Arc<LlmMessage>>>>>,
    session_to_race: Arc<std::sync::Mutex<Option<Arc<Session>>>>,
    race_message: String,
}

#[async_trait::async_trait]
impl LlmProvider for RaceOnCompactLlm {
    async fn generate_request(
        &self,
        request: astrcode_core::llm::LlmRequest,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let messages = request.messages;
        let (tx, rx) = mpsc::unbounded_channel();

        if is_compact_summary_request(&messages) {
            let session_to_race = Arc::clone(&self.session_to_race);
            let race_message = self.race_message.clone();
            tokio::spawn(async move {
                let session = session_to_race.lock().unwrap().clone();
                if let Some(session) = session {
                    session
                        .emit_durable(
                            None,
                            DurableEventPayload::RecapGenerated {
                                text: race_message,
                                source: "test".into(),
                            },
                        )
                        .await
                        .expect("race event during compact llm");
                }
                let _ = tx.send(LlmEvent::ContentDelta {
                    delta: VALID_COMPACT_SUMMARY.into(),
                });
                let _ = tx.send(LlmEvent::Done {
                    finish_reason: "stop".into(),
                });
            });
            return Ok(rx);
        }

        let main_call = self.main_calls.fetch_add(1, Ordering::SeqCst);
        if main_call == 0 {
            self.main_requests.lock().unwrap().push(messages);
            let _ = tx.send(LlmEvent::ContentDelta {
                delta: "turn after conflict".into(),
            });
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "stop".into(),
            });
        } else {
            let _ = tx.send(LlmEvent::ContentDelta {
                delta: "follow up ok".into(),
            });
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "stop".into(),
            });
        }
        Ok(rx)
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 200_000,
            max_output_tokens: 1024,
        }
    }
}

#[tokio::test]
async fn auto_compact_preserves_concurrent_tail_and_uses_summary() {
    let main_requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let session_to_race = Arc::new(std::sync::Mutex::new(None));
    let llm = Arc::new(RaceOnCompactLlm {
        main_calls: AtomicUsize::new(0),
        main_requests: Arc::clone(&main_requests),
        session_to_race: Arc::clone(&session_to_race),
        race_message: "concurrent race during compact".into(),
    });

    let (session, store, _, _) = common::spawn_session_with_context_and_services(
        Arc::clone(&llm) as Arc<dyn LlmProvider>,
        ContextSettings {
            auto_compact_enabled: true,
            compact_threshold_percent: 0.0,
            predictive_compact_enabled: false,
            compact_max_retry_attempts: 1,
            ..Default::default()
        },
    )
    .await;
    let session = Arc::new(session);
    *session_to_race.lock().unwrap() = Some(Arc::clone(&session));
    configure_system_prompt(&session).await;
    seed_history(&session, 3).await;

    let turn_id = new_turn_id();
    let handle = session
        .submit("current turn".into(), turn_id, None)
        .await
        .unwrap();
    let result = handle.wait().await.unwrap();
    assert!(result.output.is_ok(), "{:?}", result.output);

    let main_messages = main_requests
        .lock()
        .unwrap()
        .pop()
        .expect("main provider request should be captured");
    assert!(
        main_messages.iter().any(|m| is_compact_summary_message(m)),
        "provider request should use the compact summary"
    );
    assert!(
        !main_messages
            .iter()
            .any(|m| m.joined_display_text("\n").contains("old user 0")),
        "provider request should not contain compacted-away history"
    );
    assert!(
        main_messages
            .iter()
            .any(|m| m.joined_display_text("\n").contains("current turn")),
        "provider request should include the active user turn"
    );

    assert_eq!(
        compact_event_count(store.as_ref(), session.id()).await,
        1,
        "compact should append one rewrite event"
    );
    let model = session.read_model().await.unwrap();
    let provider_messages = projected_provider_messages(&model);
    assert!(
        provider_messages.iter().any(is_compact_summary_message),
        "projection should contain the compact summary"
    );
    assert!(
        model.presentation.artifacts.iter().any(|artifact| matches!(
            artifact,
            SessionArtifactView::Recap { text, source, .. }
                if text == "concurrent race during compact" && source == "test"
        )),
        "projection must preserve artifacts appended during compact"
    );
    assert!(
        provider_messages
            .iter()
            .any(|m| m.joined_display_text("\n").contains("current turn")),
        "active user turn should be in projection"
    );
    assert!(
        provider_messages
            .iter()
            .any(|m| m.joined_display_text("\n").contains("turn after conflict")),
        "turn should still complete normally"
    );

    let follow_up = session
        .submit("follow up".into(), new_turn_id(), None)
        .await
        .unwrap();
    let follow_up_result = follow_up.wait().await.unwrap();
    assert!(
        follow_up_result.output.is_ok(),
        "{:?}",
        follow_up_result.output
    );
    let after_follow_up = projected_provider_messages(&session.read_model().await.unwrap());
    assert!(
        after_follow_up
            .iter()
            .any(|m| m.joined_display_text("\n").contains("follow up ok")),
        "user can continue with a normal follow-up turn"
    );
}

#[tokio::test]
async fn compact_idle_session_preserves_tail_when_cursor_advances_during_llm() {
    use astrcode_session::compaction::{ManualCompactionOutcome, compact_manual_session};

    let session_to_race = Arc::new(std::sync::Mutex::new(None));
    let race_llm = Arc::new(RaceOnCompactLlm {
        main_calls: AtomicUsize::new(0),
        main_requests: Arc::new(std::sync::Mutex::new(Vec::new())),
        session_to_race: Arc::clone(&session_to_race),
        race_message: "race during idle compact".into(),
    });
    let context = ContextSettings {
        auto_compact_enabled: true,
        compact_threshold_percent: 0.01,
        predictive_compact_enabled: false,
        compact_max_retry_attempts: 1,
        ..Default::default()
    };
    let (session, store, _, _) = common::spawn_session_with_context_and_services(
        Arc::clone(&race_llm) as Arc<dyn LlmProvider>,
        context,
    )
    .await;
    let session = Arc::new(session);
    *session_to_race.lock().unwrap() = Some(Arc::clone(&session));
    configure_system_prompt(session.as_ref()).await;
    seed_history(session.as_ref(), 3).await;

    let session_for_race = Arc::clone(&session);
    let compact_task =
        tokio::spawn(async move { compact_manual_session(session_for_race.as_ref(), None).await });

    let outcome = compact_task.await.unwrap().unwrap();
    assert!(
        matches!(outcome, ManualCompactionOutcome::Compacted { .. }),
        "idle compact should preserve the concurrent tail, got {outcome:?}"
    );
    assert_eq!(
        compact_event_count(store.as_ref(), session.as_ref().id()).await,
        1,
        "manual compact should append one rewrite event"
    );
    let model = session.read_model().await.unwrap();
    assert!(
        model.presentation.artifacts.iter().any(|artifact| matches!(
            artifact,
            SessionArtifactView::Recap { text, source, .. }
                if text == "race during idle compact" && source == "test"
        )),
        "projection must preserve artifacts appended during compact"
    );
}
