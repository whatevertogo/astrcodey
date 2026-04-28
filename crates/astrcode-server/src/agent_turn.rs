//! Agent 回合执行器 — 共享 handler 和 session_spawner 之间的 select 循环。
//!
//! 提供一个通用的 [`drive_agent`] 函数，统一管理事件通道的创建、
//! tokio::select! 和 drain 阶段，消除两处的重复代码。

use std::future::Future;

use astrcode_core::{event::EventPayload, llm::LlmMessage};
use tokio::sync::mpsc;

use crate::agent::{Agent, AgentError, AgentTurnOutput};

/// 运行 agent 的一次 process_prompt，通过 select! + drain 实时处理事件。
///
/// `on_event` 在每个事件到达时被调用（包含 select 阶段和 drain 阶段）。
/// 返回 `(output, emitted_error)`。
pub(crate) async fn drive_agent<F, Fut>(
    agent: &Agent,
    user_text: &str,
    history: Vec<LlmMessage>,
    mut on_event: F,
) -> (Result<AgentTurnOutput, AgentError>, bool)
where
    F: FnMut(EventPayload) -> Fut,
    Fut: Future<Output = ()>,
{
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let agent_future = agent.process_prompt(user_text, history, Some(event_tx));
    tokio::pin!(agent_future);

    let mut emitted_error = false;
    let mut events_closed = false;
    let output = loop {
        tokio::select! {
            result = &mut agent_future => break result,
            payload = event_rx.recv(), if !events_closed => {
                match payload {
                    Some(payload) => {
                        if matches!(payload, EventPayload::ErrorOccurred { .. }) {
                            emitted_error = true;
                        }
                        on_event(payload).await;
                    },
                    None => events_closed = true,
                }
            },
        }
    };

    while let Some(payload) = event_rx.recv().await {
        if matches!(payload, EventPayload::ErrorOccurred { .. }) {
            emitted_error = true;
        }
        on_event(payload).await;
    }

    (output, emitted_error)
}
