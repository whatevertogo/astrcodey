//! TUI: codex-inspired transcript viewport plus bottom composer.

mod input;
mod render;
mod slash;
mod state;
mod theme;

use std::io;
use std::sync::Arc;

use crossterm::{
    cursor,
    event::{self, Event as CrosstermEvent, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use astrcode_client::client::AstrcodeClient;
use astrcode_protocol::commands::ClientCommand;

use self::input::{map_key, Action};
use self::state::{Focus, TuiState};
use self::theme::Theme;
use crate::transport::InProcessTransport;

pub async fn run() -> io::Result<()> {
    let theme = Theme::detect();

    // Setup: raw mode, alt screen
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, cursor::Hide)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let state = Arc::new(tokio::sync::Mutex::new(TuiState::new()));
    let (action_tx, mut action_rx) = mpsc::unbounded_channel::<Action>();

    // Start server in-process
    let client = spawn_and_connect(action_tx.clone()).await;

    // Input thread
    let tx = action_tx.clone();
    std::thread::spawn(move || loop {
        if let Ok(evt) = event::read() {
            match evt {
                CrosstermEvent::Key(key) => {
                    if let Some(action) = map_key(key) {
                        let _ = tx.send(action);
                    }
                }
                CrosstermEvent::Resize(_, _) => {
                    let _ = tx.send(Action::Tick);
                }
                _ => {}
            }
        }
    });

    // Event loop
    let result = run_loop(
        &mut terminal,
        state.clone(),
        &mut action_rx,
        &client,
        &theme,
    )
    .await;

    // Cleanup
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, cursor::Show)?;
    result
}

async fn spawn_and_connect(
    tx: mpsc::UnboundedSender<Action>,
) -> Option<Arc<AstrcodeClient<InProcessTransport>>> {
    let client = Arc::new(AstrcodeClient::new(InProcessTransport::start()));

    // Subscribe to events
    if let Ok(mut stream) = client.subscribe_events().await {
        let tx2 = tx.clone();
        tokio::spawn(async move {
            loop {
                match stream.recv().await {
                    Ok(astrcode_client::stream::StreamItem::Event(event)) => {
                        let _ = tx2.send(Action::StreamEvent(event));
                    }
                    Ok(astrcode_client::stream::StreamItem::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
        });
    }
    Some(client)
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: Arc<tokio::sync::Mutex<TuiState>>,
    rx: &mut mpsc::UnboundedReceiver<Action>,
    client: &Option<Arc<AstrcodeClient<InProcessTransport>>>,
    theme: &Theme,
) -> io::Result<()> {
    loop {
        let action = match rx.recv().await {
            Some(a) => a,
            None => break,
        };
        handle_action(action, &state, client).await;
        let mut s = state.lock().await;
        if s.should_quit {
            break;
        }

        if s.dirty {
            terminal.draw(|f| render::render(&s, f, theme))?;
            s.dirty = false;
        }
    }
    Ok(())
}

async fn handle_action(
    action: Action,
    state: &Arc<tokio::sync::Mutex<TuiState>>,
    client: &Option<Arc<AstrcodeClient<InProcessTransport>>>,
) {
    match action {
        Action::Quit => {
            state.lock().await.should_quit = true;
        }

        Action::Tick => {
            state.lock().await.mark_dirty();
        }

        Action::Key(key) => {
            let mut s = state.lock().await;
            // Ctrl+C → quit
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                s.should_quit = true;
                return;
            }
            match s.focus {
                Focus::Input => match key.code {
                    KeyCode::Enter => {
                        if key.modifiers.contains(KeyModifiers::SHIFT) {
                            s.insert_newline();
                            return;
                        }

                        let text = s.take_input();
                        if text.trim().is_empty() {
                            s.mark_dirty();
                            return;
                        }
                        s.remember_input(&text);
                        let submit = text.clone();
                        drop(s);

                        if let Some(command) = slash::parse(&submit) {
                            execute_slash_command(command, state, client).await;
                        } else {
                            let mut s = state.lock().await;
                            s.push_user(&submit);
                            s.status = "Working".into();
                            s.mark_dirty();
                            drop(s);

                            if let Some(ref c) = client {
                                let _ = c
                                    .send_command(&ClientCommand::SubmitPrompt {
                                        text: submit,
                                        attachments: vec![],
                                    })
                                    .await;
                            }
                        }
                    }
                    KeyCode::Char(c) => {
                        if key.modifiers.contains(KeyModifiers::CONTROL) {
                            match c {
                                'n' => {
                                    drop(s);
                                    execute_slash_command(slash::SlashCommand::New, state, client)
                                        .await;
                                }
                                'l' => {
                                    drop(s);
                                    if let Some(ref c) = client {
                                        let _ = c.send_command(&ClientCommand::ListSessions).await;
                                    }
                                }
                                _ => {}
                            }
                        } else {
                            s.insert_char(c);
                        }
                    }
                    KeyCode::Backspace => s.backspace(),
                    KeyCode::Delete => s.delete(),
                    KeyCode::Left => s.move_left(),
                    KeyCode::Right => s.move_right(),
                    KeyCode::Home => s.move_home(),
                    KeyCode::End => s.move_end(),
                    KeyCode::Up => s.history_previous(),
                    KeyCode::Down => s.history_next(),
                    KeyCode::Esc => s.close_slash(),
                    _ => {}
                },
                Focus::SlashPalette => match key.code {
                    KeyCode::Esc => s.close_slash(),
                    KeyCode::Up => {
                        let len = slash::filtered(&s.slash_filter).len();
                        s.slash_move_up(len);
                    }
                    KeyCode::Down => {
                        let len = slash::filtered(&s.slash_filter).len();
                        s.slash_move_down(len);
                    }
                    KeyCode::Enter => {
                        let options = slash::filtered(&s.slash_filter);
                        if let Some(selected) =
                            options.get(s.slash_selected.min(options.len().saturating_sub(1)))
                        {
                            let completion = slash::completion_text(selected);
                            s.set_input(completion);
                            s.focus = Focus::Input;
                            s.show_slash_palette = false;
                        }
                    }
                    KeyCode::Backspace => s.backspace(),
                    KeyCode::Delete => s.delete(),
                    KeyCode::Left => s.move_left(),
                    KeyCode::Right => s.move_right(),
                    KeyCode::Home => s.move_home(),
                    KeyCode::End => s.move_end(),
                    KeyCode::Char(c) => s.insert_char(c),
                    _ => {}
                },
            }
        }

        Action::StreamEvent(event) => {
            state.lock().await.apply(&event);
        }
    }
}

async fn execute_slash_command(
    command: slash::SlashCommand,
    state: &Arc<tokio::sync::Mutex<TuiState>>,
    client: &Option<Arc<AstrcodeClient<InProcessTransport>>>,
) {
    match command {
        slash::SlashCommand::New => {
            let cwd = std::env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| ".".into());
            {
                let mut s = state.lock().await;
                s.status = "Creating session".into();
                s.mark_dirty();
            }
            if let Some(ref c) = client {
                let _ = c
                    .send_command(&ClientCommand::CreateSession { working_dir: cwd })
                    .await;
            }
        }
        slash::SlashCommand::Resume(session_id) => {
            if session_id.is_empty() {
                let mut s = state.lock().await;
                s.status = "Usage: /resume <id>".into();
                s.mark_dirty();
                return;
            }
            if let Some(ref c) = client {
                let _ = c
                    .send_command(&ClientCommand::ResumeSession { session_id })
                    .await;
            }
        }
        slash::SlashCommand::Model(model_id) => {
            if model_id.is_empty() {
                let mut s = state.lock().await;
                s.status = "Usage: /model <name>".into();
                s.mark_dirty();
                return;
            }
            {
                let mut s = state.lock().await;
                s.model_name = model_id.clone();
                s.status = format!("Model -> {}", model_id);
                s.mark_dirty();
            }
            if let Some(ref c) = client {
                let _ = c.send_command(&ClientCommand::SetModel { model_id }).await;
            }
        }
        slash::SlashCommand::Mode(mode) => {
            if mode.is_empty() {
                let mut s = state.lock().await;
                s.status = "Usage: /mode <name>".into();
                s.mark_dirty();
                return;
            }
            if let Some(ref c) = client {
                let _ = c.send_command(&ClientCommand::SwitchMode { mode }).await;
            }
        }
        slash::SlashCommand::Compact => {
            if let Some(ref c) = client {
                let _ = c.send_command(&ClientCommand::Compact).await;
            }
        }
        slash::SlashCommand::Quit => {
            state.lock().await.should_quit = true;
        }
        slash::SlashCommand::Help => {
            let mut s = state.lock().await;
            s.status = "Commands: /new /resume /model /mode /compact /quit".into();
            s.mark_dirty();
        }
    }
}
