//! 客户端到服务器的命令类型（JSON-RPC 请求）。
//!
//! 本模块定义前端（TUI/客户端）可发送给服务器的所有命令。
//! 这些命令用于实现会话管理、提示提交、配置变更、UI 响应等功能。
//!
//! 所有命令均采用 JSON-RPC 风格的序列化格式（通过 serde 的 tag/content 属性实现），
//! 字段命名遵循 snake_case 规范。
//!
//! # 命令分类
//!
//! - **会话管理**：`CreateSession`、`ResumeSession`、`ForkSession`、
//!   `DeleteSession`、`ListSessions`、`SwitchSession`
//! - **提示与交互**：`SubmitPrompt`、`Abort`
//! - **配置变更**：`SetModel`、`Compact`
//! - **状态查询**：`GetState`
//! - **扩展命令**：`ListExtensionCommands`、`ExecuteExtensionCommand`
//! - **UI 响应**：`UiResponse`

use serde::{Deserialize, Serialize};

/// 客户端可发送给服务器的命令枚举。
///
/// 每个变体代表一种具体的客户端操作请求。命令通过 JSON-RPC 风格序列化，
/// 其中 `method` 字段标识命令类型，`params` 字段包含命令参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum ClientCommand {
    // ---- 会话管理 ----
    /// 创建新会话。
    ///
    /// # 参数
    /// - `working_dir`: 工作目录路径，用于设置会话的上下文环境
    CreateSession { working_dir: String },

    /// 恢复指定会话。
    ///
    /// # 参数
    /// - `session_id`: 要恢复的会话唯一标识符
    ResumeSession { session_id: String },

    /// 从现有会话分叉创建新会话。
    ///
    /// # 参数
    /// - `session_id`: 源会话的唯一标识符
    /// - `at_cursor`: 可选的分叉点游标，指定从哪条消息开始分叉。 若未提供，则从会话末尾分叉
    ForkSession {
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        at_cursor: Option<String>,
    },

    /// 删除指定会话。
    ///
    /// # 参数
    /// - `session_id`: 要删除的会话唯一标识符
    DeleteSession { session_id: String },

    /// 列出所有可用会话。
    ListSessions,

    /// 切换到指定会话。
    ///
    /// # 参数
    /// - `session_id`: 目标会话的唯一标识符
    SwitchSession { session_id: String },

    // ---- 提示与交互 ----
    /// 提交用户提示（prompt）给 AI 处理。
    ///
    /// # 参数
    /// - `text`: 用户输入的提示文本
    /// - `attachments`: 附加的文件或图片列表（可选，默认为空）
    SubmitPrompt {
        text: String,
        #[serde(default)]
        attachments: Vec<Attachment>,
    },

    /// 向正在执行的 turn 注入中途消息。
    ///
    /// 仅在 session 有 active turn 时有效。消息通过 `emit_durable` 持久化后
    /// 由 TurnRunner 在下一个 step boundary 消费并注入 LLM 上下文。
    ///
    /// # 参数
    /// - `text`: 要注入的消息文本
    InjectMessage { text: String },

    /// 请求生成当前对话的摘要。
    ///
    /// 仅在无 active turn 时可调用。结果通过 `RecapGenerated` 事件推送。
    Recap,

    /// 中止当前正在进行的 AI 处理操作。
    Abort,

    // ---- 配置变更 ----
    /// 设置当前会话使用的 AI 模型。
    ///
    /// # 参数
    /// - `model_id`: 模型标识符（如 "gpt-4"、"claude-3" 等）
    SetModel { model_id: String },

    /// 压缩当前会话上下文。
    ///
    /// 此操作会触发会话历史压缩，以控制上下文长度和 Token 消耗。
    Compact {
        #[serde(skip_serializing_if = "Option::is_none")]
        keep_recent_turns: Option<usize>,
    },

    // ---- 状态查询 ----
    /// 获取当前服务器/会话状态。
    GetState,

    // ---- 扩展命令 ----
    /// 列出所有可用的扩展命令。
    ListExtensionCommands,

    /// 执行指定的扩展命令。
    ///
    /// # 参数
    /// - `command_name`: 扩展命令的名称
    /// - `arguments`: 命令参数（JSON 字符串格式）
    ExecuteExtensionCommand {
        command_name: String,
        arguments: String,
    },

    // ---- UI 响应 ----
    /// 响应服务器发起的 UI 请求。
    ///
    /// # 参数
    /// - `request_id`: 对应的 UI 请求标识符
    /// - `value`: 用户响应的具体值
    UiResponse {
        request_id: String,
        value: UiResponseValue,
    },
}

/// 提示中附带的文件/图片附件。
///
/// 用于在 `SubmitPrompt` 命令中传递额外的上下文资源，
/// 如代码文件、图片、文档等。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    /// 文件名（含扩展名）。
    pub filename: String,
    /// 文件内容。
    ///
    /// 根据 `media_type` 不同，可以是：
    /// - Base64 编码的二进制数据（图片、二进制文件）
    /// - 纯文本内容（代码、文档）
    pub content: String,
    /// 媒体类型（MIME type）。
    ///
    /// 常见值：
    /// - `text/plain`：纯文本
    /// - `text/x-rust`：Rust 源代码
    /// - `image/png`、`image/jpeg`：图片
    /// - `application/pdf`：PDF 文档
    pub media_type: String,
}

/// 对服务器 UI 请求的响应值。
///
/// 用于 `UiResponse` 命令，封装用户对各种 UI 提示的响应结果。
/// 采用 `untagged` 序列化方式，根据字段自动推断变体类型。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UiResponseValue {
    /// 确认/取消类型的响应。
    ///
    /// # 字段
    /// - `accepted`: `true` 表示用户接受，`false` 表示拒绝
    Confirm { accepted: bool },
    /// 单选/多选类型的响应。
    ///
    /// # 字段
    /// - `selected`: 用户选择的选项值
    Select { selected: String },
    /// 文本输入类型的响应。
    ///
    /// # 字段
    /// - `text`: 用户输入的文本内容
    Input { text: String },
    /// 通知确认响应（无返回值）。
    ///
    /// 用于仅需要确认收到通知的场景。
    NotifyAck,
}

#[cfg(test)]
mod tests {
    use serde_json;

    use super::*;

    fn roundtrip(cmd: &ClientCommand) -> ClientCommand {
        let json = serde_json::to_string(cmd).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn submit_prompt_roundtrip() {
        let cmd = ClientCommand::SubmitPrompt {
            text: "hello".into(),
            attachments: vec![Attachment {
                filename: "test.rs".into(),
                content: "fn main() {}".into(),
                media_type: "text/x-rust".into(),
            }],
        };
        let parsed = roundtrip(&cmd);
        assert!(matches!(parsed, ClientCommand::SubmitPrompt { .. }));
        if let ClientCommand::SubmitPrompt { text, attachments } = parsed {
            assert_eq!(text, "hello");
            assert_eq!(attachments.len(), 1);
            assert_eq!(attachments[0].filename, "test.rs");
        }
    }

    #[test]
    fn submit_prompt_serializes_snake_case_method() {
        let cmd = ClientCommand::SubmitPrompt {
            text: "hi".into(),
            attachments: vec![],
        };
        let json = serde_json::to_value(&cmd).unwrap();
        assert_eq!(json["method"], "submit_prompt");
    }

    #[test]
    fn create_session_roundtrip() {
        let cmd = ClientCommand::CreateSession {
            working_dir: "/tmp".into(),
        };
        let parsed = roundtrip(&cmd);
        if let ClientCommand::CreateSession { working_dir } = parsed {
            assert_eq!(working_dir, "/tmp");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn fork_session_without_cursor() {
        let cmd = ClientCommand::ForkSession {
            session_id: "s1".into(),
            at_cursor: None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(!json.contains("at_cursor"));
        let parsed: ClientCommand = serde_json::from_str(&json).unwrap();
        if let ClientCommand::ForkSession {
            session_id,
            at_cursor,
        } = parsed
        {
            assert_eq!(session_id, "s1");
            assert!(at_cursor.is_none());
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn ui_response_confirm_roundtrip() {
        let cmd = ClientCommand::UiResponse {
            request_id: "r1".into(),
            value: UiResponseValue::Confirm { accepted: true },
        };
        let parsed = roundtrip(&cmd);
        if let ClientCommand::UiResponse { request_id, value } = parsed {
            assert_eq!(request_id, "r1");
            assert!(matches!(value, UiResponseValue::Confirm { accepted: true }));
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn ui_response_select_roundtrip() {
        let value = UiResponseValue::Select {
            selected: "option_a".into(),
        };
        let json = serde_json::to_string(&value).unwrap();
        let parsed: UiResponseValue = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, UiResponseValue::Select { .. }));
    }

    #[test]
    fn ui_response_input_roundtrip() {
        let value = UiResponseValue::Input {
            text: "some input".into(),
        };
        let json = serde_json::to_string(&value).unwrap();
        let parsed: UiResponseValue = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, UiResponseValue::Input { .. }));
    }

    #[test]
    fn list_sessions_roundtrip() {
        let cmd = ClientCommand::ListSessions;
        let parsed = roundtrip(&cmd);
        assert!(matches!(parsed, ClientCommand::ListSessions));
    }

    #[test]
    fn compact_roundtrip() {
        let cmd = ClientCommand::Compact {
            keep_recent_turns: Some(5),
        };
        let parsed = roundtrip(&cmd);
        if let ClientCommand::Compact { keep_recent_turns } = parsed {
            assert_eq!(keep_recent_turns, Some(5));
        } else {
            panic!("wrong variant");
        }
    }
}
