//! Interactive terminal mode.
//!
//! This intentionally does not use an alternate screen or a fixed ratatui
//! viewport. Output is appended to normal terminal scrollback, matching the
//! Codex-style terminal transcript and preserving native scroll behavior.

mod slash;

use std::{
    io::{self, Write},
    sync::Arc,
};

use astrcode_client::client::AstrcodeClient;
use astrcode_core::event::EventPayload;
use astrcode_protocol::{commands::ClientCommand, events::ClientNotification};

use crate::transport::InProcessTransport;

pub async fn run() -> io::Result<()> {
    let client = Arc::new(AstrcodeClient::new(InProcessTransport::start()));
    let mut stream = client
        .subscribe_events()
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

    println!("Astrcode");
    println!("Type a message, or /help. Ctrl+C or /quit to exit.");
    println!();

    loop {
        let Some(input) = read_prompt("> ")? else {
            break;
        };
        let input = input.trim_end().to_string();
        if input.trim().is_empty() {
            continue;
        }

        if let Some(command) = slash::parse(&input) {
            if execute_slash_command(command, &client, &mut stream).await? {
                break;
            }
            continue;
        }

        println!();
        println!("You");
        print_indented(&input);
        println!();
        println!("Astrcode");
        print!("  ");
        io::stdout().flush()?;

        client
            .send_command(&ClientCommand::SubmitPrompt {
                text: input,
                attachments: vec![],
            })
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

        consume_until_turn_completed(&mut stream).await?;
        println!();
    }

    Ok(())
}

fn read_prompt(prompt: &str) -> io::Result<Option<String>> {
    print!("{prompt}");
    io::stdout().flush()?;

    let mut input = String::new();
    let bytes = io::stdin().read_line(&mut input)?;
    if bytes == 0 {
        return Ok(None);
    }
    Ok(Some(input))
}

async fn execute_slash_command(
    command: slash::SlashCommand,
    client: &AstrcodeClient<InProcessTransport>,
    stream: &mut astrcode_client::stream::ConversationStream,
) -> io::Result<bool> {
    match command {
        slash::SlashCommand::New => {
            let cwd = std::env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| ".".into());
            client
                .send_command(&ClientCommand::CreateSession { working_dir: cwd })
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            consume_one_protocol_response(stream).await?;
        },
        slash::SlashCommand::Resume(session_id) => {
            if session_id.is_empty() {
                println!("Usage: /resume <id>");
                return Ok(false);
            }
            client
                .send_command(&ClientCommand::ResumeSession { session_id })
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            consume_one_protocol_response(stream).await?;
        },
        slash::SlashCommand::Model(model_id) => {
            if model_id.is_empty() {
                println!("Usage: /model <name>");
                return Ok(false);
            }
            client
                .send_command(&ClientCommand::SetModel { model_id })
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            println!("model updated");
        },
        slash::SlashCommand::Mode(mode) => {
            if mode.is_empty() {
                println!("Usage: /mode <name>");
                return Ok(false);
            }
            client
                .send_command(&ClientCommand::SwitchMode { mode })
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            println!("mode updated");
        },
        slash::SlashCommand::Compact => {
            client
                .send_command(&ClientCommand::Compact)
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            println!("compaction requested");
        },
        slash::SlashCommand::Quit => return Ok(true),
        slash::SlashCommand::Help => {
            println!("Commands: /new /resume <id> /model <name> /mode <name> /compact /quit");
        },
    }

    Ok(false)
}

async fn consume_one_protocol_response(
    stream: &mut astrcode_client::stream::ConversationStream,
) -> io::Result<()> {
    loop {
        match stream.recv().await {
            Ok(astrcode_client::stream::StreamItem::Event(notification)) => {
                print_notification(&notification)?;
                return Ok(());
            },
            Ok(astrcode_client::stream::StreamItem::Lagged(_)) => continue,
            Err(e) => return Err(io::Error::new(io::ErrorKind::Other, e.to_string())),
        }
    }
}

async fn consume_until_turn_completed(
    stream: &mut astrcode_client::stream::ConversationStream,
) -> io::Result<()> {
    loop {
        match stream.recv().await {
            Ok(astrcode_client::stream::StreamItem::Event(notification)) => {
                if print_notification(&notification)? {
                    return Ok(());
                }
            },
            Ok(astrcode_client::stream::StreamItem::Lagged(n)) => {
                println!();
                println!("  [skipped {n} events]");
                print!("  ");
                io::stdout().flush()?;
            },
            Err(e) => return Err(io::Error::new(io::ErrorKind::Other, e.to_string())),
        }
    }
}

/// Returns true when the current assistant turn is complete.
fn print_notification(notification: &ClientNotification) -> io::Result<bool> {
    match notification {
        ClientNotification::Event(event) => match &event.payload {
            EventPayload::SessionStarted { .. } => {
                println!();
                println!("Session {}", short_id(&event.session_id));
                print!("  ");
                io::stdout().flush()?;
            },
            EventPayload::AssistantTextDelta { delta, .. } => {
                print!("{delta}");
                io::stdout().flush()?;
            },
            EventPayload::AssistantMessageCompleted { .. } => {
                println!();
            },
            EventPayload::TurnCompleted { finish_reason } => {
                println!("  [{finish_reason}]");
                return Ok(true);
            },
            EventPayload::ToolCallStarted { tool_name, .. } => {
                println!();
                println!("Tool");
                println!("  {tool_name}");
            },
            EventPayload::ToolCallRequested {
                tool_name,
                arguments,
                ..
            } => {
                println!("  {tool_name} {}", compact_json(arguments));
            },
            EventPayload::ToolCallCompleted { result, .. } => {
                if result.is_error {
                    println!("  error: {}", result.content);
                } else if !result.content.is_empty() {
                    print_indented(&result.content);
                }
                print!("  ");
                io::stdout().flush()?;
            },
            EventPayload::ErrorOccurred { message, .. } => {
                println!();
                println!("Error");
                print_indented(message);
                return Ok(true);
            },
            EventPayload::CompactionCompleted {
                pre_tokens,
                post_tokens,
                ..
            } => {
                println!("Compaction {pre_tokens} -> {post_tokens} tokens");
            },
            _ => {},
        },
        ClientNotification::SessionResumed {
            session_id,
            snapshot,
        } => {
            println!("Resumed {}", short_id(session_id));
            for message in &snapshot.messages {
                println!("{}", label_for_role(&message.role));
                print_indented(&message.content);
            }
        },
        ClientNotification::SessionList { sessions } => {
            if sessions.is_empty() {
                println!("No sessions");
            } else {
                for session in sessions {
                    println!("{}", session.session_id);
                }
            }
        },
        ClientNotification::UiRequest { message, .. } => {
            println!("{message}");
        },
        ClientNotification::Error { message, .. } => {
            println!("Error");
            print_indented(message);
            return Ok(true);
        },
    }

    Ok(false)
}

fn print_indented(text: &str) {
    for line in text.lines() {
        println!("  {line}");
    }
}

fn label_for_role(role: &str) -> &'static str {
    match role {
        "user" => "You",
        "assistant" => "Astrcode",
        "tool" => "Tool",
        _ => "System",
    }
}

fn compact_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

fn short_id(session_id: &str) -> &str {
    session_id.get(..8).unwrap_or(session_id)
}
