//! JSONL framing: newline-delimited JSON for stdio transport.

use serde::{Deserialize, Serialize};

/// Protocol version identifier.
pub const PROTOCOL_VERSION: u32 = 1;

/// A framed message on the wire.
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Serialize a value as a JSONL line (JSON followed by `\n`).
pub fn to_jsonl_line<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let json = serde_json::to_string(value)?;
    Ok(format!("{}\n", json))
}

/// Parse a JSONL line into a value.
pub fn from_jsonl_line<T: for<'a> Deserialize<'a>>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim())
}

/// Write an acknowledgment message.
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

/// Write an error message.
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

/// Write an event (server notification).
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
