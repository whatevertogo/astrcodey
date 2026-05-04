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
#[serde(rename_all = "camelCase", tag = "kind")]
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
#[serde(rename_all = "camelCase", tag = "kind")]
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
        text: String,
        status: ConversationBlockStatusDto,
    },
    /// 错误。
    Error { id: String, message: String },
    /// 系统提示。
    SystemNote { id: String, text: String },
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
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ConversationDeltaDto {
    /// 追加 block。
    AppendBlock { block: ConversationBlockDto },
    /// patch block。
    PatchBlock {
        block_id: String,
        text_delta: String,
    },
    /// 完成 block。
    CompleteBlock { block_id: String },
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
    /// 工具输出流增量。
    ToolOutput {
        call_id: String,
        stream: ToolOutputStream,
        delta: String,
    },
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
