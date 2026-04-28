//! 客户端到服务器的命令类型（JSON-RPC 请求）。
//!
//! 定义前端（TUI/客户端）可发送给服务器的所有命令，
//! 包括会话管理、提示提交、配置变更和 UI 响应等。

use serde::{Deserialize, Serialize};

/// 客户端可发送给服务器的命令枚举。
///
/// 使用 `#[serde(tag = "method", content = "params")]` 进行 JSON-RPC 风格的
/// 序列化，`method` 字段标识命令类型，`params` 字段携带参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum ClientCommand {
    // ---- 会话管理 ----
    /// 创建新的会话。
    CreateSession {
        /// 会话的工作目录路径。
        working_dir: String,
    },

    /// 恢复已有会话。
    ResumeSession {
        /// 要恢复的会话 ID。
        session_id: String,
    },

    /// 从已有会话分叉出一个新会话。
    ForkSession {
        /// 被分叉的源会话 ID。
        session_id: String,
        /// 可选的分叉点游标，指定从哪条消息开始分叉。
        #[serde(skip_serializing_if = "Option::is_none")]
        at_cursor: Option<String>,
    },

    /// 删除指定会话。
    DeleteSession { session_id: String },

    /// 列出所有会话。
    ListSessions,

    /// 切换到指定会话。
    SwitchSession { session_id: String },

    // ---- 提示与交互 ----
    /// 提交用户提示（prompt）。
    SubmitPrompt {
        /// 用户输入的文本内容。
        text: String,
        /// 附带的文件/图片附件列表。
        #[serde(default)]
        attachments: Vec<Attachment>,
    },

    /// 中止当前正在进行的 LLM 推理。
    Abort,

    // ---- 配置变更 ----
    /// 切换 LLM 模型。
    SetModel {
        /// 目标模型的标识符。
        model_id: String,
    },

    /// 设置思维链（thinking）级别。
    SetThinkingLevel {
        /// 思维级别标识符。
        level: String,
    },

    /// 手动触发上下文压缩。
    Compact,

    /// 切换代理模式（如 code、architect、ask 等）。
    SwitchMode {
        /// 目标模式名称。
        mode: String,
    },

    // ---- 状态查询 ----
    /// 获取当前完整状态快照。
    GetState,

    // ---- UI 响应 ----
    /// 对服务器发起的 UI 请求进行响应。
    UiResponse {
        /// 对应的 UI 请求 ID。
        request_id: String,
        /// 响应的具体值。
        value: UiResponseValue,
    },
}

/// 提示中附带的文件/图片附件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    /// 文件名。
    pub filename: String,
    /// 文件内容（Base64 编码或纯文本）。
    pub content: String,
    /// MIME 媒体类型（如 `image/png`、`text/plain`）。
    pub media_type: String,
}

/// 对服务器 UI 请求的响应值。
///
/// 使用 `#[serde(untagged)]` 根据字段名自动推断变体类型。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UiResponseValue {
    /// 确认/取消响应。
    Confirm { accepted: bool },
    /// 从选项列表中选择一项。
    Select { selected: String },
    /// 自由文本输入。
    Input { text: String },
    /// 通知确认（用户已阅读）。
    NotifyAck,
}
