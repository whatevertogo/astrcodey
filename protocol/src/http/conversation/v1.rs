//! Conversation v1 HTTP / SSE DTO。
//!
//! conversation 是 authoritative hydration / delta 合同，直接表达后端收敛后的
//! conversation/tool display 语义，不再借 terminal alias 维持假性独立。

pub use astrcode_core::{
    CompactAppliedMeta as ConversationCompactMetaDto,
    CompactTrigger as ConversationCompactTriggerDto,
    PromptCacheDiagnostics as ConversationPromptCacheDiagnosticsDto,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::http::{AgentLifecycleDto, ChildAgentRefDto, PhaseDto, ToolOutputStreamDto};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct ConversationCursorDto(pub String);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationStepCursorDto {
    pub turn_id: String,
    pub step_index: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConversationStepProgressDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durable: Option<ConversationStepCursorDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<ConversationStepCursorDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSnapshotResponseDto {
    pub session_id: String,
    pub session_title: String,
    pub cursor: ConversationCursorDto,
    pub phase: PhaseDto,
    pub control: ConversationControlStateDto,
    #[serde(default)]
    pub step_progress: ConversationStepProgressDto,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<ConversationBlockDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub child_summaries: Vec<ConversationChildSummaryDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slash_candidates: Vec<ConversationSlashCandidateDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banner: Option<ConversationBannerDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSlashCandidatesResponseDto {
    pub items: Vec<ConversationSlashCandidateDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationStreamEnvelopeDto {
    pub session_id: String,
    pub cursor: ConversationCursorDto,
    #[serde(default)]
    pub step_progress: ConversationStepProgressDto,
    #[serde(flatten)]
    pub delta: ConversationDeltaDto,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ConversationDeltaDto {
    AppendBlock {
        block: ConversationBlockDto,
    },
    PatchBlock {
        block_id: String,
        patch: ConversationBlockPatchDto,
    },
    CompleteBlock {
        block_id: String,
        status: ConversationBlockStatusDto,
    },
    UpdateControlState {
        control: ConversationControlStateDto,
    },
    UpsertChildSummary {
        child: ConversationChildSummaryDto,
    },
    RemoveChildSummary {
        child_session_id: String,
    },
    ReplaceSlashCandidates {
        candidates: Vec<ConversationSlashCandidateDto>,
    },
    SetBanner {
        banner: ConversationBannerDto,
    },
    ClearBanner,
    RehydrateRequired {
        error: ConversationErrorEnvelopeDto,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ConversationBlockPatchDto {
    AppendMarkdown {
        markdown: String,
    },
    ReplaceMarkdown {
        markdown: String,
    },
    AppendToolStream {
        stream: ToolOutputStreamDto,
        chunk: String,
    },
    ReplaceSummary {
        summary: String,
    },
    ReplaceMetadata {
        metadata: Value,
    },
    ReplaceError {
        error: Option<String>,
    },
    ReplaceDuration {
        duration_ms: u64,
    },
    ReplaceChildRef {
        child_ref: ChildAgentRefDto,
    },
    SetTruncated {
        truncated: bool,
    },
    SetStatus {
        status: ConversationBlockStatusDto,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationBlockStatusDto {
    Streaming,
    Complete,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConversationBlockDto {
    User(ConversationUserBlockDto),
    Assistant(ConversationAssistantBlockDto),
    Thinking(ConversationThinkingBlockDto),
    PromptMetrics(ConversationPromptMetricsBlockDto),
    Plan(ConversationPlanBlockDto),
    ToolCall(ConversationToolCallBlockDto),
    Error(ConversationErrorBlockDto),
    SystemNote(ConversationSystemNoteBlockDto),
    ChildHandoff(ConversationChildHandoffBlockDto),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationUserBlockDto {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub markdown: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationAssistantBlockDto {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub status: ConversationBlockStatusDto,
    pub markdown: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_index: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationThinkingBlockDto {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub status: ConversationBlockStatusDto,
    pub markdown: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationPromptMetricsBlockDto {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub step_index: u32,
    pub estimated_tokens: u32,
    pub context_window: u32,
    pub effective_window: u32,
    pub threshold_tokens: u32,
    pub truncated_tool_results: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    pub provider_cache_metrics_supported: bool,
    #[serde(default)]
    pub prompt_cache_reuse_hits: u32,
    #[serde(default)]
    pub prompt_cache_reuse_misses: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompt_cache_unchanged_layers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_diagnostics: Option<ConversationPromptCacheDiagnosticsDto>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationPlanEventKindDto {
    Saved,
    ReviewPending,
    Presented,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationPlanReviewKindDto {
    RevisePlan,
    FinalReview,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationPlanReviewDto {
    pub kind: ConversationPlanReviewKindDto,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checklist: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConversationPlanBlockersDto {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_headings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invalid_sections: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationPlanBlockDto {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub tool_call_id: String,
    pub event_kind: ConversationPlanEventKindDto,
    pub title: String,
    pub plan_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ConversationPlanReviewDto>,
    #[serde(default, skip_serializing_if = "is_empty_plan_blockers")]
    pub blockers: ConversationPlanBlockersDto,
}

fn is_empty_plan_blockers(blockers: &ConversationPlanBlockersDto) -> bool {
    blockers.missing_headings.is_empty() && blockers.invalid_sections.is_empty()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConversationToolStreamsDto {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stdout: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stderr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationToolCallBlockDto {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub tool_call_id: String,
    pub tool_name: String,
    pub status: ConversationBlockStatusDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_ref: Option<ChildAgentRefDto>,
    #[serde(default, skip_serializing_if = "is_default_tool_streams")]
    pub streams: ConversationToolStreamsDto,
}

fn is_default_tool_streams(streams: &ConversationToolStreamsDto) -> bool {
    streams == &ConversationToolStreamsDto::default()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationTranscriptErrorCodeDto {
    ProviderError,
    ContextWindowExceeded,
    ToolFatal,
    RateLimit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationErrorBlockDto {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub code: ConversationTranscriptErrorCodeDto,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationSystemNoteKindDto {
    Compact,
    SystemNote,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSystemNoteBlockDto {
    pub id: String,
    pub note_kind: ConversationSystemNoteKindDto,
    pub markdown: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_meta: Option<ConversationLastCompactMetaDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserved_recent_turns: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationChildHandoffKindDto {
    Delegated,
    Progress,
    Returned,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationChildHandoffBlockDto {
    pub id: String,
    pub handoff_kind: ConversationChildHandoffKindDto,
    pub child: ConversationChildSummaryDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationChildSummaryDto {
    pub child_session_id: String,
    pub child_agent_id: String,
    pub title: String,
    pub lifecycle: AgentLifecycleDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_output_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_ref: Option<ChildAgentRefDto>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationSlashActionKindDto {
    InsertText,
    ExecuteCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSlashCandidateDto {
    pub id: String,
    pub title: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    pub action_kind: ConversationSlashActionKindDto,
    pub action_value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationControlStateDto {
    pub phase: PhaseDto,
    pub can_submit_prompt: bool,
    pub can_request_compact: bool,
    #[serde(default)]
    pub compact_pending: bool,
    #[serde(default)]
    pub compacting: bool,
    pub current_mode_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_compact_meta: Option<ConversationLastCompactMetaDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_plan: Option<ConversationPlanReferenceDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_tasks: Option<Vec<ConversationTaskItemDto>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationLastCompactMetaDto {
    pub trigger: ConversationCompactTriggerDto,
    #[serde(flatten)]
    pub meta: ConversationCompactMetaDto,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationPlanReferenceDto {
    pub slug: String,
    pub path: String,
    pub status: String,
    pub title: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationTaskStatusDto {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationTaskItemDto {
    pub content: String,
    pub status: ConversationTaskStatusDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationBannerErrorCodeDto {
    AuthExpired,
    CursorExpired,
    StreamDisconnected,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationErrorEnvelopeDto {
    pub code: ConversationBannerErrorCodeDto,
    pub message: String,
    #[serde(default)]
    pub rehydrate_required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationBannerDto {
    pub error: ConversationErrorEnvelopeDto,
}
