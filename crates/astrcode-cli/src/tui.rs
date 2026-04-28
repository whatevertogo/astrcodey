//! 交互式终端模式。
//!
//! 实现 astrcode 的 TUI（Terminal User Interface），包含消息记录视图、
//! 输入编辑器、斜杠命令面板和状态栏。使用 ratatui 框架进行终端渲染，
//! 通过 tokio 异步事件循环处理键盘输入和服务器事件。

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

/// 客户端类型别名，绑定进程内传输层。
type Client = AstrcodeClient<InProcessTransport>;

/// TUI 主入口：初始化终端、启动事件循环。
///
/// 创建客户端连接、进入备用终端屏幕、启动键盘读取线程，
/// 然后在主循环中通过 `tokio::select!` 同时处理键盘动作和服务器事件。
pub async fn run() -> io::Result<()> {
    let client = Arc::new(AstrcodeClient::new(InProcessTransport::start()));
    let mut stream = client.subscribe_events().await.map_err(io_error)?;
    let mut terminal = TerminalSession::enter()?;
    let theme = theme::Theme::detect();
    let mut state = TuiState::new();
    state.status = "Ready · type / for commands".into();

    // 创建无界通道用于键盘事件传递
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    spawn_keyboard_reader(action_tx.clone());

    // 首帧绘制
    terminal.draw(&state, &theme)?;

    // 主事件循环：同时等待键盘动作和服务器事件
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
                    // 将服务器事件应用到 TUI 状态
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
        // 仅在状态变更时重绘，避免不必要的终端刷新
        if state.dirty {
            terminal.draw(&state, &theme)?;
            state.dirty = false;
        }
    }

    Ok(())
}

/// 处理从键盘读取线程发来的 Action。
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

/// 处理单个键盘事件，将其映射到 TUI 操作。
///
/// 包括光标移动、文本编辑、斜杠命令面板导航、提交输入等。
async fn handle_key(event: KeyEvent, state: &mut TuiState, client: &Arc<Client>) -> io::Result<()> {
    match event.code {
        // Esc：关闭斜杠面板 / 中止当前对话轮次
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
        // Enter：Shift/Alt+Enter 插入换行，否则提交输入或确认斜杠命令
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
        // Tab：在斜杠面板中确认选择
        KeyCode::Tab if state.show_slash_palette => {
            accept_slash_selection(state, client).await?;
        },
        KeyCode::Tab => {},
        KeyCode::Backspace => state.backspace(),
        KeyCode::Delete => state.delete(),
        KeyCode::Left => state.move_left(),
        KeyCode::Right => state.move_right(),
        KeyCode::Home => state.move_home(),
        KeyCode::End => state.move_end(),
        // Up/Down：在斜杠面板中导航，否则浏览历史输入
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
        // 普通字符输入：忽略 Ctrl/Alt 组合键
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

/// 确认斜杠命令面板中的当前选中项。
///
/// 如果选中的命令需要参数但用户尚未输入，则仅补全命令前缀；
/// 否则直接提交当前输入行。
async fn accept_slash_selection(state: &mut TuiState, client: &Arc<Client>) -> io::Result<()> {
    let commands = slash::filtered(&state.slash_filter);
    let Some(spec) = commands
        .get(state.slash_selected.min(commands.len().saturating_sub(1)))
        .copied()
    else {
        state.close_slash();
        return Ok(());
    };

    // 检查当前输入是否已包含参数
    let current_has_argument = state
        .input
        .split_once(char::is_whitespace)
        .is_some_and(|(_, rest)| !rest.trim().is_empty());
    if spec.needs_argument && !current_has_argument {
        // 仅补全命令前缀，等待用户输入参数
        state.set_input(slash::command_line_for(spec));
        return Ok(());
    }

    submit_current_input(state, client).await
}

/// 提交当前输入框内容。
///
/// 空输入仅触发重绘；斜杠命令走专门的执行路径；
/// 正在流式输出时拒绝重复提交；否则将输入作为提示发送到服务器。
async fn submit_current_input(state: &mut TuiState, client: &Arc<Client>) -> io::Result<()> {
    let input = state.input.trim_end().to_string();
    if input.trim().is_empty() {
        state.mark_dirty();
        return Ok(());
    }

    // 尝试解析为斜杠命令
    if let Some(command) = slash::parse(&input) {
        state.take_input();
        execute_slash_command(command, state, client).await?;
        return Ok(());
    }

    // 正在流式输出时，不允许提交新提示
    if state.is_streaming {
        state.status = "Turn running · Esc stop".into();
        state.mark_dirty();
        return Ok(());
    }

    // 取出输入、记录历史、推送到消息列表、发送到服务器
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

/// 执行已解析的斜杠命令。
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

/// 在独立线程中轮询键盘事件，通过通道发送给主事件循环。
///
/// 使用 100ms 轮询间隔，将 crossterm 键盘事件转换为 Action。
/// 终端窗口大小变化会触发 Tick 以强制重绘。
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

/// 终端会话封装，管理 ratatui Terminal 的生命周期。
struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    /// 进入备用终端屏幕并初始化 ratatui。
    ///
    /// 启用 raw 模式、切换到备用屏幕缓冲区、清屏。
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;
        Ok(Self { terminal })
    }

    /// 根据当前 TUI 状态绘制一帧。
    fn draw(&mut self, state: &TuiState, theme: &theme::Theme) -> io::Result<()> {
        self.terminal
            .draw(|frame| render::render(state, frame, theme))
            .map(|_| ())
    }
}

impl Drop for TerminalSession {
    /// 退出时恢复终端状态：显示光标、离开备用屏幕、关闭 raw 模式。
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

/// 将实现了 Display 的错误转换为 io::Error。
fn io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

/// 截取会话 ID 的前 8 个字符作为短标识。
fn short_id(session_id: &str) -> &str {
    session_id.get(..8).unwrap_or(session_id)
}
