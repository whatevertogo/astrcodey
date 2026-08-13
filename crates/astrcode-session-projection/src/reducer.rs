//! Durable session event projection.
//!
//! EventLog is the source of truth. This module validates event ordering and fans each validated
//! event out to the independent read-model components.

use std::sync::Arc;

use astrcode_core::{
    event::{DurableEvent, DurableEventPayload, StoredEvent},
    types::SessionId,
};

use crate::{
    ForkSourceRef, ProjectionError, SessionExecutionState, SessionReadModel, SessionSummary,
    agents, execution, model_context, model_context::ModelContextValidationState, presentation,
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

/// A validated projection update that can be applied after its events are durably appended.
///
/// The batch retains only its events. Rewrite validation clones the narrow provider-input state,
/// never the complete read model.
pub struct PreparedProjectionBatch {
    first_seq: u64,
    events: Vec<StoredEvent>,
}

impl PreparedProjectionBatch {
    pub fn prepare(
        model: &SessionReadModel,
        events: Vec<DurableEvent>,
    ) -> Result<Self, ProjectionError> {
        if events.is_empty() {
            return Err(ProjectionError::EmptyBatch);
        }

        let first_seq = model
            .stats
            .last_seq
            .checked_add(1)
            .ok_or(ProjectionError::SequenceOverflow)?;
        let mut stored_events = Vec::with_capacity(events.len());
        for (index, event) in events.into_iter().enumerate() {
            let offset = u64::try_from(index).map_err(|_| ProjectionError::SequenceOverflow)?;
            let seq = first_seq
                .checked_add(offset)
                .ok_or(ProjectionError::SequenceOverflow)?;
            stored_events.push(StoredEvent::new(seq, event));
        }

        let mut model_context = stored_events
            .iter()
            .any(|event| {
                matches!(
                    event.payload,
                    DurableEventPayload::TranscriptRewritten { .. }
                )
            })
            .then(|| ModelContextValidationState::new(&model.system_prompt, &model.model_context));

        let mut current_seq = model.stats.last_seq;
        for event in &stored_events {
            validate_next_event_details(
                event.seq,
                &event.event,
                &model.identity.session_id,
                current_seq,
            )?;
            if let Some(model_context) = model_context.as_mut() {
                model_context.validate_and_apply(event)?;
            }
            current_seq = event.seq;
        }

        Ok(Self {
            first_seq,
            events: stored_events,
        })
    }

    pub const fn first_seq(&self) -> u64 {
        self.first_seq
    }

    pub fn events(&self) -> &[StoredEvent] {
        &self.events
    }

    pub fn apply(self, model: &mut Arc<SessionReadModel>) -> Vec<StoredEvent> {
        self.apply_to_model(Arc::make_mut(model))
    }

    pub fn apply_to_model(self, model: &mut SessionReadModel) -> Vec<StoredEvent> {
        for event in &self.events {
            apply_validated_event(event, model);
        }
        self.events
    }
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
        execution::apply_event(event, &mut self.execution);
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
            } => summary.first_user_message = first_user_message.clone(),
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

pub fn reduce(event: &StoredEvent, model: &mut SessionReadModel) -> Result<(), ProjectionError> {
    validate_next_event(event.seq, &event.event, model)?;
    apply_validated_event(event, model);
    Ok(())
}

fn apply_validated_event(event: &StoredEvent, model: &mut SessionReadModel) {
    model.stats.last_seq = event.seq;
    model.stats.updated_at = event.timestamp;
    model.stats.event_count += 1;

    model_context::apply_event(event, &mut model.system_prompt, &mut model.model_context);
    presentation::apply_event(event, &mut model.presentation);
    execution::apply_event(event, &mut model.execution);
    agents::apply_event(event, &mut model.agent_sessions);

    match &event.payload {
        DurableEventPayload::ModelIdChanged { model_id } => {
            model.identity.model_id = model_id.clone();
        },
        DurableEventPayload::SessionToolsConfigured { selection } => {
            model.identity.tool_selection = selection.clone();
        },
        DurableEventPayload::SessionForked {
            source_session_id,
            source_cursor,
            ..
        } => {
            model.identity.forked_from = Some(ForkSourceRef {
                session_id: source_session_id.clone(),
                cursor: source_cursor.clone(),
            });
        },
        _ => {},
    }
}

/// 校验事件能否作为读模型的下一条事实，不修改读模型。
pub fn validate_next_event(
    seq: u64,
    event: &DurableEvent,
    model: &SessionReadModel,
) -> Result<(), ProjectionError> {
    validate_next_event_details(seq, event, &model.identity.session_id, model.stats.last_seq)?;
    model_context::validate_rewrite_fingerprint(event, &model.system_prompt, &model.model_context)
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
    if let DurableEventPayload::TranscriptRewritten { source_seq, .. } = &event.payload
        && *source_seq > current_seq
    {
        return Err(ProjectionError::InvalidTranscriptRewriteSource {
            source_seq: *source_seq,
            current_seq,
        });
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

#[cfg(test)]
mod tests;
