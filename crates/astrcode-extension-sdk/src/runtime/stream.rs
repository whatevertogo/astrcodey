//! 流式 invoke 事件流。

use astrcode_core::wire::WireErrorCode;
use tokio::sync::mpsc;

use crate::s5r::{ErrorPayload, EventMsg, EventPhase};

pub type StreamOutput = serde_json::Value;

pub struct EventStream {
    rx: mpsc::Receiver<EventMsg>,
    cleanup: Option<Box<dyn FnOnce(bool) + Send>>,
}

impl EventStream {
    pub(crate) fn new(rx: mpsc::Receiver<EventMsg>, cleanup: Box<dyn FnOnce(bool) + Send>) -> Self {
        Self {
            rx,
            cleanup: Some(cleanup),
        }
    }

    pub async fn next_event(&mut self) -> Option<EventMsg> {
        let event = self.rx.recv().await;
        match event.as_ref().map(|event| &event.phase) {
            Some(EventPhase::Completed | EventPhase::Failed) => self.finish(true),
            None => self.finish(false),
            Some(EventPhase::Started | EventPhase::Delta) => {},
        }
        event
    }

    /// 收集流式输出直到 completed/failed。
    pub async fn collect_output(mut self) -> Result<StreamOutput, ErrorPayload> {
        let mut last_output = serde_json::Value::Null;
        while let Some(event) = self.next_event().await {
            match event.phase {
                EventPhase::Completed => {
                    if !event.output.is_null() {
                        last_output = event.output;
                    }
                    return Ok(last_output);
                },
                EventPhase::Failed => {
                    return Err(event.error.unwrap_or_else(|| {
                        ErrorPayload::new(
                            WireErrorCode::StreamFailed,
                            "stream failed without error",
                        )
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
            WireErrorCode::StreamClosed,
            "stream closed before completion",
        ))
    }

    fn finish(&mut self, completed: bool) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup(completed);
        }
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.finish(false);
    }
}
