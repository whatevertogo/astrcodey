//! LLM 调用能力。
//!
//! 允许扩展通过 `mpsc::Sender<LlmStreamEvent>` 获取 LLM 流式响应，
//! 无需依赖 `LlmProvider` trait 或 `LlmEvent` 类型。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::Capability;

// ─── LlmStreamEvent ────────────────────────────────────────────────────

/// 扩展可见的 LLM 流式事件。`LlmEvent` 的轻量替代。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LlmStreamEvent {
    /// 文本增量。
    ContentDelta { delta: String },
    /// 流结束。
    Done,
    /// 流错误。
    Error { message: String },
}

// ─── LlmInvokerInner ──────────────────────────────────────────────────

/// LLM 调用的能力接口。由宿主侧实现。
#[async_trait::async_trait]
pub trait LlmInvokerInner: Send + Sync + 'static {
    /// 发起一次 LLM 调用，将流式响应发送到 `sink`。
    ///
    /// `system_prompt` 和 `user_prompt` 是纯文本，实现侧负责包装为 `LlmMessage`。
    /// 返回 `Ok(())` 表示请求已提交，流式结果通过 `sink` 异步返回。
    async fn invoke(
        &self,
        system_prompt: String,
        user_prompt: String,
        sink: mpsc::Sender<LlmStreamEvent>,
    ) -> Result<(), String>;
}

// ─── LlmInvokerCap ────────────────────────────────────────────────────

/// LLM 调用能力的 newtype 包装。
pub struct LlmInvokerCap(Arc<dyn LlmInvokerInner>);

impl LlmInvokerCap {
    pub fn new(inner: Arc<dyn LlmInvokerInner>) -> Self {
        Self(inner)
    }
}

impl Capability for LlmInvokerCap {}

impl LlmInvokerCap {
    /// 发起一次 LLM 调用，将流式响应发送到 `sink`。
    pub async fn invoke(
        &self,
        system_prompt: String,
        user_prompt: String,
        sink: mpsc::Sender<LlmStreamEvent>,
    ) -> Result<(), String> {
        self.0.invoke(system_prompt, user_prompt, sink).await
    }

    /// 发起一次 LLM 调用，收集完整响应文本。
    pub async fn invoke_complete(
        &self,
        system_prompt: String,
        user_prompt: String,
    ) -> Result<String, String> {
        let (tx, mut rx) = mpsc::channel(64);
        self.invoke(system_prompt, user_prompt, tx).await?;
        let mut text = String::new();
        while let Some(event) = rx.recv().await {
            match event {
                LlmStreamEvent::ContentDelta { delta } => text.push_str(&delta),
                LlmStreamEvent::Done => break,
                LlmStreamEvent::Error { message } => return Err(message),
            }
        }
        Ok(text)
    }

    /// 发起一次 LLM 调用，返回流式接收端。
    pub async fn invoke_stream(
        &self,
        system_prompt: String,
        user_prompt: String,
    ) -> Result<mpsc::Receiver<LlmStreamEvent>, String> {
        let (tx, rx) = mpsc::channel(64);
        self.invoke(system_prompt, user_prompt, tx).await?;
        Ok(rx)
    }
}

impl std::fmt::Debug for LlmInvokerCap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmInvokerCap").finish()
    }
}
