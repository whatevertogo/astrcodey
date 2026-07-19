//! Protocol-owned enum types used by public wire contracts.
//!
//! Core enums describe internal domain state. These DTO enums freeze the JSON
//! representation independently so internal variants can evolve without
//! silently changing HTTP, SSE, or JSON-RPC contracts.

use astrcode_core::{
    config::{ProviderAuthScheme, ProviderWireFormat},
    event::{Phase, ToolOutputStream},
    extension::ExtensionCapability,
    llm::ThinkingLevel,
    permission::ApprovalDecision,
    storage::AgentSessionStatus,
    tool::{ExecutionMode, ToolOrigin},
};
use serde::{Deserialize, Serialize};

macro_rules! impl_domain_to_wire_conversion {
    ($domain:ty => $wire:ty { $($variant:ident),+ $(,)? }) => {
        impl $wire {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
        }

        impl From<$domain> for $wire {
            fn from(value: $domain) -> Self {
                match value {
                    $(<$domain>::$variant => Self::$variant,)+
                }
            }
        }
    };
}

macro_rules! impl_bidirectional_wire_conversion {
    ($domain:ty => $wire:ty { $($variant:ident),+ $(,)? }) => {
        impl_domain_to_wire_conversion!($domain => $wire { $($variant),+ });

        impl From<$wire> for $domain {
            fn from(value: $wire) -> Self {
                match value {
                    $(<$wire>::$variant => Self::$variant,)+
                }
            }
        }
    };
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseDto {
    #[default]
    Idle,
    Thinking,
    Streaming,
    CallingTool,
    Compacting,
    Error,
}

impl_bidirectional_wire_conversion!(Phase => PhaseDto {
    Idle,
    Thinking,
    Streaming,
    CallingTool,
    Compacting,
    Error,
});

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutputStreamDto {
    Stdout,
    Stderr,
}

impl_bidirectional_wire_conversion!(ToolOutputStream => ToolOutputStreamDto { Stdout, Stderr });

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecisionDto {
    AllowOnce,
    DenyOnce,
    AllowAlways,
    DenyAlways,
}

impl_bidirectional_wire_conversion!(ApprovalDecision => ApprovalDecisionDto {
    AllowOnce,
    DenyOnce,
    AllowAlways,
    DenyAlways,
});

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderWireFormatDto {
    #[serde(rename = "openai_chat_completions")]
    OpenAiChatCompletions,
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
    #[serde(rename = "google_genai")]
    GoogleGenAi,
}

impl_bidirectional_wire_conversion!(ProviderWireFormat => ProviderWireFormatDto {
    OpenAiChatCompletions,
    OpenAiResponses,
    AnthropicMessages,
    GoogleGenAi,
});

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAuthSchemeDto {
    None,
    Bearer,
    XApiKey,
    XGoogApiKey,
}

impl_bidirectional_wire_conversion!(ProviderAuthScheme => ProviderAuthSchemeDto {
    None,
    Bearer,
    XApiKey,
    XGoogApiKey,
});

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevelDto {
    Low,
    Medium,
    High,
}

impl_bidirectional_wire_conversion!(ThinkingLevel => ThinkingLevelDto { Low, Medium, High });

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionStatusDto {
    #[default]
    Running,
    Completed,
    Failed,
}

impl_bidirectional_wire_conversion!(AgentSessionStatus => AgentSessionStatusDto {
    Running,
    Completed,
    Failed,
});

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionCapabilityDto {
    SessionControl,
    SessionInspect,
    PublicHttp,
    PublicHttpDispatch,
    MainModel,
    SmallModel,
    SessionHistory,
    EmitEvents,
    ConsumeEvents,
    WorkspaceRead,
    WorkspaceWrite,
    ProcessSpawn,
    NetworkClient,
    ProviderRequest,
    InputDelivery,
    ToolIntercept,
    TurnContinuationControl,
    LiveConversation,
}

impl_bidirectional_wire_conversion!(ExtensionCapability => ExtensionCapabilityDto {
    SessionControl,
    SessionInspect,
    PublicHttp,
    PublicHttpDispatch,
    MainModel,
    SmallModel,
    SessionHistory,
    EmitEvents,
    ConsumeEvents,
    WorkspaceRead,
    WorkspaceWrite,
    ProcessSpawn,
    NetworkClient,
    ProviderRequest,
    InputDelivery,
    ToolIntercept,
    TurnContinuationControl,
    LiveConversation,
});

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOriginDto {
    Builtin,
    Bundled,
    Extension,
    Sdk,
}

impl_domain_to_wire_conversion!(ToolOrigin => ToolOriginDto {
    Builtin,
    Bundled,
    Extension,
    Sdk,
});

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionModeDto {
    #[default]
    Sequential,
    Parallel,
}

impl_domain_to_wire_conversion!(ExecutionMode => ExecutionModeDto { Sequential, Parallel });

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_wire_values<T: Serialize>(values: &[T], expected: &[&str]) {
        assert_eq!(
            serde_json::to_value(values).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }

    #[test]
    fn protocol_owned_enum_wire_values_are_stable() {
        assert_wire_values(
            PhaseDto::ALL,
            &[
                "idle",
                "thinking",
                "streaming",
                "calling_tool",
                "compacting",
                "error",
            ],
        );
        assert_wire_values(
            ProviderWireFormatDto::ALL,
            &[
                "openai_chat_completions",
                "openai_responses",
                "anthropic_messages",
                "google_genai",
            ],
        );
        assert_wire_values(
            ApprovalDecisionDto::ALL,
            &["allow_once", "deny_once", "allow_always", "deny_always"],
        );
        assert_wire_values(ToolOutputStreamDto::ALL, &["stdout", "stderr"]);
        assert_wire_values(
            ProviderAuthSchemeDto::ALL,
            &["none", "bearer", "x_api_key", "x_goog_api_key"],
        );
        assert_wire_values(ThinkingLevelDto::ALL, &["low", "medium", "high"]);
        assert_wire_values(
            AgentSessionStatusDto::ALL,
            &["running", "completed", "failed"],
        );
        assert_wire_values(
            ToolOriginDto::ALL,
            &["builtin", "bundled", "extension", "sdk"],
        );
        assert_wire_values(ExecutionModeDto::ALL, &["sequential", "parallel"]);
        assert_wire_values(
            ExtensionCapabilityDto::ALL,
            &[
                "session_control",
                "session_inspect",
                "public_http",
                "public_http_dispatch",
                "main_model",
                "small_model",
                "session_history",
                "emit_events",
                "consume_events",
                "workspace_read",
                "workspace_write",
                "process_spawn",
                "network_client",
                "provider_request",
                "input_delivery",
                "tool_intercept",
                "turn_continuation_control",
                "live_conversation",
            ],
        );
    }
}
