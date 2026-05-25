//! LlmInvokerInner 的服务端实现。
//!
//! 将已有的 `LlmProvider` 适配为 `LlmInvokerInner` 能力接口。

use std::sync::Arc;

use astrcode_core::{
    capability::{LlmInvokerCap, LlmInvokerInner, LlmStreamEvent},
    llm::{LlmContent, LlmMessage, LlmRole},
};
use tokio::sync::mpsc;

/// 将 `LlmProvider` 适配为 `LlmInvokerInner`。
pub struct ServerLlmInvoker {
    provider: Arc<dyn astrcode_core::llm::LlmProvider>,
}

impl ServerLlmInvoker {
    pub fn new(provider: Arc<dyn astrcode_core::llm::LlmProvider>) -> Self {
        Self { provider }
    }

    pub fn as_capability(self: &Arc<Self>) -> Arc<LlmInvokerCap> {
        Arc::new(LlmInvokerCap::new(self.clone()))
    }
}

#[async_trait::async_trait]
impl LlmInvokerInner for ServerLlmInvoker {
    async fn invoke(
        &self,
        system_prompt: String,
        user_prompt: String,
        sink: mpsc::Sender<LlmStreamEvent>,
    ) -> Result<(), String> {
        let messages = vec![
            LlmMessage {
                role: LlmRole::System,
                content: vec![LlmContent::Text { text: system_prompt }],
                name: None,
                reasoning_content: None,
            },
            LlmMessage {
                role: LlmRole::User,
                content: vec![LlmContent::Text { text: user_prompt }],
                name: None,
                reasoning_content: None,
            },
        ];

        let mut rx = self
            .provider
            .generate(messages, vec![])
            .await
            .map_err(|e| e.to_string())?;

        // 桥接 LlmEvent → LlmStreamEvent
        // Fire-and-forget: 流式桥接任务独立运行，sink 关闭时自动退出。
        #[allow(clippy::let_underscore_future)]
        let _ = tokio::spawn(async move {
            use astrcode_core::llm::LlmEvent;
            while let Some(event) = rx.recv().await {
                let stream_event = match event {
                    LlmEvent::ContentDelta { delta } => LlmStreamEvent::ContentDelta { delta },
                    LlmEvent::Done { .. } => {
                        let _ = sink.send(LlmStreamEvent::Done).await;
                        break;
                    },
                    LlmEvent::Error { message } => LlmStreamEvent::Error { message },
                    // 忽略扩展不需要的事件类型
                    LlmEvent::ThinkingDelta { .. }
                    | LlmEvent::ToolCallStart { .. }
                    | LlmEvent::ToolCallDelta { .. } => continue,
                };
                if sink.send(stream_event).await.is_err() {
                    break;
                }
            }
        });

        Ok(())
    }
}
