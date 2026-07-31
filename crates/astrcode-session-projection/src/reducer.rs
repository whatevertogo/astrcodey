//! 会话事件投影。
//!
//! EventLog 是唯一事实源；本模块只维护可从事件重建的内部读模型。

use astrcode_core::{
    event::{DurableEvent, DurableEventPayload, Phase, StoredEvent, TranscriptRewriteReason},
    llm::{LlmContent, LlmMessage, LlmRole, TURN_ABORTED_SOURCE, turn_aborted_context_message},
    types::SessionId,
};
use thiserror::Error;

use crate::{
    AgentSessionLinkView, AgentSessionStatus, CompactionView, ForkSourceRef,
    PendingToolApprovalView, SequencedLlmMessage, SessionExecutionState, SessionReadModel,
    SessionSummary, TOOL_CALL_CANCELLED_SOURCE, TOOL_CALL_FAILED_SOURCE, TranscriptArtifactView,
};

#[derive(Clone)]
pub struct SessionReadModelProjection {
    session_id: SessionId,
    model: Option<SessionReadModel>,
}

#[derive(Clone)]
pub struct SessionSummaryProjection {
    session_id: SessionId,
    summary: Option<SessionSummary>,
    execution: SessionExecutionState,
    last_seq: u64,
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ProjectionError {
    #[error("session {0} has no SessionStarted event")]
    MissingSessionStarted(SessionId),
    #[error("event belongs to session {actual}, expected {expected}")]
    SessionMismatch {
        expected: SessionId,
        actual: SessionId,
    },
    #[error("first event must have seq 0, got {0}")]
    InvalidFirstSequence(u64),
    #[error("first event must be SessionStarted")]
    InvalidFirstEvent,
    #[error("SessionStarted must be a session-level event")]
    SessionStartedHasTurn,
    #[error("duplicate SessionStarted at seq {0}")]
    DuplicateSessionStarted(u64),
    #[error("expected event seq {expected}, got {actual}")]
    NonContiguousSequence { expected: u64, actual: u64 },
    #[error("transcript rewrite source seq {source_seq} exceeds current seq {current_seq}")]
    InvalidTranscriptRewriteSource { source_seq: u64, current_seq: u64 },
}

impl SessionReadModelProjection {
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            model: None,
        }
    }

    pub fn apply(&mut self, event: &StoredEvent) -> Result<(), ProjectionError> {
        if event.session_id != self.session_id {
            return Err(ProjectionError::SessionMismatch {
                expected: self.session_id.clone(),
                actual: event.session_id.clone(),
            });
        }

        match self.model.as_mut() {
            Some(model) => reduce(event, model),
            None => {
                validate_first_event(event)?;
                let DurableEventPayload::SessionStarted(started) = &event.payload else {
                    return Err(ProjectionError::InvalidFirstEvent);
                };
                self.model = Some(SessionReadModel::from_started(
                    self.session_id.clone(),
                    started,
                    event.timestamp,
                ));
                Ok(())
            },
        }
    }

    pub fn snapshot(&self) -> Result<SessionReadModel, ProjectionError> {
        self.model
            .clone()
            .ok_or_else(|| ProjectionError::MissingSessionStarted(self.session_id.clone()))
    }

    pub fn last_seq(&self) -> Option<u64> {
        self.model.as_ref().map(|model| model.stats.last_seq)
    }
}

impl SessionSummaryProjection {
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            summary: None,
            execution: SessionExecutionState::default(),
            last_seq: 0,
        }
    }

    pub fn apply(&mut self, event: &StoredEvent) -> Result<(), ProjectionError> {
        if event.session_id != self.session_id {
            return Err(ProjectionError::SessionMismatch {
                expected: self.session_id.clone(),
                actual: event.session_id.clone(),
            });
        }

        let Some(summary) = self.summary.as_mut() else {
            validate_first_event(event)?;
            let DurableEventPayload::SessionStarted(started) = &event.payload else {
                return Err(ProjectionError::InvalidFirstEvent);
            };
            let model =
                SessionReadModel::from_started(self.session_id.clone(), started, event.timestamp);
            self.summary = Some(model.to_summary());
            self.last_seq = event.seq;
            return Ok(());
        };

        validate_next_event_details(event.seq, &event.event, &self.session_id, self.last_seq)?;

        apply_execution_event(event, &mut self.execution);
        match &event.payload {
            DurableEventPayload::ModelIdChanged { model_id } => {
                summary.model_id = model_id.clone();
            },
            DurableEventPayload::UserMessage { text, .. } => {
                if summary.first_user_message.is_none() {
                    summary.first_user_message = Some(text.clone());
                }
            },
            DurableEventPayload::SessionForked {
                first_user_message, ..
            } => {
                summary.first_user_message = first_user_message.clone();
            },
            _ => {},
        }
        summary.updated_at = event.timestamp.to_rfc3339();
        summary.latest_cursor = event.seq.to_string();
        summary.phase = self.execution.phase;
        self.last_seq = event.seq;
        Ok(())
    }

    pub fn snapshot(&self) -> Result<SessionSummary, ProjectionError> {
        self.summary
            .clone()
            .ok_or_else(|| ProjectionError::MissingSessionStarted(self.session_id.clone()))
    }
}

/// 从事件序列重建会话读模型。
pub fn replay(
    session_id: SessionId,
    events: &[StoredEvent],
) -> Result<SessionReadModel, ProjectionError> {
    let mut projection = SessionReadModelProjection::new(session_id);
    for event in events {
        projection.apply(event)?;
    }
    projection.snapshot()
}

/// 将单个持久事件归约到读模型。
pub fn reduce(event: &StoredEvent, model: &mut SessionReadModel) -> Result<(), ProjectionError> {
    validate_next_event(event.seq, &event.event, model)?;

    model.stats.last_seq = event.seq;
    model.stats.updated_at = event.timestamp;
    model.stats.event_count += 1;
    let event_seq = event.seq;
    apply_execution_event(event, &mut model.execution);

    match &event.payload {
        DurableEventPayload::SessionStarted(_) => {
            return Err(ProjectionError::DuplicateSessionStarted(event.seq));
        },
        DurableEventPayload::ModelIdChanged { model_id } => {
            model.identity.model_id = model_id.clone();
            model.context_usage = None;
        },
        DurableEventPayload::SessionToolsConfigured { selection } => {
            model.identity.tool_selection = selection.clone();
            model.context_usage = None;
        },
        DurableEventPayload::AgentSessionSpawned {
            child_session_id,
            agent_name,
            task,
            tool_selection: _,
            tool_call_id,
        } => {
            model.agent_sessions.push(AgentSessionLinkView {
                child_session_id: child_session_id.clone(),
                tool_call_id: tool_call_id.clone(),
                agent_name: agent_name.clone(),
                task: task.clone(),
                status: AgentSessionStatus::Running,
                final_session_id: None,
                summary: None,
                error: None,
            });
        },
        DurableEventPayload::AgentSessionCompleted {
            child_session_id,
            final_session_id,
            summary,
        } => {
            if let Some(link) = model
                .agent_sessions
                .iter_mut()
                .find(|l| l.child_session_id == *child_session_id)
            {
                link.status = AgentSessionStatus::Completed;
                link.final_session_id = Some(final_session_id.clone());
                link.summary = Some(summary.clone());
                link.error = None;
            }
        },
        DurableEventPayload::AgentSessionFailed {
            child_session_id,
            final_session_id,
            error,
        } => {
            if let Some(link) = model
                .agent_sessions
                .iter_mut()
                .find(|l| l.child_session_id == *child_session_id)
            {
                link.status = AgentSessionStatus::Failed;
                link.final_session_id = Some(final_session_id.clone());
                link.error = Some(error.clone());
                link.summary = None;
            }
        },
        DurableEventPayload::AgentSessionRecycled { child_session_id } => {
            model
                .agent_sessions
                .retain(|l| l.child_session_id != *child_session_id);
        },
        DurableEventPayload::SystemPromptConfigured {
            text,
            fingerprint,
            extra_system_prompt,
            source,
        } => {
            model.system_prompt.text = text.clone();
            model.system_prompt.extra = extra_system_prompt.clone();
            model.system_prompt.fingerprint = fingerprint.clone();
            model.system_prompt.source = *source;
            model.context_usage = None;
        },
        DurableEventPayload::UserInputAccepted { input } => {
            model.execution.pending_inputs.push(crate::PendingInput {
                accepted_seq: event_seq,
                input: input.clone(),
            });
        },
        DurableEventPayload::TurnStarted | DurableEventPayload::UserMessage { .. } => {
            if let DurableEventPayload::UserMessage {
                text,
                attachments,
                accepted_seq,
                ..
            } = &event.payload
            {
                if let Some(accepted_seq) = accepted_seq {
                    model
                        .execution
                        .pending_inputs
                        .retain(|input| input.accepted_seq != *accepted_seq);
                }
                if model.transcript.first_user_message.is_none() {
                    model.transcript.first_user_message = Some(text.clone());
                }
                model.transcript.messages.push(SequencedLlmMessage::plain(
                    LlmMessage::user_with_attachments(text, attachments),
                    event_seq,
                ));
            }
        },
        DurableEventPayload::TurnCompleted { .. } => {},
        DurableEventPayload::TurnAbortedContext => {
            model.transcript.messages.push(SequencedLlmMessage {
                message: turn_aborted_context_message(),
                updated_seq: event_seq,
                source: Some(TURN_ABORTED_SOURCE.into()),
            });
        },
        DurableEventPayload::AssistantMessageCompleted {
            text,
            reasoning_content,
            ..
        } => {
            let mut msg = LlmMessage::assistant(text);
            msg.reasoning_content = reasoning_content.clone();
            model
                .transcript
                .messages
                .push(SequencedLlmMessage::plain(msg, event_seq));
        },
        DurableEventPayload::ToolCallRequested {
            call_id,
            tool_name,
            arguments,
            raw_arguments,
        } => {
            let tool_call = LlmContent::ToolCall {
                call_id: call_id.to_string(),
                name: tool_name.clone(),
                arguments: arguments.clone(),
                raw_arguments: raw_arguments.clone(),
            };
            // Merge into the previous assistant message for this model sub-turn.
            // DeepSeek thinking mode requires reasoning_content and tool_calls to
            // be replayed on the same assistant message after tool use.
            match model.transcript.messages.last_mut() {
                Some(last) if last.message.role == LlmRole::Assistant => {
                    last.message.content.push(tool_call);
                    last.updated_seq = event_seq;
                },
                _ => {
                    model.transcript.messages.push(SequencedLlmMessage::plain(
                        LlmMessage {
                            role: LlmRole::Assistant,
                            content: vec![tool_call],
                            name: None,
                            reasoning_content: None,
                        },
                        event_seq,
                    ));
                },
            }
        },
        DurableEventPayload::ToolApprovalRequested { .. }
        | DurableEventPayload::ToolApprovalResolved { .. } => {},
        DurableEventPayload::ToolCallCompleted {
            call_id,
            tool_name,
            result,
            ..
        } => {
            apply_tool_terminal(
                model,
                call_id,
                tool_name,
                result.content.clone(),
                result.is_error,
                None,
                event_seq,
            );
        },
        DurableEventPayload::ToolCallFailed {
            call_id,
            tool_name,
            error,
            ..
        } => {
            apply_tool_terminal(
                model,
                call_id,
                tool_name,
                error.clone(),
                true,
                Some(TOOL_CALL_FAILED_SOURCE),
                event_seq,
            );
        },
        DurableEventPayload::ToolCallCancelled {
            call_id,
            tool_name,
            reason,
            ..
        } => {
            apply_tool_terminal(
                model,
                call_id,
                tool_name,
                format!("Tool cancelled: {reason}"),
                true,
                Some(TOOL_CALL_CANCELLED_SOURCE),
                event_seq,
            );
        },
        DurableEventPayload::TranscriptRewritten {
            source_seq,
            messages,
            reason,
        } => {
            apply_transcript_rewrite(model, messages, *source_seq);
            model.context_usage = None;
            match reason {
                TranscriptRewriteReason::Compaction(details) => {
                    model.compactions.push(CompactionView {
                        trigger: details.trigger.clone(),
                        pre_tokens: details.pre_tokens,
                        post_tokens: details.post_tokens,
                        summary: details.summary.clone(),
                        transcript_path: details.transcript_path.clone(),
                        seq: event.seq,
                        source_seq: *source_seq,
                        strategy: details.strategy.clone(),
                    });
                },
            }
        },
        DurableEventPayload::SessionForked {
            source_session_id,
            source_cursor,
            first_user_message,
            messages,
        } => {
            model.identity.forked_from = Some(ForkSourceRef {
                session_id: source_session_id.clone(),
                cursor: source_cursor.clone(),
            });
            model.transcript.first_user_message = first_user_message.clone();
            model.transcript.messages = messages
                .iter()
                .cloned()
                .map(|message| SequencedLlmMessage::plain(message, event_seq))
                .collect();
            model.context_usage = None;
        },
        DurableEventPayload::ErrorOccurred { message, .. } => {
            model
                .transcript
                .artifacts
                .push(TranscriptArtifactView::Error {
                    id: event.id.to_string(),
                    message: message.clone(),
                    seq: event_seq,
                });
        },
        DurableEventPayload::RecapGenerated { text, .. } => {
            model
                .transcript
                .artifacts
                .push(TranscriptArtifactView::SystemNote {
                    id: event.id.to_string(),
                    text: text.clone(),
                    seq: event_seq,
                });
        },
        DurableEventPayload::TokenUsageRecorded {
            usage,
            model_context_window,
        } => {
            if let Some(context_tokens) = usage
                .context_tokens_after_response()
                .and_then(|tokens| usize::try_from(tokens).ok())
            {
                model.context_usage = Some(crate::ContextUsageView {
                    context_tokens,
                    model_context_window: *model_context_window,
                    covered_message_count: model.transcript.messages.len(),
                });
            }
        },
        DurableEventPayload::ExtensionEvent(_) => {},
    }
    Ok(())
}

fn apply_transcript_rewrite(
    model: &mut SessionReadModel,
    messages: &[LlmMessage],
    source_seq: u64,
) {
    let tail = model
        .transcript
        .messages
        .iter()
        .filter(|message| message.updated_seq > source_seq)
        .cloned();
    model.transcript.messages = messages
        .iter()
        .cloned()
        .map(|message| SequencedLlmMessage {
            message,
            // Rewrite output represents the frozen prefix, not a new tail fact.
            // Anchoring it here also lets a later concurrent rewrite replace it cleanly.
            updated_seq: source_seq,
            source: None,
        })
        .chain(tail)
        .collect();
    model
        .transcript
        .artifacts
        .retain(|artifact| artifact.seq() > source_seq);
}

/// 校验事件能否作为读模型的下一条事实，不修改读模型。
pub fn validate_next_event(
    seq: u64,
    event: &DurableEvent,
    model: &SessionReadModel,
) -> Result<(), ProjectionError> {
    validate_next_event_details(seq, event, &model.identity.session_id, model.stats.last_seq)
}

fn validate_next_event_details(
    seq: u64,
    event: &DurableEvent,
    expected_session_id: &SessionId,
    current_seq: u64,
) -> Result<(), ProjectionError> {
    if event.session_id != *expected_session_id {
        return Err(ProjectionError::SessionMismatch {
            expected: expected_session_id.clone(),
            actual: event.session_id.clone(),
        });
    }
    if matches!(event.payload, DurableEventPayload::SessionStarted(_)) {
        return Err(ProjectionError::DuplicateSessionStarted(seq));
    }
    if let DurableEventPayload::TranscriptRewritten { source_seq, .. } = &event.payload {
        if *source_seq > current_seq {
            return Err(ProjectionError::InvalidTranscriptRewriteSource {
                source_seq: *source_seq,
                current_seq,
            });
        }
    }
    let expected_seq = current_seq.saturating_add(1);
    if seq != expected_seq {
        return Err(ProjectionError::NonContiguousSequence {
            expected: expected_seq,
            actual: seq,
        });
    }
    Ok(())
}

fn validate_first_event(event: &StoredEvent) -> Result<(), ProjectionError> {
    if event.seq != 0 {
        return Err(ProjectionError::InvalidFirstSequence(event.seq));
    }
    if event.turn_id.is_some() {
        return Err(ProjectionError::SessionStartedHasTurn);
    }
    if !matches!(event.payload, DurableEventPayload::SessionStarted(_)) {
        return Err(ProjectionError::InvalidFirstEvent);
    }
    Ok(())
}

fn apply_execution_event(event: &StoredEvent, execution: &mut SessionExecutionState) {
    match &event.payload {
        DurableEventPayload::TurnStarted => {
            execution.phase = Phase::Thinking;
            execution.unsettled_turn_id = event.turn_id.clone();
        },
        DurableEventPayload::UserInputAccepted { .. } => {},
        DurableEventPayload::UserMessage { .. }
        | DurableEventPayload::AssistantMessageCompleted { .. } => {
            execution.phase = Phase::Thinking;
        },
        DurableEventPayload::ToolCallRequested { call_id, .. } => {
            execution.pending_tool_calls.insert(call_id.clone());
            execution.phase = Phase::CallingTool;
        },
        DurableEventPayload::ToolApprovalRequested {
            call_id,
            prompt,
            rule_key,
            ..
        } => {
            execution.pending_tool_approvals.insert(
                call_id.clone(),
                PendingToolApprovalView {
                    prompt: prompt.clone(),
                    rule_key: rule_key.clone(),
                },
            );
            execution.phase = Phase::CallingTool;
        },
        DurableEventPayload::ToolApprovalResolved { call_id, .. } => {
            execution.pending_tool_approvals.remove(call_id);
        },
        DurableEventPayload::ToolCallCompleted { call_id, .. }
        | DurableEventPayload::ToolCallFailed { call_id, .. }
        | DurableEventPayload::ToolCallCancelled { call_id, .. } => {
            execution.pending_tool_calls.remove(call_id);
            execution.pending_tool_approvals.remove(call_id);
            execution.phase = if execution.pending_tool_calls.is_empty() {
                Phase::Thinking
            } else {
                Phase::CallingTool
            };
        },
        DurableEventPayload::TurnCompleted { .. }
            if event.turn_id.is_none()
                || execution.unsettled_turn_id.is_none()
                || event.turn_id.as_ref() == execution.unsettled_turn_id.as_ref() =>
        {
            execution.phase = Phase::Idle;
            execution.unsettled_turn_id = None;
            execution.pending_tool_calls.clear();
            execution.pending_tool_approvals.clear();
        },
        DurableEventPayload::SessionForked { .. } => {
            execution.phase = Phase::Idle;
            execution.unsettled_turn_id = None;
            execution.pending_tool_calls.clear();
            execution.pending_tool_approvals.clear();
        },
        DurableEventPayload::ErrorOccurred { .. } => {
            execution.phase = Phase::Error;
        },
        _ => {},
    }
}

fn apply_tool_terminal(
    model: &mut SessionReadModel,
    call_id: &astrcode_core::types::ToolCallId,
    tool_name: &str,
    content: String,
    is_error: bool,
    source: Option<&str>,
    event_seq: u64,
) {
    model.transcript.messages.push(SequencedLlmMessage {
        message: LlmMessage {
            role: LlmRole::Tool,
            content: vec![LlmContent::ToolResult {
                tool_call_id: call_id.to_string(),
                content,
                is_error,
            }],
            name: Some(tool_name.to_owned()),
            reasoning_content: None,
        },
        updated_seq: event_seq,
        source: source.map(str::to_owned),
    });
}

#[cfg(test)]
mod tests;
