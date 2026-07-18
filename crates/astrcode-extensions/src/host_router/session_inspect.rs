//! `session_inspect` 宿主适配：存储领域模型只在此处映射为稳定 wire DTO。

use std::{collections::BTreeMap, future::Future, sync::Arc};

use astrcode_core::{
    event::Phase,
    extension::{ChildToolPolicy, CompactStrategy},
    llm::{LlmContent, LlmMessage},
    storage::{
        AgentSessionLinkView, AgentSessionStatus, EventReader, SequencedLlmMessage,
        SessionReadModel, SessionSummary, StorageError,
    },
    types::SessionId,
};
use astrcode_extension_sdk::{
    s5r::ErrorPayload,
    session_inspect::{
        SessionInspectAgentSession, SessionInspectCompactBoundary, SessionInspectContent,
        SessionInspectListItem, SessionInspectListOutput, SessionInspectMessage,
        SessionInspectPendingApproval, SessionInspectPendingInteraction,
        SessionInspectProviderMessagesOutput, SessionInspectReadModel,
        SessionInspectReadModelOutput, SessionInspectSequencedMessage, SessionInspectSnapshot,
        SessionInspectSnapshotOutput, SessionInspectToolPolicy,
    },
};
use serde::Serialize;
use serde_json::Value;

use super::HOST_INVOKE_TIMEOUT;

pub(super) async fn list(reader: Arc<dyn EventReader>) -> Result<Value, ErrorPayload> {
    let summaries = storage_call("session.inspect.list", reader.list_session_summaries()).await?;
    to_value(SessionInspectListOutput {
        sessions: summaries.into_iter().map(list_item).collect(),
    })
}

pub(super) async fn snapshot(
    reader: Arc<dyn EventReader>,
    input: Value,
) -> Result<Value, ErrorPayload> {
    let session_id = session_id(&input)?;
    let model = storage_call(
        "session.inspect.snapshot",
        reader.session_read_model(&session_id),
    )
    .await?;
    let mut pending_tool_call_ids = model
        .pending_tool_calls
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    pending_tool_call_ids.sort();
    to_value(SessionInspectSnapshotOutput {
        snapshot: SessionInspectSnapshot {
            session_id: model.session_id.to_string(),
            cursor: model.cursor(),
            working_dir: model.working_dir,
            model_id: model.model_id,
            phase: phase_name(model.phase).into(),
            parent_session_id: model.parent_session_id.map(|id| id.to_string()),
            source_extension: model.source_extension,
            message_count: model.messages.len(),
            context_message_count: model.context_messages.len(),
            pending_tool_call_ids,
            agent_session_count: model.agent_sessions.len(),
        },
    })
}

pub(super) async fn read_model(
    reader: Arc<dyn EventReader>,
    input: Value,
) -> Result<Value, ErrorPayload> {
    let session_id = session_id(&input)?;
    let model = storage_call(
        "session.inspect.read_model",
        reader.session_read_model(&session_id),
    )
    .await?;
    to_value(SessionInspectReadModelOutput {
        read_model: read_model_dto(model),
    })
}

pub(super) async fn provider_messages(
    reader: Arc<dyn EventReader>,
    input: Value,
) -> Result<Value, ErrorPayload> {
    let session_id = session_id(&input)?;
    let messages = storage_call(
        "session.inspect.provider_messages",
        reader.session_provider_messages(&session_id),
    )
    .await?;
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

fn read_model_dto(model: SessionReadModel) -> SessionInspectReadModel {
    let mut pending_tool_call_ids = model
        .pending_tool_calls
        .into_iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>();
    pending_tool_call_ids.sort();
    SessionInspectReadModel {
        session_id: model.session_id.to_string(),
        messages: model
            .messages
            .into_iter()
            .map(sequenced_message_dto)
            .collect(),
        context_messages: model
            .context_messages
            .into_iter()
            .map(sequenced_message_dto)
            .collect(),
        working_dir: model.working_dir,
        model_id: model.model_id,
        phase: phase_name(model.phase).into(),
        system_prompt: model.system_prompt,
        extra_system_prompt: model.extra_system_prompt,
        system_prompt_fingerprint: model.system_prompt_fingerprint,
        pending_tool_call_ids,
        pending_tool_approvals: model
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
        pending_tool_interactions: model
            .pending_tool_interactions
            .into_iter()
            .map(|(id, interaction)| {
                (
                    id.to_string(),
                    SessionInspectPendingInteraction {
                        content: interaction.content,
                        metadata: interaction.metadata,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>(),
        created_at: model.created_at,
        updated_at: model.updated_at,
        parent_session_id: model.parent_session_id.map(|id| id.to_string()),
        tool_policy: model.tool_policy.map(tool_policy_dto),
        source_extension: model.source_extension,
        agent_sessions: model
            .agent_sessions
            .into_iter()
            .map(agent_session_dto)
            .collect(),
        compact_boundaries: model
            .compact_boundaries
            .into_iter()
            .map(|boundary| {
                let (strategy, keep_recent_turns) = compact_strategy(boundary.strategy);
                SessionInspectCompactBoundary {
                    trigger: boundary.trigger,
                    pre_tokens: boundary.pre_tokens,
                    post_tokens: boundary.post_tokens,
                    summary: boundary.summary,
                    transcript_path: boundary.transcript_path,
                    seq: boundary.seq,
                    base_event_seq: boundary.base_event_seq,
                    strategy: strategy.into(),
                    keep_recent_turns,
                }
            })
            .collect(),
        latest_seq: model.latest_seq,
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

fn tool_policy_dto(policy: ChildToolPolicy) -> SessionInspectToolPolicy {
    match policy {
        ChildToolPolicy::Allow { tools } => SessionInspectToolPolicy {
            mode: "allow".into(),
            tools,
        },
        ChildToolPolicy::Deny { tools } => SessionInspectToolPolicy {
            mode: "deny".into(),
            tools,
        },
    }
}

fn agent_session_dto(agent: AgentSessionLinkView) -> SessionInspectAgentSession {
    SessionInspectAgentSession {
        child_session_id: agent.child_session_id.to_string(),
        tool_call_id: agent.tool_call_id.map(|id| id.to_string()),
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
        phase: agent.phase.map(phase_name).map(str::to_string),
        current_tool: agent.current_tool,
    }
}

fn compact_strategy(strategy: CompactStrategy) -> (&'static str, Option<usize>) {
    match strategy {
        CompactStrategy::Auto => ("auto", None),
        CompactStrategy::Manual { keep_recent_turns } => ("manual", keep_recent_turns),
        CompactStrategy::ReactivePromptTooLong => ("reactive_prompt_too_long", None),
    }
}

fn phase_name(phase: Phase) -> &'static str {
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
    use astrcode_core::{llm::LlmMessage, storage::SessionReadModel};

    use super::*;

    #[test]
    fn read_model_mapping_uses_stable_wire_names() {
        let mut model = SessionReadModel::empty(SessionId::new("session-1"));
        model.phase = Phase::CallingTool;
        model.messages.push(SequencedLlmMessage {
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
