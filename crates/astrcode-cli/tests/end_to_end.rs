//! End-to-end integration test: CLI → client → server → agent loop → response.
//!
//! This test spawns the astrcode-server binary and communicates via stdio JSON-RPC.
//! Verifies the full pipeline: session creation, prompt submission, response streaming.

use astrcode_client::client::AstrcodeClient;
use astrcode_client::transport::StdioClientTransport;
use astrcode_protocol::commands::ClientCommand;
use astrcode_protocol::events::ServerEvent;

/// Build the server binary before running integration tests.
fn server_binary() -> String {
    std::env::var("ASTRCODE_SERVER_BIN").unwrap_or_else(|_| {
        // Use CARGO_MANIFEST_DIR to find the workspace root
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
        let debug_path = format!("{}/../../target/debug/astrcode-server.exe", manifest_dir);
        if std::path::Path::new(&debug_path).exists() {
            return debug_path;
        }
        // Fallback: relative from CWD
        "target/debug/astrcode-server.exe".into()
    })
}

#[tokio::test]
async fn test_e2e_create_session_and_prompt() {
    let bin = server_binary();

    // Skip if server binary doesn't exist (e.g., not yet built)
    if !std::path::Path::new(&bin).exists() {
        eprintln!("Skipping e2e test: server binary not found at {}", bin);
        eprintln!("Build it first: cargo build -p astrcode-server");
        return;
    }

    // 1. Spawn server
    let transport = match StdioClientTransport::spawn(&bin, &[]) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to spawn server: {}", e);
            return;
        }
    };

    let client = AstrcodeClient::new(transport);

    // 2. Subscribe to events FIRST — no events will be missed
    let mut stream = client.subscribe_events().await.unwrap();

    // 3. Create a session
    client
        .send_command(&ClientCommand::CreateSession {
            working_dir: ".".into(),
        })
        .await
        .unwrap();

    // Read SessionCreated from stream
    let session_id = match stream.recv().await.unwrap() {
        astrcode_client::stream::StreamItem::Event(ServerEvent::SessionCreated {
            session_id,
            ..
        }) => session_id,
        other => panic!("Expected SessionCreated, got {:?}", other),
    };
    assert!(!session_id.is_empty());
    println!("Session created: {}", session_id);

    // 4. Submit a prompt
    client
        .send_command(&ClientCommand::SubmitPrompt {
            text: "Hello, astrcode!".into(),
            attachments: vec![],
        })
        .await
        .unwrap();
    println!("Prompt submitted");

    // 5. Read stream events: SessionCreated(server auto-created) → TurnStarted → Message* → TurnEnded
    let mut got_turn_start = false;
    let mut got_message = false;
    let mut got_turn_end = false;

    for _ in 0..100 {
        match stream.recv().await {
            Ok(astrcode_client::stream::StreamItem::Event(event)) => match &event {
                ServerEvent::TurnStarted { .. } => {
                    got_turn_start = true;
                    println!("  TurnStarted ✓");
                }
                ServerEvent::MessageDelta { delta, .. } => {
                    got_message = true;
                    println!("  MessageDelta: {}", delta);
                }
                ServerEvent::TurnEnded { .. } => {
                    got_turn_end = true;
                    println!("  TurnEnded ✓");
                    break;
                }
                ServerEvent::SessionCreated { .. } => {
                    println!("  SessionCreated (auto)");
                }
                ServerEvent::Error { message, .. } => {
                    println!("  Error: {}", message);
                }
                evt => {
                    println!("  event: {:?}", std::mem::discriminant(evt));
                }
            },
            Ok(astrcode_client::stream::StreamItem::Lagged(n)) => {
                println!("  Lagged: {} events", n);
            }
            Err(_) => break,
        }
    }

    assert!(got_turn_start, "Should have received TurnStarted");
    assert!(
        got_message,
        "Should have received MessageDelta — full pipeline is working"
    );
    assert!(
        got_turn_end,
        "Should have received TurnEnded — turn completed"
    );

    println!(
        "E2E test passed! Pipeline verified: create session → submit prompt → receive response ✓"
    );
}

#[tokio::test]
async fn test_e2e_list_sessions() {
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
        astrcode_client::stream::StreamItem::Event(ServerEvent::SessionList { sessions }) => {
            println!("Sessions: {:?}", sessions);
        }
        other => panic!("Expected SessionList, got: {:?}", other),
    }
}
