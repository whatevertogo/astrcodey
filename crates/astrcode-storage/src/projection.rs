//! 会话事件投影。
//!
//! EventLog 是唯一事实源；本模块只维护可从事件重建的内部读模型。

use astrcode_core::{
    event::{Event, EventPayload, Phase},
    llm::{LlmContent, LlmMessage, LlmRole},
    storage::SessionReadModel,
    types::SessionId,
};

/// 从事件序列重建会话读模型。
pub(crate) fn replay(session_id: SessionId, events: &[Event]) -> SessionReadModel {
    let mut model = SessionReadModel::empty(session_id);
    for event in events {
        reduce(event, &mut model);
    }
    model
}

/// 将单个持久事件归约到读模型。
pub(crate) fn reduce(event: &Event, model: &mut SessionReadModel) {
    // seq=None 的非持久/异常事件不推进 durable cursor。
    model.latest_seq = event.seq.or(model.latest_seq);
    model.updated_at = event.timestamp.to_rfc3339();

    match &event.payload {
        EventPayload::SessionStarted {
            working_dir,
            model_id,
            parent_session_id,
        } => {
            model.working_dir = working_dir.clone();
            model.model_id = model_id.clone();
            model.parent_session_id = parent_session_id.clone();
            model.phase = Phase::Idle;
            if model.created_at.is_empty() {
                model.created_at = event.timestamp.to_rfc3339();
            }
        },
        EventPayload::SessionDeleted => {
            model.phase = Phase::Idle;
            model.messages.clear();
            model.context_messages.clear();
            model.system_prompt = None;
            model.pending_tool_calls.clear();
        },
        EventPayload::SystemPromptConfigured { text, .. } => {
            model.system_prompt = Some(text.clone());
        },
        EventPayload::TurnStarted | EventPayload::UserMessage { .. } => {
            model.phase = Phase::Thinking;
            if let EventPayload::UserMessage { text, .. } = &event.payload {
                model.messages.push(LlmMessage::user(text));
            }
        },
        EventPayload::TurnCompleted { .. } => {
            model.phase = Phase::Idle;
            model.pending_tool_calls.clear();
        },
        EventPayload::AssistantMessageStarted { .. } => {
            model.phase = Phase::Streaming;
        },
        EventPayload::AssistantMessageCompleted { text, .. } => {
            model.messages.push(LlmMessage::assistant(text));
            model.phase = Phase::Idle;
        },
        // ToolCallStarted is non-durable and only used for live UI state.
        // Retained for backwards compatibility with existing JSONL files.
        EventPayload::ToolCallStarted { .. } => {},
        EventPayload::ToolCallRequested {
            call_id,
            tool_name,
            arguments,
        } => {
            model.pending_tool_calls.insert(call_id.clone());
            let tool_call = LlmContent::ToolCall {
                call_id: call_id.to_string(),
                name: tool_name.clone(),
                arguments: arguments.clone(),
            };
            // Merge into the previous assistant message if it already contains
            // tool calls from the same batch.  OpenAI requires all parallel tool
            // calls to live in a single assistant message; splitting them across
            // consecutive assistant messages triggers a 400 protocol error.
            if let Some(last) = model.messages.last_mut() {
                if last.role == LlmRole::Assistant
                    && last
                        .content
                        .iter()
                        .any(|c| matches!(c, LlmContent::ToolCall { .. }))
                {
                    last.content.push(tool_call);
                } else {
                    model.messages.push(LlmMessage {
                        role: LlmRole::Assistant,
                        content: vec![tool_call],
                        name: None,
                    });
                }
            } else {
                model.messages.push(LlmMessage {
                    role: LlmRole::Assistant,
                    content: vec![tool_call],
                    name: None,
                });
            }
            model.phase = Phase::CallingTool;
        },
        EventPayload::ToolCallCompleted {
            call_id,
            tool_name,
            result,
        } => {
            model.pending_tool_calls.remove(call_id);
            model.messages.push(LlmMessage {
                role: LlmRole::Tool,
                content: vec![LlmContent::ToolResult {
                    tool_call_id: call_id.to_string(),
                    content: result.content.clone(),
                    is_error: result.is_error,
                }],
                name: Some(tool_name.clone()),
            });
            model.phase = if model.pending_tool_calls.is_empty() {
                Phase::Thinking
            } else {
                Phase::CallingTool
            };
        },
        // Non-durable events below: never persisted to JSONL, only broadcast for
        // live UI. Kept as no-op arms to maintain exhaustive matching for
        // backwards compatibility with existing JSONL files that may contain
        // ToolCallStarted / ToolCallBackgrounded / BackgroundTaskCompleted.
        EventPayload::CompactionStarted
        | EventPayload::AssistantTextDelta { .. }
        | EventPayload::ThinkingDelta { .. }
        | EventPayload::ToolCallArgumentsDelta { .. }
        | EventPayload::ToolOutputDelta { .. }
        | EventPayload::AgentRunStarted
        | EventPayload::AgentRunCompleted { .. }
        | EventPayload::ToolCallBackgrounded { .. }
        | EventPayload::BackgroundTaskOutput { .. }
        | EventPayload::BackgroundTaskCompleted { .. } => {},
        EventPayload::CompactBoundaryCreated { .. } => {
            model.phase = Phase::Idle;
        },
        EventPayload::SessionContinuedFromCompaction {
            context_messages,
            retained_messages,
            ..
        } => {
            // A compact continuation child session is rebuilt from the compacted
            // parent state. The compacted summary/context is preserved in
            // `context_messages`, while only the retained transcript becomes the
            // new visible `messages` list.
            model.context_messages = context_messages.clone();
            model.messages = retained_messages.clone();
            model.phase = Phase::Idle;
        },
        EventPayload::ErrorOccurred { .. } => {
            model.phase = Phase::Error;
        },
        EventPayload::Custom { .. } => {},
    }
}
