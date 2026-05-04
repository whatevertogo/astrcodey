//! 子会话派生器 — 当扩展返回 `RunSession` 时的处理逻辑。
//!
//! [`ServerSessionSpawner`] 创建子会话、用子 Agent 执行一轮对话，
//! 将事件持久化到子会话存储，并经由 [`ProgressTx`] 将关键进展
//! 转译为 [`ToolOutputDelta`] 实时反馈给父会话的 TUI。

use std::{collections::HashSet, sync::Arc};

use astrcode_context::manager::LlmContextAssembler;
use astrcode_core::{
    event::{Event, EventPayload, ToolOutputStream},
    types::{new_message_id, new_turn_id},
};
use astrcode_extensions::{
    runner::ExtensionRunner,
    runtime::{SpawnRequest, SpawnResult},
};
use tokio::sync::{Mutex, mpsc};

use super::{
    CompactContinuationAppendInput, CompactContinuationCreateInput, SessionManager,
    append_compact_continuation_events, create_compact_continuation_session,
};
use crate::{
    agent::{
        AgentLoop, AgentServices, AgentSignal, AutoCompactFailureTracker,
        compact::compact_trigger_name, drive_agent, tool_name_matches_allowlist,
    },
    bootstrap::{build_system_prompt_snapshot, build_tool_registry_snapshot, prompt_fingerprint},
};


/// 服务器端的会话派生器，实现 `SessionSpawner` trait。
///
/// 当扩展返回 `ExtensionToolOutcome::RunSession` 时，
/// 扩展运行器通过此派生器创建子会话并运行 Agent 回合。
pub(crate) struct ServerSessionSpawner {
    pub(crate) session_manager: Arc<SessionManager>,
    pub(crate) llm: Arc<dyn astrcode_core::llm::LlmProvider>,
    pub(crate) context_assembler: Arc<LlmContextAssembler>,
    pub(crate) auto_compact_failures: Arc<AutoCompactFailureTracker>,
    pub(crate) extension_runner: Arc<ExtensionRunner>,
    pub(crate) read_timeout_secs: u64,
}

#[async_trait::async_trait]
impl astrcode_extensions::runtime::SessionSpawner for ServerSessionSpawner {
    async fn spawn(
        &self,
        parent_session_id: &str,
        request: SpawnRequest,
    ) -> Result<SpawnResult, String> {
        let progress = ProgressTx::new(request.tool_call_id, request.event_tx);
        let child_name = request.name.clone();
        let user_prompt = request.user_prompt.clone();
        let model_id = match request.model_preference.clone() {
            Some(model) => model,
            None => {
                self.session_manager
                    .read_model(&parent_session_id.to_string())
                    .await
                    .map_err(|e| format!("parent session {parent_session_id} not found: {e}"))?
                    .model_id
            },
        };

        let create_event = self
            .session_manager
            .create(
                &request.working_dir,
                &model_id,
                2048,
                Some(parent_session_id),
            )
            .await
            .map_err(|e| format!("create child session: {e}"))?;

        let child_sid = create_event.session_id.to_string();
        let child_turn_id = new_turn_id();

        let tool_registry = build_tool_registry_snapshot(
            &self.extension_runner,
            &request.working_dir,
            self.read_timeout_secs,
        )
        .await;

        let allowed_tools: HashSet<String> = request.allowed_tools.iter().cloned().collect();
        let mut prompt_tools = tool_registry.list_definitions();
        if !allowed_tools.is_empty() {
            prompt_tools.retain(|tool| tool_name_matches_allowlist(&allowed_tools, &tool.name));
        }
        let (system_prompt, fingerprint) = build_system_prompt_snapshot(
            &self.extension_runner,
            &child_sid,
            &request.working_dir,
            &model_id,
            &prompt_tools,
            Some(&request.system_prompt),
        )
        .await
        .map_err(|e| format!("build child system prompt: {e}"))?;

        append_child_session_payload(
            self.session_manager.as_ref(),
            &child_sid,
            EventPayload::SystemPromptConfigured {
                text: system_prompt.clone(),
                fingerprint,
            },
        )
        .await?;

        append_child_payload(
            self.session_manager.as_ref(),
            &child_sid,
            &child_turn_id,
            EventPayload::TurnStarted,
        )
        .await?;
        append_child_payload(
            self.session_manager.as_ref(),
            &child_sid,
            &child_turn_id,
            EventPayload::UserMessage {
                message_id: new_message_id(),
                text: user_prompt.clone(),
            },
        )
        .await?;

        progress.emit(
            ToolOutputStream::Stdout,
            format!("child agent '{child_name}' started: {child_sid} using {model_id}\n"),
        );

        let agent = AgentLoop::new(
            child_sid.clone(),
            request.working_dir.clone(),
            system_prompt.clone(),
            model_id.clone(),
            AgentServices {
                llm: Arc::clone(&self.llm),
                tool_registry: Arc::clone(&tool_registry),
                extension_runner: Arc::clone(&self.extension_runner),
                context_assembler: Arc::clone(&self.context_assembler),
                session_manager: Arc::clone(&self.session_manager),
                auto_compact_failures: Arc::clone(&self.auto_compact_failures),
            },
        )
        .with_tool_allowlist(request.allowed_tools);

        let current_child_sid = Arc::new(Mutex::new(child_sid.clone()));
        let final_child_sid_ref = Arc::clone(&current_child_sid);
        let cti = child_turn_id.clone();
        let sm = Arc::clone(&self.session_manager);
        let pf = progress.clone();
        let wd = request.working_dir.clone();
        let sp = system_prompt.clone();
        let mid = model_id.clone();
        let auto_compact_failures = Arc::clone(&self.auto_compact_failures);
        let (output, emitted_error) =
            drive_agent(&agent, &user_prompt, Vec::new(), move |signal| {
                let sm = sm.clone();
                let current_child_sid = Arc::clone(&current_child_sid);
                let cti = cti.clone();
                let p = pf.clone();
                let wd = wd.clone();
                let sp = sp.clone();
                let mid = mid.clone();
                let auto_compact_failures = Arc::clone(&auto_compact_failures);
                async move {
                    match signal {
                        AgentSignal::Event(payload) => {
                            let sid = current_child_sid.lock().await.clone();
                            let _ = append_child_payload(&sm, &sid, &cti, payload.clone()).await;
                            p.forward(&payload);
                        },
                        AgentSignal::AutoCompact {
                            trigger,
                            compaction,
                            reply,
                        } => {
                            let parent_sid = current_child_sid.lock().await.clone();
                            let result = async {
                                let continuation = create_compact_continuation_session(
                                    &sm,
                                    CompactContinuationCreateInput {
                                        parent_session_id: parent_sid.clone(),
                                        working_dir: wd,
                                        model_id: mid,
                                    },
                                )
                                .await?;
                                append_compact_continuation_events(
                                    &sm,
                                    CompactContinuationAppendInput {
                                        session: continuation,
                                        system_prompt_fingerprint: prompt_fingerprint(&sp),
                                        system_prompt: sp,
                                        trigger_name: compact_trigger_name(trigger).into(),
                                        compaction,
                                    },
                                )
                                .await
                                .map(|events| events.child_session_id)
                            }
                            .await;
                            if let Ok(child_sid) = &result {
                                auto_compact_failures.transfer_session(&parent_sid, child_sid);
                                p.emit(
                                    ToolOutputStream::Stdout,
                                    format!("child agent continued: {child_sid}\n"),
                                );
                                *current_child_sid.lock().await = child_sid.clone();
                            }
                            let _ = reply.send(result);
                        },
                    }
                }
            })
            .await;
        let final_child_sid = final_child_sid_ref.lock().await.clone();

        match output {
            Ok(output) => {
                append_child_payload(
                    self.session_manager.as_ref(),
                    &final_child_sid,
                    &child_turn_id,
                    EventPayload::TurnCompleted {
                        finish_reason: output.finish_reason.clone(),
                    },
                )
                .await?;
                progress.emit(
                    ToolOutputStream::Stdout,
                    format!("child turn completed: {}\n", output.finish_reason),
                );
                Ok(SpawnResult {
                    content: output.text,
                    child_session_id: final_child_sid,
                })
            },
            Err(e) => Ok(SpawnResult {
                content: {
                    if !emitted_error {
                        append_child_payload(
                            self.session_manager.as_ref(),
                            &final_child_sid,
                            &child_turn_id,
                            EventPayload::ErrorOccurred {
                                code: -32603,
                                message: e.to_string(),
                                recoverable: false,
                            },
                        )
                        .await?;
                    }
                    append_child_payload(
                        self.session_manager.as_ref(),
                        &final_child_sid,
                        &child_turn_id,
                        EventPayload::TurnCompleted {
                            finish_reason: "error".into(),
                        },
                    )
                    .await?;
                    progress.emit(
                        ToolOutputStream::Stderr,
                        format!("child agent error: {e}\n"),
                    );
                    format!("child agent error: {e}")
                },
                child_session_id: final_child_sid,
            }),
        }
    }
}

/// 将子 agent 事件转发为父级工具调用的 [`ToolOutputDelta`] 进度事件。
///
/// 持有父级工具调用 ID 和事件发送通道。`emit` 发送字符串消息，
/// `forward` 将子 agent 事件自动转译为对应的进度描述。
#[derive(Clone)]
struct ProgressTx {
    call_id: Option<String>,
    tx: Option<mpsc::UnboundedSender<EventPayload>>,
}

impl ProgressTx {
    fn new(call_id: Option<String>, tx: Option<mpsc::UnboundedSender<EventPayload>>) -> Self {
        Self { call_id, tx }
    }

    fn emit(&self, stream: ToolOutputStream, delta: impl Into<String>) {
        let Some(call_id) = &self.call_id else { return };
        let Some(tx) = &self.tx else { return };
        let delta = delta.into();
        if delta.is_empty() {
            return;
        }
        let _ = tx.send(EventPayload::ToolOutputDelta {
            call_id: call_id.clone(),
            stream,
            delta,
        });
    }

    fn forward(&self, payload: &EventPayload) {
        if let Some((stream, delta)) = child_progress_delta(payload) {
            self.emit(stream, delta);
        }
    }
}

async fn append_child_payload(
    session_manager: &SessionManager,
    child_sid: &str,
    child_turn_id: &str,
    payload: EventPayload,
) -> Result<(), String> {
    if payload.is_durable() {
        session_manager
            .append_event(Event::new(
                child_sid.to_string(),
                Some(child_turn_id.to_string()),
                payload,
            ))
            .await
            .map_err(|e| format!("append child event: {e}"))?;
    }
    Ok(())
}

async fn append_child_session_payload(
    session_manager: &SessionManager,
    child_sid: &str,
    payload: EventPayload,
) -> Result<(), String> {
    if payload.is_durable() {
        session_manager
            .append_event(Event::new(child_sid.to_string(), None, payload))
            .await
            .map_err(|e| format!("append child session event: {e}"))?;
    }
    Ok(())
}

fn child_progress_delta(payload: &EventPayload) -> Option<(ToolOutputStream, String)> {
    match payload {
        EventPayload::AssistantMessageStarted { .. } => {
            Some((ToolOutputStream::Stdout, "child assistant started\n".into()))
        },
        EventPayload::AssistantTextDelta { delta, .. } => {
            if delta.is_empty() {
                None
            } else {
                Some((ToolOutputStream::Stdout, delta.clone()))
            }
        },
        EventPayload::AssistantMessageCompleted { text, .. } => {
            let summary = one_line_summary(text);
            if summary.is_empty() {
                None
            } else {
                Some((
                    ToolOutputStream::Stdout,
                    format!("child assistant completed: {summary}\n"),
                ))
            }
        },
        EventPayload::ToolCallStarted { tool_name, .. } => Some((
            ToolOutputStream::Stdout,
            format!("child tool started: {tool_name}\n"),
        )),
        EventPayload::ToolOutputDelta { stream, delta, .. } => {
            Some((*stream, format!("child tool output: {delta}")))
        },
        EventPayload::ToolCallCompleted {
            tool_name, result, ..
        } => {
            let stream = if result.is_error {
                ToolOutputStream::Stderr
            } else {
                ToolOutputStream::Stdout
            };
            let detail = one_line_summary(result.error.as_deref().unwrap_or(&result.content));
            let suffix = if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            };
            Some((
                stream,
                format!("child tool completed: {tool_name}{suffix}\n"),
            ))
        },
        EventPayload::ErrorOccurred { message, .. } => Some((
            ToolOutputStream::Stderr,
            format!("child error: {message}\n"),
        )),
        EventPayload::TurnCompleted { finish_reason } => Some((
            ToolOutputStream::Stdout,
            format!("child turn completed: {finish_reason}\n"),
        )),
        _ => None,
    }
}

fn one_line_summary(text: &str) -> String {
    let mut summary = text.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX_CHARS: usize = 160;
    if summary.chars().count() > MAX_CHARS {
        let truncated: String = summary.chars().take(MAX_CHARS - 1).collect();
        summary = truncated;
        summary.push('…');
    }
    summary
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use astrcode_context::manager::LlmContextAssembler;
    use astrcode_core::{
        llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
        tool::ToolDefinition,
    };
    use astrcode_extensions::{
        runner::ExtensionRunner,
        runtime::{ExtensionRuntime, SessionSpawner, SpawnRequest},
    };
    use astrcode_storage::in_memory::InMemoryEventStore;

    use super::*;

    struct CompactThenLeafLlm {
        call_count: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for CompactThenLeafLlm {
        async fn generate(
            &self,
            _messages: Vec<LlmMessage>,
            _tools: Vec<ToolDefinition>,
        ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
            // The sequence drives one nested compact:
            // call 0 asks for a missing tool so the next provider context
            // crosses the compact threshold; call 1 is the forked compact
            // summary and intentionally invalid so fallback summary rendering
            // is deterministic; later calls prove events land on the leaf.
            let call = self.call_count.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = mpsc::unbounded_channel();
            match call {
                0 => {
                    let _ = tx.send(LlmEvent::ToolCallStart {
                        call_id: "missing-tool-call".into(),
                        name: "missingTool".into(),
                        arguments: "{}".into(),
                    });
                    let _ = tx.send(LlmEvent::Done {
                        finish_reason: "tool_calls".into(),
                    });
                },
                1 => {
                    let _ = tx.send(LlmEvent::ContentDelta {
                        delta: "invalid compact summary; deterministic fallback should run".into(),
                    });
                    let _ = tx.send(LlmEvent::Done {
                        finish_reason: "stop".into(),
                    });
                },
                _ => {
                    let _ = tx.send(LlmEvent::ContentDelta {
                        delta: "leaf ok".into(),
                    });
                    let _ = tx.send(LlmEvent::Done {
                        finish_reason: "stop".into(),
                    });
                },
            }
            Ok(rx)
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: 200000,
                max_output_tokens: 1024,
            }
        }
    }

    fn test_spawner(
        session_manager: Arc<SessionManager>,
        llm: Arc<CompactThenLeafLlm>,
    ) -> ServerSessionSpawner {
        let settings = astrcode_context::settings::ContextWindowSettings {
            compact_threshold_percent: 0.0,
            ..Default::default()
        };
        ServerSessionSpawner {
            session_manager,
            llm,
            context_assembler: Arc::new(LlmContextAssembler::new(settings)),
            auto_compact_failures: Arc::new(AutoCompactFailureTracker::default()),
            extension_runner: Arc::new(ExtensionRunner::new(
                Duration::from_secs(1),
                Arc::new(ExtensionRuntime::new()),
            )),
            read_timeout_secs: 1,
        }
    }

    #[tokio::test]
    async fn spawned_session_auto_compact_returns_leaf_child() {
        let session_manager = Arc::new(SessionManager::new(Arc::new(InMemoryEventStore::new())));
        let parent = session_manager
            .create(".", "mock", 2048, None)
            .await
            .unwrap();
        let llm = Arc::new(CompactThenLeafLlm {
            call_count: AtomicUsize::new(0),
        });
        let spawner = test_spawner(Arc::clone(&session_manager), Arc::clone(&llm));
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();

        let result = spawner
            .spawn(
                &parent.session_id,
                SpawnRequest {
                    name: "nested".into(),
                    system_prompt: "nested extra prompt".into(),
                    user_prompt: "current nested prompt".into(),
                    working_dir: ".".into(),
                    allowed_tools: vec![],
                    model_preference: Some("mock".into()),
                    tool_call_id: Some("tool-call-1".into()),
                    event_tx: Some(progress_tx),
                },
            )
            .await
            .unwrap();

        let leaf = session_manager
            .read_model(&result.child_session_id)
            .await
            .unwrap();
        let previous_child_id = leaf
            .parent_session_id
            .clone()
            .expect("leaf should continue from a previous spawned child");
        assert_ne!(previous_child_id, result.child_session_id);
        assert_eq!(result.content, "leaf ok");
        assert!(llm.call_count.load(Ordering::SeqCst) >= 3);

        let previous = session_manager
            .read_model(&previous_child_id)
            .await
            .unwrap();
        let mut ancestor_id = previous_child_id.clone();
        loop {
            let ancestor = session_manager.read_model(&ancestor_id).await.unwrap();
            if ancestor.parent_session_id.as_deref() == Some(parent.session_id.as_str()) {
                break;
            }
            ancestor_id = ancestor
                .parent_session_id
                .expect("continuation chain should stay linked to the root parent");
        }
        assert!(
            previous.messages.iter().all(|message| {
                !message
                    .content
                    .iter()
                    .any(|content| matches!(content, astrcode_core::llm::LlmContent::Text { text } if text.contains("leaf ok")))
            }),
            "events after continuation should not be appended to the previous child"
        );
        assert!(
            leaf.messages.iter().any(|message| {
                message
                    .content
                    .iter()
                    .any(|content| matches!(content, astrcode_core::llm::LlmContent::Text { text } if text.contains("leaf ok")))
            }),
            "events after continuation should be appended to the leaf child"
        );

        let mut saw_continued_progress = false;
        while let Ok(payload) = progress_rx.try_recv() {
            if let EventPayload::ToolOutputDelta { delta, .. } = payload {
                saw_continued_progress |= delta.contains("child agent continued");
            }
        }
        assert!(saw_continued_progress);
    }
}
