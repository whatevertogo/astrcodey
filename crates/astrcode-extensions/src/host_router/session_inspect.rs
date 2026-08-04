//! `session_inspect` 宿主适配：存储领域模型只在此处映射为稳定 wire DTO。

use std::{future::Future, sync::Arc};

use astrcode_core::{
    compaction::CompactStrategy,
    event::Phase,
    llm::{LlmContent, LlmMessage},
    types::SessionId,
};
use astrcode_extension_sdk::{
    s5r::ErrorPayload,
    session_inspect::{
        SessionInspectAgentSession, SessionInspectCompaction, SessionInspectContent,
        SessionInspectListItem, SessionInspectListOutput, SessionInspectMessage,
        SessionInspectPendingApproval, SessionInspectProviderMessagesOutput,
        SessionInspectReadModel, SessionInspectReadModelOutput, SessionInspectSequencedMessage,
        SessionInspectSnapshot, SessionInspectSnapshotOutput,
    },
};
use astrcode_session_projection::{
    AgentSessionLinkView, AgentSessionStatus, SequencedLlmMessage, SessionReadModel, SessionSummary,
};
use astrcode_storage::{SessionReader, StorageError};
use serde::Serialize;
use serde_json::Value;

use super::HOST_INVOKE_TIMEOUT;

pub(super) async fn list(reader: Arc<dyn SessionReader>) -> Result<Value, ErrorPayload> {
    let summaries = storage_call("session.inspect.list", reader.list_session_summaries()).await?;
    to_value(SessionInspectListOutput {
        sessions: summaries.into_iter().map(list_item).collect(),
    })
}

pub(super) async fn snapshot(
    reader: Arc<dyn SessionReader>,
    input: Value,
) -> Result<Value, ErrorPayload> {
    let session_id = session_id(&input)?;
    let model = storage_call(
        "session.inspect.snapshot",
        reader.session_read_model(&session_id),
    )
    .await?;
    let mut pending_tool_call_ids = model
        .execution
        .pending_tool_calls
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    pending_tool_call_ids.sort();
    to_value(SessionInspectSnapshotOutput {
        snapshot: SessionInspectSnapshot {
            session_id: model.identity.session_id.to_string(),
            cursor: model.cursor(),
            working_dir: model.identity.working_dir.clone(),
            model_id: model.identity.model_id.clone(),
            phase: phase_name(model.execution.phase).into(),
            parent_session_id: model
                .identity
                .parent
                .as_ref()
                .map(|parent| parent.session_id.to_string()),
            source_extension: model.identity.source_extension.clone(),
            message_count: model.transcript.messages.len(),
            pending_tool_call_ids,
            agent_session_count: model.agent_sessions.len(),
        },
    })
}

pub(super) async fn read_model(
    reader: Arc<dyn SessionReader>,
    input: Value,
) -> Result<Value, ErrorPayload> {
    let session_id = session_id(&input)?;
    let model = storage_call(
        "session.inspect.read_model",
        reader.session_read_model(&session_id),
    )
    .await?;
    to_value(SessionInspectReadModelOutput {
        read_model: read_model_dto((*model).clone()),
    })
}

pub(super) async fn provider_messages(
    reader: Arc<dyn SessionReader>,
    input: Value,
) -> Result<Value, ErrorPayload> {
    let session_id = session_id(&input)?;
    let model = storage_call(
        "session.inspect.provider_messages",
        reader.session_read_model(&session_id),
    )
    .await?;
    let messages = astrcode_core::llm::provider_visible_messages(
        model
            .transcript
            .messages
            .iter()
            .map(|message| message.message.clone())
            .collect(),
    );
    to_value(SessionInspectProviderMessagesOutput {
        messages: messages.into_iter().map(message_dto).collect(),
    })
}

async fn storage_call<T, F>(operation: &str, future: F) -> Result<T, ErrorPayload>
where
    F: Future<Output = Result<T, StorageError>>,
{
    tokio::time::timeout(HOST_INVOKE_TIMEOUT, future)
        .await
        .map_err(|_| ErrorPayload::new("timeout", format!("{operation} timed out")))?
        .map_err(|error| ErrorPayload::new("session_error", error.to_string()))
}

fn session_id(input: &Value) -> Result<SessionId, ErrorPayload> {
    input
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(SessionId::new)
        .ok_or_else(|| ErrorPayload::new("invalid_input", "session_id must be a string"))
}

fn list_item(summary: SessionSummary) -> SessionInspectListItem {
    SessionInspectListItem {
        session_id: summary.session_id.to_string(),
        working_dir: summary.working_dir,
        model_id: summary.model_id,
        parent_session_id: summary.parent_session_id.map(|id| id.to_string()),
        source_extension: summary.source_extension,
        created_at: summary.created_at,
        updated_at: summary.updated_at,
        phase: phase_name(summary.phase).into(),
        latest_cursor: summary.latest_cursor,
        first_user_message: summary.first_user_message,
    }
}

pub(super) fn read_model_dto(model: SessionReadModel) -> SessionInspectReadModel {
    let mut pending_tool_call_ids = model
        .execution
        .pending_tool_calls
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    pending_tool_call_ids.sort();
    let identity = model.identity;
    let stats = model.stats;
    let prompt = model.system_prompt;
    let transcript = model.transcript;
    let execution = model.execution;
    SessionInspectReadModel {
        session_id: identity.session_id.to_string(),
        messages: transcript
            .messages
            .into_iter()
            .map(sequenced_message_dto)
            .collect(),
        working_dir: identity.working_dir,
        model_id: identity.model_id,
        phase: phase_name(execution.phase).into(),
        system_prompt: Some(prompt.text),
        extra_system_prompt: prompt.extra,
        system_prompt_fingerprint: Some(prompt.fingerprint),
        pending_tool_call_ids,
        pending_tool_approvals: execution
            .pending_tool_approvals
            .into_iter()
            .map(|(id, approval)| {
                (
                    id.to_string(),
                    SessionInspectPendingApproval {
                        prompt: approval.prompt,
                        rule_key: approval.rule_key,
                    },
                )
            })
            .collect(),
        created_at: stats.created_at.to_rfc3339(),
        updated_at: stats.updated_at.to_rfc3339(),
        parent_session_id: identity.parent.map(|parent| parent.session_id.to_string()),
        tool_selection: Some(identity.tool_selection.into()),
        source_extension: identity.source_extension,
        agent_sessions: model
            .agent_sessions
            .into_iter()
            .map(agent_session_dto)
            .collect(),
        compactions: model
            .compactions
            .into_iter()
            .map(|boundary| {
                let (strategy, keep_recent_turns) = compact_strategy(boundary.strategy);
                SessionInspectCompaction {
                    trigger: boundary.trigger,
                    pre_tokens: boundary.pre_tokens,
                    post_tokens: boundary.post_tokens,
                    summary: boundary.summary,
                    transcript_path: boundary.transcript_path,
                    seq: boundary.seq,
                    source_seq: boundary.source_seq,
                    strategy: strategy.into(),
                    keep_recent_turns,
                }
            })
            .collect(),
        latest_seq: Some(stats.last_seq),
    }
}

fn sequenced_message_dto(message: SequencedLlmMessage) -> SessionInspectSequencedMessage {
    SessionInspectSequencedMessage {
        message: message_dto(message.message),
        updated_seq: message.updated_seq,
        source: message.source,
    }
}

fn message_dto(message: LlmMessage) -> SessionInspectMessage {
    SessionInspectMessage {
        role: message.role.as_str().into(),
        content: message.content.into_iter().map(content_dto).collect(),
        name: message.name,
        reasoning_content: message.reasoning_content,
    }
}

fn content_dto(content: LlmContent) -> SessionInspectContent {
    match content {
        LlmContent::Text { text } => SessionInspectContent::Text { text },
        LlmContent::Image {
            base64,
            media_type,
            filename,
        } => SessionInspectContent::Image {
            base64,
            media_type,
            filename,
        },
        LlmContent::ToolCall {
            call_id,
            name,
            arguments,
            ..
        } => SessionInspectContent::ToolCall {
            call_id,
            name,
            arguments,
        },
        LlmContent::ToolResult {
            tool_call_id,
            content,
            is_error,
        } => SessionInspectContent::ToolResult {
            tool_call_id,
            content,
            is_error,
        },
    }
}

fn agent_session_dto(agent: AgentSessionLinkView) -> SessionInspectAgentSession {
    SessionInspectAgentSession {
        child_session_id: agent.child_session_id.to_string(),
        tool_call_id: Some(agent.tool_call_id.to_string()),
        agent_name: agent.agent_name,
        task: agent.task,
        status: match agent.status {
            AgentSessionStatus::Running => "running",
            AgentSessionStatus::Completed => "completed",
            AgentSessionStatus::Failed => "failed",
        }
        .into(),
        final_session_id: agent.final_session_id.map(|id| id.to_string()),
        summary: agent.summary,
        error: agent.error,
        phase: None,
        current_tool: None,
    }
}

fn compact_strategy(strategy: CompactStrategy) -> (&'static str, Option<usize>) {
    match strategy {
        CompactStrategy::Auto => ("auto", None),
        CompactStrategy::Manual { keep_recent_turns } => ("manual", keep_recent_turns),
        CompactStrategy::ReactivePromptTooLong => ("reactive_prompt_too_long", None),
    }
}

pub(super) fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Idle => "idle",
        Phase::Thinking => "thinking",
        Phase::Streaming => "streaming",
        Phase::CallingTool => "calling_tool",
        Phase::Compacting => "compacting",
        Phase::Error => "error",
    }
}

fn to_value(value: impl Serialize) -> Result<Value, ErrorPayload> {
    serde_json::to_value(value).map_err(|error| {
        ErrorPayload::new(
            "serialization_failed",
            format!("failed to serialize session inspect response: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        event::{
            DurableEvent, DurableEventPayload, PersistedSystemPrompt, SessionStarted, StoredEvent,
            SystemPromptSource,
        },
        llm::LlmMessage,
        tool::SessionToolSelection,
    };
    use astrcode_session_projection::replay;

    use super::*;

    #[test]
    fn read_model_mapping_uses_stable_wire_names() {
        let session_id = SessionId::new("session-1");
        let mut model = replay(
            session_id.clone(),
            &[StoredEvent::new(
                0,
                DurableEvent::session(
                    session_id,
                    DurableEventPayload::SessionStarted(SessionStarted {
                        working_dir: "/workspace".into(),
                        model_id: "model".into(),
                        parent: None,
                        tool_selection: SessionToolSelection::default(),
                        source_extension: None,
                        initial_system_prompt: PersistedSystemPrompt {
                            text: "system".into(),
                            fingerprint: "fingerprint".into(),
                            extra_system_prompt: None,
                            source: SystemPromptSource::Native,
                        },
                    }),
                ),
            )],
        )
        .unwrap();
        model.execution.phase = Phase::CallingTool;
        model.transcript.messages.push(SequencedLlmMessage {
            message: LlmMessage::user("hello"),
            updated_seq: 2,
            source: None,
        });

        let value = serde_json::to_value(SessionInspectReadModelOutput {
            read_model: read_model_dto(model),
        })
        .expect("serialize mapped model");

        assert_eq!(value["readModel"]["sessionId"], "session-1");
        assert_eq!(value["readModel"]["phase"], "calling_tool");
        assert_eq!(
            value["readModel"]["messages"][0]["message"]["content"][0]["type"],
            "text"
        );
    }
}
