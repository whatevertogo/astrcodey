//! 传输层公共错误类型。
//!
//! 统一客户端和服务端传输层可能产生的错误，避免各 crate 重复定义。

/// 传输层错误类型。
///
/// 涵盖客户端和服务端传输中可能出现的所有错误情况。
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Connection error: {0}")]
    Connection(String),
    #[error("Connection closed")]
    Disconnected,
    #[error("Stream disconnected")]
    StreamDisconnected,
    #[error("Server error: {0}")]
    Server(String),
    #[error("Unexpected response")]
    UnexpectedResponse,
}
