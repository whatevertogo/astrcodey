//! 核心共享标识符和数据类型。
//!
//! 这些类型在 astrcode 平台的所有 crate 中通用。
//!
//! 本模块定义了：
//! - 各种 ID 类型别名（[`SessionId`]、[`EventId`]、[`TurnId`] 等）
//! - ID 验证函数 [`validate_session_id`]
//! - ID 生成函数 [`new_session_id`]、[`new_event_id`] 等
//! - 项目哈希计算函数 [`project_hash_from_path`]

use std::path::PathBuf;

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

/// 项目标识符，从工作目录路径派生。
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

/// 从工作目录路径派生稳定的项目哈希值。
///
/// 使用 SHA-256 对规范化路径进行哈希，确保跨 Rust 版本和平台的稳定性。
/// 截断为 16 个十六进制字符以提高可读性。
pub fn project_hash_from_path(path: &PathBuf) -> ProjectHash {
    use sha2::{Digest, Sha256};
    // 获取规范路径，失败时回退到原始路径
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    // 取哈希前 8 字节（16 个十六进制字符）
    format!("{:016x}", hasher.finalize())
}
