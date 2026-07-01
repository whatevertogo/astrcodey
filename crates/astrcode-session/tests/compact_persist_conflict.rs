//! Compact 持久化 CAS 与 turn 内回退行为的集成测试。

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use astrcode_core::{
    config::{
        AgentSettings, ContextSettings, EffectiveConfig, ExtensionSettings, LlmSettings,
        ProviderAuthScheme, ProviderWireFormat,
    },
    context::{
        COMPACT_SUMMARY_MARKER, CompactIfNeededOutcome, CompactMessagesOptions, CompactRequestFn,
        CompactResult, CompactSummaryRenderOptions, ContextAssembler, ContextPrepareInput,
        NoopPostCompactEnricher, PreparedCompaction, is_compact_summary_message,
    },
    event::EventPayload,
    extension::CompactStrategy,
    llm::{LlmContent, LlmError, LlmEvent, LlmMessage, LlmProvider, LlmRole, ModelLimits},
    prompt::{PromptFileProvider, PromptFiles, PromptPlan, PromptProvider, SystemPromptInput},
    storage::EventStore,
    tool::ToolDefinition,
    types::{SessionId, new_message_id, new_session_id, new_turn_id},
};
use astrcode_kernel::extension_runtime::NoopExtensionRuntime;
use astrcode_session::{
    Session, SessionCreateParams, SessionHostServices, SessionRuntimeServices, SessionRuntimeState,
    compact::persist_compact_result,
};
use astrcode_storage::in_memory::InMemoryEventStore;
use astrcode_support::hash::hex_fingerprint;
use tokio::sync::mpsc;

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

struct TestPromptProvider;

#[async_trait::async_trait]
impl PromptProvider for TestPromptProvider {
    async fn assemble(&self, _input: SystemPromptInput) -> PromptPlan {
        PromptPlan::from_system_prompt("integration system prompt".into())
    }
}

struct TestPromptFileProvider;

#[async_trait::async_trait]
impl PromptFileProvider for TestPromptFileProvider {
    async fn load(&self, _working_dir: &str, _include_agents_rules: bool) -> PromptFiles {
        PromptFiles::default()
    }
}

struct TestContextAssembler {
    settings: ContextSettings,
}

#[async_trait::async_trait]
impl ContextAssembler for TestContextAssembler {
    fn settings(&self) -> &ContextSettings {
        &self.settings
    }

    fn should_auto_compact(&self, input: &ContextPrepareInput<'_>) -> bool {
        self.settings.auto_compact_enabled && !input.messages.is_empty()
    }

    async fn compact_if_needed(
        &self,
        messages: Vec<LlmMessage>,
        _system_prompt: Option<&str>,
        _custom_instructions: &[String],
        _render_options: CompactSummaryRenderOptions,
        options: CompactMessagesOptions,
        mut request_text: CompactRequestFn,
    ) -> CompactIfNeededOutcome {
        if !options.run {
            return CompactIfNeededOutcome::NotRun { messages };
        }
        if messages.is_empty() {
            return CompactIfNeededOutcome::Skipped { messages };
        }

        let summary = if options.use_llm {
            request_text(vec![LlmMessage::user(
                "Do not call tools. Summarize the conversation for compaction.",
            )])
            .await
            .unwrap()
        } else {
            "deterministic compact summary".into()
        };
        let retained_messages = messages.last().cloned().into_iter().collect::<Vec<_>>();
        let context_messages = vec![LlmMessage::user(format!(
            "{COMPACT_SUMMARY_MARKER}\nSummary:\n{summary}\n</compact_summary>"
        ))];
        let mut compacted_messages = context_messages.clone();
        compacted_messages.extend(retained_messages.clone());
        CompactIfNeededOutcome::Applied {
            messages: compacted_messages,
            compaction: PreparedCompaction {
                result: CompactResult {
                    pre_tokens: messages.len(),
                    post_tokens: retained_messages.len().saturating_add(1),
                    summary,
                    messages_removed: messages.len().saturating_sub(retained_messages.len()),
                    context_messages,
                    retained_messages,
                    transcript_path: None,
                },
                llm_api_failed: false,
            },
        }
    }
}

fn test_caps(llm: Arc<dyn LlmProvider>, context: ContextSettings) -> Arc<SessionRuntimeServices> {
    let context_assembler = Arc::new(TestContextAssembler {
        settings: context.clone(),
    });
    let effective = EffectiveConfig {
        llm: LlmSettings {
            provider_kind: "mock".into(),
            base_url: String::new(),
            api_key: String::new(),
            wire_format: ProviderWireFormat::OpenAiChatCompletions,
            auth_scheme: ProviderAuthScheme::Bearer,
            model_id: "mock-model".into(),
            max_tokens: 1024,
            context_limit: 200_000,
            connect_timeout_secs: 1,
            read_timeout_secs: 1,
            max_retries: 0,
            retry_base_delay_ms: 0,
            supports_prompt_cache_key: false,
            supports_stream_usage: false,
            prompt_cache_retention: None,
            reasoning: false,
            thinking_level: None,
        },
        small_llm: LlmSettings {
            provider_kind: "mock".into(),
            base_url: String::new(),
            api_key: String::new(),
            wire_format: ProviderWireFormat::OpenAiChatCompletions,
            auth_scheme: ProviderAuthScheme::Bearer,
            model_id: "mock-model".into(),
            max_tokens: 1024,
            context_limit: 200_000,
            connect_timeout_secs: 1,
            read_timeout_secs: 1,
            max_retries: 0,
            retry_base_delay_ms: 0,
            supports_prompt_cache_key: false,
            supports_stream_usage: false,
            prompt_cache_retention: None,
            reasoning: false,
            thinking_level: None,
        },
        context,
        agent: AgentSettings::default(),
        permissions: Default::default(),
        extensions: ExtensionSettings::default(),
    };
    Arc::new(SessionRuntimeServices::new(
        llm.clone(),
        llm,
        effective,
        SessionHostServices {
            extension_runner: Arc::new(NoopExtensionRuntime),
            context_assembler,
            post_compact_enricher: Arc::new(NoopPostCompactEnricher),
            prompt_provider: Arc::new(TestPromptProvider),
            prompt_file_provider: Arc::new(TestPromptFileProvider),
            tool_packs: Vec::new(),
        },
    ))
}

async fn spawn_session(
    llm: Arc<dyn LlmProvider>,
    context: ContextSettings,
) -> (Session, Arc<dyn EventStore>) {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
    let caps = test_caps(llm, context);
    let sid = new_session_id();
    let runtime = Arc::new(SessionRuntimeState::new(
        caps.llm(),
        caps.small_llm(),
        "mock-model".into(),
    ));
    let working_dir = std::env::temp_dir().join(sid.as_str());
    std::fs::create_dir_all(&working_dir).unwrap();
    let session = Session::create_with_params(SessionCreateParams {
        store: Arc::clone(&store),
        sid: sid.clone(),
        working_dir: working_dir.to_string_lossy().into_owned(),
        model_id: "mock-model".into(),
        parent: None,
        tool_policy: None,
        source_extension: None,
        runtime,
        caps,
    })
    .await
    .unwrap();
    session.refresh_tools(&working_dir.to_string_lossy()).await;
    (session, store)
}

fn is_compact_summary_request(messages: &[LlmMessage]) -> bool {
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
                EventPayload::UserMessage {
                    message_id: new_message_id(),
                    text: format!("old user {index} {}", "x ".repeat(24)),
                    attachments: vec![],
                },
            )
            .await
            .unwrap();
        session
            .emit_durable(
                None,
                EventPayload::AssistantMessageCompleted {
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
            EventPayload::SystemPromptConfigured {
                text: "integration system prompt".into(),
                fingerprint: hex_fingerprint(b"integration system prompt"),
                extra_system_prompt: None,
            },
        )
        .await
        .unwrap();
}

fn sample_compaction() -> CompactResult {
    CompactResult {
        pre_tokens: 100,
        post_tokens: 10,
        summary: "integration summary".into(),
        messages_removed: 2,
        context_messages: vec![LlmMessage::user(
            "<compact_summary>\nSummary:\nintegration\n</compact_summary>",
        )],
        retained_messages: vec![LlmMessage::user("kept tail")],
        transcript_path: None,
    }
}

async fn compact_boundary_event_count(store: &dyn EventStore, session_id: &SessionId) -> usize {
    store
        .replay_events(session_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| matches!(event.payload, EventPayload::CompactBoundaryCreated { .. }))
        .count()
}

/// 在 compact LLM 调用期间注入 durable 事件，使 `base_event_seq` 过期。
///
/// 事件在 mock 内部、LLM 返回前注入，避免测试侧与 mock 之间的 Notify/oneshot 竞态。
struct RaceOnCompactLlm {
    main_calls: AtomicUsize,
    main_requests: Arc<std::sync::Mutex<Vec<Vec<LlmMessage>>>>,
    session_to_race: Arc<std::sync::Mutex<Option<Arc<Session>>>>,
    race_message: String,
}

#[async_trait::async_trait]
impl LlmProvider for RaceOnCompactLlm {
    async fn generate(
        &self,
        messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
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
                            EventPayload::UserMessage {
                                message_id: new_message_id(),
                                text: race_message,
                                attachments: vec![],
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
async fn persist_compact_result_accepts_new_tail_events() {
    let (session, store) = spawn_session(Arc::new(StaticOkLlm), ContextSettings::default()).await;
    configure_system_prompt(&session).await;
    seed_history(&session, 2).await;

    let stale_seq = session
        .latest_cursor()
        .await
        .unwrap()
        .expect("session should have cursor after seeding")
        .parse::<u64>()
        .expect("cursor should be u64 event seq");

    session
        .emit_durable(
            None,
            EventPayload::UserMessage {
                message_id: new_message_id(),
                text: "race event".into(),
                attachments: vec![],
            },
        )
        .await
        .unwrap();

    // Even if new events arrive after `base_event_seq` was observed, persist should succeed.
    let persisted = persist_compact_result(
        &session,
        &sample_compaction(),
        "auto_threshold",
        "integration system prompt",
        &hex_fingerprint(b"integration system prompt"),
        None,
        stale_seq,
        CompactStrategy::Auto,
    )
    .await
    .expect("persist should tolerate new tail events");
    assert_eq!(persisted.base_event_seq, stale_seq);
    assert_eq!(
        compact_boundary_event_count(store.as_ref(), session.id()).await,
        1,
        "persist should append compact boundary events once"
    );
    let provider_messages = session.read_model().await.unwrap().provider_messages();
    assert!(
        provider_messages
            .iter()
            .any(|m| m.joined_display_text("\n").contains("kept tail")),
        "retained messages should be queryable after persist"
    );
    assert!(
        provider_messages
            .iter()
            .any(|m| m.joined_display_text("\n").contains("race event")),
        "tail delta events must be preserved after compaction"
    );
}

#[tokio::test]
async fn auto_compact_persist_race_preserves_tail_and_uses_compact_summary() {
    let main_requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let session_to_race = Arc::new(std::sync::Mutex::new(None));
    let llm = Arc::new(RaceOnCompactLlm {
        main_calls: AtomicUsize::new(0),
        main_requests: Arc::clone(&main_requests),
        session_to_race: Arc::clone(&session_to_race),
        race_message: "concurrent race during compact".into(),
    });

    let (session, store) = spawn_session(
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
        .submit("current turn".into(), vec![], turn_id)
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
        main_messages.iter().any(is_compact_summary_message),
        "provider request should use compact summary"
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
        compact_boundary_event_count(store.as_ref(), session.id()).await,
        1,
        "persist should append compact boundary events"
    );
    let model = session.read_model().await.unwrap();
    let provider_messages = model.provider_messages();
    assert!(
        provider_messages.iter().any(is_compact_summary_message),
        "projection should expose compact summary after persist"
    );
    assert!(
        provider_messages.iter().any(|m| m
            .joined_display_text("\n")
            .contains("concurrent race during compact")),
        "projection must preserve tail delta user message that arrived during compact"
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
        .submit("follow up".into(), vec![], new_turn_id())
        .await
        .unwrap();
    let follow_up_result = follow_up.wait().await.unwrap();
    assert!(
        follow_up_result.output.is_ok(),
        "{:?}",
        follow_up_result.output
    );
    let after_follow_up = session.read_model().await.unwrap().provider_messages();
    assert!(
        after_follow_up
            .iter()
            .any(|m| m.joined_display_text("\n").contains("follow up ok")),
        "user can continue with a normal follow-up turn"
    );
}

#[tokio::test]
async fn compact_idle_session_skips_when_cursor_races_during_llm() {
    use astrcode_session::compaction_run::{
        IdleCompactionOutcome, IdleCompactionParams, compact_idle_session,
    };

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
    let (session, store) = spawn_session(
        Arc::clone(&race_llm) as Arc<dyn LlmProvider>,
        context.clone(),
    )
    .await;
    let session = Arc::new(session);
    *session_to_race.lock().unwrap() = Some(Arc::clone(&session));
    configure_system_prompt(session.as_ref()).await;
    seed_history(session.as_ref(), 3).await;

    let state = session.read_model().await.unwrap();
    let caps = test_caps(race_llm.clone(), context);
    let extension_runner = caps.extension_runner_arc();
    let context_assembler = caps.context_assembler_arc();
    let llm = caps.llm();
    let tools = session
        .refresh_tools(&state.working_dir)
        .await
        .list_definitions();
    let provider_messages = state.provider_messages();

    let session_for_race = Arc::clone(&session);
    let compact_task = tokio::spawn(async move {
        compact_idle_session(
            session_for_race.as_ref(),
            extension_runner.as_ref(),
            context_assembler.as_ref(),
            llm,
            &state,
            &tools,
            IdleCompactionParams {
                keep_recent_turns: None,
                transcript_path: None,
                provider_messages,
            },
        )
        .await
    });

    let outcome = compact_task.await.unwrap().unwrap();
    assert!(
        matches!(outcome, IdleCompactionOutcome::Compacted { .. }),
        "idle compact should persist even when cursor races, got {outcome:?}"
    );
    assert_eq!(
        compact_boundary_event_count(store.as_ref(), session.as_ref().id()).await,
        1,
        "compact boundary should be written after persist"
    );
    let model = session.read_model().await.unwrap();
    let provider_messages = model.provider_messages();
    assert!(
        provider_messages.iter().any(|m| m
            .joined_display_text("\n")
            .contains("race during idle compact")),
        "projection must preserve tail delta user message that arrived during compact"
    );
}

struct StaticOkLlm;

#[async_trait::async_trait]
impl LlmProvider for StaticOkLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(LlmEvent::ContentDelta { delta: "ok".into() });
        let _ = tx.send(LlmEvent::Done {
            finish_reason: "stop".into(),
        });
        Ok(rx)
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 200_000,
            max_output_tokens: 1024,
        }
    }
}
