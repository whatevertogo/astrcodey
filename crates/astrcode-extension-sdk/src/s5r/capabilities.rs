//! `astrcode.*` 能力与 [`ExtensionCapability`] 的映射。

use crate::extension::ExtensionCapability;

/// 将 enum 能力映射为 s5r 线缆名（`astrcode.*` 或 snake_case 请求名）。
pub fn astrcode_capability_name(cap: ExtensionCapability) -> &'static str {
    match cap {
        ExtensionCapability::SessionControl => "astrcode.session.control",
        ExtensionCapability::MainModel => "astrcode.llm.main_chat",
        ExtensionCapability::SmallModel => "astrcode.llm.small_chat",
        ExtensionCapability::SessionHistory => "astrcode.session.read_events",
        ExtensionCapability::EmitEvents => "astrcode.event.emit",
        ExtensionCapability::WorkspaceRead => "astrcode.workspace.read",
        ExtensionCapability::ProcessSpawn => "astrcode.process.spawn",
        ExtensionCapability::NetworkClient => "astrcode.network.client",
    }
}

/// manifest / Initialize 请求中的 snake_case 名。
pub fn capability_to_wire(cap: ExtensionCapability) -> &'static str {
    match cap {
        ExtensionCapability::SessionControl => "session_control",
        ExtensionCapability::MainModel => "main_model",
        ExtensionCapability::SmallModel => "small_model",
        ExtensionCapability::SessionHistory => "session_history",
        ExtensionCapability::EmitEvents => "emit_events",
        ExtensionCapability::WorkspaceRead => "workspace_read",
        ExtensionCapability::ProcessSpawn => "process_spawn",
        ExtensionCapability::NetworkClient => "network_client",
    }
}

pub fn capability_from_wire(name: &str) -> Option<ExtensionCapability> {
    match name {
        "session_control" => Some(ExtensionCapability::SessionControl),
        "main_model" => Some(ExtensionCapability::MainModel),
        "small_model" => Some(ExtensionCapability::SmallModel),
        "session_history" => Some(ExtensionCapability::SessionHistory),
        "emit_events" => Some(ExtensionCapability::EmitEvents),
        "workspace_read" => Some(ExtensionCapability::WorkspaceRead),
        "process_spawn" => Some(ExtensionCapability::ProcessSpawn),
        "network_client" => Some(ExtensionCapability::NetworkClient),
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
