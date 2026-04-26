use std::sync::Arc;

use async_trait::async_trait;
use reqwest::{Client as HttpClient, Method};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;

const AUTH_HEADER_NAME: &str = "x-astrcode-token";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportRequest {
    pub method: TransportMethod,
    pub url: String,
    pub auth_token: Option<String>,
    pub query: Vec<(String, String)>,
    pub json_body: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportMethod {
    Get,
    Post,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportResponse {
    pub status: u16,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub id: Option<String>,
    pub event: Option<String>,
    pub data: String,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TransportError {
    #[error("http {status}: {body}")]
    Http { status: u16, body: String },
    #[error("{message}")]
    Network { message: String },
    #[error("{message}")]
    StreamDisconnected { message: String },
    #[error("{message}")]
    UnexpectedResponse { message: String },
}

pub type TransportEventReceiver = mpsc::Receiver<Result<SseEvent, TransportError>>;

#[async_trait]
pub trait ClientTransport: Send + Sync {
    async fn execute(&self, request: TransportRequest)
    -> Result<TransportResponse, TransportError>;

    async fn open_sse(
        &self,
        request: TransportRequest,
        buffer: usize,
    ) -> Result<TransportEventReceiver, TransportError>;
}

#[derive(Debug, Clone)]
pub struct ReqwestTransport {
    client: Arc<HttpClient>,
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestTransport {
    pub fn new() -> Self {
        Self {
            client: Arc::new(HttpClient::new()),
        }
    }

    fn apply_request(&self, request: &TransportRequest) -> reqwest::RequestBuilder {
        let method = match request.method {
            TransportMethod::Get => Method::GET,
            TransportMethod::Post => Method::POST,
        };

        let mut url =
            reqwest::Url::parse(&request.url).expect("client request url should be valid");
        if !request.query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in &request.query {
                pairs.append_pair(key, value);
            }
        }

        let mut builder = self.client.request(method, url);
        if let Some(token) = &request.auth_token {
            builder = builder.header(AUTH_HEADER_NAME, token);
        }
        if let Some(body) = &request.json_body {
            builder = builder.json(body);
        }
        builder
    }
}

#[async_trait]
impl ClientTransport for ReqwestTransport {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        let response =
            self.apply_request(&request)
                .send()
                .await
                .map_err(|error| TransportError::Network {
                    message: format!("request failed: {error}"),
                })?;

        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|error| TransportError::Network {
                message: format!("read response body failed: {error}"),
            })?;

        if !(200..300).contains(&status) {
            return Err(TransportError::Http { status, body });
        }

        Ok(TransportResponse { status, body })
    }

    async fn open_sse(
        &self,
        request: TransportRequest,
        buffer: usize,
    ) -> Result<TransportEventReceiver, TransportError> {
        let response =
            self.apply_request(&request)
                .send()
                .await
                .map_err(|error| TransportError::Network {
                    message: format!("open sse failed: {error}"),
                })?;

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let body = response
                .text()
                .await
                .map_err(|error| TransportError::Network {
                    message: format!("read sse error body failed: {error}"),
                })?;
            return Err(TransportError::Http { status, body });
        }

        let (sender, receiver) = mpsc::channel(buffer.max(1));
        tokio::spawn(async move {
            let mut response = response;
            let mut pending = String::new();

            loop {
                match response.chunk().await {
                    Ok(Some(chunk)) => {
                        pending.push_str(&String::from_utf8_lossy(&chunk));
                        while let Some(raw_event) = drain_next_event(&mut pending) {
                            if raw_event.trim().is_empty() {
                                continue;
                            }
                            let parsed = parse_sse_event(&raw_event);
                            if parsed.data.is_empty()
                                && parsed.event.is_none()
                                && parsed.id.is_none()
                            {
                                continue;
                            }
                            if sender.send(Ok(parsed)).await.is_err() {
                                return;
                            }
                        }
                    },
                    Ok(None) => {
                        let _ = sender
                            .send(Err(TransportError::StreamDisconnected {
                                message: "sse stream closed".to_string(),
                            }))
                            .await;
                        return;
                    },
                    Err(error) => {
                        let _ = sender
                            .send(Err(TransportError::StreamDisconnected {
                                message: format!("sse stream disconnected: {error}"),
                            }))
                            .await;
                        return;
                    },
                }
            }
        });

        Ok(receiver)
    }
}

fn drain_next_event(pending: &mut String) -> Option<String> {
    let normalized = pending.replace("\r\n", "\n");
    if let Some(position) = normalized.find("\n\n") {
        let event = normalized[..position].to_string();
        *pending = normalized[position + 2..].to_string();
        return Some(event);
    }

    if *pending != normalized {
        *pending = normalized;
    }

    None
}

fn parse_sse_event(raw: &str) -> SseEvent {
    let mut id = None;
    let mut event = None;
    let mut data_lines = Vec::new();

    for line in raw.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }

        if let Some(value) = line.strip_prefix("id:") {
            id = Some(value.trim().to_string());
            continue;
        }

        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim().to_string());
            continue;
        }

        if let Some(value) = line.strip_prefix("data:") {
            data_lines.push(value.trim_start().to_string());
        }
    }

    SseEvent {
        id,
        event,
        data: data_lines.join("\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::{SseEvent, drain_next_event, parse_sse_event};

    #[test]
    fn drain_next_event_normalizes_crlf() {
        let mut pending = "id: 1\r\ndata: hello\r\n\r\nid: 2\r\ndata: world".to_string();
        assert_eq!(
            drain_next_event(&mut pending),
            Some("id: 1\ndata: hello".to_string())
        );
        assert_eq!(pending, "id: 2\ndata: world");
    }

    #[test]
    fn parse_sse_event_joins_multiple_data_lines() {
        assert_eq!(
            parse_sse_event("id: 1\nevent: message\ndata: first\ndata: second"),
            SseEvent {
                id: Some("1".to_string()),
                event: Some("message".to_string()),
                data: "first\nsecond".to_string(),
            }
        );
    }
}
