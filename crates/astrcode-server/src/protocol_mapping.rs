//! Internal domain and extension types mapped at the server protocol boundary.

use astrcode_extension_sdk::extension::{
    ExtensionCapability, ExtensionEventDecl, ExtensionHttpMethod, Keybinding, SlashCommand,
    StatusItem,
};
use astrcode_protocol::{
    agent_session_link::{AgentSessionLinkDto, AgentSessionStatusDto},
    events::{KeybindingDto, StatusItemInfoDto},
    http::{ExtensionEventDeclDto, ExtensionSlashCommandDto, StatusItemDto},
    wire::{ExtensionCapabilityDto, ExtensionHttpMethodDto},
};
use astrcode_session_projection::{AgentSessionLinkView, AgentSessionStatus};

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
    use astrcode_core::types::{SessionId, ToolCallId};

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
}
