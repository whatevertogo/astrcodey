//! Agent — the ephemeral turn processor created from session events.

use std::sync::Arc;

use crate::capability::CapabilityRouter;
use astrcode_core::llm::{LlmEvent, LlmMessage, LlmProvider, LlmRole};
use astrcode_core::prompt::{PromptContext, PromptProvider};
use astrcode_core::tool::ToolResult;
use astrcode_core::types::*;
use astrcode_protocol::events::ServerEvent;
use tokio::sync::mpsc;


/// Agent — a transient turn processor.
///
/// Created from a session's event log, processes one turn,
/// appends new events to the session, and is discarded.
pub struct Agent {
    session_id: SessionId,
    working_dir: String,
    llm: Arc<dyn LlmProvider>,
    prompt: Arc<dyn PromptProvider>,
    capability: Arc<CapabilityRouter>,
    model_id: String,
}

impl Agent {
    pub fn new(
        session_id: SessionId,
        working_dir: String,
        llm: Arc<dyn LlmProvider>,
        prompt: Arc<dyn PromptProvider>,
        capability: Arc<CapabilityRouter>,
        model_id: String,
    ) -> Self {
        Self {
            session_id,
            working_dir,
            llm,
            prompt,
            capability,
            model_id,
        }
    }

    /// Process a user prompt through the full agent loop.
    ///
    /// When `event_tx` is Some, real-time ServerEvents are streamed.
    /// When None, only the final AgentTurnOutput is returned (useful for tests).
    pub async fn process_prompt(
        &self,
        user_text: &str,
        history: Vec<LlmMessage>,
        event_tx: Option<mpsc::UnboundedSender<ServerEvent>>,
    ) -> Result<AgentTurnOutput, AgentError> {
        let turn_id = new_turn_id();
        let _ = event_tx.as_ref().map(|tx| {
            tx.send(ServerEvent::TurnStarted {
                turn_id: turn_id.clone(),
            })
        });

        let mut messages = history;
        messages.push(LlmMessage::user(user_text));

        let tools = self.capability.list_definitions().await;

        // Build prompt context
        let prompt_ctx = PromptContext {
            working_dir: self.working_dir.clone(),
            os: std::env::consts::OS.into(),
            shell: "bash".into(),
            date: chrono::Utc::now().format("%Y-%m-%d").to_string(),
            available_tools: tools
                .iter()
                .map(|t| t.name.clone())
                .collect::<Vec<_>>()
                .join(", "),
            custom: Default::default(),
        };

        let plan = self.prompt.assemble(prompt_ctx).await;
        if !plan.system_blocks.is_empty() {
            let system_text: String = plan
                .system_blocks
                .iter()
                .map(|b| b.content.clone())
                .collect::<Vec<_>>()
                .join("\n\n");
            messages.insert(0, LlmMessage::system(system_text));
        }

        let mut final_text = String::new();
        let mut final_reason = String::new();
        let mut all_tool_results: Vec<ToolResult> = Vec::new();

        loop {
            let mut rx = self.llm.generate(messages.clone(), tools.clone()).await?;
            let message_id = new_message_id();
            let mut message_started = false;
            let mut current_text = String::new();
            let mut tool_calls: Vec<PendingToolCall> = Vec::new();

            while let Some(event) = rx.recv().await {
                match event {
                    LlmEvent::ContentDelta { delta } => {
                        if let Some(ref tx) = event_tx {
                            if !message_started {
                                let _ = tx.send(ServerEvent::MessageStart {
                                    message_id: message_id.clone(),
                                    role: "assistant".into(),
                                });
                                message_started = true;
                            }
                            let _ = tx.send(ServerEvent::MessageDelta {
                                message_id: message_id.clone(),
                                delta: delta.clone(),
                            });
                        }
                        current_text.push_str(&delta);
                    }
                    LlmEvent::ToolCallStart {
                        call_id,
                        name,
                        arguments,
                    } => {
                        let _ = event_tx.as_ref().map(|tx| {
                            tx.send(ServerEvent::ToolCallStart {
                                call_id: call_id.clone(),
                                tool_name: name.clone(),
                                arguments: serde_json::json!({"raw": arguments}),
                            })
                        });
                        tool_calls.push(PendingToolCall {
                            call_id,
                            name,
                            arguments,
                        });
                    }
                    LlmEvent::ToolCallDelta { call_id, delta } => {
                        if let Some(tc) = tool_calls.iter_mut().find(|t| t.call_id == call_id) {
                            tc.arguments.push_str(&delta);
                        }
                    }
                    LlmEvent::Done { finish_reason } => {
                        // End the current message if started
                        if let Some(ref tx) = event_tx {
                            if message_started {
                                let _ = tx.send(ServerEvent::MessageEnd {
                                    message_id: message_id.clone(),
                                });
                            }
                        }
                        if tool_calls.is_empty() {
                            final_text = current_text;
                            final_reason = finish_reason;
                            if !final_text.is_empty() {
                                messages.push(LlmMessage::assistant(&final_text));
                            }
                            let _ = event_tx.as_ref().map(|tx| {
                                tx.send(ServerEvent::TurnEnded {
                                    turn_id: turn_id.clone(),
                                    finish_reason: final_reason.clone(),
                                })
                            });
                            return Ok(AgentTurnOutput {
                                turn_id,
                                text: final_text,
                                finish_reason: final_reason,
                                tool_results: all_tool_results,
                            });
                        }
                        break; // Process tool calls below
                    }
                    LlmEvent::Error { message } => {
                        let _ = event_tx.as_ref().map(|tx| {
                            let _ = tx.send(ServerEvent::Error {
                                code: -32603,
                                message: message.clone(),
                            });
                            let _ = tx.send(ServerEvent::TurnEnded {
                                turn_id: turn_id.clone(),
                                finish_reason: "error".into(),
                            });
                        });
                        return Err(AgentError::Llm(message));
                    }
                }
            }

            // Execute tool calls
            for tc in &tool_calls {
                let args: serde_json::Value =
                    serde_json::from_str(&tc.arguments).unwrap_or(serde_json::Value::Null);
                match self.capability.execute(&tc.name, args).await {
                    Ok(result) => {
                        let _ = event_tx.as_ref().map(|tx| {
                            tx.send(ServerEvent::ToolCallEnd {
                                call_id: tc.call_id.clone(),
                                result: astrcode_protocol::events::ToolCallResultDto {
                                    call_id: tc.call_id.clone(),
                                    content: result.content.clone(),
                                    is_error: result.is_error,
                                },
                            })
                        });
                        messages.push(LlmMessage {
                            role: LlmRole::Tool,
                            content: vec![astrcode_core::llm::LlmContent::ToolResult {
                                tool_call_id: tc.call_id.clone(),
                                content: result.content.clone(),
                                is_error: result.is_error,
                            }],
                            name: Some(tc.name.clone()),
                        });
                        all_tool_results.push(result);
                    }
                    Err(e) => {
                        let err_msg = format!("Error: {}", e);
                        let _ = event_tx.as_ref().map(|tx| {
                            tx.send(ServerEvent::ToolCallEnd {
                                call_id: tc.call_id.clone(),
                                result: astrcode_protocol::events::ToolCallResultDto {
                                    call_id: tc.call_id.clone(),
                                    content: err_msg.clone(),
                                    is_error: true,
                                },
                            })
                        });
                        let err_result = ToolResult {
                            call_id: tc.call_id.clone(),
                            content: err_msg.clone(),
                            is_error: true,
                            metadata: Default::default(),
                        };
                        messages.push(LlmMessage {
                            role: LlmRole::Tool,
                            content: vec![astrcode_core::llm::LlmContent::ToolResult {
                                tool_call_id: tc.call_id.clone(),
                                content: err_msg,
                                is_error: true,
                            }],
                            name: Some(tc.name.clone()),
                        });
                        all_tool_results.push(err_result);
                    }
                }
            }
        }
    }
}

/// Output from an agent turn.
pub struct AgentTurnOutput {
    pub turn_id: TurnId,
    pub text: String,
    pub finish_reason: String,
    pub tool_results: Vec<ToolResult>,
}

struct PendingToolCall {
    call_id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("LLM error: {0}")]
    Llm(String),
    #[error("Tool error: {0}")]
    Tool(#[from] astrcode_core::tool::ToolError),
}

impl From<astrcode_core::llm::LlmError> for AgentError {
    fn from(e: astrcode_core::llm::LlmError) -> Self {
        AgentError::Llm(e.to_string())
    }
}
