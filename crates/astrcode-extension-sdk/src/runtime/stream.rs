//! 流式 invoke 事件流。

use tokio::sync::mpsc;

use crate::s5r::{ErrorPayload, EventMsg, EventPhase};

pub type StreamOutput = serde_json::Value;

pub struct EventStream {
    rx: mpsc::UnboundedReceiver<EventMsg>,
}

impl EventStream {
    pub(crate) fn new(rx: mpsc::UnboundedReceiver<EventMsg>) -> Self {
        Self { rx }
    }

    pub async fn next_event(&mut self) -> Option<EventMsg> {
        self.rx.recv().await
    }

    /// 收集流式输出直到 completed/failed。
    pub async fn collect_output(mut self) -> Result<StreamOutput, ErrorPayload> {
        let mut last_output = serde_json::Value::Null;
        while let Some(event) = self.rx.recv().await {
            match event.phase {
                EventPhase::Completed => {
                    if !event.output.is_null() {
                        last_output = event.output;
                    }
                    return Ok(last_output);
                },
                EventPhase::Failed => {
                    return Err(event.error.unwrap_or_else(|| {
                        ErrorPayload::new("stream_failed", "stream failed without error")
                    }));
                },
                EventPhase::Delta => {
                    if !event.data.is_null() {
                        last_output = event.data;
                    }
                },
                EventPhase::Started => {},
            }
        }
        Err(ErrorPayload::new(
            "stream_closed",
            "stream closed before completion",
        ))
    }
}
