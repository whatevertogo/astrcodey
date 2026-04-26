//! 协议错误类型
//!
//! 定义插件协议中的错误载荷和错误枚举。
//! `ErrorPayload` 用于在 JSON-RPC 消息中传输结构化错误信息，
//! `ProtocolError` 是本地 Rust 错误类型，用于内部错误处理。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// 结构化错误载荷，用于在 JSON-RPC 消息中传输错误信息。
///
/// `retriable` 字段指示调用方是否可以安全地重试此操作。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ErrorPayload {
    /// 错误码（机器可读）
    pub code: String,
    /// 错误描述（人类可读）
    pub message: String,
    /// 错误详情（结构化数据）
    #[serde(default)]
    pub details: Value,
    /// 是否可以安全重试
    #[serde(default)]
    pub retriable: bool,
}

/// 协议层错误枚举。
///
/// 涵盖协议版本不匹配、消息格式错误、取消、传输关闭等错误场景。
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// 不支持的协议版本
    #[error("unsupported protocol version: {0}")]
    UnsupportedVersion(String),
    /// 消息格式无效
    #[error("invalid message: {0}")]
    InvalidMessage(String),
    /// 请求被取消
    #[error("request cancelled: {0}")]
    Cancelled(String),
    /// 传输通道已关闭
    #[error("transport closed: {0}")]
    TransportClosed(String),
    /// 收到意外的消息类型
    #[error("unexpected message: {0}")]
    UnexpectedMessage(String),
}
