//! HTTP/SSE 入口使用的线缆 DTO。
//!
//! 这些类型只描述外部协议形状；server 负责把 storage read model 映射到这里，
//! storage 不依赖也不返回这些 DTO。

use astrcode_core::event::{Phase, ToolOutputStream};
use serde::{Deserialize, Serialize};

/// 新建会话请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionRequest {
    /// 会话工作目录。
    pub working_dir: String,
}

/// 新建会话响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionResponseDto {
    /// 创建出的会话 ID。
    pub session_id: String,
}

/// 提交 prompt 请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
    /// 用户输入文本。
    pub text: String,
}

/// prompt 提交结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind"
)]
pub enum PromptSubmitResponse {
    /// 已接受并异步执行。
    Accepted {
        /// 会话 ID。
        session_id: String,
        /// 回合 ID。
        turn_id: String,
        /// 如果该请求隐式 fork，则记录来源。v1 总是 None。
        #[serde(skip_serializing_if = "Option::is_none")]
        branched_from_session_id: Option<String>,
    },
    /// 请求已同步处理完成。
    Handled {
        /// 会话 ID。
        session_id: String,
        /// 说明文本。
        message: String,
    },
}

/// 手动 compact 请求。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactSessionRequest {
    /// 额外 compact 指令，v1 暂不接入生产链路。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// 手动 compact 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactSessionResponse {
    /// compact 是否已接受。
    pub accepted: bool,
    /// compact 是否被延后。
    pub deferred: bool,
    /// compact continuation 创建的子会话 ID。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_session_id: Option<String>,
    /// 说明文本。
    pub message: String,
}

/// 斜杠命令列表响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommandListResponseDto {
    /// 当前会话可执行的斜杠命令。
    pub commands: Vec<SlashCommandInfoDto>,
}

/// 可执行斜杠命令信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommandInfoDto {
    /// 命令名称（不含前导斜杠 `/`）。
    pub name: String,
    /// 人类可读描述。
    pub description: String,
    /// 是否需要参数。
    pub needs_argument: bool,
    /// 命令来源：`builtin`、`plugin` 或 `skill`。
    pub source: String,
}

impl From<crate::events::ExtensionCommandInfo> for SlashCommandInfoDto {
    fn from(cmd: crate::events::ExtensionCommandInfo) -> Self {
        Self {
            name: cmd.name,
            description: cmd.description,
            needs_argument: cmd.needs_argument,
            source: cmd.source,
        }
    }
}

/// fork 请求的冻结线缆形状。v1 route 返回 501。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkSessionRequest {
    /// 可选来源 turn。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// 可选来源 durable seq。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_seq: Option<u64>,
}

/// 会话列表项。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListItemDto {
    /// 会话 ID。
    pub session_id: String,
    /// 工作目录。
    pub working_dir: String,
    /// 显示名。
    pub display_name: String,
    /// 标题。
    pub title: String,
    /// 创建时间。
    pub created_at: String,
    /// 更新时间。
    pub updated_at: String,
    /// 父会话 ID。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// 父会话 seq，v1 未接线。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_storage_seq: Option<u64>,
    /// 当前阶段。
    pub phase: Phase,
    /// 首条用户消息内容，无消息时为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_user_message: Option<String>,
}

/// 会话列表响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListResponseDto {
    /// 会话列表。
    pub sessions: Vec<SessionListItemDto>,
}

/// conversation cursor。v1 中它是 snapshot 最新 durable seq 的十进制字符串。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationCursorDto {
    /// 最新 durable seq。
    pub value: String,
}

/// 父会话派生的子 Agent 会话链接（HTTP DTO）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionLinkDto {
    /// 子会话 ID。
    pub child_session_id: String,
    /// 子 Agent 名称。
    pub agent_name: String,
    /// 子 Agent 任务描述。
    pub task: String,
}

/// conversation 全量快照响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSnapshotResponseDto {
    /// 会话 ID。
    pub session_id: String,
    /// 会话标题。
    pub session_title: String,
    /// 快照对应 cursor。
    pub cursor: ConversationCursorDto,
    /// 当前阶段。
    pub phase: Phase,
    /// 控制状态。
    pub control: ConversationControlStateDto,
    /// 对话块。
    pub blocks: Vec<ConversationBlockDto>,
    /// 父会话派生的子 Agent 会话列表。
    #[serde(default)]
    pub agent_sessions: Vec<AgentSessionLinkDto>,
}

/// conversation 控制状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationControlStateDto {
    /// 当前阶段。
    pub phase: Phase,
    /// 是否允许提交 prompt。
    pub can_submit_prompt: bool,
    /// 是否允许请求 compact。
    pub can_request_compact: bool,
    /// 是否有 compact 等待中。
    pub compact_pending: bool,
    /// 是否正在 compact。
    pub compacting: bool,
    /// 当前模式 ID，v1 暂无。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_mode_id: Option<String>,
    /// 活跃 turn ID，v1 snapshot 暂无。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_turn_id: Option<String>,
}

/// conversation 块。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind"
)]
pub enum ConversationBlockDto {
    /// 用户消息。
    User { id: String, text: String },
    /// 助手消息。
    Assistant {
        id: String,
        text: String,
        status: ConversationBlockStatusDto,
    },
    /// 工具调用或工具结果。
    ToolCall {
        id: String,
        name: String,
        /// LLM 对本次调用的参数（用于折叠摘要行显示）。
        arguments: String,
        /// 工具执行结果（展开后显示）。
        text: String,
        status: ConversationBlockStatusDto,
    },
    /// 错误。
    Error { id: String, message: String },
    /// 系统提示。
    SystemNote { id: String, text: String },
    /// Compact 压缩摘要。
    CompactSummary {
        id: String,
        summary: String,
        trigger: String,
        pre_tokens: usize,
        post_tokens: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        transcript_path: Option<String>,
    },
}

/// conversation 块状态。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConversationBlockStatusDto {
    /// 正在流式更新。
    Streaming,
    /// 已完成。
    Complete,
    /// 失败。
    Error,
}

/// SSE 信封。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationStreamEnvelopeDto {
    /// 会话 ID。
    pub session_id: String,
    /// 当前事件 cursor。
    pub cursor: ConversationCursorDto,
    /// 增量载荷。
    pub delta: ConversationDeltaDto,
}

/// SSE conversation 增量。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind"
)]
pub enum ConversationDeltaDto {
    /// 追加 block。
    AppendBlock { block: ConversationBlockDto },
    /// patch block。
    PatchBlock {
        block_id: String,
        text_delta: String,
    },
    /// 用持久化后的最终内容完成或补齐 block。
    FinalizeBlock { block: ConversationBlockDto },
    /// 控制状态更新。
    UpdateControlState {
        control: ConversationControlStateDto,
    },
    /// 服务端检测到 receiver lag，客户端应重新拉全量 snapshot。
    RehydrateRequired,
    /// 当前会话已经 continuation 到新的子会话。
    SessionContinued {
        parent_session_id: String,
        new_session_id: String,
        parent_cursor: ConversationCursorDto,
    },
    /// 更新 toolCall block 的 arguments 字段（用于折叠摘要行显示参数）。
    PatchArguments {
        /// 工具调用 block 的 ID。
        block_id: String,
        /// 参数的格式化文本。
        arguments: String,
    },
    /// 工具输出流增量。
    ToolOutput {
        call_id: String,
        stream: ToolOutputStream,
        delta: String,
    },
    /// 推理模型思维链增量。
    ThinkingDelta { delta: String },
}

/// HTTP 错误响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationErrorEnvelopeDto {
    /// 错误码。
    pub code: String,
    /// 错误消息。
    pub message: String,
}

/// 删除项目响应（删除某工作目录下所有会话）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteProjectResponseDto {
    /// 被删除的会话数量。
    pub deleted_count: usize,
}

// ── Config / Models DTOs ──

/// GET /api/config 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigViewResponseDto {
    pub config_path: String,
    pub active_profile: String,
    pub active_model: String,
    pub profiles: Vec<ProfileDto>,
    pub warning: Option<String>,
}

/// 配置文件中的 Profile 信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileDto {
    pub name: String,
    pub provider_kind: String,
    pub base_url: String,
    pub has_api_key: bool,
    pub models: Vec<ModelDto>,
}

/// Profile 中的模型信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDto {
    pub id: String,
    pub max_tokens: Option<u32>,
    pub context_limit: Option<usize>,
}

/// POST /api/config/active-selection 请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateActiveSelectionRequest {
    pub active_profile: String,
    pub active_model: String,
}

/// POST /api/config/active-selection 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateActiveSelectionResponseDto {
    pub success: bool,
    pub warning: Option<String>,
}

/// POST /api/config/reload 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigReloadResponseDto {
    pub active_profile: String,
    pub active_model: String,
}

/// GET /api/models/current 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentModelResponseDto {
    pub profile_name: String,
    pub model_id: String,
    pub provider_kind: String,
}

/// GET /api/models 响应中的单个模型。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableModelDto {
    pub profile_name: String,
    pub model_id: String,
    pub provider_kind: String,
}

/// GET /api/models 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelListResponseDto {
    pub models: Vec<AvailableModelDto>,
}

/// POST /api/models/test 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelTestResponseDto {
    pub success: bool,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_stream_fixture_matches_wire_contract() {
        let fixture = include_str!("../fixtures/conversation-stream.json");
        let envelopes: Vec<ConversationStreamEnvelopeDto> =
            serde_json::from_str(fixture).expect("fixture should deserialize");

        assert_eq!(envelopes.len(), 5);

        match &envelopes[0].delta {
            ConversationDeltaDto::PatchBlock {
                block_id,
                text_delta,
            } => {
                assert_eq!(block_id, "assistant-1");
                assert_eq!(text_delta, "hello");
            },
            other => panic!("unexpected fixture delta: {other:?}"),
        }

        match &envelopes[1].delta {
            ConversationDeltaDto::FinalizeBlock {
                block: ConversationBlockDto::Assistant { id, text, status },
            } => {
                assert_eq!(id, "assistant-1");
                assert_eq!(text, "complete answer");
                assert!(matches!(status, ConversationBlockStatusDto::Complete));
            },
            other => panic!("unexpected fixture delta: {other:?}"),
        }

        match &envelopes[4].delta {
            ConversationDeltaDto::PatchArguments {
                block_id,
                arguments,
            } => {
                assert_eq!(block_id, "tool-1");
                assert_eq!(arguments, "Cargo.toml");
            },
            other => panic!("unexpected fixture delta: {other:?}"),
        }

        let encoded = serde_json::to_string(&envelopes[0]).expect("fixture should serialize");
        assert!(encoded.contains("\"blockId\""));
        assert!(encoded.contains("\"textDelta\""));
        assert!(!encoded.contains("block_id"));
        assert!(!encoded.contains("text_delta"));
    }
}
