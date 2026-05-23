//! 服务器到客户端的协议通知类型。
//!
//! 定义服务器向连接的客户端推送的所有通知，
//! 包括运行时事件、会话列表、UI 交互请求和错误信息。

use astrcode_core::event::{Event, Phase};
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
        session_id: String,
        snapshot: SessionSnapshot,
    },

    /// 会话列表（响应 `ListSessions` 命令）。
    SessionList { sessions: Vec<SessionListItem> },

    /// 服务器发起的 UI 交互请求（确认、选择、输入等）。
    UiRequest {
        request_id: String,
        kind: UiRequestKind,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        options: Option<Vec<String>>,
        /// 超时时间（秒），超时后服务器可自动处理。
        #[serde(default)]
        timeout_secs: u64,
    },

    /// 错误通知。
    Error { code: i32, message: String },

    /// 插件注册的斜杠命令列表（响应 `ListExtensionCommands`）。
    ExtensionCommandList {
        commands: Vec<ExtensionCommandInfo>,
        /// 插件注册的快捷键绑定。
        #[serde(default)]
        keybindings: Vec<KeybindingInfoDto>,
        /// 插件注册的状态栏项（含初始值）。
        #[serde(default)]
        status_items: Vec<StatusItemInfoDto>,
    },

    /// 插件斜杠命令执行结果。
    ExtensionCommandResult {
        command_name: String,
        content: String,
        is_error: bool,
    },

    /// 插件状态栏项更新。
    StatusItemUpdate {
        /// 状态栏项 ID。
        id: String,
        /// 新的显示文本。空字符串表示隐藏。
        text: String,
    },

    /// 扩展注册表发生变化，客户端应清空并重新拉取命令/快捷键/状态栏快照。
    ExtensionRegistryChanged,
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
    pub session_id: String,
    /// ISO 8601 格式。
    pub created_at: String,
    /// ISO 8601 格式。
    pub last_active_at: String,
    pub working_dir: String,
    pub parent_session_id: Option<String>,
    /// 会话标题（首条用户消息摘要或工作目录名）。
    #[serde(default)]
    pub title: Option<String>,
}

/// 子 Agent 会话的运行状态。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionStatusDto {
    #[default]
    Running,
    Completed,
    Failed,
}

impl From<astrcode_core::storage::AgentSessionStatus> for AgentSessionStatusDto {
    fn from(status: astrcode_core::storage::AgentSessionStatus) -> Self {
        match status {
            astrcode_core::storage::AgentSessionStatus::Running => AgentSessionStatusDto::Running,
            astrcode_core::storage::AgentSessionStatus::Completed => {
                AgentSessionStatusDto::Completed
            },
            astrcode_core::storage::AgentSessionStatus::Failed => AgentSessionStatusDto::Failed,
        }
    }
}

/// 父会话派生的子 Agent 会话链接（JSON-RPC 线缆 DTO）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionLinkDto {
    pub child_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default)]
    pub status: AgentSessionStatusDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_tool: Option<String>,
}

/// 会话快照，用于客户端重连或状态恢复。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session_id: String,
    /// 事件日志游标，标识快照的位置。
    pub cursor: String,
    pub messages: Vec<MessageDto>,
    pub model_id: String,
    pub working_dir: String,
    #[serde(default)]
    pub agent_sessions: Vec<AgentSessionLinkDto>,
}

/// 快照中的单条消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageDto {
    pub role: String,
    pub content: String,
}

/// 插件注册的斜杠命令信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionCommandInfo {
    /// 命令名称（不含前导斜杠 `/`）。
    pub name: String,
    pub description: String,
    pub needs_argument: bool,
    /// 命令来源：`builtin`、`extension` 或 `skill`。
    pub source: String,
}

/// 快捷键绑定信息 DTO（通过 ExtensionCommandList 下发到客户端）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeybindingInfoDto {
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

/// 状态栏项信息 DTO（通过 ExtensionCommandList 下发到客户端）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusItemInfoDto {
    /// 唯一标识。
    pub id: String,
    /// 初始显示文本。
    pub text: String,
    /// 排序优先级（越小越靠左）。
    #[serde(default)]
    pub priority: i32,
}
