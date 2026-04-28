//! 服务器到客户端的协议通知类型。
//!
//! 定义服务器向连接的客户端推送的所有通知，
//! 包括运行时事件、会话列表、UI 交互请求和错误信息。

use astrcode_core::event::Event;
use serde::{Deserialize, Serialize};

/// 服务器推送给客户端的通知枚举。
///
/// 运行时/会话事实通过核心 [`Event`] 类型传递；
/// 协议层特有的交互（如会话列表、UI 请求）不写入事件日志。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum ClientNotification {
    /// 核心运行时事件（消息、工具调用、token 统计等）。
    Event(Event),

    /// 会话恢复通知，携带完整快照供客户端重建状态。
    SessionResumed {
        /// 恢复的会话 ID。
        session_id: String,
        /// 会话的完整快照数据。
        snapshot: SessionSnapshot,
    },

    /// 会话列表（响应 `ListSessions` 命令）。
    SessionList {
        /// 所有会话的摘要信息列表。
        sessions: Vec<SessionListItem>,
    },

    /// 服务器发起的 UI 交互请求（确认、选择、输入等）。
    UiRequest {
        /// 请求的唯一标识符，客户端响应时需回传此 ID。
        request_id: String,
        /// UI 交互的类型。
        kind: UiRequestKind,
        /// 展示给用户的消息文本。
        message: String,
        /// 选项列表（仅 `Select` 类型使用）。
        #[serde(skip_serializing_if = "Option::is_none")]
        options: Option<Vec<String>>,
        /// 超时时间（秒），超时后服务器可自动处理。
        #[serde(default)]
        timeout_secs: u64,
    },

    /// 错误通知。
    Error {
        /// 错误码。
        code: i32,
        /// 错误描述信息。
        message: String,
    },
}

/// UI 交互请求的类型。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiRequestKind {
    /// 是/否确认对话框。
    Confirm,
    /// 从选项列表中单选。
    Select,
    /// 自由文本输入框。
    Input,
    /// 信息性通知（无需用户操作，仅需确认已读）。
    Notify,
}

/// 会话列表中的单条会话摘要。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListItem {
    /// 会话的唯一标识符。
    pub session_id: String,
    /// 会话创建时间（ISO 8601 格式）。
    pub created_at: String,
    /// 最后活跃时间（ISO 8601 格式）。
    pub last_active_at: String,
    /// 会话的工作目录路径。
    pub working_dir: String,
    /// 父会话 ID（如果是分叉会话则有值）。
    pub parent_session_id: Option<String>,
}

/// 会话快照，用于客户端重连或状态恢复。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// 会话的唯一标识符。
    pub session_id: String,
    /// 事件日志游标，标识快照的位置。
    pub cursor: String,
    /// 快照中的消息列表。
    pub messages: Vec<MessageDto>,
    /// 当前使用的模型标识符。
    pub model_id: String,
    /// 会话的工作目录路径。
    pub working_dir: String,
}

/// 快照中的单条消息。
///
/// 作为线缆（wire）传输的 DTO 类型，仅包含角色和文本内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageDto {
    /// 消息角色（如 `user`、`assistant`、`system`）。
    pub role: String,
    /// 消息文本内容。
    pub content: String,
}
