//! 认证相关 DTO
//!
//! 定义 bootstrap token 与 session token 交换的请求/响应结构。
//! 认证流程：server 启动时生成短期 bootstrap token → 嵌入前端 HTML →
//! 前端用 bootstrap token 换取长期 session token → 后续所有 API 请求使用 session token。

use serde::{Deserialize, Serialize};

/// POST /api/auth/exchange 请求体——用短期 bootstrap token 换取长期 session token。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AuthExchangeRequest {
    /// 短期 bootstrap token（由 server 启动时生成，嵌入到前端 HTML 中）
    pub token: String,
}

/// POST /api/auth/exchange 响应体——返回认证成功后的 session token。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AuthExchangeResponse {
    /// 认证是否成功
    pub ok: bool,
    /// 长期 session token（用于后续所有 API 请求的 Authorization 头）
    pub token: String,
    /// token 过期时间的毫秒时间戳
    pub expires_at_ms: i64,
}
