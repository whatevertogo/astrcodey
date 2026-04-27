//! Agent — the ephemeral turn processor created from session events.

use std::{sync::Arc, time::Instant};

use astrcode_core::{
    event::EventPayload,
    llm::{LlmContent, LlmEvent, LlmMessage, LlmProvider, LlmRole},
    prompt::{PromptContext, PromptProvider},
    tool::ToolResult,
    types::*,
};
use tokio::sync::mpsc;

use crate::capability::CapabilityRouter;

/// Agent — a transient turn processor.
///
/// Created from a session projection, processes one turn, emits event payloads,
/// and is discarded. Session identity and persistence stay in the handler.
pub struct Agent {
    _session_id: SessionId,
    working_dir: String,
    llm: Arc<dyn LlmProvider>,
    prompt: Arc<dyn PromptProvider>,
    capability: Arc<CapabilityRouter>,
    _model_id: String,
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
            _session_id: session_id,
            working_dir,
            llm,
            prompt,
            capability,
            _model_id: model_id,
        }
    }

    /// Process a user prompt through the full agent loop.
    ///
    /// When `event_tx` is Some, real-time event payloads are streamed. The
    /// handler wraps them with session/turn metadata and decides durability.
    pub async fn process_prompt(
        &self,
        user_text: &str,
        history: Vec<LlmMessage>,
        event_tx: Option<mpsc::UnboundedSender<EventPayload>>,
    ) -> Result<AgentTurnOutput, AgentError> {
        let mut messages = history;
        messages.push(LlmMessage::user(user_text));

        let tools = self.capability.list_definitions().await;

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
                        if let Some(tx) = &event_tx {
                            if !message_started {
                                let _ = tx.send(EventPayload::AssistantMessageStarted {
                                    message_id: message_id.clone(),
                                });
                                message_started = true;
                            }
                            let _ = tx.send(EventPayload::AssistantTextDelta {
                                message_id: message_id.clone(),
                                delta: delta.clone(),
                            });
                        }
                        current_text.push_str(&delta);
                    },
                    LlmEvent::ToolCallStart {
                        call_id,
                        name,
                        arguments,
                    } => {
                        if let Some(tx) = &event_tx {
                            let _ = tx.send(EventPayload::ToolCallStarted {
                                call_id: call_id.clone(),
                                tool_name: name.clone(),
                            });
                            if !arguments.is_empty() {
                                let _ = tx.send(EventPayload::ToolCallArgumentsDelta {
                                    call_id: call_id.clone(),
                                    delta: arguments.clone(),
                                });
                            }
                        }
                        tool_calls.push(PendingToolCall {
                            call_id,
                            name,
                            arguments,
                        });
                    },
                    LlmEvent::ToolCallDelta { call_id, delta } => {
                        if let Some(tc) = tool_calls.iter_mut().find(|t| t.call_id == call_id) {
                            tc.arguments.push_str(&delta);
                        }
                        if let Some(tx) = &event_tx {
                            let _ =
                                tx.send(EventPayload::ToolCallArgumentsDelta { call_id, delta });
                        }
                    },
                    LlmEvent::Done { finish_reason } => {
                        if !current_text.is_empty() {
                            if let Some(tx) = &event_tx {
                                if message_started {
                                    let _ = tx.send(EventPayload::AssistantMessageCompleted {
                                        message_id: message_id.clone(),
                                        text: current_text.clone(),
                                    });
                                }
                            }
                            messages.push(LlmMessage::assistant(&current_text));
                            final_text.push_str(&current_text);
                        }

                        if tool_calls.is_empty() {
                            return Ok(AgentTurnOutput {
                                text: final_text,
                                finish_reason,
                                tool_results: all_tool_results,
                            });
                        }
                        break;
                    },
                    LlmEvent::Error { message } => {
                        if let Some(tx) = &event_tx {
                            let _ = tx.send(EventPayload::ErrorOccurred {
                                code: -32603,
                                message: message.clone(),
                                recoverable: false,
                            });
                        }
                        return Err(AgentError::Llm(message));
                    },
                }
            }

            for tc in &tool_calls {
                let args: serde_json::Value =
                    serde_json::from_str(&tc.arguments).unwrap_or(serde_json::Value::Null);

                if let Some(tx) = &event_tx {
                    let _ = tx.send(EventPayload::ToolCallRequested {
                        call_id: tc.call_id.clone(),
                        tool_name: tc.name.clone(),
                        arguments: args.clone(),
                    });
                }

                messages.push(LlmMessage {
                    role: LlmRole::Assistant,
                    content: vec![LlmContent::ToolCall {
                        call_id: tc.call_id.clone(),
                        name: tc.name.clone(),
                        arguments: args.clone(),
                    }],
                    name: None,
                });

                let started_at = Instant::now();
                match self.capability.execute(&tc.name, args).await {
                    Ok(mut result) => {
                        result.call_id = tc.call_id.clone();
                        result.duration_ms = Some(started_at.elapsed().as_millis() as u64);
                        if result.is_error && result.error.is_none() {
                            result.error = Some(result.content.clone());
                        }

                        if let Some(tx) = &event_tx {
                            let _ = tx.send(EventPayload::ToolCallCompleted {
                                call_id: tc.call_id.clone(),
                                tool_name: tc.name.clone(),
                                result: result.clone(),
                            });
                        }
                        messages.push(LlmMessage {
                            role: LlmRole::Tool,
                            content: vec![LlmContent::ToolResult {
                                tool_call_id: tc.call_id.clone(),
                                content: result.content.clone(),
                                is_error: result.is_error,
                            }],
                            name: Some(tc.name.clone()),
                        });
                        all_tool_results.push(result);
                    },
                    Err(e) => {
                        let err_msg = format!("Error: {}", e);
                        let err_result = ToolResult {
                            call_id: tc.call_id.clone(),
                            content: err_msg.clone(),
                            is_error: true,
                            error: Some(err_msg.clone()),
                            metadata: Default::default(),
                            duration_ms: Some(started_at.elapsed().as_millis() as u64),
                        };

                        if let Some(tx) = &event_tx {
                            let _ = tx.send(EventPayload::ToolCallCompleted {
                                call_id: tc.call_id.clone(),
                                tool_name: tc.name.clone(),
                                result: err_result.clone(),
                            });
                        }
                        messages.push(LlmMessage {
                            role: LlmRole::Tool,
                            content: vec![LlmContent::ToolResult {
                                tool_call_id: tc.call_id.clone(),
                                content: err_msg,
                                is_error: true,
                            }],
                            name: Some(tc.name.clone()),
                        });
                        all_tool_results.push(err_result);
                    },
                }
            }
        }
    }
}

/// Output from an agent turn.
pub struct AgentTurnOutput {
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
