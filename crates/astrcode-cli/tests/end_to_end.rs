//! End-to-end integration test: CLI -> client -> server -> agent loop -> response.
//!
//! This test spawns the astrcode-server binary and communicates via stdio JSON-RPC.
//! Verifies the full pipeline: session creation, prompt submission, response streaming.

use astrcode_client::{client::AstrcodeClient, transport::StdioClientTransport};
use astrcode_core::event::EventPayload;
use astrcode_protocol::{commands::ClientCommand, events::ClientNotification};

/// Build the server binary before running integration tests.
fn server_binary() -> String {
    std::env::var("ASTRCODE_SERVER_BIN").unwrap_or_else(|_| {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
        let debug_path = format!("{}/../../target/debug/astrcode-server.exe", manifest_dir);
        if std::path::Path::new(&debug_path).exists() {
            return debug_path;
        }
        "target/debug/astrcode-server.exe".into()
    })
}

fn stdio_e2e_enabled() -> bool {
    std::env::var("ASTRCODE_RUN_STDIO_E2E").as_deref() == Ok("1")
}

#[tokio::test]
async fn test_e2e_create_session_and_prompt() {
    if !stdio_e2e_enabled() {
        eprintln!("Skipping stdio e2e; set ASTRCODE_RUN_STDIO_E2E=1 to run it");
        return;
    }

    let bin = server_binary();
    if !std::path::Path::new(&bin).exists() {
        eprintln!("Skipping e2e test: server binary not found at {}", bin);
        eprintln!("Build it first: cargo build -p astrcode-server");
        return;
    }

    let transport = match StdioClientTransport::spawn(&bin, &[]) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to spawn server: {}", e);
            return;
        },
    };

    let client = AstrcodeClient::new(transport);
    let mut stream = client.subscribe_events().await.unwrap();

    client
        .send_command(&ClientCommand::CreateSession {
            working_dir: ".".into(),
        })
        .await
        .unwrap();

    let session_id = match stream.recv().await.unwrap() {
        astrcode_client::stream::StreamItem::Event(ClientNotification::Event(event))
            if matches!(event.payload, EventPayload::SessionStarted { .. }) =>
        {
            event.session_id
        },
        other => panic!("Expected SessionStarted event, got {:?}", other),
    };
    assert!(!session_id.is_empty());

    client
        .send_command(&ClientCommand::SubmitPrompt {
            text: "Hello, astrcode!".into(),
            attachments: vec![],
        })
        .await
        .unwrap();

    let mut got_turn_start = false;
    let mut got_message = false;
    let mut got_turn_end = false;

    for _ in 0..100 {
        match stream.recv().await {
            Ok(astrcode_client::stream::StreamItem::Event(notification)) => match notification {
                ClientNotification::Event(event) => match event.payload {
                    EventPayload::TurnStarted => {
                        got_turn_start = true;
                    },
                    EventPayload::AssistantTextDelta { .. } => {
                        got_message = true;
                    },
                    EventPayload::TurnCompleted { .. } => {
                        got_turn_end = true;
                        break;
                    },
                    EventPayload::ErrorOccurred { message, .. } => {
                        eprintln!("server error event: {message}");
                    },
                    _ => {},
                },
                ClientNotification::Error { message, .. } => {
                    eprintln!("server error notification: {message}");
                },
                _ => {},
            },
            Ok(astrcode_client::stream::StreamItem::Lagged(_)) => {},
            Err(_) => break,
        }
    }

    assert!(got_turn_start, "Should have received TurnStarted");
    assert!(
        got_message,
        "Should have received AssistantTextDelta; full pipeline should stream"
    );
    assert!(got_turn_end, "Should have received TurnCompleted");
}

#[tokio::test]
async fn test_e2e_list_sessions() {
    if !stdio_e2e_enabled() {
        eprintln!("Skipping stdio e2e; set ASTRCODE_RUN_STDIO_E2E=1 to run it");
        return;
    }

    let bin = server_binary();
    if !std::path::Path::new(&bin).exists() {
        eprintln!("Skipping: server binary not found");
        return;
    }

    let transport = StdioClientTransport::spawn(&bin, &[]).unwrap();
    let client = AstrcodeClient::new(transport);
    let mut stream = client.subscribe_events().await.unwrap();

    client
        .send_command(&ClientCommand::ListSessions)
        .await
        .unwrap();

    match stream.recv().await.unwrap() {
        astrcode_client::stream::StreamItem::Event(ClientNotification::SessionList {
            sessions,
        }) => {
            println!("Sessions: {:?}", sessions);
        },
        other => panic!("Expected SessionList, got: {:?}", other),
    }
}
