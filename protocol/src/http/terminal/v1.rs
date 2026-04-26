//! Terminal v1 HTTP / SSE DTO。
//!
//! 这些类型定义 terminal surface 的 authoritative snapshot 与 stream 合同。
//! 它们显式区分：
//! - transcript block 里的 turn-scoped 错误
//! - banner / status 里的连接级错误
//!
//! 这样 client 与 TUI 不需要猜测某个错误该进入 transcript 还是连接状态区。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::http::{AgentLifecycleDto, ChildAgentRefDto, PhaseDto, ToolOutputStreamDto};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct TerminalCursorDto(pub String);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSnapshotResponseDto {
    pub session_id: String,
    pub session_title: String,
    pub cursor: TerminalCursorDto,
    pub phase: PhaseDto,
    pub control: TerminalControlStateDto,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<TerminalBlockDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub child_summaries: Vec<TerminalChildSummaryDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slash_candidates: Vec<TerminalSlashCandidateDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banner: Option<TerminalBannerDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSlashCandidatesResponseDto {
    pub items: Vec<TerminalSlashCandidateDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalStreamEnvelopeDto {
    pub session_id: String,
    pub cursor: TerminalCursorDto,
    #[serde(flatten)]
    pub delta: TerminalDeltaDto,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum TerminalDeltaDto {
    AppendBlock {
        block: TerminalBlockDto,
    },
    PatchBlock {
        block_id: String,
        patch: TerminalBlockPatchDto,
    },
    CompleteBlock {
        block_id: String,
        status: TerminalBlockStatusDto,
    },
    UpdateControlState {
        control: TerminalControlStateDto,
    },
    UpsertChildSummary {
        child: TerminalChildSummaryDto,
    },
    RemoveChildSummary {
        child_session_id: String,
    },
    ReplaceSlashCandidates {
        candidates: Vec<TerminalSlashCandidateDto>,
    },
    SetBanner {
        banner: TerminalBannerDto,
    },
    ClearBanner,
    RehydrateRequired {
        error: TerminalErrorEnvelopeDto,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum TerminalBlockPatchDto {
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
    SetStatus {
        status: TerminalBlockStatusDto,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminalBlockStatusDto {
    Streaming,
    Complete,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TerminalBlockDto {
    User(TerminalUserBlockDto),
    Assistant(TerminalAssistantBlockDto),
    Thinking(TerminalThinkingBlockDto),
    ToolCall(TerminalToolCallBlockDto),
    ToolStream(TerminalToolStreamBlockDto),
    Error(TerminalErrorBlockDto),
    SystemNote(TerminalSystemNoteBlockDto),
    ChildHandoff(TerminalChildHandoffBlockDto),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalUserBlockDto {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub markdown: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalAssistantBlockDto {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub status: TerminalBlockStatusDto,
    pub markdown: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalThinkingBlockDto {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub status: TerminalBlockStatusDto,
    pub markdown: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalToolCallBlockDto {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub tool_name: String,
    pub status: TerminalBlockStatusDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalToolStreamBlockDto {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
    pub stream: ToolOutputStreamDto,
    pub status: TerminalBlockStatusDto,
    pub content: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminalTranscriptErrorCodeDto {
    ProviderError,
    ContextWindowExceeded,
    ToolFatal,
    RateLimit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalErrorBlockDto {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub code: TerminalTranscriptErrorCodeDto,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminalSystemNoteKindDto {
    Compact,
    SystemNote,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSystemNoteBlockDto {
    pub id: String,
    pub note_kind: TerminalSystemNoteKindDto,
    pub markdown: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminalChildHandoffKindDto {
    Delegated,
    Progress,
    Returned,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalChildHandoffBlockDto {
    pub id: String,
    pub handoff_kind: TerminalChildHandoffKindDto,
    pub child: TerminalChildSummaryDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalChildSummaryDto {
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
pub enum TerminalSlashActionKindDto {
    InsertText,
    ExecuteCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSlashCandidateDto {
    pub id: String,
    pub title: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    pub action_kind: TerminalSlashActionKindDto,
    pub action_value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalControlStateDto {
    pub phase: PhaseDto,
    pub can_submit_prompt: bool,
    pub can_request_compact: bool,
    #[serde(default)]
    pub compact_pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_turn_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminalBannerErrorCodeDto {
    AuthExpired,
    CursorExpired,
    StreamDisconnected,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalErrorEnvelopeDto {
    pub code: TerminalBannerErrorCodeDto,
    pub message: String,
    #[serde(default)]
    pub rehydrate_required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalBannerDto {
    pub error: TerminalErrorEnvelopeDto,
}
