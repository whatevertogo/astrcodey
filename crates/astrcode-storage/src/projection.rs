//! 会话事件投影。
//!
//! EventLog 是唯一事实源；本模块只维护可从事件重建的内部读模型。

use astrcode_core::{
    event::{Event, EventPayload, Phase},
    llm::{LlmContent, LlmMessage, LlmRole},
    storage::{
        AgentSessionLinkView, AgentSessionStatus, BackgroundToolCallView, CompactBoundaryView,
        SessionReadModel,
    },
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
            tool_policy,
        } => {
            model.working_dir = working_dir.clone();
            model.model_id = model_id.clone();
            model.parent_session_id = parent_session_id.clone();
            model.tool_policy = tool_policy.clone();
            model.phase = Phase::Idle;
            if model.created_at.is_empty() {
                model.created_at = event.timestamp.to_rfc3339();
            }
        },
        EventPayload::ModelIdChanged { model_id } => {
            model.model_id = model_id.clone();
        },
        EventPayload::SessionDeleted => {
            model.phase = Phase::Idle;
            model.messages.clear();
            model.context_messages.clear();
            model.system_prompt = None;
            model.pending_tool_calls.clear();
            model.background_tool_calls.clear();
            model.compact_boundaries.clear();
            model.agent_sessions.clear();
        },
        EventPayload::AgentSessionSpawned {
            child_session_id,
            agent_name,
            task,
            tool_policy: _,
        } => {
            model.agent_sessions.push(AgentSessionLinkView {
                child_session_id: child_session_id.clone(),
                agent_name: agent_name.clone(),
                task: task.clone(),
                status: AgentSessionStatus::Running,
            });
        },
        EventPayload::AgentSessionCompleted {
            child_session_id, ..
        }
        | EventPayload::AgentSessionFailed {
            child_session_id, ..
        } => {
            if let Some(link) = model
                .agent_sessions
                .iter_mut()
                .find(|l| l.child_session_id == *child_session_id)
            {
                link.status = match &event.payload {
                    EventPayload::AgentSessionCompleted { .. } => AgentSessionStatus::Completed,
                    EventPayload::AgentSessionFailed { .. } => AgentSessionStatus::Failed,
                    _ => unreachable!(),
                };
            }
        },
        EventPayload::SystemPromptConfigured {
            text,
            fingerprint,
            extra_system_prompt,
        } => {
            model.system_prompt = Some(text.clone());
            model.extra_system_prompt = extra_system_prompt.clone();
            model.system_prompt_fingerprint = Some(fingerprint.clone());
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
        EventPayload::AssistantMessageCompleted {
            text,
            reasoning_content,
            ..
        } => {
            let mut msg = LlmMessage::assistant(text);
            msg.reasoning_content = reasoning_content.clone();
            model.messages.push(msg);
            model.phase = Phase::Idle;
        },
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
            // Merge into the previous assistant message for this model sub-turn.
            // DeepSeek thinking mode requires reasoning_content and tool_calls to
            // be replayed on the same assistant message after tool use.
            if let Some(last) = model.messages.last_mut() {
                if last.role == LlmRole::Assistant {
                    last.content.push(tool_call);
                } else {
                    model.messages.push(LlmMessage {
                        role: LlmRole::Assistant,
                        content: vec![tool_call],
                        name: None,
                        reasoning_content: None,
                    });
                }
            } else {
                model.messages.push(LlmMessage {
                    role: LlmRole::Assistant,
                    content: vec![tool_call],
                    name: None,
                    reasoning_content: None,
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
            if let Some(task_id) = result
                .metadata
                .get("task_id")
                .and_then(serde_json::Value::as_str)
            {
                model.background_tool_calls.insert(
                    call_id.clone(),
                    BackgroundToolCallView {
                        task_id: task_id.into(),
                        completed: !result
                            .metadata
                            .get("backgrounded")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false),
                    },
                );
            }
            model.messages.push(LlmMessage {
                role: LlmRole::Tool,
                content: vec![LlmContent::ToolResult {
                    tool_call_id: call_id.to_string(),
                    content: result.content.clone(),
                    is_error: result.is_error,
                }],
                name: Some(tool_name.clone()),
                reasoning_content: None,
            });
            model.phase = if model.pending_tool_calls.is_empty() {
                Phase::Thinking
            } else {
                Phase::CallingTool
            };
        },
        EventPayload::CompactBoundaryCreated {
            trigger,
            pre_tokens,
            post_tokens,
            summary,
            transcript_path,
            ..
        } => {
            model.compact_boundaries.push(CompactBoundaryView {
                trigger: trigger.clone(),
                pre_tokens: *pre_tokens,
                post_tokens: *post_tokens,
                summary: summary.clone(),
                transcript_path: transcript_path.clone(),
                seq: event.seq.unwrap_or_default(),
            });
            model.phase = Phase::Idle;
        },
        EventPayload::SessionContinuedFromCompaction {
            context_messages,
            retained_messages,
            ..
        } => {
            model.context_messages = context_messages.clone();
            model.messages = retained_messages.clone();
            model.phase = Phase::Idle;
        },
        EventPayload::ErrorOccurred { .. } => {
            model.phase = Phase::Error;
        },
        // Non-durable events: never persisted to JSONL, only broadcast for live UI.
        EventPayload::ToolCallStarted { .. }
        | EventPayload::CompactionStarted
        | EventPayload::AssistantTextDelta { .. }
        | EventPayload::ThinkingDelta { .. }
        | EventPayload::ToolCallArgumentsDelta { .. }
        | EventPayload::ToolOutputDelta { .. }
        | EventPayload::AgentRunStarted
        | EventPayload::AgentRunCompleted { .. }
        | EventPayload::ToolCallBackgrounded { .. }
        | EventPayload::BackgroundTaskOutput { .. }
        | EventPayload::BackgroundTaskCompleted { .. } => {},
        EventPayload::Custom { .. } => {},
    }
}
