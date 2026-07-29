//! [`StorageError`] —— 存储操作错误类型。
//!
//! 从 `storage` 根模块拆出,仅含错误枚举。

use astrcode_core::types::SessionId;

/// 存储操作产生的错误。
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// 找不到指定的会话。
    #[error("Session not found: {0}")]
    NotFound(SessionId),
    #[error("Session already exists: {0}")]
    AlreadyExists(SessionId),
    /// IO 错误。
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// 序列化/反序列化错误。
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    /// 无效的会话 ID。
    #[error("Invalid session ID: {0}")]
    InvalidId(String),
    /// 调用方提交的持久事件不能作为会话的下一条事实。
    #[error("Invalid durable event: {0}")]
    InvalidEvent(String),
    /// 乐观并发写入所依据的 projection 已经过期。
    #[error("Concurrent modification: expected seq {expected_seq}, current seq {current_seq}")]
    ConcurrentModification { expected_seq: u64, current_seq: u64 },
    /// 持久事件流不能构造合法的会话状态。
    #[error("Corrupt session event log: {0}")]
    CorruptLog(String),
    /// 锁操作错误。
    #[error("Lock error: {0}")]
    LockError(String),
    /// 当前存储实现不支持该能力。
    #[error("Unsupported storage operation: {0}")]
    Unsupported(String),
}
