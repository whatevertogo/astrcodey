//! Interactive terminal mode.

mod input;
mod render;
mod slash;
mod state;
mod theme;

use std::{
    io::{self, Stdout},
    sync::Arc,
    time::Duration,
};

use astrcode_client::{client::AstrcodeClient, stream::StreamItem};
use astrcode_protocol::commands::ClientCommand;
use crossterm::{
    event::{self, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use input::Action;
use ratatui::{Terminal, backend::CrosstermBackend};
use state::TuiState;
use tokio::sync::mpsc;

use crate::transport::InProcessTransport;

type Client = AstrcodeClient<InProcessTransport>;

pub async fn run() -> io::Result<()> {
    let client = Arc::new(AstrcodeClient::new(InProcessTransport::start()));
    let mut stream = client.subscribe_events().await.map_err(io_error)?;
    let mut terminal = TerminalSession::enter()?;
    let theme = theme::Theme::detect();
    let mut state = TuiState::new();
    state.status = "Ready · type / for commands".into();

    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    spawn_keyboard_reader(action_tx.clone());

    terminal.draw(&state, &theme)?;

    loop {
        tokio::select! {
            action = action_rx.recv() => {
                let Some(action) = action else {
                    break;
                };
                handle_action(action, &mut state, &client).await?;
            },
            item = stream.recv() => {
                match item.map_err(io_error)? {
                    StreamItem::Event(notification) => state.apply(&notification),
                    StreamItem::Lagged(n) => {
                        state.status = format!("Skipped {n} event(s)");
                        state.mark_dirty();
                    },
                }
            },
        }

        if state.should_quit {
            break;
        }
        if state.dirty {
            terminal.draw(&state, &theme)?;
            state.dirty = false;
        }
    }

    Ok(())
}

async fn handle_action(
    action: Action,
    state: &mut TuiState,
    client: &Arc<Client>,
) -> io::Result<()> {
    match action {
        Action::Quit => {
            state.should_quit = true;
            state.mark_dirty();
        },
        Action::Tick => state.mark_dirty(),
        Action::Key(event) => handle_key(event, state, client).await?,
    }
    Ok(())
}

async fn handle_key(event: KeyEvent, state: &mut TuiState, client: &Arc<Client>) -> io::Result<()> {
    match event.code {
        KeyCode::Esc => {
            if state.show_slash_palette {
                state.close_slash();
            } else if state.is_streaming {
                client
                    .send_command(&ClientCommand::Abort)
                    .await
                    .map_err(io_error)?;
                state.status = "Stopping turn".into();
                state.mark_dirty();
            } else {
                state.mark_dirty();
            }
        },
        KeyCode::Enter => {
            if event.modifiers.contains(KeyModifiers::SHIFT)
                || event.modifiers.contains(KeyModifiers::ALT)
            {
                state.insert_newline();
            } else if state.show_slash_palette {
                accept_slash_selection(state, client).await?;
            } else {
                submit_current_input(state, client).await?;
            }
        },
        KeyCode::Tab => {
            if state.show_slash_palette {
                accept_slash_selection(state, client).await?;
            }
        },
        KeyCode::Backspace => state.backspace(),
        KeyCode::Delete => state.delete(),
        KeyCode::Left => state.move_left(),
        KeyCode::Right => state.move_right(),
        KeyCode::Home => state.move_home(),
        KeyCode::End => state.move_end(),
        KeyCode::Up => {
            if state.show_slash_palette {
                state.slash_move_up(slash::filtered(&state.slash_filter).len());
            } else {
                state.history_previous();
            }
        },
        KeyCode::Down => {
            if state.show_slash_palette {
                state.slash_move_down(slash::filtered(&state.slash_filter).len());
            } else {
                state.history_next();
            }
        },
        KeyCode::Char(ch) => {
            if event
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            {
                return Ok(());
            }
            state.insert_char(ch);
        },
        _ => {},
    }

    Ok(())
}

async fn accept_slash_selection(state: &mut TuiState, client: &Arc<Client>) -> io::Result<()> {
    let commands = slash::filtered(&state.slash_filter);
    let Some(spec) = commands
        .get(state.slash_selected.min(commands.len().saturating_sub(1)))
        .copied()
    else {
        state.close_slash();
        return Ok(());
    };

    let current_has_argument = state
        .input
        .split_once(char::is_whitespace)
        .is_some_and(|(_, rest)| !rest.trim().is_empty());
    if spec.needs_argument && !current_has_argument {
        state.set_input(slash::command_line_for(spec));
        return Ok(());
    }

    submit_current_input(state, client).await
}

async fn submit_current_input(state: &mut TuiState, client: &Arc<Client>) -> io::Result<()> {
    let input = state.input.trim_end().to_string();
    if input.trim().is_empty() {
        state.mark_dirty();
        return Ok(());
    }

    if let Some(command) = slash::parse(&input) {
        state.take_input();
        execute_slash_command(command, state, client).await?;
        return Ok(());
    }

    if state.is_streaming {
        state.status = "Turn running · Esc stop".into();
        state.mark_dirty();
        return Ok(());
    }

    let input = state.take_input().trim_end().to_string();
    state.remember_input(&input);
    state.push_user(&input);
    client
        .send_command(&ClientCommand::SubmitPrompt {
            text: input,
            attachments: vec![],
        })
        .await
        .map_err(io_error)?;
    Ok(())
}

async fn execute_slash_command(
    command: slash::SlashCommand,
    state: &mut TuiState,
    client: &Arc<Client>,
) -> io::Result<()> {
    match command {
        slash::SlashCommand::New => {
            let cwd = std::env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| ".".into());
            client
                .send_command(&ClientCommand::CreateSession { working_dir: cwd })
                .await
                .map_err(io_error)?;
            state.status = "Creating session".into();
        },
        slash::SlashCommand::Resume(session_id) => {
            if session_id.is_empty() {
                state.status = "Usage: /resume <id>".into();
                state.mark_dirty();
                return Ok(());
            }
            client
                .send_command(&ClientCommand::ResumeSession { session_id })
                .await
                .map_err(io_error)?;
            state.status = "Resuming session".into();
        },
        slash::SlashCommand::Sessions => {
            client
                .send_command(&ClientCommand::ListSessions)
                .await
                .map_err(io_error)?;
            state.status = "Listing sessions".into();
        },
        slash::SlashCommand::Quit => {
            state.should_quit = true;
        },
        slash::SlashCommand::Help => {
            state.push_message(
                state::MessageRole::System,
                "Commands".into(),
                slash::help_text(),
                false,
                None,
            );
            state.status = "Ready".into();
        },
    }
    state.mark_dirty();
    Ok(())
}

fn spawn_keyboard_reader(action_tx: mpsc::UnboundedSender<Action>) {
    std::thread::spawn(move || {
        loop {
            match event::poll(Duration::from_millis(100)) {
                Ok(true) => match event::read() {
                    Ok(event::Event::Key(key)) => {
                        if let Some(action) = input::map_key(key) {
                            if action_tx.send(action).is_err() {
                                break;
                            }
                        }
                    },
                    Ok(event::Event::Resize(_, _)) => {
                        if action_tx.send(Action::Tick).is_err() {
                            break;
                        }
                    },
                    Ok(_) => {},
                    Err(_) => {
                        let _ = action_tx.send(Action::Quit);
                        break;
                    },
                },
                Ok(false) => {},
                Err(_) => {
                    let _ = action_tx.send(Action::Quit);
                    break;
                },
            }
        }
    });
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;
        Ok(Self { terminal })
    }

    fn draw(&mut self, state: &TuiState, theme: &theme::Theme) -> io::Result<()> {
        self.terminal
            .draw(|frame| render::render(state, frame, theme))
            .map(|_| ())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

fn io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::Other, error.to_string())
}

fn short_id(session_id: &str) -> &str {
    session_id.get(..8).unwrap_or(session_id)
}
