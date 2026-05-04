//! 核心共享标识符和数据类型。
//!
//! 这些类型在 astrcode 平台的所有 crate 中通用。
//!
//! 本模块定义了：
//! - 各种 ID 类型别名（[`SessionId`]、[`EventId`]、[`TurnId`] 等）
//! - ID 验证函数 [`validate_session_id`]
//! - ID 生成函数 [`new_session_id`]、[`new_event_id`] 等
//! - 项目标识符派生函数 [`project_key_from_path`] 和 [`project_hash_from_path`]

use std::path::{Path, PathBuf};

/// 会话的唯一标识符。
///
/// 会话是基于事件溯源的持久化工作单元，
/// 所有 Agent 交互都在会话中发生。
pub type SessionId = String;

/// 会话事件日志中事件的唯一标识符。
pub type EventId = String;

/// 轮次的唯一标识符（一个"用户提示 + Agent 回复"的交互周期）。
pub type TurnId = String;

/// 轮次内消息（用户或助手）的唯一标识符。
pub type MessageId = String;

/// 轮次内工具调用的唯一标识符。
pub type ToolCallId = String;

/// 会话事件日志中的位置游标。
/// 对客户端不透明；服务器用于分页和恢复。
pub type Cursor = String;

/// 项目标识符，从工作目录路径派生，可安全作为单个目录名。
pub type ProjectKey = String;

/// 旧版项目哈希标识符。
pub type ProjectHash = String;

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
    uuid::Uuid::new_v4().to_string()
}

/// 生成新的唯一事件 ID（基于 UUID v4）。
pub fn new_event_id() -> EventId {
    uuid::Uuid::new_v4().to_string()
}

/// 生成新的唯一轮次 ID（基于 UUID v4）。
pub fn new_turn_id() -> TurnId {
    uuid::Uuid::new_v4().to_string()
}

/// 生成新的唯一消息 ID（基于 UUID v4）。
pub fn new_message_id() -> MessageId {
    uuid::Uuid::new_v4().to_string()
}

/// 从工作目录路径派生可读的稳定项目标识符。
///
/// 目录名不能直接包含 Windows 路径中的 `:` 和 `\` 等字符，因此只对文件系统
/// 不安全字符做百分号编码，尽量保留原始路径的可读性。
pub fn project_key_from_path(path: &Path) -> ProjectKey {
    let canonical = canonical_project_path(path);
    let display_path = display_project_path(&canonical.to_string_lossy());
    encode_project_path(&display_path)
}

/// 从工作目录路径派生旧版稳定项目哈希值。
///
/// 使用 SHA-256 对规范化路径进行哈希，确保跨 Rust 版本和平台的稳定性。
pub fn project_hash_from_path(path: &Path) -> ProjectHash {
    use sha2::{Digest, Sha256};
    let canonical = canonical_project_path(path);
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    format!("{:016x}", hasher.finalize())
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

fn encode_project_path(path: &str) -> String {
    let mut encoded = String::new();
    for ch in path.chars() {
        if is_project_key_safe(ch) {
            encoded.push(ch);
        } else {
            push_percent_encoded(&mut encoded, ch);
        }
    }
    encoded
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
    fn project_key_keeps_path_readable_while_encoding_separators() {
        let key = project_key_from_path(Path::new(r"D:\work\astrcode"));

        assert_eq!(key, "D%3A%5Cwork%5Castrcode");
    }

    #[test]
    fn project_key_preserves_unicode_path_segments() {
        let key = project_key_from_path(Path::new(r"D:\简历\astrcode%lab"));

        assert!(key.contains("简历"));
        assert!(key.contains("%25lab"));
    }

    #[test]
    fn project_key_omits_windows_verbatim_prefix() {
        let key = encode_project_path(&display_project_path(r"\\?\D:\work\astrcode"));

        assert_eq!(key, "D%3A%5Cwork%5Castrcode");
    }

    #[test]
    fn project_hash_stays_opaque_for_legacy_lookup() {
        let hash = project_hash_from_path(Path::new(r"D:\work\astrcode"));

        assert!(hash.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_ne!(hash, project_key_from_path(Path::new(r"D:\work\astrcode")));
    }
}
