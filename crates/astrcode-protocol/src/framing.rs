//! JSONL 帧协议模块——基于换行分隔 JSON 的 stdio 传输层。
//!
//! 定义 JSON-RPC 2.0 消息的序列化/反序列化格式，
//! 以及用于 stdio 管道通信的 JSONL（JSON Lines）帧协议。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{commands::ClientCommand, events::ClientNotification};

/// 协议版本号标识符。
pub const PROTOCOL_VERSION: u32 = 1;

/// 线缆上的 JSON-RPC 2.0 帧消息。
///
/// 兼容 JSON-RPC 2.0 规范，支持请求（带 `id` + `method`）、
/// 响应（带 `id` + `result`/`error`）和通知（无 `id`）三种模式。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcMessage {
    /// JSON-RPC 版本，固定为 `"2.0"`。
    pub jsonrpc: String,
    /// 请求/响应的唯一标识符（通知类型为 `None`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    /// 要调用的方法名（仅请求类型有值）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// 方法调用的参数（仅请求类型有值）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
    /// 成功响应的结果（仅响应类型有值）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// 错误响应的详情（仅错误响应有值）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC 错误对象。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// 错误码（遵循 JSON-RPC 2.0 规范的错误码约定）。
    pub code: i32,
    /// 人类可读的错误描述。
    pub message: String,
    /// 附加的错误详情数据。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// 将值序列化为 JSONL 行（JSON 后跟换行符 `\n`）。
pub fn to_jsonl_line<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let json = serde_json::to_string(value)?;
    Ok(format!("{}\n", json))
}

/// 将 JSONL 行反序列化为指定类型的值。
///
/// 会自动去除行首尾的空白字符（包括换行符）。
pub fn from_jsonl_line<T: for<'a> Deserialize<'a>>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim())
}

/// 构造一个成功确认响应消息。
///
/// # 参数
/// - `id`：对应请求的 ID
pub fn ack_message(id: u64) -> JsonRpcMessage {
    JsonRpcMessage {
        jsonrpc: "2.0".into(),
        id: Some(id),
        method: None,
        params: None,
        result: Some(serde_json::json!({"ok": true})),
        error: None,
    }
}

/// 构造一个错误响应消息。
///
/// # 参数
/// - `id`：对应请求的 ID（通知类型无 ID 时传 `None`）
/// - `code`：错误码
/// - `message`：错误描述
pub fn error_message(id: Option<u64>, code: i32, message: &str) -> JsonRpcMessage {
    JsonRpcMessage {
        jsonrpc: "2.0".into(),
        id,
        method: None,
        params: None,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.into(),
            data: None,
        }),
    }
}

/// 将客户端命令包装成 JSON-RPC request。
pub fn command_to_jsonrpc_request(
    command: &ClientCommand,
    id: u64,
) -> Result<JsonRpcMessage, serde_json::Error> {
    let mut value = serde_json::to_value(command)?;
    let Some(object) = value.as_object_mut() else {
        return Ok(JsonRpcMessage {
            jsonrpc: "2.0".into(),
            id: Some(id),
            method: Some("unknown".into()),
            params: None,
            result: None,
            error: None,
        });
    };
    let method = object
        .remove("method")
        .and_then(|value| value.as_str().map(|method| method.to_string()))
        .unwrap_or_else(|| "unknown".into());
    Ok(JsonRpcMessage {
        jsonrpc: "2.0".into(),
        id: Some(id),
        method: Some(method),
        params: object.remove("params"),
        result: None,
        error: None,
    })
}

/// 从 JSON-RPC request 解出客户端命令。
pub fn command_from_jsonrpc_request(
    message: &JsonRpcMessage,
) -> Result<ClientCommand, serde_json::Error> {
    let mut object = Map::new();
    if let Some(method) = &message.method {
        object.insert("method".into(), Value::String(method.clone()));
    }
    if let Some(params) = &message.params {
        object.insert("params".into(), params.clone());
    }
    serde_json::from_value(Value::Object(object))
}

/// 将服务端通知包装成 JSON-RPC notification。
pub fn notification_to_jsonrpc_message(
    notification: &ClientNotification,
) -> Result<JsonRpcMessage, serde_json::Error> {
    let mut value = serde_json::to_value(notification)?;
    let Some(object) = value.as_object_mut() else {
        return Ok(event_message("unknown", &Value::Null));
    };
    let event = object
        .remove("event")
        .and_then(|value| value.as_str().map(|event| event.to_string()))
        .unwrap_or_else(|| "unknown".into());
    Ok(JsonRpcMessage {
        jsonrpc: "2.0".into(),
        id: None,
        method: Some(event),
        params: object.remove("data"),
        result: None,
        error: None,
    })
}

/// 从 JSON-RPC notification 解出服务端通知。
pub fn notification_from_jsonrpc_message(
    message: &JsonRpcMessage,
) -> Result<ClientNotification, serde_json::Error> {
    let mut object = Map::new();
    if let Some(method) = &message.method {
        object.insert("event".into(), Value::String(method.clone()));
    }
    if let Some(params) = &message.params {
        object.insert("data".into(), params.clone());
    }
    serde_json::from_value(Value::Object(object))
}

/// 构造一个服务器事件通知消息（无 ID，属于通知模式）。
///
/// # 参数
/// - `event`：事件名称（作为 `method` 字段）
/// - `data`：事件数据（作为 `params` 字段）
pub fn event_message(event: &str, data: &serde_json::Value) -> JsonRpcMessage {
    JsonRpcMessage {
        jsonrpc: "2.0".into(),
        id: None,
        method: Some(event.into()),
        params: Some(data.clone()),
        result: None,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_trip_jsonl() {
        let msg = ack_message(42);
        let line = to_jsonl_line(&msg).unwrap();
        let parsed: JsonRpcMessage = from_jsonl_line(&line).unwrap();
        assert_eq!(parsed.id, Some(42));
        assert_eq!(parsed.jsonrpc, "2.0");
    }

    #[test]
    fn test_error_message() {
        let msg = error_message(Some(1), -32600, "Invalid Request");
        assert!(msg.error.is_some());
        assert_eq!(msg.error.unwrap().code, -32600);
    }
}
