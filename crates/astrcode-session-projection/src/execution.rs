use std::collections::{BTreeMap, HashSet};

use astrcode_core::{
    event::{DurableEventPayload, Phase, StoredEvent},
    types::{ToolCallId, TurnId},
    user_input::UserInput,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingToolApprovalView {
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingInput {
    pub accepted_seq: u64,
    pub input: UserInput,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionExecutionState {
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsettled_turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_inputs: Vec<PendingInput>,
    pub pending_tool_calls: HashSet<ToolCallId>,
    pub pending_tool_approvals: BTreeMap<ToolCallId, PendingToolApprovalView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_step: Option<ActiveStepView>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActiveStepView {
    pub step_index: u32,
    pub attempt: u32,
    #[serde(default)]
    pub completed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

pub(crate) fn apply_event(event: &StoredEvent, execution: &mut SessionExecutionState) {
    match &event.payload {
        DurableEventPayload::TurnStarted => {
            execution.phase = Phase::Thinking;
            execution.unsettled_turn_id = event.turn_id.clone();
        },
        DurableEventPayload::StepStarted {
            step_index,
            attempt,
        } => {
            execution.active_step = Some(ActiveStepView {
                step_index: *step_index,
                attempt: *attempt,
                completed: false,
                finish_reason: None,
            });
        },
        DurableEventPayload::StepCompleted {
            step_index,
            attempt,
            finish_reason,
        } => {
            if let Some(step) = execution.active_step.as_mut()
                && step.step_index == *step_index
                && step.attempt == *attempt
            {
                step.completed = true;
                step.finish_reason = finish_reason.clone();
            }
        },
        DurableEventPayload::UserInputAccepted { input } => {
            execution.pending_inputs.push(PendingInput {
                accepted_seq: event.seq,
                input: input.clone(),
            });
        },
        DurableEventPayload::UserMessage { accepted_seq, .. } => {
            if let Some(accepted_seq) = accepted_seq {
                execution
                    .pending_inputs
                    .retain(|input| input.accepted_seq != *accepted_seq);
            }
            execution.phase = Phase::Thinking;
        },
        DurableEventPayload::AssistantMessageCompleted { .. } => {
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
            settle(execution, Phase::Idle);
        },
        DurableEventPayload::SessionForked { .. } => settle(execution, Phase::Idle),
        DurableEventPayload::ErrorOccurred { .. } => {
            execution.phase = Phase::Error;
        },
        _ => {},
    }
}

fn settle(execution: &mut SessionExecutionState, phase: Phase) {
    execution.phase = phase;
    execution.unsettled_turn_id = None;
    execution.pending_tool_calls.clear();
    execution.pending_tool_approvals.clear();
    execution.active_step = None;
}
