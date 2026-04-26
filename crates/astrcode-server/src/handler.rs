//! Command handler — processes ClientCommand using ServerRuntime.
//!
//! Transport-agnostic: used by both stdio binary and in-process CLI.

use std::sync::Arc;

use astrcode_core::types::SessionId;
use astrcode_protocol::commands::ClientCommand;
use astrcode_protocol::events::ServerEvent;
use tokio::sync::{broadcast, mpsc};

use crate::agent::Agent;
use crate::bootstrap::ServerRuntime;

/// Handles commands and emits events to a broadcast channel.
pub struct CommandHandler {
    runtime: Arc<ServerRuntime>,
    event_tx: broadcast::Sender<ServerEvent>,
    active_session_id: Option<SessionId>,
}

impl CommandHandler {
    pub fn new(runtime: Arc<ServerRuntime>, event_tx: broadcast::Sender<ServerEvent>) -> Self {
        Self {
            runtime,
            event_tx,
            active_session_id: None,
        }
    }

    pub async fn handle(&mut self, cmd: ClientCommand) -> Result<(), String> {
        match cmd {
            ClientCommand::CreateSession { working_dir } => {
                let model_id = self.runtime.effective.llm.model_id.clone();
                match self.runtime.session_manager.create(&working_dir, &model_id, 2048).await {
                    Ok(sid) => {
                        self.active_session_id = Some(sid.clone());
                        let _ = self.event_tx.send(ServerEvent::SessionCreated { session_id: sid, working_dir });
                    }
                    Err(e) => {
                        let _ = self.event_tx.send(ServerEvent::Error { code: -32603, message: e.to_string() });
                    }
                }
            }

            ClientCommand::SubmitPrompt { text, .. } => {
                let sid = self.ensure_session().await?;
                let session = self.runtime.session_manager.get(&sid).await
                    .ok_or_else(|| format!("Session {sid} vanished"))?;
                let history = { session.state.read().await.messages.clone() };
                let working_dir = { session.state.read().await.working_dir.clone() };

                // Record user message
                let _ = self.runtime.session_manager.append_event(&sid,
                    astrcode_core::storage::SessionEvent::UserMessage {
                        event_id: astrcode_core::types::new_event_id(),
                        turn_id: astrcode_core::types::new_turn_id(),
                        timestamp: chrono::Utc::now(),
                        text: text.clone(),
                    }).await;

                let agent = Agent::new(sid.clone(), working_dir,
                    self.runtime.llm_provider.clone(), self.runtime.prompt_provider.clone(),
                    self.runtime.capability.clone(), self.runtime.effective.llm.model_id.clone());

                let (event_tx, mut event_rx) = mpsc::unbounded_channel();
                let session_mgr = self.runtime.session_manager.clone();
                let text_for_agent = text.clone();
                let session_id = sid.clone();

                tokio::spawn(async move {
                    let result = agent.process_prompt(&text_for_agent, history, Some(event_tx)).await;

                    // Record assistant message + turn end
                    if let Ok(ref out) = result {
                        if !out.text.is_empty() {
                            let _ = session_mgr.append_event(&session_id,
                                astrcode_core::storage::SessionEvent::AssistantMessage {
                                    event_id: astrcode_core::types::new_event_id(),
                                    turn_id: out.turn_id.clone(),
                                    message_id: astrcode_core::types::new_message_id(),
                                    timestamp: chrono::Utc::now(),
                                    text: out.text.clone(),
                                }).await;
                        }
                        let _ = session_mgr.append_event(&session_id,
                            astrcode_core::storage::SessionEvent::TurnEnd {
                                turn_id: out.turn_id.clone(),
                                timestamp: chrono::Utc::now(),
                                finish_reason: out.finish_reason.clone(),
                            }).await;
                    }
                });

                while let Some(event) = event_rx.recv().await {
                    let _ = self.event_tx.send(event);
                }
            }

            ClientCommand::ListSessions => {
                let sessions = self.runtime.session_manager.list().await.unwrap_or_default();
                let items: Vec<_> = sessions.into_iter().map(|sid|
                    astrcode_protocol::events::SessionListItem {
                        session_id: sid, created_at: String::new(), last_active_at: String::new(),
                        working_dir: String::new(), parent_session_id: None,
                    }
                ).collect();
                let _ = self.event_tx.send(ServerEvent::SessionList { sessions: items });
            }

            ClientCommand::Abort => {
                let _ = self.event_tx.send(ServerEvent::AgentEnded {
                    reason: "aborted".into(),
                });
            }

            ClientCommand::ResumeSession { session_id } => {
                if self
                    .runtime
                    .session_manager
                    .get(&session_id)
                    .await
                    .is_some()
                {
                    self.active_session_id = Some(session_id.clone());
                    let _ = self.event_tx.send(ServerEvent::SessionResumed {
                        session_id,
                        snapshot: astrcode_protocol::events::SessionSnapshot {
                            session_id: String::new(),
                            cursor: String::new(),
                            messages: vec![],
                            model_id: String::new(),
                            working_dir: String::new(),
                        },
                    });
                } else {
                    let _ = self.event_tx.send(ServerEvent::Error {
                        code: 40401,
                        message: format!("Session not found: {session_id}"),
                    });
                }
            }

            _ => {
                let _ = self.event_tx.send(ServerEvent::Error {
                    code: -32601,
                    message: "Not implemented".into(),
                });
            }
        }
        Ok(())
    }

    async fn ensure_session(&mut self) -> Result<SessionId, String> {
        if let Some(ref sid) = self.active_session_id { return Ok(sid.clone()); }
        let model_id = self.runtime.effective.llm.model_id.clone();
        let wd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| ".".into());
        let sid = self.runtime.session_manager.create(&wd, &model_id, 2048).await
            .map_err(|e| format!("create session: {e}"))?;
        self.active_session_id = Some(sid.clone());
        let _ = self.event_tx.send(ServerEvent::SessionCreated { session_id: sid.clone(), working_dir: wd });
        Ok(sid)
    }
}
