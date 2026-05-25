use std::time::Duration;

use serde_json::Value;
use tokio::sync::Mutex;

use crate::{
    config::McpServerConfig,
    pool::McpPoolError,
    protocol::{self, CallToolResult, JsonRpcResponse, McpTool},
};

/// Long-lived HTTP MCP connection with its negotiated session.
pub(crate) struct HttpPooledClient {
    url: String,
    headers: Vec<(String, String)>,
    session: HttpSession,
    client: reqwest::Client,
    timeout: Duration,
    request_lock: Mutex<()>,
}

struct HttpSession {
    session_id: Option<String>,
    protocol_version: String,
}

impl HttpPooledClient {
    pub(crate) async fn initialize(
        server: &McpServerConfig,
        timeout: Duration,
    ) -> Result<Self, McpPoolError> {
        let url = server.url.as_deref().unwrap_or("").to_string();
        let headers = server
            .headers
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Vec<_>>();
        let client = reqwest::Client::new();
        let response = post(
            &client,
            &url,
            &headers,
            protocol::initialize_request(1),
            None,
            timeout,
            ResponseKind::Request { expected_id: 1 },
        )
        .await?;
        let initialize =
            protocol::parse_initialize(response.result).map_err(McpPoolError::Result)?;
        let session = HttpSession {
            session_id: response.session_id,
            protocol_version: initialize
                .protocol_version
                .unwrap_or_else(|| protocol::MCP_PROTOCOL_VERSION.to_string()),
        };

        post(
            &client,
            &url,
            &headers,
            protocol::initialized_notification(),
            Some(&session),
            timeout,
            ResponseKind::Notification,
        )
        .await?;

        Ok(Self {
            url,
            headers,
            session,
            client,
            timeout,
            request_lock: Mutex::new(()),
        })
    }

    pub(crate) async fn list_tools(&self) -> Result<Vec<McpTool>, McpPoolError> {
        let _request = self.request_lock.lock().await;
        let result = self.request(protocol::list_tools_request(2), 2).await?;
        protocol::parse_list_tools(result).map_err(McpPoolError::Result)
    }

    pub(crate) async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<CallToolResult, McpPoolError> {
        let _request = self.request_lock.lock().await;
        let result = self
            .request(protocol::call_tool_request(2, tool_name, arguments), 2)
            .await?;
        protocol::parse_call_tool(result).map_err(McpPoolError::Result)
    }

    async fn request(&self, body: Value, expected_id: u64) -> Result<Value, McpPoolError> {
        Ok(post(
            &self.client,
            &self.url,
            &self.headers,
            body,
            Some(&self.session),
            self.timeout,
            ResponseKind::Request { expected_id },
        )
        .await?
        .result)
    }
}

struct RpcResult {
    result: Value,
    session_id: Option<String>,
}

#[derive(Clone, Copy)]
enum ResponseKind {
    Request { expected_id: u64 },
    Notification,
}

async fn post(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    body: Value,
    session: Option<&HttpSession>,
    timeout: Duration,
    kind: ResponseKind,
) -> Result<RpcResult, McpPoolError> {
    let mut request = client.post(url).timeout(timeout).json(&body);
    for (key, value) in headers {
        request = request.header(key.as_str(), value.as_str());
    }
    request = request
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header(
            "MCP-Protocol-Version",
            session
                .map(|session| session.protocol_version.as_str())
                .unwrap_or(protocol::MCP_PROTOCOL_VERSION),
        );
    if let Some(session_id) = session.and_then(|session| session.session_id.as_deref()) {
        request = request.header("Mcp-Session-Id", session_id);
    }

    let response = request.send().await.map_err(|source| {
        if source.is_timeout() {
            McpPoolError::HttpTimeout {
                url: url.to_string(),
            }
        } else {
            McpPoolError::Http {
                message: format!("send request to {url}: {source}"),
            }
        }
    })?;
    let status = response.status();
    let session_id = response
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string);
    let content_type = response
        .headers()
        .get("Content-Type")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = response.text().await.map_err(|source| McpPoolError::Http {
        message: format!("read response body: {source}"),
    })?;

    Ok(RpcResult {
        result: parse_response(url, status, content_type.as_deref(), &body, kind)?,
        session_id,
    })
}

fn parse_response(
    url: &str,
    status: reqwest::StatusCode,
    content_type: Option<&str>,
    body: &str,
    kind: ResponseKind,
) -> Result<Value, McpPoolError> {
    if matches!(kind, ResponseKind::Notification) && status == reqwest::StatusCode::ACCEPTED {
        return Ok(Value::Null);
    }
    if !status.is_success() {
        return Err(McpPoolError::Http {
            message: format!("HTTP {status} from {url}; body: {body}"),
        });
    }
    if body.trim().is_empty() {
        return match kind {
            ResponseKind::Notification => Ok(Value::Null),
            ResponseKind::Request { .. } => Err(McpPoolError::Http {
                message: format!("empty JSON-RPC response body from {url}"),
            }),
        };
    }
    match content_type.map(str::to_ascii_lowercase) {
        Some(content_type) if content_type.starts_with("text/event-stream") => {
            parse_sse(url, body, kind)
        },
        _ => parse_json(url, body, kind),
    }
}

fn parse_json(url: &str, body: &str, kind: ResponseKind) -> Result<Value, McpPoolError> {
    let response: JsonRpcResponse =
        serde_json::from_str(body).map_err(|source| McpPoolError::Http {
            message: format!("parse JSON-RPC response from {url}: {source}; body: {body}"),
        })?;
    response_result(response, kind)
}

fn parse_sse(url: &str, body: &str, kind: ResponseKind) -> Result<Value, McpPoolError> {
    let mut data_lines = Vec::new();
    for line in body.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            if let Some(result) = parse_sse_event(url, &data_lines, kind)? {
                return Ok(result);
            }
            data_lines.clear();
            continue;
        }
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim_start().to_string());
        }
    }
    if let Some(result) = parse_sse_event(url, &data_lines, kind)? {
        return Ok(result);
    }
    Err(McpPoolError::Http {
        message: format!("SSE response from {url} did not contain the expected JSON-RPC response"),
    })
}

fn parse_sse_event(
    url: &str,
    data_lines: &[String],
    kind: ResponseKind,
) -> Result<Option<Value>, McpPoolError> {
    if data_lines.is_empty() {
        return Ok(None);
    }
    let data = data_lines.join("\n");
    let response: JsonRpcResponse =
        serde_json::from_str(&data).map_err(|source| McpPoolError::Http {
            message: format!("parse SSE JSON-RPC response from {url}: {source}; data: {data}"),
        })?;
    match kind {
        ResponseKind::Request { expected_id } if response.id != Some(expected_id) => Ok(None),
        _ => response_result(response, kind).map(Some),
    }
}

fn response_result(response: JsonRpcResponse, kind: ResponseKind) -> Result<Value, McpPoolError> {
    if let Some(error) = response.error {
        return Err(McpPoolError::Rpc {
            code: error.code,
            message: error.message,
            stderr: String::new(),
        });
    }
    if let ResponseKind::Request { expected_id } = kind {
        if response.id != Some(expected_id) {
            return Err(McpPoolError::MismatchedResponse {
                expected: expected_id,
                actual: response.id,
                stderr: String::new(),
            });
        }
    }
    Ok(response.result.unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn accepts_empty_initialized_notification_response() {
        assert_eq!(
            parse_response(
                "http://localhost/mcp",
                reqwest::StatusCode::ACCEPTED,
                None,
                "",
                ResponseKind::Notification,
            )
            .unwrap(),
            Value::Null
        );
    }

    #[test]
    fn sse_request_skips_notifications_before_matching_response() {
        let body = format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"jsonrpc":"2.0","method":"notifications/progress","params":{}}),
            json!({"jsonrpc":"2.0","id":2,"result":{"tools":[]}})
        );

        assert_eq!(
            parse_response(
                "http://localhost/mcp",
                reqwest::StatusCode::OK,
                Some("text/event-stream"),
                &body,
                ResponseKind::Request { expected_id: 2 },
            )
            .unwrap(),
            json!({"tools": []})
        );
    }
}
