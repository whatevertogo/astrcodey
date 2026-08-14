//! JSONL 帧协议模块——基于换行分隔 JSON 的 stdio 传输层。
//!
//! 定义 JSON-RPC 2.0 消息的序列化/反序列化格式，
//! 以及用于 stdio 管道通信的 JSONL（JSON Lines）帧协议。

use serde::{Deserialize, Serialize, ser::Error as _};
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
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcMessage {
    fn new() -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: None,
            method: None,
            params: None,
            result: None,
            error: None,
        }
    }
}

/// JSON-RPC 错误对象。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// 将值序列化为 JSONL 行（JSON 后跟换行符 `\n`）。
pub fn to_jsonl_line<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let mut json = serde_json::to_string(value)?;
    json.push('\n');
    Ok(json)
}

/// 将 JSONL 行反序列化为指定类型的值。
///
/// 会自动去除行首尾的空白字符（包括换行符）。
pub fn from_jsonl_line<T: for<'a> Deserialize<'a>>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim())
}

/// 构造一个错误响应消息。
pub fn error_message(id: Option<u64>, code: i32, message: &str) -> JsonRpcMessage {
    let mut msg = JsonRpcMessage::new();
    msg.id = id;
    msg.error = Some(JsonRpcError {
        code,
        message: message.into(),
        data: None,
    });
    msg
}

/// 将客户端命令包装成 JSON-RPC request。
pub fn command_to_jsonrpc_request(
    command: &ClientCommand,
    id: u64,
) -> Result<JsonRpcMessage, serde_json::Error> {
    let mut value = serde_json::to_value(command)?;
    let Some(object) = value.as_object_mut() else {
        return Err(serde_json::Error::custom(
            "command did not serialize to a JSON object",
        ));
    };
    let method = object
        .remove("method")
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .ok_or_else(|| serde_json::Error::custom("command method tag is missing or invalid"))?;
    let mut msg = JsonRpcMessage::new();
    msg.id = Some(id);
    msg.method = Some(method);
    msg.params = object.remove("params");
    Ok(msg)
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
        return Err(serde_json::Error::custom(
            "notification did not serialize to a JSON object",
        ));
    };
    let event = object
        .remove("event")
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .ok_or_else(|| serde_json::Error::custom("notification event tag is missing or invalid"))?;
    let mut msg = JsonRpcMessage::new();
    msg.method = Some(event);
    msg.params = object.remove("data");
    Ok(msg)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_trip_jsonl() {
        let msg = error_message(Some(42), -32600, "Invalid Request");
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
