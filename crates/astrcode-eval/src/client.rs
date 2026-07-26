//! 轻量 HTTP 客户端，封装 server API 调用。

use std::time::Duration;

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
            .map_err(|error| EvalError::Server(format!("health check request: {error}")))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(EvalError::Server(format!(
                "server health check failed: {}",
                response.status()
            )))
        }
    }

    /// 创建 session，返回 session_id。
    pub async fn create_session(&self, working_dir: &str) -> Result<String, EvalError> {
        let resp = self
            .http
            .post(format!("{}/api/sessions", self.base_url))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "workingDir": working_dir }))
            .send()
            .await
            .map_err(|e| EvalError::Client(format!("create_session: {e}")))?;
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| EvalError::Client(format!("create_session body: {e}")))?;
        body["sessionId"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| EvalError::Client("missing sessionId in response".into()))
    }

    /// 提交 prompt。
    pub async fn submit_prompt(&self, session_id: &str, text: &str) -> Result<(), EvalError> {
        let resp = self
            .http
            .post(format!(
                "{}/api/sessions/{}/prompt",
                self.base_url, session_id
            ))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "text": text }))
            .send()
            .await
            .map_err(|e| EvalError::Client(format!("submit_prompt: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(EvalError::Client(format!("submit_prompt {status}: {body}")));
        }
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
                self.abort(session_id).await.ok();
                return Err(EvalError::Client("timeout".into()));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            match self.get_phase(session_id).await {
                Ok(phase) if phase == "idle" => return Ok(()),
                Ok(phase) if phase == "error" => {
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
        self.http
            .post(format!(
                "{}/api/sessions/{}/abort",
                self.base_url, session_id
            ))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| EvalError::Client(format!("abort: {e}")))?;
        Ok(())
    }

    /// 获取当前 phase。
    async fn get_phase(&self, session_id: &str) -> Result<String, EvalError> {
        let resp = self
            .http
            .get(format!(
                "{}/api/sessions/{}/conversation",
                self.base_url, session_id
            ))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| EvalError::Client(format!("get_phase: {e}")))?;
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| EvalError::Client(format!("get_phase body: {e}")))?;
        body["phase"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| EvalError::Client("conversation response is missing phase".into()))
    }
}
