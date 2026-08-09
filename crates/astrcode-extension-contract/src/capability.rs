use serde::{Deserialize, Deserializer, Serialize, Serializer};

macro_rules! extension_capabilities {
    ($($variant:ident { wire: $wire:literal, grant: $grant:literal }),* $(,)?) => {
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

            /// 解析线缆名；未知名返回 `None`（旧扩展/未来能力由调用方透传）。
            pub fn parse(name: &str) -> Option<Self> {
                match name {
                    $($wire => Some(Self::$variant),)*
                    _ => None,
                }
            }

            /// 授权目录名（`astrcode.*` 能力前缀）。
            pub const fn grant_name(self) -> &'static str {
                match self {
                    $(Self::$variant => $grant),*
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
    SessionControl { wire: "session_control", grant: "astrcode.session.control" },
    SessionInspect { wire: "session_inspect", grant: "astrcode.session.inspect" },
    PublicHttp { wire: "public_http", grant: "astrcode.extension.http.public_route" },
    AuthenticatedHttp { wire: "authenticated_http", grant: "astrcode.extension.http.authenticated_route" },
    PublicHttpDispatch { wire: "public_http_dispatch", grant: "astrcode.extension.http.public" },
    MainModel { wire: "main_model", grant: "astrcode.llm.main_chat" },
    SmallModel { wire: "small_model", grant: "astrcode.llm.small_chat" },
    SessionHistory { wire: "session_history", grant: "astrcode.session.read_events" },
    EmitCustomEvents { wire: "emit_custom_events", grant: "astrcode.event.emit" },
    ConsumeCustomEvents { wire: "consume_custom_events", grant: "astrcode.event.consume" },
    WorkspaceRead { wire: "workspace_read", grant: "astrcode.workspace.read" },
    WorkspaceWrite { wire: "workspace_write", grant: "astrcode.workspace.write" },
    ProcessSpawn { wire: "process_spawn", grant: "astrcode.process.spawn" },
    NetworkClient { wire: "network_client", grant: "astrcode.network.client" },
    ProviderRequest { wire: "provider_request", grant: "astrcode.extension.provider_request" },
    InputDelivery { wire: "input_delivery", grant: "astrcode.extension.input_delivery" },
    ToolIntercept { wire: "tool_intercept", grant: "astrcode.extension.tool_intercept" },
    TurnContinuationControl { wire: "turn_continuation_control", grant: "astrcode.extension.turn_continuation_control" },
    LiveConversation { wire: "live_conversation", grant: "astrcode.extension.live_conversation" },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_names_round_trip_and_wire_strings_are_stable() {
        let mut wires = std::collections::HashSet::new();
        for capability in [
            ExtensionCapability::SessionControl,
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
            assert!(capability.grant_name().starts_with("astrcode."));
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
