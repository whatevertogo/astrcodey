//! Protocol-owned enum types used by public wire contracts.
//!
//! Core enums describe internal domain state. These DTO enums freeze the JSON
//! representation independently so internal variants can evolve without
//! silently changing HTTP, SSE, or JSON-RPC contracts.

use astrcode_core::{
    config::{ProviderAuthScheme, ProviderWireFormat},
    event::{Phase, ToolOutputStream},
    llm::{LlmRole, ThinkingLevel},
    permission::{ApprovalDecision, ApprovalMode},
    tool::{ExecutionMode, ToolOrigin},
};
use serde::{Deserialize, Serialize};

macro_rules! impl_wire_values {
    ($wire:ty { $($variant:ident),+ $(,)? }) => {
        impl $wire {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
        }
    };
}

pub(crate) use impl_wire_values;

macro_rules! impl_domain_to_wire_conversion {
    ($domain:ty => $wire:ty { $($variant:ident),+ $(,)? }) => {
        impl_wire_values!($wire { $($variant),+ });

        impl From<$domain> for $wire {
            fn from(value: $domain) -> Self {
                match value {
                    $(<$domain>::$variant => Self::$variant,)+
                }
            }
        }
    };
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandAvailabilityDto {
    AllTransports,
    InteractiveOnly,
}

impl_wire_values!(CommandAvailabilityDto {
    AllTransports,
    InteractiveOnly,
});

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionCommandKindDto {
    CompactSession,
    SelectModel,
}

impl_wire_values!(SessionCommandKindDto {
    CompactSession,
    SelectModel,
});

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "command", rename_all = "snake_case")]
pub enum CommandExecutionDto {
    Extension,
    Host(SessionCommandKindDto),
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionSourceDto {
    Builtin,
    Disk,
    Unknown,
}

impl_wire_values!(ExtensionSourceDto {
    Builtin,
    Disk,
    Unknown,
});

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionStageStatusDto {
    Unknown,
    Running,
    Succeeded,
    Failed,
    Skipped,
}

impl_wire_values!(ExtensionStageStatusDto {
    Unknown,
    Running,
    Succeeded,
    Failed,
    Skipped,
});

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRoleDto {
    System,
    User,
    Assistant,
    Tool,
}

impl_domain_to_wire_conversion!(LlmRole => MessageRoleDto {
    System,
    User,
    Assistant,
    Tool,
});

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ExtensionHttpMethodDto {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl_wire_values!(ExtensionHttpMethodDto {
    Get,
    Post,
    Put,
    Patch,
    Delete,
});

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseDto {
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
#[serde(rename_all = "snake_case")]
pub enum ApprovalModeDto {
    Manual,
    Yolo,
}

impl_bidirectional_wire_conversion!(ApprovalMode => ApprovalModeDto { Manual, Yolo });

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderWireFormatDto {
    #[serde(rename = "openai_chat_completions")]
    OpenAiChatCompletions,
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
}

impl_bidirectional_wire_conversion!(ProviderWireFormat => ProviderWireFormatDto {
    OpenAiChatCompletions,
    OpenAiResponses,
    AnthropicMessages,
});

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAuthSchemeDto {
    None,
    Bearer,
    XApiKey,
}

impl_bidirectional_wire_conversion!(ProviderAuthScheme => ProviderAuthSchemeDto {
    None,
    Bearer,
    XApiKey,
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

/// User-facing thinking capability. Provider wire encoding remains an internal
/// config/provider concern and is intentionally omitted from this DTO.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingCapabilityDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_effort: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_max: Option<u32>,
    pub can_disable: bool,
}

impl From<astrcode_core::llm::thinking::ThinkingCapability> for ThinkingCapabilityDto {
    fn from(value: astrcode_core::llm::thinking::ThinkingCapability) -> Self {
        Self {
            allowed_effort: value.allowed_effort,
            budget_min: value.budget_min,
            budget_max: value.budget_max,
            can_disable: value.can_disable,
        }
    }
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionStatusDto {
    Running,
    Completed,
    Failed,
}

impl_wire_values!(AgentSessionStatusDto {
    Running,
    Completed,
    Failed,
});

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionCapabilityDto {
    SessionControl,
    SessionCommand,
    SessionInspect,
    PublicHttp,
    AuthenticatedHttp,
    PublicHttpDispatch,
    MainModel,
    SmallModel,
    SessionHistory,
    EmitCustomEvents,
    ConsumeCustomEvents,
    WorkspaceRead,
    WorkspaceWrite,
    WorkspaceSensitivePaths,
    ToolResultRead,
    ProcessSpawn,
    NetworkClient,
    ProviderRequest,
    InputDelivery,
    ToolIntercept,
    TurnContinuationControl,
    LiveConversation,
}

impl_wire_values!(ExtensionCapabilityDto {
    SessionControl,
    SessionCommand,
    SessionInspect,
    PublicHttp,
    AuthenticatedHttp,
    PublicHttpDispatch,
    MainModel,
    SmallModel,
    SessionHistory,
    EmitCustomEvents,
    ConsumeCustomEvents,
    WorkspaceRead,
    WorkspaceWrite,
    WorkspaceSensitivePaths,
    ToolResultRead,
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
    Bundled,
    Extension,
}

impl_domain_to_wire_conversion!(ToolOrigin => ToolOriginDto {
    Bundled,
    Extension,
});

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionModeDto {
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
            ],
        );
        assert_wire_values(
            ApprovalDecisionDto::ALL,
            &["allow_once", "deny_once", "allow_always", "deny_always"],
        );
        assert_wire_values(ApprovalModeDto::ALL, &["manual", "yolo"]);
        assert!(serde_json::from_str::<ApprovalModeDto>(r#""future_mode""#).is_err());
        assert_wire_values(
            CommandAvailabilityDto::ALL,
            &["all_transports", "interactive_only"],
        );
        assert_wire_values(
            SessionCommandKindDto::ALL,
            &["compact_session", "select_model"],
        );
        assert_eq!(
            serde_json::to_value(CommandExecutionDto::Host(
                SessionCommandKindDto::CompactSession
            ))
            .unwrap(),
            serde_json::json!({"kind": "host", "command": "compact_session"})
        );
        assert_wire_values(ExtensionSourceDto::ALL, &["builtin", "disk", "unknown"]);
        assert_wire_values(
            ExtensionStageStatusDto::ALL,
            &["unknown", "running", "succeeded", "failed", "skipped"],
        );
        assert_wire_values(
            MessageRoleDto::ALL,
            &["system", "user", "assistant", "tool"],
        );
        assert_wire_values(
            ExtensionHttpMethodDto::ALL,
            &["GET", "POST", "PUT", "PATCH", "DELETE"],
        );
        assert_wire_values(ToolOutputStreamDto::ALL, &["stdout", "stderr"]);
        assert_wire_values(ProviderAuthSchemeDto::ALL, &["none", "bearer", "x_api_key"]);
        assert_wire_values(ThinkingLevelDto::ALL, &["low", "medium", "high"]);
        assert_wire_values(
            AgentSessionStatusDto::ALL,
            &["running", "completed", "failed"],
        );
        assert_wire_values(ToolOriginDto::ALL, &["bundled", "extension"]);
        assert_wire_values(ExecutionModeDto::ALL, &["sequential", "parallel"]);
        assert_wire_values(
            ExtensionCapabilityDto::ALL,
            &[
                "session_control",
                "session_command",
                "session_inspect",
                "public_http",
                "authenticated_http",
                "public_http_dispatch",
                "main_model",
                "small_model",
                "session_history",
                "emit_custom_events",
                "consume_custom_events",
                "workspace_read",
                "workspace_write",
                "workspace_sensitive_paths",
                "tool_result_read",
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
