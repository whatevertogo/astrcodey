//! HTTP API 数据传输对象（DTO）模块
//!
//! 本模块定义了 server 与前端之间通过 HTTP/SSE 通信的所有请求/响应数据结构。
//! 所有 DTO 使用 serde 进行序列化，字段采用 camelCase 命名以匹配前端约定。
//!
//! ## 子模块划分
//!
//! - `auth`: 认证相关 DTO（bootstrap token 交换 session token）
//! - `composer`: 输入框候选列表 DTO
//! - `config`: 配置查看、保存、连接测试相关 DTO
//! - `event`: Agent 事件流 DTO，用于 SSE 实时推送和会话回放
//! - `model`: 模型信息 DTO
//! - `runtime`: 运行时状态、指标、插件健康度 DTO
//! - `session`: 会话管理 DTO（创建、列表、提示词提交）
//! - `session_event`: 会话目录事件 DTO（创建/删除/分支通知）

mod agent;
mod auth;
mod composer;
mod config;
pub mod conversation;
mod event;
mod model;
mod runtime;
mod session;
mod session_event;
pub mod terminal;
mod tool;

pub use agent::{
    AgentExecuteRequestDto, AgentExecuteResponseDto, AgentLifecycleDto, AgentProfileDto,
    AgentTurnOutcomeDto, ChildAgentRefDto, ChildSessionLineageKindDto,
    ChildSessionNotificationKindDto, LineageSnapshotDto, SubRunStatusDto, SubRunStatusSourceDto,
    SubagentContextOverridesDto,
};
pub use auth::{AuthExchangeRequest, AuthExchangeResponse};
pub use composer::{
    ComposerOptionActionKindDto, ComposerOptionDto, ComposerOptionKindDto,
    ComposerOptionsResponseDto,
};
pub use config::{
    ConfigReloadResponse, ConfigView, ProfileView, SaveActiveSelectionRequest,
    TestConnectionRequest, TestResultDto,
};
pub use conversation::v1::{
    ConversationAssistantBlockDto, ConversationBannerDto, ConversationBannerErrorCodeDto,
    ConversationBlockDto, ConversationBlockPatchDto, ConversationBlockStatusDto,
    ConversationChildHandoffBlockDto, ConversationChildHandoffKindDto, ConversationChildSummaryDto,
    ConversationControlStateDto, ConversationCursorDto, ConversationDeltaDto,
    ConversationErrorBlockDto, ConversationErrorEnvelopeDto, ConversationLastCompactMetaDto,
    ConversationPlanBlockDto, ConversationPlanBlockersDto, ConversationPlanEventKindDto,
    ConversationPlanReferenceDto, ConversationPlanReviewDto, ConversationPlanReviewKindDto,
    ConversationSlashActionKindDto, ConversationSlashCandidateDto,
    ConversationSlashCandidatesResponseDto, ConversationSnapshotResponseDto,
    ConversationStepCursorDto, ConversationStepProgressDto, ConversationStreamEnvelopeDto,
    ConversationSystemNoteBlockDto, ConversationSystemNoteKindDto, ConversationTaskItemDto,
    ConversationTaskStatusDto, ConversationThinkingBlockDto, ConversationToolCallBlockDto,
    ConversationToolStreamsDto, ConversationTranscriptErrorCodeDto, ConversationUserBlockDto,
};
pub use event::{
    ArtifactRefDto, CloseRequestParentDeliveryPayloadDto, CompletedParentDeliveryPayloadDto,
    ExecutionControlDto, FailedParentDeliveryPayloadDto, ForkModeDto, PROTOCOL_VERSION,
    ParentDeliveryDto, ParentDeliveryOriginDto, ParentDeliveryPayloadDto,
    ParentDeliveryTerminalSemanticsDto, PhaseDto, ProgressParentDeliveryPayloadDto,
    ResolvedExecutionLimitsDto, ResolvedSubagentContextOverridesDto, SubRunFailureCodeDto,
    SubRunFailureDto, SubRunHandoffDto, SubRunOutcomeDto, SubRunResultDto, SubRunStorageModeDto,
    ToolOutputStreamDto,
};
pub use model::{CurrentModelInfoDto, ModelOptionDto};
pub use runtime::{
    AgentCollaborationScorecardDto, ExecutionDiagnosticsDto, OperationMetricsDto, PluginHealthDto,
    PluginRuntimeStateDto, ReplayMetricsDto, RuntimeCapabilityDto, RuntimeMetricsDto,
    RuntimePluginDto, RuntimeReloadResponseDto, RuntimeStatusDto, SubRunExecutionMetricsDto,
};
pub use session::{
    CompactSessionRequest, CompactSessionResponse, CreateSessionRequest, DeleteProjectResultDto,
    ForkSessionRequest, ModeSummaryDto, PromptRequest, PromptSkillInvocation, PromptSubmitResponse,
    SessionListItem, SessionModeStateDto, SwitchModeRequest,
};
pub use session_event::{SessionCatalogEventEnvelope, SessionCatalogEventPayload};
pub use terminal::v1::{
    TerminalAssistantBlockDto, TerminalBannerDto, TerminalBannerErrorCodeDto, TerminalBlockDto,
    TerminalBlockPatchDto, TerminalBlockStatusDto, TerminalChildHandoffBlockDto,
    TerminalChildHandoffKindDto, TerminalChildSummaryDto, TerminalControlStateDto,
    TerminalCursorDto, TerminalDeltaDto, TerminalErrorBlockDto, TerminalErrorEnvelopeDto,
    TerminalSlashActionKindDto, TerminalSlashCandidateDto, TerminalSlashCandidatesResponseDto,
    TerminalSnapshotResponseDto, TerminalStreamEnvelopeDto, TerminalSystemNoteBlockDto,
    TerminalSystemNoteKindDto, TerminalThinkingBlockDto, TerminalToolCallBlockDto,
    TerminalToolStreamBlockDto, TerminalTranscriptErrorCodeDto, TerminalUserBlockDto,
};
pub use tool::{ToolDescriptorDto, ToolExecuteRequestDto, ToolExecuteResponseDto};
