//! Headless exec mode — single-shot prompt execution (in-process).

use astrcode_client::client::AstrcodeClient;
use astrcode_protocol::commands::ClientCommand;
use astrcode_protocol::events::ServerEvent;

use crate::transport::InProcessTransport;

pub async fn run(prompt: &str, jsonl: bool, _timeout_secs: u64) -> Result<(), String> {
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

    loop {
        match stream.recv().await {
            Ok(astrcode_client::stream::StreamItem::Event(event)) => match event {
                ServerEvent::MessageDelta { delta, .. } => {
                    if !jsonl {
                        print!("{delta}");
                    }
                }
                ServerEvent::TurnEnded { .. } => {
                    if !jsonl {
                        println!();
                    }
                    break;
                }
                ServerEvent::Error { message, .. } => {
                    eprintln!("Error: {message}");
                    break;
                }
                _ => {
                    if jsonl {
                        println!("{}", serde_json::to_string(&event).unwrap_or_default());
                    }
                }
            },
            Ok(astrcode_client::stream::StreamItem::Lagged(_)) => {}
            Err(_) => break,
        }
    }
    Ok(())
}
