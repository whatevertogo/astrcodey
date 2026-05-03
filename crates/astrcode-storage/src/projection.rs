//! 会话事件投影。
//!
//! EventLog 是唯一事实源；本模块只维护可从事件重建的内部读模型。

use astrcode_core::{
    event::{Event, EventPayload, Phase},
    llm::{LlmContent, LlmMessage, LlmRole},
    storage::{ConversationReadModel, SessionReadModel},
};

/// 从事件序列重建会话读模型。
pub(crate) fn replay(session_id: String, events: &[Event]) -> SessionReadModel {
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
        EventPayload::AssistantMessageStarted { .. }
        | EventPayload::AssistantTextDelta { .. }
        | EventPayload::ThinkingDelta { .. } => {
            model.phase = Phase::Streaming;
        },
        EventPayload::AssistantMessageCompleted { text, .. } => {
            model.messages.push(LlmMessage::assistant(text));
            model.phase = Phase::Idle;
        },
        EventPayload::ToolCallStarted { call_id, .. } => {
            model.pending_tool_calls.insert(call_id.clone());
            model.phase = Phase::CallingTool;
        },
        EventPayload::ToolCallArgumentsDelta { .. } | EventPayload::ToolOutputDelta { .. } => {
            model.phase = Phase::CallingTool;
        },
        EventPayload::ToolCallRequested {
            call_id,
            tool_name,
            arguments,
        } => {
            model.pending_tool_calls.insert(call_id.clone());
            model.messages.push(LlmMessage {
                role: LlmRole::Assistant,
                content: vec![LlmContent::ToolCall {
                    call_id: call_id.clone(),
                    name: tool_name.clone(),
                    arguments: arguments.clone(),
                }],
                name: None,
            });
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
                    tool_call_id: call_id.clone(),
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
        EventPayload::CompactionStarted => {
            model.phase = Phase::Compacting;
        },
        EventPayload::CompactionApplied {
            messages_removed,
            context_messages,
        } => {
            let drain_end = (*messages_removed).min(model.messages.len());
            model.messages.drain(..drain_end);
            model.context_messages = context_messages.clone();
        },
        EventPayload::CompactionCompleted { .. } => {
            model.phase = Phase::Idle;
        },
        EventPayload::AgentRunStarted => {
            model.phase = Phase::Thinking;
        },
        EventPayload::AgentRunCompleted { .. } => {
            model.phase = Phase::Idle;
        },
        EventPayload::ErrorOccurred { .. } => {
            model.phase = Phase::Error;
        },
        EventPayload::Custom { .. } => {},
    }
}

pub(crate) fn conversation_snapshot(model: SessionReadModel) -> ConversationReadModel {
    ConversationReadModel { session: model }
}
