//! 轻量 HTTP 客户端，封装 server API 调用。

use std::time::Duration;

use astrcode_protocol::{
    http::{
        ConversationSnapshotResponseDto, CreateSessionRequest, CreateSessionResponseDto,
        PromptRequest, PromptSubmitResponse,
    },
    wire::PhaseDto,
};
use serde::de::DeserializeOwned;

use crate::EvalError;

/// Eval 专用 HTTP 客户端。
pub struct EvalClient {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl EvalClient {
    pub fn new(base_url: &str, token: &str) -> Result<Self, EvalError> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| EvalError::Client(format!("build eval HTTP client: {error}")))?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            http,
        })
    }

    /// 检查 server 配置端点是否可认证访问。
    pub async fn health_check(&self) -> Result<(), EvalError> {
        let response = self
            .http
            .get(format!("{}/api/config", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|error| EvalError::Client(format!("health check request: {error}")))?;
        ensure_success(response, "health check").await?;
        Ok(())
    }

    /// 创建 session，返回 session_id。
    pub async fn create_session(&self, working_dir: &str) -> Result<String, EvalError> {
        let response = self
            .http
            .post(format!("{}/api/sessions", self.base_url))
            .bearer_auth(&self.token)
            .json(&CreateSessionRequest {
                working_dir: working_dir.to_string(),
                tool_selection: None,
            })
            .send()
            .await
            .map_err(|error| EvalError::Client(format!("create session request: {error}")))?;
        let body: CreateSessionResponseDto =
            decode_json_response(response, "create session").await?;
        Ok(body.session_id)
    }

    /// 提交 prompt。
    pub async fn submit_prompt(&self, session_id: &str, text: &str) -> Result<(), EvalError> {
        let response = self
            .http
            .post(format!(
                "{}/api/sessions/{}/prompt",
                self.base_url, session_id
            ))
            .bearer_auth(&self.token)
            .json(&PromptRequest {
                text: text.to_string(),
                attachments: Vec::new(),
            })
            .send()
            .await
            .map_err(|error| EvalError::Client(format!("submit prompt request: {error}")))?;
        let _: PromptSubmitResponse = decode_json_response(response, "submit prompt").await?;
        Ok(())
    }

    /// 等待 session 完成（轮询 phase 直到 idle）。
    ///
    /// `timeout_secs` 为 `0` 时无限等待；正数表示到期后中止 session。
    pub async fn wait_completion(
        &self,
        session_id: &str,
        timeout_secs: u64,
    ) -> Result<(), EvalError> {
        let deadline = (timeout_secs > 0)
            .then(|| tokio::time::Instant::now() + Duration::from_secs(timeout_secs));
        let mut consecutive_errors = 0;
        loop {
            if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                if let Err(error) = self.abort(session_id).await {
                    return Err(EvalError::Client(format!(
                        "timeout; aborting session {session_id} also failed: {error}"
                    )));
                }
                return Err(EvalError::Client("timeout".into()));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            match self.get_phase(session_id).await {
                Ok(PhaseDto::Idle) => return Ok(()),
                Ok(PhaseDto::Error) => {
                    return Err(EvalError::Client("session entered error phase".into()));
                },
                Ok(_) => consecutive_errors = 0,
                Err(error) => {
                    consecutive_errors += 1;
                    if consecutive_errors >= 5 {
                        return Err(EvalError::Client(format!(
                            "lost contact with session after {consecutive_errors} attempts: \
                             {error}"
                        )));
                    }
                },
            }
        }
    }

    /// 中止 session。
    pub async fn abort(&self, session_id: &str) -> Result<(), EvalError> {
        let response = self
            .http
            .post(format!(
                "{}/api/sessions/{}/abort",
                self.base_url, session_id
            ))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|error| EvalError::Client(format!("abort session request: {error}")))?;
        ensure_success(response, "abort session").await?;
        Ok(())
    }

    /// 获取当前 phase。
    async fn get_phase(&self, session_id: &str) -> Result<PhaseDto, EvalError> {
        let response = self
            .http
            .get(format!(
                "{}/api/sessions/{}/conversation",
                self.base_url, session_id
            ))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|error| {
                EvalError::Client(format!("get conversation phase request: {error}"))
            })?;
        let body: ConversationSnapshotResponseDto =
            decode_json_response(response, "get conversation phase").await?;
        Ok(body.phase)
    }
}

async fn decode_json_response<T>(
    response: reqwest::Response,
    operation: &str,
) -> Result<T, EvalError>
where
    T: DeserializeOwned,
{
    ensure_success(response, operation)
        .await?
        .json()
        .await
        .map_err(|error| {
            EvalError::Client(format!(
                "{operation} returned an invalid response body: {error}"
            ))
        })
}

async fn ensure_success(
    response: reqwest::Response,
    operation: &str,
) -> Result<reqwest::Response, EvalError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let body = response.text().await.map_err(|error| {
        EvalError::Client(format!(
            "{operation} failed with HTTP {status}; response body could not be read: {error}"
        ))
    })?;
    let detail = if body.trim().is_empty() {
        "empty response body"
    } else {
        body.trim()
    };
    Err(EvalError::Client(format!(
        "{operation} failed with HTTP {status}: {detail}"
    )))
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    use super::*;

    #[tokio::test]
    async fn abort_reports_unsuccessful_status_and_server_detail() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 2048];
            let size = stream.read(&mut request).unwrap();
            let request = std::str::from_utf8(&request[..size]).unwrap();
            assert!(request.starts_with("POST /api/sessions/session-1/abort HTTP/1.1"));

            let body = r#"{"code":"abort_failed","message":"no active turn"}"#;
            write!(
                stream,
                "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: \
                 {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let client = EvalClient::new(&format!("http://{address}"), "token").unwrap();
        let error = client.abort("session-1").await.unwrap_err().to_string();

        assert!(error.contains("abort session"));
        assert!(error.contains("409 Conflict"));
        assert!(error.contains("no active turn"));
        server.join().unwrap();
    }
}
