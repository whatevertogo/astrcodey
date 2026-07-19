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

impl From<Phase> for PhaseDto {
    fn from(value: Phase) -> Self {
        match value {
            Phase::Idle => Self::Idle,
            Phase::Thinking => Self::Thinking,
            Phase::Streaming => Self::Streaming,
            Phase::CallingTool => Self::CallingTool,
            Phase::Compacting => Self::Compacting,
            Phase::Error => Self::Error,
        }
    }
}

impl From<PhaseDto> for Phase {
    fn from(value: PhaseDto) -> Self {
        match value {
            PhaseDto::Idle => Self::Idle,
            PhaseDto::Thinking => Self::Thinking,
            PhaseDto::Streaming => Self::Streaming,
            PhaseDto::CallingTool => Self::CallingTool,
            PhaseDto::Compacting => Self::Compacting,
            PhaseDto::Error => Self::Error,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutputStreamDto {
    Stdout,
    Stderr,
}

impl From<ToolOutputStream> for ToolOutputStreamDto {
    fn from(value: ToolOutputStream) -> Self {
        match value {
            ToolOutputStream::Stdout => Self::Stdout,
            ToolOutputStream::Stderr => Self::Stderr,
        }
    }
}

impl From<ToolOutputStreamDto> for ToolOutputStream {
    fn from(value: ToolOutputStreamDto) -> Self {
        match value {
            ToolOutputStreamDto::Stdout => Self::Stdout,
            ToolOutputStreamDto::Stderr => Self::Stderr,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecisionDto {
    AllowOnce,
    DenyOnce,
    AllowAlways,
    DenyAlways,
}

impl From<ApprovalDecision> for ApprovalDecisionDto {
    fn from(value: ApprovalDecision) -> Self {
        match value {
            ApprovalDecision::AllowOnce => Self::AllowOnce,
            ApprovalDecision::DenyOnce => Self::DenyOnce,
            ApprovalDecision::AllowAlways => Self::AllowAlways,
            ApprovalDecision::DenyAlways => Self::DenyAlways,
        }
    }
}

impl From<ApprovalDecisionDto> for ApprovalDecision {
    fn from(value: ApprovalDecisionDto) -> Self {
        match value {
            ApprovalDecisionDto::AllowOnce => Self::AllowOnce,
            ApprovalDecisionDto::DenyOnce => Self::DenyOnce,
            ApprovalDecisionDto::AllowAlways => Self::AllowAlways,
            ApprovalDecisionDto::DenyAlways => Self::DenyAlways,
        }
    }
}

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

impl From<ProviderWireFormat> for ProviderWireFormatDto {
    fn from(value: ProviderWireFormat) -> Self {
        match value {
            ProviderWireFormat::OpenAiChatCompletions => Self::OpenAiChatCompletions,
            ProviderWireFormat::OpenAiResponses => Self::OpenAiResponses,
            ProviderWireFormat::AnthropicMessages => Self::AnthropicMessages,
            ProviderWireFormat::GoogleGenAi => Self::GoogleGenAi,
        }
    }
}

impl From<ProviderWireFormatDto> for ProviderWireFormat {
    fn from(value: ProviderWireFormatDto) -> Self {
        match value {
            ProviderWireFormatDto::OpenAiChatCompletions => Self::OpenAiChatCompletions,
            ProviderWireFormatDto::OpenAiResponses => Self::OpenAiResponses,
            ProviderWireFormatDto::AnthropicMessages => Self::AnthropicMessages,
            ProviderWireFormatDto::GoogleGenAi => Self::GoogleGenAi,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAuthSchemeDto {
    None,
    Bearer,
    XApiKey,
    XGoogApiKey,
}

impl From<ProviderAuthScheme> for ProviderAuthSchemeDto {
    fn from(value: ProviderAuthScheme) -> Self {
        match value {
            ProviderAuthScheme::None => Self::None,
            ProviderAuthScheme::Bearer => Self::Bearer,
            ProviderAuthScheme::XApiKey => Self::XApiKey,
            ProviderAuthScheme::XGoogApiKey => Self::XGoogApiKey,
        }
    }
}

impl From<ProviderAuthSchemeDto> for ProviderAuthScheme {
    fn from(value: ProviderAuthSchemeDto) -> Self {
        match value {
            ProviderAuthSchemeDto::None => Self::None,
            ProviderAuthSchemeDto::Bearer => Self::Bearer,
            ProviderAuthSchemeDto::XApiKey => Self::XApiKey,
            ProviderAuthSchemeDto::XGoogApiKey => Self::XGoogApiKey,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevelDto {
    Low,
    Medium,
    High,
}

impl From<ThinkingLevel> for ThinkingLevelDto {
    fn from(value: ThinkingLevel) -> Self {
        match value {
            ThinkingLevel::Low => Self::Low,
            ThinkingLevel::Medium => Self::Medium,
            ThinkingLevel::High => Self::High,
        }
    }
}

impl From<ThinkingLevelDto> for ThinkingLevel {
    fn from(value: ThinkingLevelDto) -> Self {
        match value {
            ThinkingLevelDto::Low => Self::Low,
            ThinkingLevelDto::Medium => Self::Medium,
            ThinkingLevelDto::High => Self::High,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionStatusDto {
    #[default]
    Running,
    Completed,
    Failed,
}

impl From<AgentSessionStatus> for AgentSessionStatusDto {
    fn from(value: AgentSessionStatus) -> Self {
        match value {
            AgentSessionStatus::Running => Self::Running,
            AgentSessionStatus::Completed => Self::Completed,
            AgentSessionStatus::Failed => Self::Failed,
        }
    }
}

impl From<AgentSessionStatusDto> for AgentSessionStatus {
    fn from(value: AgentSessionStatusDto) -> Self {
        match value {
            AgentSessionStatusDto::Running => Self::Running,
            AgentSessionStatusDto::Completed => Self::Completed,
            AgentSessionStatusDto::Failed => Self::Failed,
        }
    }
}

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

impl From<ExtensionCapability> for ExtensionCapabilityDto {
    fn from(value: ExtensionCapability) -> Self {
        match value {
            ExtensionCapability::SessionControl => Self::SessionControl,
            ExtensionCapability::SessionInspect => Self::SessionInspect,
            ExtensionCapability::PublicHttp => Self::PublicHttp,
            ExtensionCapability::PublicHttpDispatch => Self::PublicHttpDispatch,
            ExtensionCapability::MainModel => Self::MainModel,
            ExtensionCapability::SmallModel => Self::SmallModel,
            ExtensionCapability::SessionHistory => Self::SessionHistory,
            ExtensionCapability::EmitEvents => Self::EmitEvents,
            ExtensionCapability::ConsumeEvents => Self::ConsumeEvents,
            ExtensionCapability::WorkspaceRead => Self::WorkspaceRead,
            ExtensionCapability::WorkspaceWrite => Self::WorkspaceWrite,
            ExtensionCapability::ProcessSpawn => Self::ProcessSpawn,
            ExtensionCapability::NetworkClient => Self::NetworkClient,
            ExtensionCapability::ProviderRequest => Self::ProviderRequest,
            ExtensionCapability::InputDelivery => Self::InputDelivery,
            ExtensionCapability::ToolIntercept => Self::ToolIntercept,
            ExtensionCapability::TurnContinuationControl => Self::TurnContinuationControl,
            ExtensionCapability::LiveConversation => Self::LiveConversation,
        }
    }
}

impl From<ExtensionCapabilityDto> for ExtensionCapability {
    fn from(value: ExtensionCapabilityDto) -> Self {
        match value {
            ExtensionCapabilityDto::SessionControl => Self::SessionControl,
            ExtensionCapabilityDto::SessionInspect => Self::SessionInspect,
            ExtensionCapabilityDto::PublicHttp => Self::PublicHttp,
            ExtensionCapabilityDto::PublicHttpDispatch => Self::PublicHttpDispatch,
            ExtensionCapabilityDto::MainModel => Self::MainModel,
            ExtensionCapabilityDto::SmallModel => Self::SmallModel,
            ExtensionCapabilityDto::SessionHistory => Self::SessionHistory,
            ExtensionCapabilityDto::EmitEvents => Self::EmitEvents,
            ExtensionCapabilityDto::ConsumeEvents => Self::ConsumeEvents,
            ExtensionCapabilityDto::WorkspaceRead => Self::WorkspaceRead,
            ExtensionCapabilityDto::WorkspaceWrite => Self::WorkspaceWrite,
            ExtensionCapabilityDto::ProcessSpawn => Self::ProcessSpawn,
            ExtensionCapabilityDto::NetworkClient => Self::NetworkClient,
            ExtensionCapabilityDto::ProviderRequest => Self::ProviderRequest,
            ExtensionCapabilityDto::InputDelivery => Self::InputDelivery,
            ExtensionCapabilityDto::ToolIntercept => Self::ToolIntercept,
            ExtensionCapabilityDto::TurnContinuationControl => Self::TurnContinuationControl,
            ExtensionCapabilityDto::LiveConversation => Self::LiveConversation,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOriginDto {
    Builtin,
    Bundled,
    Extension,
    Sdk,
}

impl From<ToolOrigin> for ToolOriginDto {
    fn from(value: ToolOrigin) -> Self {
        match value {
            ToolOrigin::Builtin => Self::Builtin,
            ToolOrigin::Bundled => Self::Bundled,
            ToolOrigin::Extension => Self::Extension,
            ToolOrigin::Sdk => Self::Sdk,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionModeDto {
    #[default]
    Sequential,
    Parallel,
}

impl From<ExecutionMode> for ExecutionModeDto {
    fn from(value: ExecutionMode) -> Self {
        match value {
            ExecutionMode::Sequential => Self::Sequential,
            ExecutionMode::Parallel => Self::Parallel,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn protocol_owned_enum_wire_values_are_stable() {
        assert_eq!(
            serde_json::to_value([
                PhaseDto::Idle,
                PhaseDto::Thinking,
                PhaseDto::Streaming,
                PhaseDto::CallingTool,
                PhaseDto::Compacting,
                PhaseDto::Error,
            ])
            .unwrap(),
            json!([
                "idle",
                "thinking",
                "streaming",
                "calling_tool",
                "compacting",
                "error"
            ])
        );
        assert_eq!(
            serde_json::to_value([
                ProviderWireFormatDto::OpenAiChatCompletions,
                ProviderWireFormatDto::OpenAiResponses,
                ProviderWireFormatDto::AnthropicMessages,
                ProviderWireFormatDto::GoogleGenAi,
            ])
            .unwrap(),
            json!([
                "openai_chat_completions",
                "openai_responses",
                "anthropic_messages",
                "google_genai"
            ])
        );
        assert_eq!(
            serde_json::to_value([
                ApprovalDecisionDto::AllowOnce,
                ApprovalDecisionDto::DenyOnce,
                ApprovalDecisionDto::AllowAlways,
                ApprovalDecisionDto::DenyAlways,
            ])
            .unwrap(),
            json!(["allow_once", "deny_once", "allow_always", "deny_always"])
        );
        assert_eq!(
            serde_json::to_value([ToolOutputStreamDto::Stdout, ToolOutputStreamDto::Stderr,])
                .unwrap(),
            json!(["stdout", "stderr"])
        );
        assert_eq!(
            serde_json::to_value([
                ProviderAuthSchemeDto::None,
                ProviderAuthSchemeDto::Bearer,
                ProviderAuthSchemeDto::XApiKey,
                ProviderAuthSchemeDto::XGoogApiKey,
            ])
            .unwrap(),
            json!(["none", "bearer", "x_api_key", "x_goog_api_key"])
        );
        assert_eq!(
            serde_json::to_value([
                ThinkingLevelDto::Low,
                ThinkingLevelDto::Medium,
                ThinkingLevelDto::High,
            ])
            .unwrap(),
            json!(["low", "medium", "high"])
        );
        assert_eq!(
            serde_json::to_value([
                AgentSessionStatusDto::Running,
                AgentSessionStatusDto::Completed,
                AgentSessionStatusDto::Failed,
            ])
            .unwrap(),
            json!(["running", "completed", "failed"])
        );
        assert_eq!(
            serde_json::to_value([
                ToolOriginDto::Builtin,
                ToolOriginDto::Bundled,
                ToolOriginDto::Extension,
                ToolOriginDto::Sdk,
            ])
            .unwrap(),
            json!(["builtin", "bundled", "extension", "sdk"])
        );
        assert_eq!(
            serde_json::to_value([ExecutionModeDto::Sequential, ExecutionModeDto::Parallel])
                .unwrap(),
            json!(["sequential", "parallel"])
        );
        assert_eq!(
            serde_json::to_value([
                ExtensionCapabilityDto::SessionControl,
                ExtensionCapabilityDto::SessionInspect,
                ExtensionCapabilityDto::PublicHttp,
                ExtensionCapabilityDto::PublicHttpDispatch,
                ExtensionCapabilityDto::MainModel,
                ExtensionCapabilityDto::SmallModel,
                ExtensionCapabilityDto::SessionHistory,
                ExtensionCapabilityDto::EmitEvents,
                ExtensionCapabilityDto::ConsumeEvents,
                ExtensionCapabilityDto::WorkspaceRead,
                ExtensionCapabilityDto::WorkspaceWrite,
                ExtensionCapabilityDto::ProcessSpawn,
                ExtensionCapabilityDto::NetworkClient,
                ExtensionCapabilityDto::ProviderRequest,
                ExtensionCapabilityDto::InputDelivery,
                ExtensionCapabilityDto::ToolIntercept,
                ExtensionCapabilityDto::TurnContinuationControl,
                ExtensionCapabilityDto::LiveConversation,
            ])
            .unwrap(),
            json!([
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
                "live_conversation"
            ])
        );
    }
}
