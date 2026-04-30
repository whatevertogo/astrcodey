//! 交互式终端模式 (Inline Viewport)。
//!
//! TUI 运行在主屏幕上，底部渲染固定高度的输入面板。
//! 消息记录通过 `insert_before()` 写入终端原生 scrollback，
//! 用户可用终端原生滚轮/键盘翻页查看历史消息。

mod input;
mod render;
mod slash;
mod state;
mod theme;

use std::{
    io::{self, Stdout, Write},
    sync::Arc,
    time::Duration,
};

use astrcode_client::{client::AstrcodeClient, stream::StreamItem};
use astrcode_protocol::commands::ClientCommand;
use crossterm::{
    event::{self, KeyCode, KeyEvent, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode},
};
use input::Action;
use ratatui::{
    Terminal, TerminalOptions, Viewport, backend::CrosstermBackend, prelude::Widget, text::Text,
    widgets::Paragraph,
};
use render::message_to_lines;
use state::TuiState;
use tokio::sync::mpsc;

use crate::transport::InProcessTransport;

type Client = AstrcodeClient<InProcessTransport>;

/// TUI 主入口：初始化终端、启动事件循环。
pub async fn run() -> io::Result<()> {
    let client = Arc::new(AstrcodeClient::new(InProcessTransport::start()));
    let mut stream = client.subscribe_events().await.map_err(io_error)?;
    let mut terminal = TerminalSession::enter()?;
    let theme = theme::Theme::detect();
    let mut state = TuiState::new();
    state.status = "Ready · type / for commands".into();

    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    spawn_keyboard_reader(action_tx.clone());

    // 首帧绘制
    terminal.draw_bottom(&state, &theme)?;

    loop {
        tokio::select! {
            action = action_rx.recv() => {
                let Some(action) = action else { break };
                handle_action(action, &mut state, &client, &mut terminal).await?;
                flush_scrollback(&mut state, &mut terminal, &theme)?;
            },
            item = stream.recv() => {
                match item.map_err(io_error)? {
                    StreamItem::Event(notification) => {
                        state.apply(&notification);
                        flush_scrollback(&mut state, &mut terminal, &theme)?;
                    },
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
            terminal.draw_bottom(&state, &theme)?;
            state.dirty = false;
        }
    }

    Ok(())
}

// ─── Action 处理 ──────────────────────────────────────────────────────

async fn handle_action(
    action: Action,
    state: &mut TuiState,
    client: &Arc<Client>,
    terminal: &mut TerminalSession,
) -> io::Result<()> {
    match action {
        Action::Quit => state.should_quit = true,
        Action::Tick => state.mark_dirty(),
        Action::Key(event) => handle_key(event, state, client, terminal).await?,
    }
    state.mark_dirty();
    Ok(())
}

async fn handle_key(
    event: KeyEvent,
    state: &mut TuiState,
    client: &Arc<Client>,
    _terminal: &mut TerminalSession,
) -> io::Result<()> {
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
                submit_current_input(state, client, Some(_terminal)).await?;
            }
        },
        KeyCode::Tab if state.show_slash_palette => {
            accept_slash_selection(state, client).await?;
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
    submit_current_input(state, client, None).await
}

async fn submit_current_input(
    state: &mut TuiState,
    client: &Arc<Client>,
    _terminal: Option<&mut TerminalSession>,
) -> io::Result<()> {
    let input = state.input.trim_end().to_string();
    if input.trim().is_empty() {
        return Ok(());
    }

    if let Some(command) = slash::parse(&input) {
        let input = state.take_input();
        state.remember_input(&input);
        execute_slash_command(command, state, client).await?;
        return Ok(());
    }

    if state.is_streaming {
        state.status = "Turn running · Esc stop".into();
        return Ok(());
    }

    let input = state.take_input();
    state.remember_input(&input);
    state.push_user(&input);
    state.mark_dirty();

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
    _command: slash::SlashCommand,
    state: &mut TuiState,
    _client: &Arc<Client>,
) -> io::Result<()> {
    state.push_message(
        state::MessageRole::System,
        "System".into(),
        format!("Slash command executed"),
        false,
        None,
    );
    state.mark_dirty();
    Ok(())
}

// ─── 键盘读取线程 ─────────────────────────────────────────────────────

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
                    // 不处理鼠标事件 — 原生选择/滚轮由终端管理
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

// ─── 终端会话 ─────────────────────────────────────────────────────────

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        // 交替滚动：滚轮在 raw 模式下也工作
        // 光标移到底部，这样 inline viewport 在屏幕最后一行
        let (_, rows) = crossterm::terminal::size()?;
        write!(stdout, "\x1b[{};1H", rows)?;
        stdout.flush()?;

        let options = TerminalOptions {
            viewport: Viewport::Inline(6),
        };
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::with_options(backend, options)?;
        Ok(Self { terminal })
    }

    /// 绘制底部面板（状态栏 + 输入编辑器 + 底部信息栏）。
    fn draw_bottom(&mut self, state: &TuiState, theme: &theme::Theme) -> io::Result<()> {
        self.terminal
            .draw(|frame| render::render(state, frame, theme))
            .map(|_| ())
    }

    /// 将消息内容插入终端 scrollback（在 viewport 上方）。
    fn insert_message(&mut self, msg: &state::Message, theme: &theme::Theme) -> io::Result<()> {
        let width = self.terminal.size()?.width;
        let lines = message_to_lines(msg, width, theme);
        let height = lines.len() as u16;
        self.terminal.insert_before(height, |buf| {
            let p = Paragraph::new(Text::from(lines.clone()));
            Widget::render(p, buf.area, buf);
        })?;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = disable_raw_mode();
    }
}

/// 将 scrollback_queue 中的消息全部写入终端原生 scrollback。
fn flush_scrollback(
    state: &mut TuiState,
    terminal: &mut TerminalSession,
    theme: &theme::Theme,
) -> io::Result<()> {
    let msgs: Vec<_> = state.scrollback_queue.drain(..).collect();
    for msg in msgs {
        terminal.insert_message(&msg, theme)?;
    }
    Ok(())
}

fn io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

fn short_id(session_id: &str) -> &str {
    session_id.get(..8).unwrap_or(session_id)
}
