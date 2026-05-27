//! JSON-RPC 2.0 错误对象。
//!
//! 供 MCP 客户端等外部 JSON-RPC 协议解析使用。
//! astrcode 主命令路径已改为进程内 enum + HTTP REST，不再经 stdio JSONL 帧。

use serde::{Deserialize, Serialize};

/// JSON-RPC 错误对象。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}
