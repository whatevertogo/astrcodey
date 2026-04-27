//! Command handler — processes ClientCommand using ServerRuntime.
//!
//! Transport-agnostic: used by both stdio binary and in-process CLI.

use std::sync::Arc;

use astrcode_core::{
    config::ModelSelection,
    event::{Event, EventPayload},
    extension::ExtensionEvent,
    llm::{LlmContent, LlmMessage},
    types::{SessionId, TurnId, new_message_id, new_turn_id},
};
use astrcode_extensions::context::ServerExtensionContext;
use astrcode_protocol::{
    commands::ClientCommand,
    events::{ClientNotification, MessageDto, SessionListItem, SessionSnapshot},
};
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinHandle,
};

use crate::{agent::Agent, bootstrap::ServerRuntime};

/// Handles commands and emits notifications to a broadcast channel.
pub struct CommandHandler {
    runtime: Arc<ServerRuntime>,
    event_tx: broadcast::Sender<ClientNotification>,
    active_session_id: Option<SessionId>,
    active_turn: Option<ActiveTurn>,
}

struct ActiveTurn {
    session_id: SessionId,
    turn_id: TurnId,
    handle: JoinHandle<()>,
}

impl CommandHandler {
    pub fn new(
        runtime: Arc<ServerRuntime>,
        event_tx: broadcast::Sender<ClientNotification>,
    ) -> Self {
        Self {
            runtime,
            event_tx,
            active_session_id: None,
            active_turn: None,
        }
    }

    pub async fn handle(&mut self, cmd: ClientCommand) -> Result<(), String> {
        self.clear_finished_turn();

        match cmd {
            ClientCommand::CreateSession { working_dir } => {
                self.create_session(working_dir).await;
            },

            ClientCommand::SubmitPrompt { text, .. } => {
                self.submit_prompt(text).await?;
            },

            ClientCommand::ListSessions => {
                let sessions = self
                    .runtime
                    .session_manager
                    .list()
                    .await
                    .unwrap_or_default();
                let items: Vec<_> = sessions
                    .into_iter()
                    .map(|sid| SessionListItem {
                        session_id: sid,
                        created_at: String::new(),
                        last_active_at: String::new(),
                        working_dir: String::new(),
                        parent_session_id: None,
                    })
                    .collect();
                let _ = self
                    .event_tx
                    .send(ClientNotification::SessionList { sessions: items });
            },

            ClientCommand::Abort => {
                self.abort_active_turn().await?;
            },

            ClientCommand::ResumeSession { session_id }
            | ClientCommand::SwitchSession { session_id } => {
                self.resume_session(session_id).await;
            },

            ClientCommand::DeleteSession { session_id } => {
                // Dispatch SessionShutdown hook before deletion
                {
                    let ext_ctx = ServerExtensionContext::new(
                        session_id.clone(),
                        String::new(),
                        ModelSelection {
                            profile_name: String::new(),
                            model: self.runtime.effective.llm.model_id.clone(),
                            provider_kind: String::new(),
                        },
                    );
                    if let Err(e) = self
                        .runtime
                        .extension_runner
                        .dispatch(ExtensionEvent::SessionShutdown, &ext_ctx)
                        .await
                    {
                        self.send_error(-32603, &e.to_string());
                        return Ok(());
                    }
                }
                match self.runtime.session_manager.delete(&session_id).await {
                    Ok(()) => {
                        if self.active_session_id.as_ref() == Some(&session_id) {
                            self.active_session_id = None;
                        }
                    },
                    Err(e) => self.send_error(40401, &format!("Session not found: {e}")),
                }
            },

            _ => {
                self.send_error(-32601, "Not implemented");
            },
        }
        Ok(())
    }

    async fn create_session(&mut self, working_dir: String) {
        let model_id = self.runtime.effective.llm.model_id.clone();
        match self
            .runtime
            .session_manager
            .create(&working_dir, &model_id, 2048)
            .await
        {
            Ok(event) => {
                self.active_session_id = Some(event.session_id.clone());
                let ext_ctx = ServerExtensionContext::new(
                    event.session_id.clone(),
                    working_dir.clone(),
                    ModelSelection {
                        profile_name: String::new(),
                        model: self.runtime.effective.llm.model_id.clone(),
                        provider_kind: String::new(),
                    },
                );
                if let Err(e) = self
                    .runtime
                    .extension_runner
                    .dispatch(ExtensionEvent::SessionStart, &ext_ctx)
                    .await
                {
                    self.send_error(-32603, &e.to_string());
                    return;
                }
                let _ = self.event_tx.send(ClientNotification::Event(event));
            },
            Err(e) => self.send_error(-32603, &e.to_string()),
        }
    }

    async fn submit_prompt(&mut self, text: String) -> Result<(), String> {
        if self.active_turn.is_some() {
            self.send_error(40900, "A turn is already running");
            return Ok(());
        }

        let sid = self.ensure_session().await?;

        // Dispatch UserPromptSubmit hook
        {
            let ext_ctx = ServerExtensionContext::new(
                sid.clone(),
                std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| ".".into()),
                ModelSelection {
                    profile_name: String::new(),
                    model: self.runtime.effective.llm.model_id.clone(),
                    provider_kind: String::new(),
                },
            );
            if let Err(e) = self
                .runtime
                .extension_runner
                .dispatch(ExtensionEvent::UserPromptSubmit, &ext_ctx)
                .await
            {
                self.send_error(-32603, &e.to_string());
                return Ok(());
            }
        }
        let session = self
            .runtime
            .session_manager
            .get(&sid)
            .await
            .ok_or_else(|| format!("Session {sid} vanished"))?;

        let state = session.state.read().await.clone();
        let history = state.messages;
        let working_dir = state.working_dir;
        let turn_id = new_turn_id();

        self.record_and_broadcast(&sid, Some(&turn_id), EventPayload::TurnStarted)
            .await?;
        self.record_and_broadcast(
            &sid,
            Some(&turn_id),
            EventPayload::UserMessage {
                message_id: new_message_id(),
                text: text.clone(),
            },
        )
        .await?;
        self.record_and_broadcast(&sid, Some(&turn_id), EventPayload::AgentRunStarted)
            .await?;

        let handle = spawn_agent_turn(
            self.runtime.clone(),
            self.event_tx.clone(),
            sid.clone(),
            turn_id.clone(),
            working_dir,
            history,
            text,
        );
        self.active_turn = Some(ActiveTurn {
            session_id: sid,
            turn_id,
            handle,
        });
        Ok(())
    }

    async fn abort_active_turn(&mut self) -> Result<(), String> {
        let Some(active_turn) = self.active_turn.take() else {
            self.send_error(40400, "No active turn");
            return Ok(());
        };

        if !active_turn.handle.is_finished() {
            active_turn.handle.abort();
        }

        record_and_broadcast(
            &self.runtime,
            &self.event_tx,
            &active_turn.session_id,
            Some(&active_turn.turn_id),
            EventPayload::TurnCompleted {
                finish_reason: "aborted".into(),
            },
        )
        .await?;
        record_and_broadcast(
            &self.runtime,
            &self.event_tx,
            &active_turn.session_id,
            Some(&active_turn.turn_id),
            EventPayload::AgentRunCompleted {
                reason: "aborted".into(),
            },
        )
        .await?;
        Ok(())
    }

    fn clear_finished_turn(&mut self) {
        if self
            .active_turn
            .as_ref()
            .is_some_and(|active_turn| active_turn.handle.is_finished())
        {
            self.active_turn = None;
        }
    }

    async fn resume_session(&mut self, session_id: SessionId) {
        match self.runtime.session_manager.resume(&session_id).await {
            Ok(session) => {
                let state = session.state.read().await;
                self.active_session_id = Some(session_id.clone());
                let snapshot = SessionSnapshot {
                    session_id: session_id.clone(),
                    cursor: String::new(),
                    messages: state.messages.iter().map(message_to_dto).collect(),
                    model_id: state.model_id.clone(),
                    working_dir: state.working_dir.clone(),
                };
                let _ = self.event_tx.send(ClientNotification::SessionResumed {
                    session_id,
                    snapshot,
                });
            },
            Err(e) => self.send_error(40401, &format!("Session not found: {e}")),
        }
    }

    async fn ensure_session(&mut self) -> Result<SessionId, String> {
        if let Some(ref sid) = self.active_session_id {
            return Ok(sid.clone());
        }

        let model_id = self.runtime.effective.llm.model_id.clone();
        let wd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".into());
        let event = self
            .runtime
            .session_manager
            .create(&wd, &model_id, 2048)
            .await
            .map_err(|e| format!("create session: {e}"))?;

        let sid = event.session_id.clone();
        self.active_session_id = Some(sid.clone());
        let _ = self.event_tx.send(ClientNotification::Event(event));
        Ok(sid)
    }

    async fn record_and_broadcast(
        &self,
        session_id: &SessionId,
        turn_id: Option<&TurnId>,
        payload: EventPayload,
    ) -> Result<Event, String> {
        record_and_broadcast(&self.runtime, &self.event_tx, session_id, turn_id, payload).await
    }

    fn send_error(&self, code: i32, message: &str) {
        let _ = self.event_tx.send(ClientNotification::Error {
            code,
            message: message.into(),
        });
    }
}

fn spawn_agent_turn(
    runtime: Arc<ServerRuntime>,
    event_tx: broadcast::Sender<ClientNotification>,
    sid: SessionId,
    turn_id: TurnId,
    working_dir: String,
    history: Vec<LlmMessage>,
    text: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let agent = Agent::new(
            sid.clone(),
            working_dir,
            runtime.llm_provider.clone(),
            runtime.prompt_provider.clone(),
            runtime.capability.clone(),
            runtime.extension_runner.clone(),
            runtime.effective.llm.model_id.clone(),
            runtime.context_settings.summary_reserve_tokens * 3,
        );

        let (agent_event_tx, mut agent_event_rx) = mpsc::unbounded_channel();
        let agent_future = agent.process_prompt(&text, history, Some(agent_event_tx));
        tokio::pin!(agent_future);

        let mut emitted_error = false;
        let mut events_closed = false;
        let output = loop {
            tokio::select! {
                result = &mut agent_future => break result,
                payload = agent_event_rx.recv(), if !events_closed => {
                    match payload {
                        Some(payload) => {
                            if matches!(payload, EventPayload::ErrorOccurred { .. }) {
                                emitted_error = true;
                            }
                            let _ = record_and_broadcast(
                                &runtime,
                                &event_tx,
                                &sid,
                                Some(&turn_id),
                                payload,
                            )
                            .await;
                        },
                        None => {
                            events_closed = true;
                        },
                    }
                },
            }
        };

        while let Some(payload) = agent_event_rx.recv().await {
            if matches!(payload, EventPayload::ErrorOccurred { .. }) {
                emitted_error = true;
            }
            let _ = record_and_broadcast(&runtime, &event_tx, &sid, Some(&turn_id), payload).await;
        }

        match output {
            Ok(output) => {
                let _ = record_and_broadcast(
                    &runtime,
                    &event_tx,
                    &sid,
                    Some(&turn_id),
                    EventPayload::TurnCompleted {
                        finish_reason: output.finish_reason.clone(),
                    },
                )
                .await;
                let _ = record_and_broadcast(
                    &runtime,
                    &event_tx,
                    &sid,
                    Some(&turn_id),
                    EventPayload::AgentRunCompleted {
                        reason: output.finish_reason,
                    },
                )
                .await;
            },
            Err(e) => {
                if !emitted_error {
                    let _ = record_and_broadcast(
                        &runtime,
                        &event_tx,
                        &sid,
                        Some(&turn_id),
                        EventPayload::ErrorOccurred {
                            code: -32603,
                            message: e.to_string(),
                            recoverable: false,
                        },
                    )
                    .await;
                }
                let _ = record_and_broadcast(
                    &runtime,
                    &event_tx,
                    &sid,
                    Some(&turn_id),
                    EventPayload::TurnCompleted {
                        finish_reason: "error".into(),
                    },
                )
                .await;
                let _ = record_and_broadcast(
                    &runtime,
                    &event_tx,
                    &sid,
                    Some(&turn_id),
                    EventPayload::AgentRunCompleted {
                        reason: "error".into(),
                    },
                )
                .await;
            },
        }
    })
}

async fn record_and_broadcast(
    runtime: &ServerRuntime,
    event_tx: &broadcast::Sender<ClientNotification>,
    session_id: &SessionId,
    turn_id: Option<&TurnId>,
    payload: EventPayload,
) -> Result<Event, String> {
    let event = Event::new(session_id.clone(), turn_id.cloned(), payload);
    let event = if event.payload.is_durable() {
        runtime
            .session_manager
            .append_event(event)
            .await
            .map_err(|e| e.to_string())?
    } else {
        event
    };

    let _ = event_tx.send(ClientNotification::Event(event.clone()));
    Ok(event)
}

fn message_to_dto(message: &LlmMessage) -> MessageDto {
    MessageDto {
        role: message.role.as_str().to_string(),
        content: message
            .content
            .iter()
            .map(content_to_text)
            .collect::<Vec<_>>()
            .join(""),
    }
}

fn content_to_text(content: &LlmContent) -> String {
    match content {
        LlmContent::Text { text } => text.clone(),
        LlmContent::Image { .. } => "[image]".into(),
        LlmContent::ToolCall {
            name, arguments, ..
        } => format!("tool call: {name}({arguments})"),
        LlmContent::ToolResult { content, .. } => content.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::{future, sync::Arc, time::Duration};

    use astrcode_core::{
        config::{EffectiveConfig, LlmSettings, OpenAiApiMode},
        event::EventPayload,
        llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
        prompt::{PromptContext, PromptPlan, PromptProvider},
        tool::ToolDefinition,
    };
    use astrcode_protocol::{commands::ClientCommand, events::ClientNotification};
    use astrcode_storage::noop::NoopEventStore;
    use tokio::sync::mpsc;

    use super::*;
    use crate::{capability::CapabilityRouter, session::SessionManager};

    struct MockLlm;

    #[async_trait::async_trait]
    impl LlmProvider for MockLlm {
        async fn generate(
            &self,
            _messages: Vec<LlmMessage>,
            _tools: Vec<ToolDefinition>,
        ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
            let (tx, rx) = mpsc::unbounded_channel();
            let _ = tx.send(LlmEvent::ContentDelta {
                delta: "hello".into(),
            });
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "stop".into(),
            });
            Ok(rx)
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: 1024,
                max_output_tokens: 1024,
            }
        }
    }

    struct EmptyPrompt;

    #[async_trait::async_trait]
    impl PromptProvider for EmptyPrompt {
        async fn assemble(&self, _context: PromptContext) -> PromptPlan {
            PromptPlan {
                system_blocks: vec![],
                prepend_messages: vec![],
                append_messages: vec![],
                extra_tools: vec![],
            }
        }
    }

    struct PendingLlm;

    #[async_trait::async_trait]
    impl LlmProvider for PendingLlm {
        async fn generate(
            &self,
            _messages: Vec<LlmMessage>,
            _tools: Vec<ToolDefinition>,
        ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
            future::pending().await
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: 1024,
                max_output_tokens: 1024,
            }
        }
    }

    fn test_runtime_with_llm(llm_provider: Arc<dyn LlmProvider>) -> Arc<ServerRuntime> {
        use astrcode_context::{
            budget::ToolResultBudget, file_access::FileAccessTracker,
            settings::ContextWindowSettings,
        };
        Arc::new(ServerRuntime {
            session_manager: Arc::new(SessionManager::new(Arc::new(NoopEventStore::new()))),
            llm_provider,
            prompt_provider: Arc::new(EmptyPrompt),
            capability: Arc::new(CapabilityRouter::new()),
            extension_runner: Arc::new(astrcode_extensions::runner::ExtensionRunner::new(
                Duration::from_secs(1),
            )),
            effective: EffectiveConfig {
                llm: LlmSettings {
                    provider_kind: "mock".into(),
                    base_url: String::new(),
                    api_key: String::new(),
                    api_mode: OpenAiApiMode::ChatCompletions,
                    model_id: "mock-model".into(),
                    max_tokens: 1024,
                    context_limit: 1024,
                    connect_timeout_secs: 1,
                    read_timeout_secs: 1,
                    max_retries: 0,
                    retry_base_delay_ms: 0,
                },
            },
            context_settings: ContextWindowSettings::default(),
            tool_result_budget: Arc::new(ToolResultBudget::new(8192, 65536, 24576)),
            file_access_tracker: Arc::new(std::sync::Mutex::new(FileAccessTracker::new(64))),
        })
    }

    fn test_runtime() -> Arc<ServerRuntime> {
        test_runtime_with_llm(Arc::new(MockLlm))
    }

    async fn recv_event(
        event_rx: &mut broadcast::Receiver<ClientNotification>,
    ) -> ClientNotification {
        tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("event should arrive")
            .expect("event channel should stay open")
    }

    async fn wait_for_turn_completed(
        event_rx: &mut broadcast::Receiver<ClientNotification>,
    ) -> String {
        loop {
            let notification = recv_event(event_rx).await;
            let ClientNotification::Event(event) = notification else {
                continue;
            };
            if let EventPayload::TurnCompleted { finish_reason } = event.payload {
                return finish_reason;
            }
        }
    }

    async fn collect_turn_ids_until_completed(
        event_rx: &mut broadcast::Receiver<ClientNotification>,
    ) -> (String, Vec<Option<TurnId>>) {
        let mut turn_ids = Vec::new();
        loop {
            let notification = recv_event(event_rx).await;
            let ClientNotification::Event(event) = notification else {
                continue;
            };
            match event.payload {
                EventPayload::TurnStarted
                | EventPayload::UserMessage { .. }
                | EventPayload::AssistantMessageCompleted { .. } => {
                    turn_ids.push(event.turn_id);
                },
                EventPayload::TurnCompleted { finish_reason } => {
                    turn_ids.push(event.turn_id);
                    return (finish_reason, turn_ids);
                },
                _ => {},
            }
        }
    }

    #[tokio::test]
    async fn submit_prompt_uses_one_turn_id_for_turn_events() {
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(64);
        let mut handler = CommandHandler::new(test_runtime(), event_tx);

        handler
            .handle(ClientCommand::CreateSession {
                working_dir: ".".into(),
            })
            .await
            .unwrap();
        handler
            .handle(ClientCommand::SubmitPrompt {
                text: "hi".into(),
                attachments: vec![],
            })
            .await
            .unwrap();
        let (finish_reason, turn_ids) = collect_turn_ids_until_completed(&mut event_rx).await;
        assert_eq!(finish_reason, "stop");

        assert!(
            turn_ids.len() >= 4,
            "expected turn lifecycle, user and assistant events"
        );
        let first = turn_ids[0].clone();
        assert!(first.is_some(), "turn events should carry a turn_id");
        assert!(
            turn_ids.iter().all(|turn_id| *turn_id == first),
            "all events in one prompt should share the same turn_id"
        );
    }

    #[tokio::test]
    async fn submit_prompt_rejects_second_running_turn() {
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(64);
        let mut handler =
            CommandHandler::new(test_runtime_with_llm(Arc::new(PendingLlm)), event_tx);

        handler
            .handle(ClientCommand::CreateSession {
                working_dir: ".".into(),
            })
            .await
            .unwrap();
        handler
            .handle(ClientCommand::SubmitPrompt {
                text: "first".into(),
                attachments: vec![],
            })
            .await
            .unwrap();
        handler
            .handle(ClientCommand::SubmitPrompt {
                text: "second".into(),
                attachments: vec![],
            })
            .await
            .unwrap();

        let mut saw_busy = false;
        while let Ok(notification) = event_rx.try_recv() {
            if let ClientNotification::Error { code: 40900, .. } = notification {
                saw_busy = true;
                break;
            }
        }
        assert!(saw_busy, "second prompt should be rejected while turn runs");

        handler.handle(ClientCommand::Abort).await.unwrap();
    }

    #[tokio::test]
    async fn abort_stops_active_turn_and_records_completion() {
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(64);
        let mut handler =
            CommandHandler::new(test_runtime_with_llm(Arc::new(PendingLlm)), event_tx);

        handler
            .handle(ClientCommand::CreateSession {
                working_dir: ".".into(),
            })
            .await
            .unwrap();
        handler
            .handle(ClientCommand::SubmitPrompt {
                text: "keep running".into(),
                attachments: vec![],
            })
            .await
            .unwrap();
        assert!(handler.active_turn.is_some());

        handler.handle(ClientCommand::Abort).await.unwrap();

        assert!(handler.active_turn.is_none());
        assert_eq!(wait_for_turn_completed(&mut event_rx).await, "aborted");
    }
}
