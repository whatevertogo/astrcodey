//! 客户端错误类型定义。
//!
//! 涵盖传输层错误、服务端错误、认证错误、序列化错误等客户端可能遇到的异常情况。

use crate::transport::TransportError;

/// 客户端操作中可能产生的错误。
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// 底层传输层错误（IO、连接断开等）。
    #[error("Transport error: {0}")]
    Transport(#[from] TransportError),
    /// 服务端返回的业务错误消息。
    #[error("Server error: {0}")]
    Server(String),
    /// 服务端返回了不符合预期的响应类型。
    #[error("Unexpected response from server")]
    UnexpectedResponse,
    /// 认证令牌已过期。
    #[error("Auth expired")]
    AuthExpired,
    /// 认证被拒绝。
    #[error("Auth denied")]
    AuthDenied,
    /// 指定的会话不存在。
    #[error("Session not found")]
    SessionNotFound,
    /// 事件流连接已断开。
    #[error("Stream disconnected")]
    StreamDisconnected,
    /// JSON 序列化/反序列化错误。
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}
