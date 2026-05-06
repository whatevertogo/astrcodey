//! 无头执行模式 —— 单次提示执行（进程内）。
//!
//! 该模块实现了 CLI 的 `exec` 子命令，用于在不需要交互式 TUI 的情况下
//! 一次性提交提示并输出结果。支持纯文本和 JSONL 两种输出格式。

use astrcode_client::client::AstrcodeClient;
use astrcode_core::event::EventPayload;
use astrcode_protocol::{commands::ClientCommand, events::ClientNotification};

use crate::transport::InProcessTransport;

/// 执行单次提示并等待响应完成。
pub async fn run(prompt: &str, jsonl: bool, timeout_secs: u64) -> Result<(), String> {
    let client = AstrcodeClient::new(InProcessTransport::start());

    let _sid = client
        .create_session(".")
        .await
        .map_err(|e| format!("Cannot create session: {e}"))?;

    let mut stream = client
        .subscribe_events()
        .await
        .map_err(|e| format!("Cannot subscribe: {e}"))?;

    client
        .send_command(&ClientCommand::SubmitPrompt {
            text: prompt.into(),
            attachments: vec![],
        })
        .await
        .map_err(|e| format!("Cannot submit: {e}"))?;

    let deadline = (timeout_secs > 0)
        .then(|| tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs));

    loop {
        let recv_result = if let Some(deadline) = deadline {
            tokio::time::timeout_at(deadline, stream.recv())
                .await
                .map_err(|_| format!("exec timed out after {timeout_secs}s"))?
        } else {
            stream.recv().await
        };
        let notification = match recv_result {
            Ok(n) => n,
            Err(_) => break,
        };
        match notification {
            ClientNotification::Event(core_event) => match core_event.payload {
                EventPayload::AssistantTextDelta { delta, .. } => {
                    if !jsonl {
                        print!("{delta}");
                    }
                },
                EventPayload::TurnCompleted { .. } => {
                    if !jsonl {
                        println!();
                    }
                    break;
                },
                EventPayload::ErrorOccurred { message, .. } => {
                    eprintln!("Error: {message}");
                    break;
                },
                _ => {
                    if jsonl {
                        println!(
                            "{}",
                            serde_json::to_string(&ClientNotification::Event(core_event))
                                .unwrap_or_default()
                        );
                    }
                },
            },
            ClientNotification::Error { message, .. } => {
                eprintln!("Error: {message}");
                break;
            },
            other => {
                if jsonl {
                    println!("{}", serde_json::to_string(&other).unwrap_or_default());
                }
            },
        }
    }
    Ok(())
}
