//! Internal domain and extension types mapped at the server protocol boundary.

use astrcode_context::is_compact_summary_message;
use astrcode_core::llm::{LlmContent, LlmMessage};
use astrcode_extension_sdk::extension::{
    ExtensionCapability, ExtensionEventDecl, ExtensionHttpMethod, Keybinding, SlashCommand,
    StatusItem,
};
use astrcode_protocol::{
    agent_session_link::{AgentSessionLinkDto, AgentSessionStatusDto},
    events::{
        ExtensionCommandInfoDto, KeybindingDto, MessageDto, SessionSnapshot, StatusItemInfoDto,
    },
    http::{ExtensionEventDeclDto, ExtensionSlashCommandDto, SlashCommandInfoDto, StatusItemDto},
    wire::{CommandSourceDto, ExtensionCapabilityDto, ExtensionHttpMethodDto, MessageRoleDto},
};
use astrcode_session_projection::{AgentSessionLinkView, AgentSessionStatus, SessionReadModel};

use crate::session_command_contract::{CommandInfo, CommandSource};

pub(crate) fn agent_session_link_to_dto(link: &AgentSessionLinkView) -> AgentSessionLinkDto {
    AgentSessionLinkDto {
        child_session_id: link.child_session_id.to_string(),
        tool_call_id: Some(link.tool_call_id.to_string()),
        agent_name: Some(link.agent_name.clone()),
        task: Some(link.task.clone()),
        status: Some(agent_session_status_to_dto(link.status)),
        final_session_id: link.final_session_id.as_ref().map(ToString::to_string),
        summary: link.summary.clone(),
        error: link.error.clone(),
        phase: None,
        current_tool: None,
    }
}

pub(crate) fn session_snapshot(state: &SessionReadModel) -> SessionSnapshot {
    SessionSnapshot {
        session_id: state.identity.session_id.to_string(),
        cursor: state.cursor(),
        messages: state
            .transcript
            .messages
            .iter()
            .map(|message| message_to_dto(&message.message))
            .collect(),
        model_id: state.identity.model_id.clone(),
        working_dir: state.identity.working_dir.clone(),
        agent_sessions: state
            .agent_sessions
            .iter()
            .map(agent_session_link_to_dto)
            .collect(),
    }
}

/// Compact summaries are synthetic user messages internally, but system
/// messages on the client-facing protocol.
pub(crate) fn message_to_dto(message: &LlmMessage) -> MessageDto {
    let content = message
        .content
        .iter()
        .map(content_display_text)
        .collect::<String>();
    let is_compact_summary = is_compact_summary_message(message);
    let role = if is_compact_summary {
        MessageRoleDto::System
    } else {
        message.role.into()
    };

    MessageDto {
        role,
        content,
        is_compact_summary: Some(is_compact_summary),
    }
}

fn content_display_text(content: &LlmContent) -> String {
    match content {
        LlmContent::ToolCall {
            name, arguments, ..
        } if name == "upsertSessionPlan" => arguments
            .get("content")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        other => other.to_display_text(),
    }
}

fn agent_session_status_to_dto(status: AgentSessionStatus) -> AgentSessionStatusDto {
    match status {
        AgentSessionStatus::Running => AgentSessionStatusDto::Running,
        AgentSessionStatus::Completed => AgentSessionStatusDto::Completed,
        AgentSessionStatus::Failed => AgentSessionStatusDto::Failed,
    }
}

pub(crate) fn extension_capability_to_dto(
    capability: ExtensionCapability,
) -> ExtensionCapabilityDto {
    match capability {
        ExtensionCapability::SessionControl => ExtensionCapabilityDto::SessionControl,
        ExtensionCapability::SessionInspect => ExtensionCapabilityDto::SessionInspect,
        ExtensionCapability::PublicHttp => ExtensionCapabilityDto::PublicHttp,
        ExtensionCapability::AuthenticatedHttp => ExtensionCapabilityDto::AuthenticatedHttp,
        ExtensionCapability::PublicHttpDispatch => ExtensionCapabilityDto::PublicHttpDispatch,
        ExtensionCapability::MainModel => ExtensionCapabilityDto::MainModel,
        ExtensionCapability::SmallModel => ExtensionCapabilityDto::SmallModel,
        ExtensionCapability::SessionHistory => ExtensionCapabilityDto::SessionHistory,
        ExtensionCapability::EmitEvents => ExtensionCapabilityDto::EmitEvents,
        ExtensionCapability::ConsumeEvents => ExtensionCapabilityDto::ConsumeEvents,
        ExtensionCapability::WorkspaceRead => ExtensionCapabilityDto::WorkspaceRead,
        ExtensionCapability::WorkspaceWrite => ExtensionCapabilityDto::WorkspaceWrite,
        ExtensionCapability::ProcessSpawn => ExtensionCapabilityDto::ProcessSpawn,
        ExtensionCapability::NetworkClient => ExtensionCapabilityDto::NetworkClient,
        ExtensionCapability::ProviderRequest => ExtensionCapabilityDto::ProviderRequest,
        ExtensionCapability::InputDelivery => ExtensionCapabilityDto::InputDelivery,
        ExtensionCapability::ToolIntercept => ExtensionCapabilityDto::ToolIntercept,
        ExtensionCapability::TurnContinuationControl => {
            ExtensionCapabilityDto::TurnContinuationControl
        },
        ExtensionCapability::LiveConversation => ExtensionCapabilityDto::LiveConversation,
    }
}

pub(crate) fn extension_http_method_to_dto(method: ExtensionHttpMethod) -> ExtensionHttpMethodDto {
    match method {
        ExtensionHttpMethod::Get => ExtensionHttpMethodDto::Get,
        ExtensionHttpMethod::Post => ExtensionHttpMethodDto::Post,
        ExtensionHttpMethod::Put => ExtensionHttpMethodDto::Put,
        ExtensionHttpMethod::Patch => ExtensionHttpMethodDto::Patch,
        ExtensionHttpMethod::Delete => ExtensionHttpMethodDto::Delete,
    }
}

pub(crate) fn keybinding_to_dto(binding: Keybinding) -> KeybindingDto {
    KeybindingDto {
        key: binding.key,
        command: binding.command,
        arguments: binding.arguments,
        description: binding.description,
    }
}

pub(crate) fn status_item_to_dto(item: StatusItem) -> StatusItemDto {
    StatusItemDto {
        id: item.id,
        text: item.text,
        priority: item.priority,
        tooltip: item.tooltip,
    }
}

pub(crate) fn status_item_to_info_dto(item: StatusItem) -> StatusItemInfoDto {
    StatusItemInfoDto {
        id: item.id,
        text: item.text,
        priority: item.priority,
    }
}

pub(crate) fn command_info_to_stdio_dto(command: CommandInfo) -> ExtensionCommandInfoDto {
    ExtensionCommandInfoDto {
        name: command.name,
        description: command.description,
        needs_argument: command.needs_argument,
        requires_idle: command.requires_idle,
        argument_completions: command.argument_completions,
        priority: command.priority,
        source: command_source_to_dto(command.source),
    }
}

pub(crate) fn command_info_to_http_dto(command: CommandInfo) -> SlashCommandInfoDto {
    SlashCommandInfoDto {
        name: command.name,
        description: command.description,
        needs_argument: command.needs_argument,
        requires_idle: command.requires_idle,
        argument_completions: command.argument_completions,
        priority: command.priority,
        source: command_source_to_dto(command.source),
    }
}

fn command_source_to_dto(source: CommandSource) -> CommandSourceDto {
    match source {
        CommandSource::Builtin => CommandSourceDto::Builtin,
        CommandSource::Extension => CommandSourceDto::Extension,
        CommandSource::Skill => CommandSourceDto::Skill,
    }
}

pub(crate) fn extension_slash_command_to_dto(command: SlashCommand) -> ExtensionSlashCommandDto {
    ExtensionSlashCommandDto {
        name: command.name,
        description: command.description,
        args_schema: command.args_schema,
        requires_idle: command.requires_idle,
        argument_completions: command.argument_completions,
        priority: command.priority,
    }
}

pub(crate) fn extension_event_decl_to_dto(event: ExtensionEventDecl) -> ExtensionEventDeclDto {
    ExtensionEventDeclDto {
        event_type: event.event_type,
        schema_version: event.schema_version,
        durable: event.durable,
        max_payload_bytes: event.max_payload_bytes,
    }
}

#[cfg(test)]
mod tests {
    use astrcode_context::is_compact_summary_text;
    use astrcode_core::{
        llm::{LlmContent, LlmMessage, LlmRole},
        types::{SessionId, ToolCallId},
    };

    use super::*;

    #[test]
    fn agent_session_mapping_preserves_snapshot_fields() {
        let link = AgentSessionLinkView {
            child_session_id: SessionId::from("child-1"),
            tool_call_id: ToolCallId::from("tool-1"),
            agent_name: "reviewer".into(),
            task: "review changes".into(),
            status: AgentSessionStatus::Completed,
            final_session_id: Some(SessionId::from("child-final")),
            summary: Some("done".into()),
            error: None,
        };

        let dto = agent_session_link_to_dto(&link);

        assert_eq!(dto.child_session_id, "child-1");
        assert_eq!(dto.tool_call_id.as_deref(), Some("tool-1"));
        assert_eq!(dto.status, Some(AgentSessionStatusDto::Completed));
        assert_eq!(dto.final_session_id.as_deref(), Some("child-final"));
        assert_eq!(dto.summary.as_deref(), Some("done"));
        assert!(dto.phase.is_none());
    }

    #[test]
    fn message_mapping_preserves_wire_shape_and_compact_summary_semantics() {
        let regular = simple_text_message("Hello, how are you?");
        let regular = message_to_dto(&regular);
        assert_eq!(regular.role, MessageRoleDto::User);
        assert_eq!(regular.content, "Hello, how are you?");
        assert_eq!(regular.is_compact_summary, Some(false));
        assert_eq!(
            serde_json::to_value(&regular).unwrap(),
            serde_json::json!({
                "role": "user",
                "content": "Hello, how are you?",
                "is_compact_summary": false
            })
        );

        let compact =
            simple_text_message("<compact_summary>\nSummary:\nTest summary\n</compact_summary>");
        let compact = message_to_dto(&compact);
        assert_eq!(compact.role, MessageRoleDto::System);
        assert!(compact.content.contains("<compact_summary>"));
        assert_eq!(compact.is_compact_summary, Some(true));
        assert_eq!(
            serde_json::to_value(&compact).unwrap(),
            serde_json::json!({
                "role": "system",
                "content": "<compact_summary>\nSummary:\nTest summary\n</compact_summary>",
                "is_compact_summary": true
            })
        );

        let legacy: MessageDto = serde_json::from_value(serde_json::json!({
            "role": "system",
            "content": "Legacy system message"
        }))
        .unwrap();
        assert_eq!(legacy.is_compact_summary, None);
        assert!(!is_compact_summary_text(&legacy.content));
    }

    #[test]
    fn command_mapping_preserves_sources_at_each_transport_boundary() {
        let cases = [
            (CommandSource::Builtin, CommandSourceDto::Builtin),
            (CommandSource::Extension, CommandSourceDto::Extension),
            (CommandSource::Skill, CommandSourceDto::Skill),
        ];

        for (source, expected_source) in cases {
            let command = CommandInfo {
                name: "review".into(),
                description: "Review the current changes".into(),
                needs_argument: true,
                requires_idle: true,
                argument_completions: true,
                priority: 7,
                source,
            };

            let stdio = command_info_to_stdio_dto(command.clone());
            assert_eq!(stdio.name, command.name);
            assert_eq!(stdio.source, expected_source);

            let http = command_info_to_http_dto(command);
            assert_eq!(http.name, stdio.name);
            assert_eq!(http.description, stdio.description);
            assert_eq!(http.needs_argument, stdio.needs_argument);
            assert_eq!(http.requires_idle, stdio.requires_idle);
            assert_eq!(http.argument_completions, stdio.argument_completions);
            assert_eq!(http.priority, stdio.priority);
            assert_eq!(http.source, expected_source);
        }
    }

    fn simple_text_message(text: &str) -> LlmMessage {
        LlmMessage {
            role: LlmRole::User,
            content: vec![LlmContent::Text { text: text.into() }],
            name: None,
            reasoning_content: None,
        }
    }
}
