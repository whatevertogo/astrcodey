//! Headless exec mode — single-shot prompt execution (in-process).

use astrcode_client::client::AstrcodeClient;
use astrcode_core::event::EventPayload;
use astrcode_protocol::{commands::ClientCommand, events::ClientNotification};

use crate::transport::InProcessTransport;

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
        match recv_result {
            Ok(astrcode_client::stream::StreamItem::Event(event)) => match event {
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
            },
            Ok(astrcode_client::stream::StreamItem::Lagged(_)) => {},
            Err(_) => break,
        }
    }
    Ok(())
}
