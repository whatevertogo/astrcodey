//! `astrcode.*` 能力与 [`ExtensionCapability`] 的映射。

use crate::extension::ExtensionCapability;

/// 将 enum 能力映射为 s5r 线缆名（`astrcode.*` 或 snake_case 请求名）。
pub fn astrcode_capability_name(cap: ExtensionCapability) -> &'static str {
    match cap {
        ExtensionCapability::SessionControl => "astrcode.session.control",
        ExtensionCapability::SessionInspect => "astrcode.session.inspect",
        ExtensionCapability::PublicHttp => "astrcode.extension.http.public_route",
        ExtensionCapability::PublicHttpDispatch => "astrcode.extension.http.public",
        ExtensionCapability::MainModel => "astrcode.llm.main_chat",
        ExtensionCapability::SmallModel => "astrcode.llm.small_chat",
        ExtensionCapability::SessionHistory => "astrcode.session.read_events",
        ExtensionCapability::EmitEvents => "astrcode.event.emit",
        ExtensionCapability::ConsumeEvents => "astrcode.event.consume",
        ExtensionCapability::WorkspaceRead => "astrcode.workspace.read",
        ExtensionCapability::WorkspaceWrite => "astrcode.workspace.write",
        ExtensionCapability::ProcessSpawn => "astrcode.process.spawn",
        ExtensionCapability::NetworkClient => "astrcode.network.client",
        ExtensionCapability::ProviderRequest => "astrcode.extension.provider_request",
        ExtensionCapability::InputDelivery => "astrcode.extension.input_delivery",
        ExtensionCapability::ToolIntercept => "astrcode.extension.tool_intercept",
        ExtensionCapability::TurnContinuationControl => {
            "astrcode.extension.turn_continuation_control"
        },
        ExtensionCapability::LiveConversation => "astrcode.extension.live_conversation",
    }
}

/// manifest / Initialize 请求中的 snake_case 名。
pub fn capability_to_wire(cap: ExtensionCapability) -> &'static str {
    match cap {
        ExtensionCapability::SessionControl => "session_control",
        ExtensionCapability::SessionInspect => "session_inspect",
        ExtensionCapability::PublicHttp => "public_http",
        ExtensionCapability::PublicHttpDispatch => "public_http_dispatch",
        ExtensionCapability::MainModel => "main_model",
        ExtensionCapability::SmallModel => "small_model",
        ExtensionCapability::SessionHistory => "session_history",
        ExtensionCapability::EmitEvents => "emit_events",
        ExtensionCapability::ConsumeEvents => "consume_events",
        ExtensionCapability::WorkspaceRead => "workspace_read",
        ExtensionCapability::WorkspaceWrite => "workspace_write",
        ExtensionCapability::ProcessSpawn => "process_spawn",
        ExtensionCapability::NetworkClient => "network_client",
        ExtensionCapability::ProviderRequest => "provider_request",
        ExtensionCapability::InputDelivery => "input_delivery",
        ExtensionCapability::ToolIntercept => "tool_intercept",
        ExtensionCapability::TurnContinuationControl => "turn_continuation_control",
        ExtensionCapability::LiveConversation => "live_conversation",
    }
}

pub fn capability_from_wire(name: &str) -> Option<ExtensionCapability> {
    match name {
        "session_control" => Some(ExtensionCapability::SessionControl),
        "session_inspect" => Some(ExtensionCapability::SessionInspect),
        "public_http" => Some(ExtensionCapability::PublicHttp),
        "public_http_dispatch" => Some(ExtensionCapability::PublicHttpDispatch),
        "main_model" => Some(ExtensionCapability::MainModel),
        "small_model" => Some(ExtensionCapability::SmallModel),
        "session_history" => Some(ExtensionCapability::SessionHistory),
        "emit_events" => Some(ExtensionCapability::EmitEvents),
        "consume_events" => Some(ExtensionCapability::ConsumeEvents),
        "workspace_read" => Some(ExtensionCapability::WorkspaceRead),
        "workspace_write" => Some(ExtensionCapability::WorkspaceWrite),
        "process_spawn" => Some(ExtensionCapability::ProcessSpawn),
        "network_client" => Some(ExtensionCapability::NetworkClient),
        "provider_request" => Some(ExtensionCapability::ProviderRequest),
        "input_delivery" => Some(ExtensionCapability::InputDelivery),
        "tool_intercept" => Some(ExtensionCapability::ToolIntercept),
        "turn_continuation_control" => Some(ExtensionCapability::TurnContinuationControl),
        "live_conversation" => Some(ExtensionCapability::LiveConversation),
        _ => None,
    }
}

pub fn is_astrcode_capability(name: &str) -> bool {
    name.starts_with("astrcode.")
}

pub fn is_reserved_capability_prefix(name: &str) -> bool {
    name.starts_with("handler.") || name.starts_with("astrcode.") || name.starts_with("internal.")
}

/// `astrcode.session.control` 子动作。
pub fn session_control_action(cap: &str) -> Option<&str> {
    cap.strip_prefix("astrcode.session.control.")
}
