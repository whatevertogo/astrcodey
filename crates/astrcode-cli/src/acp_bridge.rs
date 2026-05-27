//! Stdio ↔ WebSocket JSON-RPC 桥（无 server 业务逻辑）。
//!
//! IDE 通过子进程 stdio 连接 `astrcode acp`；本模块仅转发帧到本地
//! `astrcode server` 的 `/api/acp/ws`。

use astrcode_support::hostpaths::astrcode_dir;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        client::IntoClientRequest,
        http::header::AUTHORIZATION,
        http::HeaderValue,
        Message,
    },
};

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("cannot read {path}: {source}")]
    ReadRunInfo {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid run.json at {path}: {source}")]
    InvalidRunInfo {
        path: String,
        source: serde_json::Error,
    },
    #[error("server address not configured: start `astrcode server` or pass --server-addr")]
    MissingServerAddr,
    #[error("auth token not configured: pass --auth-token or ensure run.json has authToken")]
    MissingAuthToken,
    #[error("invalid server address `{addr}`: {reason}")]
    InvalidServerAddr { addr: String, reason: String },
    #[error("websocket connect failed: {0}")]
    Connect(String),
    #[error("websocket request invalid: {0}")]
    Request(String),
    #[error("stdio read failed: {0}")]
    Stdin(std::io::Error),
    #[error("stdio write failed: {0}")]
    Stdout(std::io::Error),
    #[error("websocket send failed: {0}")]
    Send(String),
    #[error("websocket receive failed: {0}")]
    Receive(String),
    #[error("ACP bridge expects UTF-8 text JSON-RPC frames; got binary websocket payload")]
    BinaryFrame,
}

#[derive(Clone, Debug)]
pub struct BridgeConfig {
    pub ws_url: String,
    pub auth_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunInfo {
    port: u16,
    auth_token: String,
}

/// 解析连接目标：CLI 参数优先，否则读 `~/.astrcode/run.json`。
pub fn resolve_config(
    server_addr: Option<String>,
    auth_token: Option<String>,
) -> Result<BridgeConfig, BridgeError> {
    if server_addr.is_some() || auth_token.is_some() {
        let ws_url = server_addr
            .map(|addr| ws_acp_url(&addr))
            .transpose()?
            .ok_or(BridgeError::MissingServerAddr)?;
        let auth_token = auth_token.ok_or(BridgeError::MissingAuthToken)?;
        return Ok(BridgeConfig { ws_url, auth_token });
    }

    let path = astrcode_dir().join("run.json");
    let content = std::fs::read_to_string(&path).map_err(|source| BridgeError::ReadRunInfo {
        path: path.display().to_string(),
        source,
    })?;
    let info: RunInfo =
        serde_json::from_str(&content).map_err(|source| BridgeError::InvalidRunInfo {
            path: path.display().to_string(),
            source,
        })?;
    Ok(BridgeConfig {
        ws_url: format!("ws://127.0.0.1:{}/api/acp/ws", info.port),
        auth_token: info.auth_token,
    })
}

/// 双向转发 stdio JSONL ↔ server WebSocket，直到任一侧关闭。
pub async fn run(config: BridgeConfig) -> Result<(), BridgeError> {
    let request = build_ws_request(&config)?;
    let (ws, _) = connect_async(request)
        .await
        .map_err(|error| BridgeError::Connect(error.to_string()))?;
    let (mut ws_tx, mut ws_rx) = ws.split();

    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    loop {
        tokio::select! {
            line = stdin.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        if line.is_empty() {
                            continue;
                        }
                        ws_tx
                            .send(Message::Text(line.into()))
                            .await
                            .map_err(|error| BridgeError::Send(error.to_string()))?;
                    }
                    Ok(None) => break,
                    Err(error) => return Err(BridgeError::Stdin(error)),
                }
            }
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        stdout.write_all(text.as_bytes()).await.map_err(BridgeError::Stdout)?;
                        stdout.write_all(b"\n").await.map_err(BridgeError::Stdout)?;
                        stdout.flush().await.map_err(BridgeError::Stdout)?;
                    }
                    Some(Ok(Message::Binary(_))) => return Err(BridgeError::BinaryFrame),
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(error)) => {
                        return Err(BridgeError::Receive(error.to_string()));
                    }
                }
            }
        }
    }

    Ok(())
}

fn build_ws_request(
    config: &BridgeConfig,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, BridgeError> {
    let mut request = config
        .ws_url
        .as_str()
        .into_client_request()
        .map_err(|error| BridgeError::Request(error.to_string()))?;
    request.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", config.auth_token))
            .map_err(|error| BridgeError::Request(error.to_string()))?,
    );
    Ok(request)
}

fn ws_acp_url(server_addr: &str) -> Result<String, BridgeError> {
    let trimmed = server_addr.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(BridgeError::InvalidServerAddr {
            addr: server_addr.to_string(),
            reason: "empty address".into(),
        });
    }

    if trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        return Ok(append_acp_path(trimmed));
    }

    let (scheme, host) = if let Some(rest) = trimmed.strip_prefix("https://") {
        ("wss", rest)
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        ("ws", rest)
    } else {
        ("ws", trimmed)
    };

    Ok(append_acp_path(&format!("{scheme}://{host}")))
}

fn append_acp_path(base: &str) -> String {
    if base.ends_with("/api/acp/ws") {
        base.to_string()
    } else {
        format!("{base}/api/acp/ws")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_acp_url_from_http_base() {
        assert_eq!(
            ws_acp_url("http://127.0.0.1:3847").unwrap(),
            "ws://127.0.0.1:3847/api/acp/ws"
        );
    }

    #[test]
    fn ws_acp_url_preserves_existing_path() {
        assert_eq!(
            ws_acp_url("ws://127.0.0.1:3847/api/acp/ws").unwrap(),
            "ws://127.0.0.1:3847/api/acp/ws"
        );
    }

    #[test]
    fn resolve_config_from_explicit_args() {
        let config = resolve_config(
            Some("http://127.0.0.1:4000".into()),
            Some("secret".into()),
        )
        .unwrap();
        assert_eq!(config.ws_url, "ws://127.0.0.1:4000/api/acp/ws");
        assert_eq!(config.auth_token, "secret");
    }

    #[test]
    fn resolve_config_requires_token_with_server_addr() {
        let err = resolve_config(Some("http://127.0.0.1:3847".into()), None).unwrap_err();
        assert!(matches!(err, BridgeError::MissingAuthToken));
    }

    #[test]
    fn resolve_config_requires_server_with_token_only() {
        let err = resolve_config(None, Some("secret".into())).unwrap_err();
        assert!(matches!(err, BridgeError::MissingServerAddr));
    }
}
