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
    pub fn new(base_url: &str, token: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            http: reqwest::Client::new(),
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

    /// 等待 session 完成（轮询 phase 直到 idle 或超时）。
    pub async fn wait_completion(
        &self,
        session_id: &str,
        timeout_secs: u64,
    ) -> Result<(), EvalError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            if tokio::time::Instant::now() >= deadline {
                self.abort(session_id).await.ok();
                return Err(EvalError::Client("timeout".into()));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            match self.get_phase(session_id).await {
                Ok(phase) if phase == "idle" || phase == "error" => return Ok(()),
                Ok(_) => continue,
                Err(_) => continue,
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
        Ok(body["phase"].as_str().unwrap_or("unknown").to_string())
    }
}
