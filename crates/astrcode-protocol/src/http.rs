//! HTTP/SSE 入口使用的线缆 DTO。
//!
//! 这些类型只描述外部协议形状；server 负责把 storage read model 映射到这里，
//! storage 不依赖也不返回这些 DTO。

use astrcode_core::event::{Phase, ToolOutputStream};
use serde::{Deserialize, Serialize};

pub use crate::events::AgentSessionStatusDto;

/// 新建会话请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionRequest {
    pub working_dir: String,
}

/// 新建会话响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionResponseDto {
    pub session_id: String,
}

/// 提交 prompt 请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
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
        session_id: String,
        turn_id: String,
        /// 如果该请求隐式 fork，则记录来源。v1 总是 None。
        #[serde(skip_serializing_if = "Option::is_none")]
        branched_from_session_id: Option<String>,
    },
    /// 请求已同步处理完成。
    Handled { session_id: String, message: String },
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
    pub accepted: bool,
    pub deferred: bool,
    /// compact continuation 创建的子会话 ID。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_session_id: Option<String>,
    pub message: String,
}

/// 斜杠命令列表响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommandListResponseDto {
    pub commands: Vec<SlashCommandInfoDto>,
    /// 插件注册的快捷键绑定。
    #[serde(default)]
    pub keybindings: Vec<KeybindingDto>,
    /// 插件注册的状态栏项（含初始值）。
    #[serde(default)]
    pub status_items: Vec<StatusItemDto>,
}

/// 快捷键绑定 DTO。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeybindingDto {
    /// 快捷键描述（如 "shift+tab"）。
    pub key: String,
    /// 触发的命令名（不含 `/`）。
    pub command: String,
    /// 命令参数。
    #[serde(default)]
    pub arguments: String,
    /// 人类可读描述。
    pub description: String,
}

/// 状态栏项 DTO。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusItemDto {
    /// 唯一标识。
    pub id: String,
    /// 显示文本。
    pub text: String,
    /// 排序优先级。
    #[serde(default)]
    pub priority: i32,
}

/// 可执行斜杠命令信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommandInfoDto {
    /// 命令名称（不含前导斜杠 `/`）。
    pub name: String,
    pub description: String,
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
    pub session_id: String,
    pub working_dir: String,
    pub display_name: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// 父会话 seq，v1 未接线。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_storage_seq: Option<u64>,
    pub phase: Phase,
    /// 首条用户消息内容，无消息时为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_user_message: Option<String>,
    /// 创建该子 session 的扩展 ID。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_plugin: Option<String>,
}

/// 会话列表响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListResponseDto {
    pub sessions: Vec<SessionListItemDto>,
}

/// conversation cursor。v1 中它是 snapshot 最新 durable seq 的十进制字符串。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationCursorDto {
    pub value: String,
}

/// 父会话派生的子 Agent 会话链接（HTTP DTO，camelCase 序列化）。
///
/// 与 [`events::AgentSessionLinkDto`](crate::events::AgentSessionLinkDto) 字段相同，
/// 但 serde 使用 `camelCase` 以匹配 HTTP/SSE 线缆格式。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpAgentSessionLinkDto {
    pub child_session_id: String,
    pub agent_name: String,
    pub task: String,
    #[serde(default)]
    pub status: AgentSessionStatusDto,
}

/// conversation 全量快照响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSnapshotResponseDto {
    pub session_id: String,
    pub session_title: String,
    pub cursor: ConversationCursorDto,
    pub phase: Phase,
    pub control: ConversationControlStateDto,
    pub blocks: Vec<ConversationBlockDto>,
    #[serde(default)]
    pub agent_sessions: Vec<HttpAgentSessionLinkDto>,
}

/// conversation 控制状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationControlStateDto {
    pub phase: Phase,
    pub can_submit_prompt: bool,
    pub can_request_compact: bool,
    pub compact_pending: bool,
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
    User {
        id: String,
        text: String,
    },
    Assistant {
        id: String,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        status: ConversationBlockStatusDto,
    },
    ToolCall {
        id: String,
        name: String,
        /// LLM 对本次调用的参数（用于折叠摘要行显示）。
        arguments: String,
        /// 工具执行结果（展开后显示）。
        text: String,
        status: ConversationBlockStatusDto,
        /// 后台任务 ID（仅后台化任务携带，用于前端面板追踪）。
        #[serde(skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        /// 工具元数据（如 planContent、path 等），不进入 LLM 上下文。
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
    Error {
        id: String,
        message: String,
    },
    SystemNote {
        id: String,
        text: String,
    },
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
    Streaming,
    Backgrounded,
    Complete,
    Error,
}

/// SSE 信封。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationStreamEnvelopeDto {
    pub session_id: String,
    pub cursor: ConversationCursorDto,
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
    AppendBlock {
        block: ConversationBlockDto,
    },
    PatchBlock {
        block_id: String,
        text_delta: String,
    },
    /// 用持久化后的最终内容完成或补齐 block。
    FinalizeBlock {
        block: ConversationBlockDto,
    },
    UpdateControlState {
        control: ConversationControlStateDto,
    },
    /// 服务端检测到 receiver lag，客户端应重新拉全量 snapshot。
    RehydrateRequired,
    SessionContinued {
        parent_session_id: String,
        new_session_id: String,
        parent_cursor: ConversationCursorDto,
    },
    /// 更新 toolCall block 的 arguments 字段（用于折叠摘要行显示参数）。
    PatchArguments {
        block_id: String,
        arguments: String,
    },
    ToolOutput {
        call_id: String,
        stream: ToolOutputStream,
        delta: String,
    },
    ThinkingDelta {
        block_id: String,
        delta: String,
    },
    /// 工具调用被移入后台执行。
    ToolCallBackgrounded {
        call_id: String,
        task_id: String,
    },
    /// Agent 子会话状态变更（新增 / 完成 / 失败）。
    AgentSessionUpdated {
        agent_session: HttpAgentSessionLinkDto,
    },
}

/// HTTP 错误响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationErrorEnvelopeDto {
    pub code: String,
    pub message: String,
}

/// 删除项目响应（删除某工作目录下所有会话）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteProjectResponseDto {
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
                block:
                    ConversationBlockDto::Assistant {
                        id,
                        text,
                        reasoning_content: _,
                        status,
                    },
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

    #[test]
    fn thinking_delta_uses_block_id_wire_name() {
        let delta = ConversationDeltaDto::ThinkingDelta {
            block_id: "assistant-1".into(),
            delta: "reasoning".into(),
        };

        let encoded = serde_json::to_string(&delta).unwrap();
        assert!(encoded.contains("\"blockId\""));
        assert!(!encoded.contains("block_id"));

        let decoded: ConversationDeltaDto = serde_json::from_str(&encoded).unwrap();
        match decoded {
            ConversationDeltaDto::ThinkingDelta { block_id, delta } => {
                assert_eq!(block_id, "assistant-1");
                assert_eq!(delta, "reasoning");
            },
            other => panic!("unexpected delta: {other:?}"),
        }
    }
}
