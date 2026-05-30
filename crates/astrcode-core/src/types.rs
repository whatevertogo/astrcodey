//! 核心共享标识符和数据类型。
//!
//! 这些类型在 astrcode 平台的所有 crate 中通用。
//!
//! 本模块定义了：
//! - 类型安全的 ID newtype（[`SessionId`]、[`EventId`]、[`TurnId`] 等）
//! - ID 验证函数 [`validate_session_id`]
//! - ID 生成函数 [`new_session_id`]、[`new_event_id`] 等
//! - 项目标识符派生函数 [`project_key_from_path`]

use std::{
    convert::Infallible,
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
};

use serde::{Deserialize, Serialize};

macro_rules! id_newtype {
    ($(#[$meta:meta])* pub struct $name:ident;) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// 从字符串构造 ID。构造本身不做校验；边界安全性由调用方负责。
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// 返回底层字符串视图。
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// 消耗 ID 并返回底层字符串。
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl FromStr for $name {
            type Err = Infallible;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Ok(Self::from(value))
            }
        }
    };
}

id_newtype! {
    /// 会话的唯一标识符。
    ///
    /// 会话是基于事件溯源的持久化工作单元，
    /// 所有 Agent 交互都在会话中发生。
    pub struct SessionId;
}

id_newtype! {
    /// 会话事件日志中事件的唯一标识符。
    pub struct EventId;
}

id_newtype! {
    /// 轮次的唯一标识符（一个"用户提示 + Agent 回复"的交互周期）。
    pub struct TurnId;
}

id_newtype! {
    /// 轮次内消息（用户或助手）的唯一标识符。
    pub struct MessageId;
}

id_newtype! {
    /// 轮次内工具调用的唯一标识符。
    pub struct ToolCallId;
}

/// 会话事件日志中的位置游标。
/// 对客户端不透明；服务器用于分页和恢复。
pub type Cursor = String;

/// 项目标识符，从工作目录路径派生，可安全作为单个目录名。
pub type ProjectKey = String;

/// 标识符验证错误类型。
#[derive(Debug, Clone, thiserror::Error)]
pub enum IdError {
    /// ID 中包含无效字符。
    #[error("Invalid characters in ID: {0}")]
    InvalidCharacters(String),
    /// ID 中存在路径遍历尝试。
    #[error("Path traversal attempt in ID: {0}")]
    PathTraversal(String),
}

/// 验证会话 ID 是否可安全用于文件系统操作。
///
/// 仅允许字母数字、连字符、下划线和 'T' 字符。
/// 拒绝 `.` 和 `:` 以防止路径遍历攻击。
pub fn validate_session_id(id: &str) -> Result<(), IdError> {
    if id.is_empty() {
        return Err(IdError::InvalidCharacters("empty ID".into()));
    }
    // 检查路径遍历和路径分隔符
    if id.contains("..") || id.contains('/') || id.contains('\\') {
        return Err(IdError::PathTraversal(id.into()));
    }
    // 逐字符检查，仅允许安全字符
    for ch in id.chars() {
        if !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_' && ch != 'T' {
            return Err(IdError::InvalidCharacters(format!(
                "character '{}' not allowed in ID",
                ch
            )));
        }
    }
    Ok(())
}

/// 生成新的唯一会话 ID（基于 UUID v4）。
pub fn new_session_id() -> SessionId {
    SessionId::new(uuid::Uuid::new_v4().to_string())
}

/// 生成新的唯一事件 ID（基于 UUID v4）。
pub fn new_event_id() -> EventId {
    EventId::new(uuid::Uuid::new_v4().to_string())
}

/// 生成新的唯一轮次 ID（基于 UUID v4）。
pub fn new_turn_id() -> TurnId {
    TurnId::new(uuid::Uuid::new_v4().to_string())
}

/// 生成新的唯一消息 ID（基于 UUID v4）。
pub fn new_message_id() -> MessageId {
    MessageId::new(uuid::Uuid::new_v4().to_string())
}

/// 从工作目录路径派生可读的稳定项目标识符。
///
/// 目录名不能直接包含 Windows 路径中的 `:` 和 `\` 等字符。路径分隔符会转换为
/// `-`，路径片段内部只对文件系统不安全字符和 `-` 做百分号编码。
pub fn project_key_from_path(path: &Path) -> ProjectKey {
    let canonical = canonical_project_path(path);
    let display_path = display_project_path(&canonical.to_string_lossy());
    human_project_key(&display_path)
}

fn canonical_project_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn display_project_path(path: &str) -> String {
    if let Some(path) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{path}");
    }
    if let Some(path) = path.strip_prefix(r"\\?\") {
        return path.to_string();
    }
    path.to_string()
}

fn human_project_key(path: &str) -> String {
    let trimmed = path.trim_matches(['/', '\\']);
    let mut parts = Vec::new();
    for (index, raw_part) in trimmed.split(['/', '\\']).enumerate() {
        let part = if index == 0 {
            raw_part.strip_suffix(':').unwrap_or(raw_part)
        } else {
            raw_part
        };
        if !part.is_empty() {
            parts.push(encode_project_path_component(part));
        }
    }
    if parts.is_empty() {
        encode_project_path_component(trimmed)
    } else {
        parts.join("-")
    }
}

fn encode_project_path_component(path: &str) -> String {
    let mut encoded = String::new();
    for ch in path.chars() {
        if is_project_key_component_safe(ch) {
            encoded.push(ch);
        } else {
            push_percent_encoded(&mut encoded, ch);
        }
    }
    encoded
}

fn is_project_key_component_safe(ch: char) -> bool {
    is_project_key_safe(ch) && ch != '-'
}

fn is_project_key_safe(ch: char) -> bool {
    !ch.is_control()
        && !matches!(
            ch,
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '%'
        )
}

fn push_percent_encoded(output: &mut String, ch: char) {
    let mut buffer = [0_u8; 4];
    for byte in ch.encode_utf8(&mut buffer).as_bytes() {
        output.push_str(&format!("%{byte:02X}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_id_serializes_as_plain_string() {
        let session_id = SessionId::from("session-1");

        assert_eq!(serde_json::to_string(&session_id).unwrap(), "\"session-1\"");
        assert_eq!(session_id.to_string(), "session-1");
        assert_eq!(session_id.as_str(), "session-1");
        assert_eq!(session_id.clone().into_string(), "session-1");
    }

    #[test]
    fn project_key_keeps_path_readable_while_encoding_separators() {
        let key = project_key_from_path(Path::new(r"D:\work\astrcode"));

        assert_eq!(key, "D-work-astrcode");
    }

    #[test]
    fn project_key_preserves_unicode_path_segments() {
        let key = project_key_from_path(Path::new(r"D:\简历\astrcode%lab"));

        assert!(key.contains("简历"));
        assert!(key.contains("%25lab"));
    }

    #[test]
    fn project_key_omits_windows_verbatim_prefix() {
        let key = human_project_key(&display_project_path(r"\\?\D:\work\astrcode"));

        assert_eq!(key, "D-work-astrcode");
    }

    #[test]
    fn project_key_encodes_delimiter_inside_path_segments() {
        let key = project_key_from_path(Path::new(r"D:\work-dir\astrcode"));

        assert_eq!(key, "D-work%2Ddir-astrcode");
    }
}
