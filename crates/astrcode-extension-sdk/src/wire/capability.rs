use serde::{Deserialize, Deserializer, Serialize, Serializer};

macro_rules! extension_capabilities {
    ($($variant:ident => $wire:literal),* $(,)?) => {
        /// Host capabilities an extension may explicitly request.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum ExtensionCapability {
            $($variant),*
        }

        impl ExtensionCapability {
            /// manifest / Initialize 请求中的稳定 snake_case 线缆名。
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire),*
                }
            }

            /// 解析线缆名；未知名返回 `None`。
            pub fn parse(name: &str) -> Option<Self> {
                match name {
                    $($wire => Some(Self::$variant),)*
                    _ => None,
                }
            }

        }

        impl Serialize for ExtensionCapability {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for ExtensionCapability {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                const EXPECTED: &[&str] = &[$($wire),*];
                let name = String::deserialize(deserializer)?;
                Self::parse(&name)
                    .ok_or_else(|| serde::de::Error::unknown_variant(&name, EXPECTED))
            }
        }
    };
}

extension_capabilities! {
    SessionControl => "session_control",
    SessionCommand => "session_command",
    SessionInspect => "session_inspect",
    PublicHttp => "public_http",
    AuthenticatedHttp => "authenticated_http",
    PublicHttpDispatch => "public_http_dispatch",
    MainModel => "main_model",
    SmallModel => "small_model",
    SessionHistory => "session_history",
    EmitCustomEvents => "emit_custom_events",
    ConsumeCustomEvents => "consume_custom_events",
    WorkspaceRead => "workspace_read",
    WorkspaceWrite => "workspace_write",
    WorkspaceSensitivePaths => "workspace_sensitive_paths",
    ToolResultRead => "tool_result_read",
    ProcessSpawn => "process_spawn",
    NetworkClient => "network_client",
    ProviderRequest => "provider_request",
    InputDelivery => "input_delivery",
    ToolIntercept => "tool_intercept",
    TurnContinuationControl => "turn_continuation_control",
    LiveConversation => "live_conversation",
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_names_round_trip_and_wire_strings_are_stable() {
        let mut wires = std::collections::HashSet::new();
        for capability in [
            ExtensionCapability::SessionControl,
            ExtensionCapability::SessionCommand,
            ExtensionCapability::SessionInspect,
            ExtensionCapability::PublicHttp,
            ExtensionCapability::AuthenticatedHttp,
            ExtensionCapability::PublicHttpDispatch,
            ExtensionCapability::MainModel,
            ExtensionCapability::SmallModel,
            ExtensionCapability::SessionHistory,
            ExtensionCapability::EmitCustomEvents,
            ExtensionCapability::ConsumeCustomEvents,
            ExtensionCapability::WorkspaceRead,
            ExtensionCapability::WorkspaceWrite,
            ExtensionCapability::WorkspaceSensitivePaths,
            ExtensionCapability::ToolResultRead,
            ExtensionCapability::ProcessSpawn,
            ExtensionCapability::NetworkClient,
            ExtensionCapability::ProviderRequest,
            ExtensionCapability::InputDelivery,
            ExtensionCapability::ToolIntercept,
            ExtensionCapability::TurnContinuationControl,
            ExtensionCapability::LiveConversation,
        ] {
            let wire = capability.as_str();
            assert!(wires.insert(wire), "duplicate wire name {wire}");
            assert_eq!(ExtensionCapability::parse(wire), Some(capability));
            assert_eq!(
                serde_json::to_value(capability).unwrap(),
                serde_json::Value::String(wire.into()),
                "serde must serialize to the wire name"
            );
        }
        assert_eq!(ExtensionCapability::parse("future_capability"), None);
        assert!(
            serde_json::from_value::<ExtensionCapability>(serde_json::json!("future_capability"))
                .is_err()
        );
    }
}
