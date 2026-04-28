//! 协议版本协商模块。
//!
//! 定义客户端/服务器握手时的版本交换类型，
//! 以及版本协商算法（选择双方都支持的最高版本）。

use serde::{Deserialize, Serialize};

/// 客户端发起的初始化握手请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeRequest {
    /// 客户端期望使用的协议版本号。
    pub protocol_version: u32,
    /// 客户端身份信息。
    pub client_info: ClientInfo,
}

/// 服务器对初始化请求的响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResponse {
    /// 服务器最终接受的协议版本号。
    pub accepted_version: u32,
    /// 服务器身份信息。
    pub server_info: ServerInfo,
}

/// 客户端身份信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    /// 客户端名称（如 `astrcode-tui`）。
    pub name: String,
    /// 客户端版本号（语义化版本字符串）。
    pub version: String,
}

/// 服务器身份信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    /// 服务器名称（如 `astrcode-server`）。
    pub name: String,
    /// 服务器版本号。
    pub version: String,
    /// 服务器支持的所有协议版本号列表。
    pub protocol_versions: Vec<u32>,
    /// 服务器声明的能力标志。
    pub capabilities: ServerCapabilities,
}

/// 服务器能力标志集合。
///
/// 客户端可据此判断服务器支持哪些特性，从而调整行为。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerCapabilities {
    /// 是否支持流式响应。
    pub streaming: bool,
    /// 是否支持会话分叉。
    pub session_fork: bool,
    /// 是否支持上下文压缩。
    pub compaction: bool,
    /// 是否支持扩展插件。
    pub extensions: bool,
}

/// 在客户端和服务器之间协商协议版本。
///
/// 优先返回客户端请求的版本；若服务器不支持该版本，
/// 则返回双方都支持的最高版本；若完全不兼容则返回 `None`。
///
/// # 参数
/// - `client_requested`：客户端请求的协议版本号
/// - `server_supported`：服务器支持的版本号列表
///
/// # 返回值
/// - `Some(version)`：协商成功的版本号
/// - `None`：双方无兼容版本
pub fn negotiate_version(client_requested: u32, server_supported: &[u32]) -> Option<u32> {
    // 客户端请求的版本恰好被服务器支持，直接使用
    if server_supported.contains(&client_requested) {
        return Some(client_requested);
    }
    // 否则在服务器支持的版本中，找到不超过客户端请求版本的最高版本
    server_supported
        .iter()
        .copied()
        .filter(|v| *v <= client_requested)
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_negotiate_exact_match() {
        let result = negotiate_version(1, &[1, 2]);
        assert_eq!(result, Some(1));
    }

    #[test]
    fn test_negotiate_highest_compatible() {
        let result = negotiate_version(3, &[1, 2]);
        assert_eq!(result, Some(2));
    }

    #[test]
    fn test_negotiate_incompatible() {
        let result = negotiate_version(1, &[2, 3]);
        assert_eq!(result, None);
    }
}
