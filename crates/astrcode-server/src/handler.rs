//! Command handler — processes ClientCommand using ServerRuntime.
//!
//! Transport-agnostic: used by both stdio binary and in-process CLI.

use std::sync::Arc;

use astrcode_core::{
    event::{Event, EventPayload},
    llm::{LlmContent, LlmMessage},
    types::{SessionId, TurnId, new_message_id, new_turn_id},
};
use astrcode_protocol::{
    commands::ClientCommand,
    events::{ClientNotification, MessageDto, SessionListItem, SessionSnapshot},
};
use tokio::sync::{broadcast, mpsc};

use crate::{agent::Agent, bootstrap::ServerRuntime};

/// Handles commands and emits notifications to a broadcast channel.
pub struct CommandHandler {
    runtime: Arc<ServerRuntime>,
    event_tx: broadcast::Sender<ClientNotification>,
    active_session_id: Option<SessionId>,
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
        }
    }

    pub async fn handle(&mut self, cmd: ClientCommand) -> Result<(), String> {
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
                if let Some(sid) = self.active_session_id.clone() {
                    let _ = self
                        .record_and_broadcast(
                            &sid,
                            None,
                            EventPayload::AgentRunCompleted {
                                reason: "aborted".into(),
                            },
                        )
                        .await;
                } else {
                    self.send_error(40400, "No active session");
                }
            },

            ClientCommand::ResumeSession { session_id }
            | ClientCommand::SwitchSession { session_id } => {
                self.resume_session(session_id).await;
            },

            ClientCommand::DeleteSession { session_id } => {
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
                let _ = self.event_tx.send(ClientNotification::Event(event));
            },
            Err(e) => self.send_error(-32603, &e.to_string()),
        }
    }

    async fn submit_prompt(&mut self, text: String) -> Result<(), String> {
        let sid = self.ensure_session().await?;
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

        let agent = Agent::new(
            sid.clone(),
            working_dir,
            self.runtime.llm_provider.clone(),
            self.runtime.prompt_provider.clone(),
            self.runtime.capability.clone(),
            self.runtime.effective.llm.model_id.clone(),
        );

        let (agent_event_tx, mut agent_event_rx) = mpsc::unbounded_channel();
        let text_for_agent = text.clone();
        let agent_task = tokio::spawn(async move {
            agent
                .process_prompt(&text_for_agent, history, Some(agent_event_tx))
                .await
        });

        let mut emitted_error = false;
        while let Some(payload) = agent_event_rx.recv().await {
            if matches!(payload, EventPayload::ErrorOccurred { .. }) {
                emitted_error = true;
            }
            self.record_and_broadcast(&sid, Some(&turn_id), payload)
                .await?;
        }

        match agent_task
            .await
            .map_err(|e| format!("agent task failed: {e}"))?
        {
            Ok(output) => {
                self.record_and_broadcast(
                    &sid,
                    Some(&turn_id),
                    EventPayload::TurnCompleted {
                        finish_reason: output.finish_reason.clone(),
                    },
                )
                .await?;
                self.record_and_broadcast(
                    &sid,
                    Some(&turn_id),
                    EventPayload::AgentRunCompleted {
                        reason: output.finish_reason,
                    },
                )
                .await?;
            },
            Err(e) => {
                if !emitted_error {
                    self.record_and_broadcast(
                        &sid,
                        Some(&turn_id),
                        EventPayload::ErrorOccurred {
                            code: -32603,
                            message: e.to_string(),
                            recoverable: false,
                        },
                    )
                    .await?;
                }
                self.record_and_broadcast(
                    &sid,
                    Some(&turn_id),
                    EventPayload::TurnCompleted {
                        finish_reason: "error".into(),
                    },
                )
                .await?;
                self.record_and_broadcast(
                    &sid,
                    Some(&turn_id),
                    EventPayload::AgentRunCompleted {
                        reason: "error".into(),
                    },
                )
                .await?;
            },
        }

        Ok(())
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
        let event = Event::new(session_id.clone(), turn_id.cloned(), payload);
        let event = if event.payload.is_durable() {
            self.runtime
                .session_manager
                .append_event(event)
                .await
                .map_err(|e| e.to_string())?
        } else {
            event
        };

        let _ = self.event_tx.send(ClientNotification::Event(event.clone()));
        Ok(event)
    }

    fn send_error(&self, code: i32, message: &str) {
        let _ = self.event_tx.send(ClientNotification::Error {
            code,
            message: message.into(),
        });
    }
}

fn message_to_dto(message: &LlmMessage) -> MessageDto {
    MessageDto {
        role: format!("{:?}", message.role).to_lowercase(),
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
    use std::{sync::Arc, time::Duration};

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

    fn test_runtime() -> Arc<ServerRuntime> {
        Arc::new(ServerRuntime {
            session_manager: Arc::new(SessionManager::new(Arc::new(NoopEventStore::new()))),
            llm_provider: Arc::new(MockLlm),
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
        })
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

        let mut turn_ids = Vec::new();
        while let Ok(notification) = event_rx.try_recv() {
            let ClientNotification::Event(event) = notification else {
                continue;
            };
            match event.payload {
                EventPayload::TurnStarted
                | EventPayload::UserMessage { .. }
                | EventPayload::AssistantMessageCompleted { .. }
                | EventPayload::TurnCompleted { .. } => {
                    turn_ids.push(event.turn_id);
                },
                _ => {},
            }
        }

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
}
