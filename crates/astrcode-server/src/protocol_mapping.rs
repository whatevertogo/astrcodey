//! Internal domain and extension types mapped at the server protocol boundary.

use astrcode_context::is_compact_summary_message;
use astrcode_core::llm::{LlmContent, LlmMessage};
use astrcode_extension_sdk::extension::{
    CustomEventDeclaration, CustomEventSourceFilter, CustomEventSubscription, ExtensionCapability,
    ExtensionHttpMethod, Keybinding, SlashCommand, StatusItem, TransportFeature,
};
use astrcode_protocol::{
    agent_session_link::{AgentSessionLinkDto, AgentSessionStatusDto},
    events::{
        ExtensionCommandInfoDto, KeybindingDto, MessageDto, SessionSnapshot, StatusItemInfoDto,
    },
    http::{
        CustomEventDeclarationDto, CustomEventDeliveryDto, CustomEventSourceFilterDto,
        CustomEventSubscriptionDto, ExtensionSlashCommandDto, SlashCommandInfoDto, StatusItemDto,
        TransportFeatureDto,
    },
    wire::{
        CommandAvailabilityDto, CommandExecutionDto, ExtensionCapabilityDto,
        ExtensionHttpMethodDto, MessageRoleDto, SessionCommandKindDto,
    },
};
use astrcode_session_projection::{AgentSessionLinkView, AgentSessionStatus, SessionReadModel};

use crate::session_command_contract::CommandInfo;

pub(crate) fn agent_session_link_to_dto(link: &AgentSessionLinkView) -> AgentSessionLinkDto {
    AgentSessionLinkDto {
        child_session_id: link.child_session_id.to_string(),
        tool_call_id: link.tool_call_id.as_ref().map(ToString::to_string),
        agent_name: link.agent_name.clone(),
        task: link.task.clone(),
        status: agent_session_status_to_dto(link.status),
        final_session_id: link.final_session_id.as_ref().map(ToString::to_string),
        summary: link.summary.clone(),
        error: link.error.clone(),
    }
}

pub(crate) fn session_snapshot(state: &SessionReadModel) -> SessionSnapshot {
    SessionSnapshot {
        session_id: state.identity.session_id.to_string(),
        cursor: state.cursor(),
        messages: state
            .model_context
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
        is_compact_summary,
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
        ExtensionCapability::SessionCommand => ExtensionCapabilityDto::SessionCommand,
        ExtensionCapability::SessionInspect => ExtensionCapabilityDto::SessionInspect,
        ExtensionCapability::PublicHttp => ExtensionCapabilityDto::PublicHttp,
        ExtensionCapability::AuthenticatedHttp => ExtensionCapabilityDto::AuthenticatedHttp,
        ExtensionCapability::PublicHttpDispatch => ExtensionCapabilityDto::PublicHttpDispatch,
        ExtensionCapability::MainModel => ExtensionCapabilityDto::MainModel,
        ExtensionCapability::SmallModel => ExtensionCapabilityDto::SmallModel,
        ExtensionCapability::SessionHistory => ExtensionCapabilityDto::SessionHistory,
        ExtensionCapability::EmitCustomEvents => ExtensionCapabilityDto::EmitCustomEvents,
        ExtensionCapability::ConsumeCustomEvents => ExtensionCapabilityDto::ConsumeCustomEvents,
        ExtensionCapability::WorkspaceRead => ExtensionCapabilityDto::WorkspaceRead,
        ExtensionCapability::WorkspaceWrite => ExtensionCapabilityDto::WorkspaceWrite,
        ExtensionCapability::ToolResultRead => ExtensionCapabilityDto::ToolResultRead,
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

pub(crate) fn transport_feature_to_dto(feature: TransportFeature) -> TransportFeatureDto {
    match feature {
        TransportFeature::AuthenticatedHttp => TransportFeatureDto::AuthenticatedHttp,
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
        extension_id: command.extension_id,
        description: command.description,
        needs_argument: command.needs_argument,
        requires_idle: command.requires_idle,
        argument_completions: command.argument_completions,
        priority: command.priority,
    }
}

pub(crate) fn command_info_to_http_dto(command: CommandInfo) -> SlashCommandInfoDto {
    command_info_to_stdio_dto(command).into()
}

pub(crate) fn extension_slash_command_to_dto(command: SlashCommand) -> ExtensionSlashCommandDto {
    ExtensionSlashCommandDto {
        name: command.name,
        description: command.description,
        args_schema: command.args_schema,
        requires_idle: command.requires_idle,
        argument_completions: command.argument_completions,
        priority: command.priority,
        availability: match command.availability {
            astrcode_extension_sdk::extension::CommandAvailability::AllTransports => {
                CommandAvailabilityDto::AllTransports
            },
            astrcode_extension_sdk::extension::CommandAvailability::InteractiveOnly => {
                CommandAvailabilityDto::InteractiveOnly
            },
        },
        execution: match command.execution {
            astrcode_extension_sdk::extension::CommandExecution::Extension => {
                CommandExecutionDto::Extension
            },
            astrcode_extension_sdk::extension::CommandExecution::Host(command) => {
                CommandExecutionDto::Host(match command {
                    astrcode_extension_sdk::extension::SessionCommandKind::CompactSession => {
                        SessionCommandKindDto::CompactSession
                    },
                    astrcode_extension_sdk::extension::SessionCommandKind::SelectModel => {
                        SessionCommandKindDto::SelectModel
                    },
                })
            },
        },
    }
}

pub(crate) fn custom_event_declaration_to_dto(
    event: CustomEventDeclaration,
) -> CustomEventDeclarationDto {
    CustomEventDeclarationDto {
        event_type: event.event_type,
        schema_version: event.schema_version,
        delivery: match event.delivery {
            astrcode_extension_sdk::extension::CustomEventDelivery::SessionDurable => {
                CustomEventDeliveryDto::SessionDurable
            },
            astrcode_extension_sdk::extension::CustomEventDelivery::SessionLive => {
                CustomEventDeliveryDto::SessionLive
            },
            astrcode_extension_sdk::extension::CustomEventDelivery::GlobalLive => {
                CustomEventDeliveryDto::GlobalLive
            },
        },
        max_payload_bytes: event.max_payload_bytes,
    }
}

pub(crate) fn custom_event_subscription_to_dto(
    subscription: CustomEventSubscription,
) -> CustomEventSubscriptionDto {
    CustomEventSubscriptionDto {
        id: subscription.id,
        event_type: subscription.event_type,
        source: match subscription.source {
            CustomEventSourceFilter::Any => CustomEventSourceFilterDto::Any,
            CustomEventSourceFilter::Extension { extension_id } => {
                CustomEventSourceFilterDto::Extension { extension_id }
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        llm::{LlmContent, LlmMessage, LlmRole},
        types::{SessionId, ToolCallId},
    };

    use super::*;

    #[test]
    fn agent_session_mapping_preserves_snapshot_fields() {
        let link = AgentSessionLinkView {
            child_session_id: SessionId::from("child-1"),
            tool_call_id: Some(ToolCallId::from("tool-1")),
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
        assert_eq!(dto.status, AgentSessionStatusDto::Completed);
        assert_eq!(dto.final_session_id.as_deref(), Some("child-final"));
        assert_eq!(dto.summary.as_deref(), Some("done"));
    }

    #[test]
    fn message_mapping_preserves_wire_shape_and_compact_summary_semantics() {
        let regular = simple_text_message("Hello, how are you?");
        let regular = message_to_dto(&regular);
        assert_eq!(regular.role, MessageRoleDto::User);
        assert_eq!(regular.content, "Hello, how are you?");
        assert!(!regular.is_compact_summary);
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
        assert!(compact.is_compact_summary);
        assert_eq!(
            serde_json::to_value(&compact).unwrap(),
            serde_json::json!({
                "role": "system",
                "content": "<compact_summary>\nSummary:\nTest summary\n</compact_summary>",
                "is_compact_summary": true
            })
        );
    }

    #[test]
    fn command_mapping_preserves_extension_identity_at_each_transport_boundary() {
        let command = CommandInfo {
            name: "review".into(),
            extension_id: "review-extension".into(),
            description: "Review the current changes".into(),
            needs_argument: true,
            requires_idle: true,
            argument_completions: true,
            priority: 7,
        };

        let stdio = command_info_to_stdio_dto(command.clone());
        assert_eq!(stdio.name, command.name);
        assert_eq!(stdio.extension_id, command.extension_id);

        let http = command_info_to_http_dto(command);
        assert_eq!(http.name, stdio.name);
        assert_eq!(http.extension_id, stdio.extension_id);
        assert_eq!(http.description, stdio.description);
        assert_eq!(http.needs_argument, stdio.needs_argument);
        assert_eq!(http.requires_idle, stdio.requires_idle);
        assert_eq!(http.argument_completions, stdio.argument_completions);
        assert_eq!(http.priority, stdio.priority);
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
