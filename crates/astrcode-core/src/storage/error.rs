//! [`StorageError`] —— 存储操作错误类型。
//!
//! 从 `storage` 根模块拆出,仅含错误枚举。

use crate::types::SessionId;

/// 存储操作产生的错误。
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// 找不到指定的会话。
    #[error("Session not found: {0}")]
    NotFound(SessionId),
    /// IO 错误。
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// 序列化/反序列化错误。
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    /// 无效的会话 ID。
    #[error("Invalid session ID: {0}")]
    InvalidId(String),
    /// 锁操作错误。
    #[error("Lock error: {0}")]
    LockError(String),
    /// 当前存储实现不支持该能力。
    #[error("Unsupported storage operation: {0}")]
    Unsupported(String),
}
