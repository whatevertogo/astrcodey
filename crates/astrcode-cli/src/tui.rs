//! 交互式终端模式（原生 scrollback + 底部固定面板）。
//!
//! TUI 运行在主屏幕上，底部只保留很小的交互面板。
//! 消息记录通过 inline viewport 写入终端原生 scrollback，
//! 用户可用终端原生滚轮/键盘翻页查看历史消息。

mod composer;
mod custom_terminal;
mod input;
mod insert_history;
mod render;
mod slash;
mod state;
mod terminal_probe;
mod theme;
mod tool_display;
mod tui_event;

use std::{
    io::{self, Stdout},
    sync::Arc,
};

use astrcode_client::client::AstrcodeClient;
use astrcode_protocol::commands::ClientCommand;
use crossterm::{
    SynchronizedUpdate,
    event::{DisableBracketedPaste, EnableBracketedPaste, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use custom_terminal::Terminal as CustomTerminal;
use input::is_press_event;
use insert_history::insert_history_lines;
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::Position,
};
use render::scrollback_entry_to_lines;
use state::TuiState;
use tokio_stream::StreamExt;
use tui_event::{EventBroker, EventStream as TuiEventStream, TerminalFocus, TuiEvent};

use crate::transport::InProcessTransport;

type Client = AstrcodeClient<InProcessTransport>;

const INLINE_VIEWPORT_HEIGHT: u16 = 4;

/// TUI 主入口：初始化终端、启动事件循环。
pub async fn run() -> io::Result<()> {
    let client = Arc::new(AstrcodeClient::new(InProcessTransport::start()));
    let mut stream = client.subscribe_events().await.map_err(io_error)?;
    let mut terminal = TerminalSession::enter()?;
    let theme = theme::Theme::detect();
    let mut state = TuiState::new();

    // 创建事件流管理器
    let broker = EventBroker::new();
    let focus = TerminalFocus::new();
    let (draw_tx, _) = tokio::sync::broadcast::channel::<()>(16);

    // 创建 TUI 事件流
    let mut event_stream = TuiEventStream::new(broker, draw_tx.subscribe(), focus);

    // 首帧绘制
    terminal.draw_frame(&mut state, &theme)?;
    state.dirty = false;

    let mut exit_reason = None::<String>;
    loop {
        tokio::select! {
            // TUI 事件（键盘、粘贴、resize、绘制）
            event = event_stream.next() => {
                let Some(event) = event else {
                    exit_reason = Some("event stream ended".into());
                    break;
                };
                handle_tui_event(event, &mut state, &client, &mut terminal).await?;
            },
            // 服务器事件：先 await 第一条，再 try_recv 批量 drain
            notification = stream.recv() => {
                state.apply(&notification.map_err(io_error)?);
                for pending in stream.drain_pending() {
                    state.apply(&pending);
                }
            },
        }

        if state.should_quit {
            break;
        }
        if state.dirty {
            terminal.draw_frame(&mut state, &theme)?;
            state.dirty = false;
        }
    }

    drop(terminal);

    if let Some(reason) = exit_reason {
        eprintln!("[TUI] exited abnormally: {reason}");
    }

    Ok(())
}

/// 处理 TUI 事件。
async fn handle_tui_event(
    event: TuiEvent,
    state: &mut TuiState,
    client: &Arc<Client>,
    terminal: &mut TerminalSession,
) -> io::Result<()> {
    match event {
        TuiEvent::Key(key_event) => {
            if is_press_event(&key_event) {
                handle_key(key_event, state, client, terminal).await?;
            }
        },
        TuiEvent::Paste(text) => {
            let text = normalize_paste(&text);
            state.insert_paste(&text);
            state.mark_dirty();
        },
        TuiEvent::Draw => {
            // 计划的重绘事件（包括 resize 触发的重绘）
            state.mark_dirty();
        },
    }
    Ok(())
}

async fn handle_key(
    event: KeyEvent,
    state: &mut TuiState,
    client: &Arc<Client>,
    terminal: &mut TerminalSession,
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
                submit_current_input(state, client).await?;
            }
        },
        KeyCode::Tab if state.show_slash_palette => {
            complete_slash_selection(state);
        },
        KeyCode::Backspace if event.modifiers.contains(KeyModifiers::ALT) => {
            state.delete_previous_word();
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
            } else if !state.move_visual_up(terminal.composer_width()) {
                state.history_previous();
            }
        },
        KeyCode::Down => {
            if state.show_slash_palette {
                state.slash_move_down(slash::filtered(&state.slash_filter).len());
            } else if !state.move_visual_down(terminal.composer_width()) {
                state.history_next();
            }
        },
        KeyCode::Char(ch) if event.modifiers.contains(KeyModifiers::CONTROL) => {
            match ch.to_ascii_lowercase() {
                'a' => state.move_home(),
                'e' => state.move_end(),
                'u' => state.delete_before_cursor(),
                'k' => state.delete_after_cursor(),
                'w' => state.delete_previous_word(),
                _ => {},
            }
        },
        KeyCode::Char(ch) => {
            if event.modifiers.contains(KeyModifiers::ALT) {
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

    let cmd_name = spec.usage.split_whitespace().next().unwrap_or(spec.usage);
    let argument = state
        .input_text()
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest.trim())
        .unwrap_or("");

    if spec.needs_argument && argument.is_empty() {
        state.set_input(format!("{cmd_name} "));
        return Ok(());
    }

    let full_input = if argument.is_empty() {
        cmd_name.to_string()
    } else {
        format!("{cmd_name} {argument}")
    };
    state.set_input(full_input);
    submit_current_input(state, client).await
}

/// Tab completion: replace input with full command name without submitting.
fn complete_slash_selection(state: &mut TuiState) {
    let commands = slash::filtered(&state.slash_filter);
    let Some(spec) = commands
        .get(state.slash_selected.min(commands.len().saturating_sub(1)))
        .copied()
    else {
        return;
    };
    state.set_input(slash::command_line_for(spec));
}

async fn submit_current_input(state: &mut TuiState, client: &Arc<Client>) -> io::Result<()> {
    let input = state.input_text().trim_end().to_string();
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
    command: slash::SlashCommand,
    state: &mut TuiState,
    client: &Arc<Client>,
) -> io::Result<()> {
    match command {
        slash::SlashCommand::New => {
            let working_dir = std::env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| ".".into());
            client
                .send_command(&ClientCommand::CreateSession { working_dir })
                .await
                .map_err(io_error)?;
            state.status = "Creating session".into();
        },
        slash::SlashCommand::Resume(session_id) => {
            if session_id.trim().is_empty() {
                state.push_message(
                    state::MessageRole::System,
                    "Usage".into(),
                    "/resume <session-id>".into(),
                    false,
                    None,
                );
            } else {
                let session_id = resolve_session_id(state, &session_id);
                client
                    .send_command(&ClientCommand::ResumeSession { session_id })
                    .await
                    .map_err(io_error)?;
                state.status = "Resuming session".into();
            }
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
                "Help".into(),
                slash_help_text(),
                false,
                None,
            );
        },
    }
    state.mark_dirty();
    Ok(())
}

// ─── 终端会话 ─────────────────────────────────────────────────────────

struct TerminalSession {
    terminal: CustomTerminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnableBracketedPaste)?;

        let mut backend = CrosstermBackend::new(stdout);

        // 探测初始光标位置（必须在 raw mode 之后，使用 CPR 逃逸序列）
        #[cfg(unix)]
        let cursor_pos = match terminal_probe::cursor_position(terminal_probe::DEFAULT_TIMEOUT) {
            Ok(Some(pos)) => pos,
            Ok(None) => {
                tracing::warn!("initial cursor position probe timed out; defaulting to origin");
                Position { x: 0, y: 0 }
            },
            Err(err) => {
                tracing::warn!(
                    "failed to read initial cursor position; defaulting to origin: {err}"
                );
                Position { x: 0, y: 0 }
            },
        };

        #[cfg(not(unix))]
        let cursor_pos = backend.get_cursor_position().unwrap_or_else(|err| {
            tracing::warn!("failed to read initial cursor position; defaulting to origin: {err}");
            Position { x: 0, y: 0 }
        });

        let terminal = CustomTerminal::with_options_and_cursor_position(backend, cursor_pos)?;

        Ok(Self { terminal })
    }

    /// 将待提交历史写入原生 scrollback，并绘制底部面板。
    ///
    /// 使用 Codex 的 draw_with_resize_reflow 模式：
    /// 1. 在 sync_update 之前查询光标位置（避免与事件读取竞争）
    /// 2. 在 sync_update 内部： a. 应用 pending_viewport_area（光标位置启发式） b.
    ///    update_inline_viewport 处理 resize-reflow c. flush scrollback d. invalidate
    ///    viewport（如需要） e. draw
    fn draw_frame(&mut self, state: &mut TuiState, theme: &theme::Theme) -> io::Result<()> {
        // Precompute viewport area adjustment from cursor position heuristic
        // BEFORE entering the synchronized update, to avoid racing with the
        // event reader.
        let pending_viewport_area = self.pending_viewport_area()?;

        let viewport_height = if state.show_slash_palette {
            INLINE_VIEWPORT_HEIGHT + 4
        } else {
            INLINE_VIEWPORT_HEIGHT
        };

        let _ = io::stdout().sync_update(|_| {
            // Apply cursor-based viewport adjustment if resize moved the cursor
            if let Some(new_area) = pending_viewport_area {
                self.terminal.set_viewport_area(new_area);
                self.terminal.clear()?;
            }

            let needs_full_repaint = self.terminal.update_inline_viewport(viewport_height)?;

            self.flush_scrollback(state, theme)?;

            if needs_full_repaint {
                self.terminal.invalidate_viewport();
            }

            self.terminal
                .draw(|frame| render::render(state, frame, theme))
        })?;

        Ok(())
    }

    /// Cursor position heuristic to detect resize-induced viewport drift.
    ///
    /// When the terminal is resized, the emulator reflows content which can
    /// shift the cursor position. By comparing the actual cursor position
    /// with the last known position, we can compute how the viewport should
    /// be adjusted to keep the inline viewport at the correct screen location.
    fn pending_viewport_area(&mut self) -> io::Result<Option<ratatui::layout::Rect>> {
        let screen_size = self.terminal.size()?;
        let last_known_screen_size = self.terminal.last_known_screen_size;
        if screen_size != last_known_screen_size {
            if let Ok(cursor_pos) = self.terminal.get_cursor_position() {
                let last_known_cursor_pos = self.terminal.last_known_cursor_pos;
                // If the cursor moved due to resize reflow, adjust the viewport
                // to keep it in the same relative position.
                if cursor_pos.y != last_known_cursor_pos.y {
                    let offset = ratatui::layout::Offset {
                        x: 0,
                        y: cursor_pos.y as i32 - last_known_cursor_pos.y as i32,
                    };
                    return Ok(Some(self.terminal.viewport_area.offset(offset)));
                }
            }
        }
        Ok(None)
    }

    fn composer_width(&self) -> usize {
        self.terminal.composer_width()
    }

    /// 将条目插入终端 scrollback（在 viewport 上方）。
    fn insert_scrollback_entry(
        &mut self,
        entry: &state::ScrollbackEntry,
        theme: &theme::Theme,
    ) -> io::Result<()> {
        let width = self.terminal.viewport_area.width;
        let lines = scrollback_entry_to_lines(entry, width, theme);
        insert_history_lines(&mut self.terminal, lines)
    }

    /// 刷新 scrollback_queue 中的所有条目。
    fn flush_scrollback(&mut self, state: &mut TuiState, theme: &theme::Theme) -> io::Result<()> {
        let entries: Vec<_> = state.scrollback_queue.drain(..).collect();
        for entry in entries {
            self.insert_scrollback_entry(&entry, theme)?;
        }
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();
    }
}


fn io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

fn short_id(session_id: &str) -> &str {
    session_id.get(..8).unwrap_or(session_id)
}

fn normalize_paste(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn slash_help_text() -> String {
    [
        "/new                 create a fresh session",
        "/sessions            list known sessions",
        "/resume <id>         resume a session",
        "/help                show this help",
        "/quit                exit astrcode",
    ]
    .join("\n")
}

fn resolve_session_id(state: &TuiState, input: &str) -> String {
    let needle = input.trim();
    state
        .available_sessions
        .iter()
        .find(|session_id| session_id.starts_with(needle))
        .cloned()
        .unwrap_or_else(|| needle.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constants() {
        const { assert!(INLINE_VIEWPORT_HEIGHT > 0) };
    }
}
