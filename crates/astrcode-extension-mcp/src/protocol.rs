use serde::Deserialize;
use serde_json::{Value, json};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug, Deserialize)]
pub(crate) struct JsonRpcResponse {
    pub(crate) id: Option<u64>,
    #[serde(default)]
    pub(crate) result: Option<Value>,
    #[serde(default)]
    pub(crate) error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct JsonRpcError {
    pub(crate) code: i64,
    pub(crate) message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ListToolsResult {
    #[serde(default)]
    pub(crate) tools: Vec<McpTool>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct McpTool {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    pub(crate) input_schema: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CallToolResult {
    #[serde(default)]
    pub(crate) content: Vec<Value>,
    #[serde(default, rename = "isError")]
    pub(crate) is_error: bool,
    #[serde(default, rename = "structuredContent")]
    pub(crate) structured_content: Option<Value>,
    #[serde(default, rename = "_meta")]
    pub(crate) meta: Option<Value>,
}

pub(crate) fn initialize_request(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "astrcode",
                "version": env!("CARGO_PKG_VERSION"),
            }
        }
    })
}

pub(crate) fn initialized_notification() -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    })
}

pub(crate) fn list_tools_request(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/list",
        "params": {}
    })
}

pub(crate) fn call_tool_request(id: u64, name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": name,
            "arguments": arguments,
        }
    })
}

pub(crate) fn parse_list_tools(result: Value) -> Result<Vec<McpTool>, serde_json::Error> {
    serde_json::from_value::<ListToolsResult>(result).map(|result| result.tools)
}

pub(crate) fn parse_call_tool(result: Value) -> Result<CallToolResult, serde_json::Error> {
    serde_json::from_value(result)
}

pub(crate) fn render_call_content(result: &CallToolResult) -> String {
    let text = result
        .content
        .iter()
        .filter_map(|item| {
            (item.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| item.get("text").and_then(Value::as_str))
                .flatten()
        })
        .collect::<Vec<_>>();
    if !text.is_empty() {
        return text.join("\n");
    }

    if let Some(structured) = &result.structured_content {
        return serde_json::to_string_pretty(structured).unwrap_or_else(|_| structured.to_string());
    }

    if !result.content.is_empty() {
        return serde_json::to_string_pretty(&result.content)
            .unwrap_or_else(|_| Value::Array(result.content.clone()).to_string());
    }

    String::new()
}

pub(crate) fn serialize_line(message: &Value) -> String {
    let mut line = message.to_string();
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_newline_delimited_json_rpc() {
        let line = serialize_line(&list_tools_request(7));

        assert!(line.ends_with('\n'));
        assert!(line.contains("\"method\":\"tools/list\""));
        assert!(line.contains("\"id\":7"));
    }

    #[test]
    fn parses_tool_list() {
        let tools = parse_list_tools(json!({
            "tools": [{
                "name": "echo",
                "description": "Echo text",
                "inputSchema": {"type": "object"}
            }]
        }))
        .unwrap();

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].input_schema, Some(json!({"type": "object"})));
    }

    #[test]
    fn renders_text_content_first() {
        let result = parse_call_tool(json!({
            "content": [{"type": "text", "text": "hello"}],
            "structuredContent": {"ignored": true}
        }))
        .unwrap();

        assert_eq!(render_call_content(&result), "hello");
    }
}
