//! 无头执行模式 —— 单次提示执行（进程内）。
//!
//! 该模块实现了 CLI 的 `exec` 子命令，用于在不需要交互式 TUI 的情况下
//! 一次性提交提示并输出结果。支持纯文本和 JSONL 两种输出格式。

use std::io::Write;

use astrcode_client::{client::AstrcodeClient, error::ClientError};
use astrcode_core::event::EventPayload;
use astrcode_protocol::{commands::ClientCommand, events::ClientNotification};
use thiserror::Error;

use crate::transport::InProcessTransport;

#[derive(Debug, Error)]
pub enum ExecError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("exec timed out after {0}s")]
    Timeout(u64),
    #[error("write stdout: {0}")]
    WriteStdout(#[from] std::io::Error),
    #[error("serialize jsonl: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotificationAction {
    Continue,
    Finish,
}

/// 执行单次提示并等待响应完成。
pub async fn run(prompt: &str, jsonl: bool, timeout_secs: u64) -> Result<(), ExecError> {
    let client = AstrcodeClient::new(InProcessTransport::start());

    let _sid = client.create_session(".").await?;

    let mut stream = client.subscribe_events().await?;

    client
        .send_command(&ClientCommand::SubmitPrompt {
            text: prompt.into(),
            attachments: vec![],
        })
        .await?;

    let deadline = (timeout_secs > 0)
        .then(|| tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs));

    loop {
        let recv_result = if let Some(deadline) = deadline {
            tokio::time::timeout_at(deadline, stream.recv())
                .await
                .map_err(|_| ExecError::Timeout(timeout_secs))?
        } else {
            stream.recv().await
        };
        let notification = match recv_result {
            Ok(n) => n,
            Err(_) => break,
        };
        let action = render_notification(
            &notification,
            jsonl,
            &mut std::io::stdout(),
            &mut std::io::stderr(),
        )?;
        if action == NotificationAction::Finish {
            break;
        }
    }
    Ok(())
}

fn render_notification(
    notification: &ClientNotification,
    jsonl: bool,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<NotificationAction, ExecError> {
    if jsonl {
        write_jsonl(notification, out)?;
        return Ok(notification_action(notification));
    }

    match notification {
        ClientNotification::Event(core_event) => match &core_event.payload {
            EventPayload::AssistantTextDelta { delta, .. } => {
                write!(out, "{delta}")?;
                Ok(NotificationAction::Continue)
            },
            EventPayload::TurnCompleted { .. } => {
                writeln!(out)?;
                Ok(NotificationAction::Finish)
            },
            EventPayload::ErrorOccurred { message, .. } => {
                writeln!(err, "Error: {message}")?;
                Ok(NotificationAction::Finish)
            },
            _ => Ok(NotificationAction::Continue),
        },
        ClientNotification::Error { message, .. } => {
            writeln!(err, "Error: {message}")?;
            Ok(NotificationAction::Finish)
        },
        _ => Ok(NotificationAction::Continue),
    }
}

fn write_jsonl(notification: &ClientNotification, out: &mut impl Write) -> Result<(), ExecError> {
    serde_json::to_writer(&mut *out, notification)?;
    writeln!(out)?;
    Ok(())
}

fn notification_action(notification: &ClientNotification) -> NotificationAction {
    match notification {
        ClientNotification::Event(core_event) => match core_event.payload {
            EventPayload::TurnCompleted { .. } | EventPayload::ErrorOccurred { .. } => {
                NotificationAction::Finish
            },
            _ => NotificationAction::Continue,
        },
        ClientNotification::Error { .. } => NotificationAction::Finish,
        _ => NotificationAction::Continue,
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        event::{Event, EventPayload},
        types::SessionId,
    };

    use super::*;

    fn notification(payload: EventPayload) -> ClientNotification {
        ClientNotification::Event(Event::new(SessionId::from("session-1"), None, payload))
    }

    #[test]
    fn jsonl_output_includes_streaming_delta() {
        let notification = notification(EventPayload::AssistantTextDelta {
            message_id: "message-1".into(),
            delta: "hello".into(),
        });
        let mut out = Vec::new();
        let mut err = Vec::new();

        let action = render_notification(&notification, true, &mut out, &mut err).unwrap();

        let line: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(line["event"], "event");
        assert_eq!(line["data"]["payload"]["type"], "assistant_text_delta");
        assert_eq!(line["data"]["payload"]["delta"], "hello");
        assert!(err.is_empty());
        assert_eq!(action, NotificationAction::Continue);
    }

    #[test]
    fn jsonl_output_includes_turn_completion_before_finishing() {
        let notification = notification(EventPayload::TurnCompleted {
            finish_reason: "stop".into(),
        });
        let mut out = Vec::new();
        let mut err = Vec::new();

        let action = render_notification(&notification, true, &mut out, &mut err).unwrap();

        let line: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(line["data"]["payload"]["type"], "turn_completed");
        assert!(err.is_empty());
        assert_eq!(action, NotificationAction::Finish);
    }

    #[test]
    fn text_output_keeps_plain_transcript_behavior() {
        let notification = notification(EventPayload::AssistantTextDelta {
            message_id: "message-1".into(),
            delta: "hello".into(),
        });
        let mut out = Vec::new();
        let mut err = Vec::new();

        let action = render_notification(&notification, false, &mut out, &mut err).unwrap();

        assert_eq!(String::from_utf8(out).unwrap(), "hello");
        assert!(err.is_empty());
        assert_eq!(action, NotificationAction::Continue);
    }
}
