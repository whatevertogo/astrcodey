//! TUI — interactive terminal mode.
//!
//! Inline viewport design (same as codex-rs):
//! - History is written to terminal native scrollback via insert_history_lines
//! - Bottom panel (composer + status + footer) lives in a fixed inline viewport
//! - User can scroll up with mouse/keyboard to see history
//! - Resize: pending_viewport_area heuristic adjusts viewport position
//! - ToolRenderer / MessageRenderer registries (pi-mono design)
//! - AdaptiveChunkingPolicy for streaming (codex design)

// Suppress dead_code for Component infrastructure (built ahead of wiring).
#![allow(dead_code, unused_imports)]

pub(crate) mod app;
pub(crate) mod command;
pub(crate) mod composer;
pub(crate) mod custom_terminal;
pub(crate) mod ext;
pub(crate) mod frame;
pub(crate) mod insert_history;
pub(crate) mod keybinding;
pub(crate) mod render;
pub(crate) mod store;
pub(crate) mod streaming;
pub(crate) mod terminal;
pub(crate) mod terminal_probe;
pub(crate) mod theme;

use std::{io, sync::Arc};

use astrcode_client::client::AstrcodeClient;
use astrcode_protocol::commands::{ClientCommand, UiResponseValue};
use crossterm::event::{KeyCode, KeyModifiers};
use tokio_stream::StreamExt;

use self::{
    app::App,
    command::slash::{self, SlashCommand},
    frame::{
        FrameRequester,
        event_stream::{EventBroker, EventStream, TerminalFocus, TuiEvent},
    },
    streaming::{chunking::AdaptiveChunkingPolicy, commit_tick::run_commit_tick},
    terminal::TerminalSession,
    theme::Theme,
};
use crate::transport::InProcessTransport;

type Client = AstrcodeClient<InProcessTransport>;

/// TUI entry point — called from main.rs.
pub async fn run() -> io::Result<()> {
    let client = Arc::new(AstrcodeClient::new(InProcessTransport::start()));
    let mut server_stream = client.subscribe_events().await.map_err(io_error)?;

    let mut terminal = TerminalSession::enter()?;
    let theme = Theme::detect();
    let mut app = App::new(theme.clone());

    // Frame scheduling — draw_tx drives the event_stream's draw channel
    let (draw_tx, draw_rx) = tokio::sync::broadcast::channel::<()>(16);
    let _frame_requester = FrameRequester::new(draw_tx.clone());

    // Input event stream
    let broker = EventBroker::new();
    let focus = TerminalFocus::new();
    let mut event_stream = EventStream::new(broker, draw_rx, focus);

    // Streaming chunking policy
    let mut chunking_policy = AdaptiveChunkingPolicy::new();

    // Initial draw
    let panel = build_panel(&app, &theme);
    terminal.draw_frame(|frame| {
        render_panel(frame, &panel);
    })?;

    // Query extension commands
    client
        .send_command(&ClientCommand::ListExtensionCommands)
        .await
        .map_err(io_error)?;

    let mut exit_reason = None::<String>;

    loop {
        let dirty;
        tokio::select! {
            // Input events (keyboard, paste, resize/draw)
            event = event_stream.next() => {
                let Some(event) = event else {
                    exit_reason = Some("event stream ended".into());
                    break;
                };
                match event {
                    TuiEvent::Key(key) => {
                        handle_key(key, &mut app, &client, &mut terminal).await?;
                    },
                    TuiEvent::Paste(text) => {
                        let text = normalize_paste(&text);
                        app.composer.insert_paste(&text);
                    },
                    TuiEvent::Draw => {},
                    TuiEvent::ScrollUp(_) | TuiEvent::ScrollDown(_) => {},
                }
                dirty = true;
            },
            // Server notifications
            notification = server_stream.recv() => {
                app.apply(&notification.map_err(io_error)?);
                for pending in server_stream.drain_pending() {
                    app.apply(&pending);
                }
                // Commit streaming lines
                let now = std::time::Instant::now();
                for ctrl in app.stream_states.values_mut() {
                    let output = run_commit_tick(&mut chunking_policy, Some(ctrl), now);
                    for line in output.lines {
                        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                        app.scrollback_queue.push(store::transcript::ScrollbackEntry::StreamText {
                            role: store::transcript::MessageRole::Assistant,
                            text,
                        });
                    }
                }
                dirty = true;
            },
        }

        if app.needs_extension_refresh {
            app.needs_extension_refresh = false;
            if app.active_session_id.is_some() {
                client
                    .send_command(&ClientCommand::ListExtensionCommands)
                    .await
                    .map_err(io_error)?;
            }
        }

        if app.should_quit {
            break;
        }
        if dirty {
            // Session switch: clear old content before flushing new scrollback.
            if app.needs_terminal_reset {
                app.needs_terminal_reset = false;
                terminal.reset_for_session_switch()?;
            }
            // Flush scrollback entries into terminal native scrollback.
            let entries = std::mem::take(&mut app.scrollback_queue);
            terminal.flush_scrollback(entries, &theme, &app.message_renderers)?;
            // Redraw the bottom panel (inline viewport).
            let panel = build_panel(&app, &theme);
            let panel_height = panel_total_height(&panel);
            terminal.draw_frame_with_height(panel_height, |frame| {
                render_panel(frame, &panel);
            })?;
        }
    }

    drop(terminal);

    if let Some(reason) = exit_reason {
        eprintln!("[TUI] exited abnormally: {reason}");
    }

    Ok(())
}

// ─── Key handling ─────────────────────────────────────────────────────────────

async fn handle_key(
    key: crossterm::event::KeyEvent,
    app: &mut App,
    client: &Arc<Client>,
    terminal: &mut TerminalSession,
) -> io::Result<()> {
    // 服务端 UI picker 模式：拦截 Up/Down/Enter/Esc
    if app.ui_picker.is_some() {
        match key.code {
            KeyCode::Esc => {
                app.close_ui_picker();
                app.status_text = "Ready".into();
            },
            KeyCode::Up => {
                app.ui_picker_up();
            },
            KeyCode::Down => {
                app.ui_picker_down();
            },
            KeyCode::Enter => {
                if let Some((request_id, selected)) = app.ui_picker_accept() {
                    client
                        .send_command(&ClientCommand::UiResponse {
                            request_id,
                            value: UiResponseValue::Select { selected },
                        })
                        .await
                        .map_err(io_error)?;
                    app.status_text = "Selection submitted".into();
                }
            },
            _ => {},
        }
        return Ok(());
    }

    // Session picker 模式：拦截 Up/Down/Enter/Esc
    if app.session_picker.is_some() {
        match key.code {
            KeyCode::Esc => {
                app.close_session_picker();
            },
            KeyCode::Up => {
                app.session_picker_up();
            },
            KeyCode::Down => {
                app.session_picker_down();
            },
            KeyCode::Enter => {
                if let Some(sid) = app.session_picker_accept() {
                    client
                        .send_command(&ClientCommand::ResumeSession { session_id: sid })
                        .await
                        .map_err(io_error)?;
                    app.status_text = "Resuming session".into();
                }
            },
            _ => {},
        }
        return Ok(());
    }

    // 任何非 Ctrl+C 的按键重置退出等待
    if !(key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)) {
        app.reset_quit_pending();
    }

    match key.code {
        KeyCode::Esc => {
            if app.is_streaming {
                client
                    .send_command(&ClientCommand::Abort)
                    .await
                    .map_err(io_error)?;
                app.status_text = "Stopping turn".into();
            } else if app.show_slash_palette {
                app.close_slash();
            }
        },
        KeyCode::Enter => {
            if key.modifiers.contains(KeyModifiers::SHIFT)
                || key.modifiers.contains(KeyModifiers::ALT)
            {
                app.composer.insert_char('\n');
            } else if app.show_slash_palette {
                accept_slash_selection(app, client).await?;
            } else {
                submit_current_input(app, client).await?;
            }
        },
        KeyCode::Tab if app.show_slash_palette => {
            complete_slash_selection(app);
        },
        // Shift+Tab: 查询插件注册的快捷键绑定
        KeyCode::BackTab => {
            dispatch_keybinding("shift+tab", app, client).await?;
        },
        KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => {
            app.composer.delete_previous_word();
        },
        KeyCode::Backspace => {
            app.composer.backspace();
            app.sync_slash_filter_pub();
        },
        KeyCode::Delete => {
            app.composer.delete();
        },
        KeyCode::Left => {
            app.composer.move_left();
        },
        KeyCode::Right => {
            app.composer.move_right();
        },
        KeyCode::Home => {
            app.composer.move_home();
        },
        KeyCode::End => {
            app.composer.move_end();
        },
        KeyCode::Up => {
            if app.show_slash_palette {
                app.slash_move_up();
            } else if !app.composer.move_visual_up(terminal.composer_width()) {
                app.history_previous();
            }
        },
        KeyCode::Down => {
            if app.show_slash_palette {
                app.slash_move_down();
            } else if !app.composer.move_visual_down(terminal.composer_width()) {
                app.history_next();
            }
        },
        KeyCode::Char(ch) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            match ch.to_ascii_lowercase() {
                'a' => {
                    app.composer.move_home();
                },
                'e' => {
                    app.composer.move_end();
                },
                'u' => {
                    app.composer.delete_before_cursor();
                },
                'k' => {
                    app.composer.delete_after_cursor();
                },
                'w' => {
                    app.composer.delete_previous_word();
                },
                'c' => {
                    app.handle_quit_request();
                },
                _ => {},
            }
        },
        KeyCode::PageUp => {
            // No-op in inline viewport mode (user scrolls with terminal native scroll).
        },
        KeyCode::PageDown => {
            // No-op in inline viewport mode.
        },
        KeyCode::Char(ch) => {
            if key.modifiers.contains(KeyModifiers::ALT) {
                return Ok(());
            }
            app.composer.insert_char(ch);
            app.sync_slash_filter_pub();
        },
        _ => {},
    }
    Ok(())
}

async fn accept_slash_selection(app: &mut App, client: &Arc<Client>) -> io::Result<()> {
    let commands = slash::filtered(&app.slash_filter, &app.extension_commands);
    let Some(spec) = commands
        .get(app.slash_selected.min(commands.len().saturating_sub(1)))
        .cloned()
    else {
        app.close_slash();
        return Ok(());
    };

    let cmd_name = spec.usage.split_whitespace().next().unwrap_or(&spec.usage);
    let argument = app
        .input_text()
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest.trim())
        .unwrap_or("");

    if spec.needs_argument && argument.is_empty() {
        app.set_input(format!("{cmd_name} "));
        return Ok(());
    }

    let full_input = if argument.is_empty() {
        cmd_name.to_string()
    } else {
        format!("{cmd_name} {argument}")
    };
    app.set_input(full_input);
    submit_current_input(app, client).await
}

fn complete_slash_selection(app: &mut App) {
    let commands = slash::filtered(&app.slash_filter, &app.extension_commands);
    let Some(spec) = commands
        .get(app.slash_selected.min(commands.len().saturating_sub(1)))
        .cloned()
    else {
        return;
    };
    app.set_input(slash::command_line_for(&spec));
}

async fn submit_current_input(app: &mut App, client: &Arc<Client>) -> io::Result<()> {
    let input = app.input_text().trim_end().to_string();
    if input.trim().is_empty() {
        return Ok(());
    }

    if let Some(command) = slash::parse(
        &input,
        &app.extension_commands
            .iter()
            .map(|c| c.name.clone())
            .collect::<Vec<_>>(),
    ) {
        let input = app.take_input();
        app.remember_input(&input);
        execute_slash_command(command, app, client).await?;
        return Ok(());
    }

    if app.is_streaming {
        let input = app.take_input();
        if input.trim().is_empty() {
            return Ok(());
        }
        app.remember_input(&input);
        app.push_user(&input);
        app.status_text = "Message queued".into();
        client
            .send_command(&ClientCommand::InjectMessage { text: input })
            .await
            .map_err(io_error)?;
        return Ok(());
    }

    let input = app.take_input();
    app.remember_input(&input);
    app.push_user(&input);

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
    command: SlashCommand,
    app: &mut App,
    client: &Arc<Client>,
) -> io::Result<()> {
    match command {
        SlashCommand::New => {
            let working_dir = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| ".".into());
            client
                .send_command(&ClientCommand::CreateSession { working_dir })
                .await
                .map_err(io_error)?;
            app.status_text = "Creating session".into();
        },
        SlashCommand::Resume(session_id) => {
            if session_id.trim().is_empty() {
                // 无参数：打开 session picker
                client
                    .send_command(&ClientCommand::ListSessions)
                    .await
                    .map_err(io_error)?;
                app.open_session_picker();
            } else {
                let sid = app.resolve_session_id(&session_id);
                client
                    .send_command(&ClientCommand::ResumeSession { session_id: sid })
                    .await
                    .map_err(io_error)?;
                app.status_text = "Resuming session".into();
            }
        },
        SlashCommand::Compact => {
            client
                .send_command(&ClientCommand::Compact)
                .await
                .map_err(io_error)?;
            app.status_text = "Compacting session".into();
        },
        SlashCommand::Recap => {
            client
                .send_command(&ClientCommand::Recap)
                .await
                .map_err(io_error)?;
            app.status_text = "Generating recap...".into();
        },
        SlashCommand::Quit => {
            app.should_quit = true;
        },
        SlashCommand::Help => {
            let mut lines = vec![
                "/new                 create a fresh session".into(),
                "/resume              resume a session (picker)".into(),
                "/resume <id>         resume a session by id".into(),
                "/help                show this help".into(),
                "/quit                exit astrcode".into(),
            ];
            for cmd in &app.extension_commands {
                let padding = if cmd.needs_argument { " <args>" } else { "" };
                lines.push(format!("/{}{}", cmd.name, padding));
            }
            app.push_message(
                store::transcript::MessageRole::System,
                "Help".into(),
                lines.join("\n"),
                false,
                None,
            );
        },
        SlashCommand::Extension { name, arguments } => {
            client
                .send_command(&ClientCommand::ExecuteExtensionCommand {
                    command_name: name,
                    arguments,
                })
                .await
                .map_err(io_error)?;
            app.status_text = "Executing command".into();
        },
    }
    Ok(())
}

/// 通过插件注册的 keybinding 表分发快捷键 → 执行对应的扩展命令。
async fn dispatch_keybinding(key_id: &str, app: &mut App, client: &Arc<Client>) -> io::Result<()> {
    if let Some((command, arguments)) = keybinding::find_command_for_key(&app.keybindings, key_id) {
        client
            .send_command(&ClientCommand::ExecuteExtensionCommand {
                command_name: command.to_string(),
                arguments: arguments.to_string(),
            })
            .await
            .map_err(io_error)?;
    }
    Ok(())
}

// ─── Rendering ────────────────────────────────────────────────────────────────

/// Panel state built from App, then rendered into the Frame.
struct Panel {
    composer_lines: Vec<ratatui::text::Line<'static>>,
    slash_lines: Vec<ratatui::text::Line<'static>>,
    status_line: ratatui::text::Line<'static>,
    footer_line: ratatui::text::Line<'static>,
    cursor_col: u16,
    cursor_row: u16,
}

fn build_panel(app: &App, theme: &Theme) -> Panel {
    use ratatui::text::{Line, Span};
    use render::layout_visual_text;

    let width = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
        .saturating_sub(4);

    let vl = layout_visual_text(app.composer.text(), width, Some(app.composer.cursor()));
    let cursor_col = 2 + vl.cursor_column.unwrap_or(0) as u16;
    let cursor_row = vl.cursor_row.unwrap_or(0) as u16;

    let composer_lines: Vec<Line<'static>> = if app.composer.text().is_empty() {
        vec![Line::from(vec![
            Span::styled("> ", theme.assistant_label),
            Span::styled(
                "Ask astrcode to inspect, edit, or explain...",
                theme.composer_placeholder,
            ),
        ])]
    } else {
        vl.lines
            .into_iter()
            .enumerate()
            .map(|(idx, line)| {
                let prefix = if idx == 0 { "> " } else { "  " };
                Line::from(vec![
                    Span::styled(prefix, theme.assistant_label),
                    Span::styled(line, theme.composer),
                ])
            })
            .collect()
    };

    let status_line = Line::from(Span::styled(format!("  {}", app.status_text), theme.dim));

    // Slash palette: 输入 / 时显示匹配的命令列表（滑动窗口）
    let slash_lines: Vec<Line<'static>> = if app.show_slash_palette {
        let commands = slash::filtered(&app.slash_filter, &app.extension_commands);
        let max_visible = 8usize;
        let total = commands.len();
        let selected = app.slash_selected.min(total.saturating_sub(1));

        // 滑动窗口：保证选中项始终可见
        let window_start = if total <= max_visible || selected < max_visible / 2 {
            0
        } else if selected >= total.saturating_sub(max_visible / 2) {
            total.saturating_sub(max_visible)
        } else {
            selected.saturating_sub(max_visible / 2)
        };
        let window_end = (window_start + max_visible).min(total);

        let mut lines: Vec<Line<'static>> = Vec::new();

        // 顶部溢出指示
        if window_start > 0 {
            lines.push(Line::from(Span::styled(
                format!("    ↑ {} more", window_start),
                theme.dim,
            )));
        }

        for (i, cmd) in commands[window_start..window_end].iter().enumerate() {
            let abs_i = window_start + i;
            let marker = if abs_i == selected { "▸" } else { " " };
            let desc = if cmd.description.chars().count() > 40 {
                let truncated: String = cmd.description.chars().take(39).collect();
                format!("{truncated}…")
            } else {
                cmd.description.clone()
            };
            if abs_i == selected {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {marker} /{:<16} ", cmd.name),
                        theme.popup_selected,
                    ),
                    Span::styled(desc, theme.popup_selected),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {marker} /"), theme.dim),
                    Span::styled(format!("{:<16} ", cmd.name), theme.body),
                    Span::styled(desc, theme.dim),
                ]));
            }
        }

        // 底部溢出指示
        if window_end < total {
            lines.push(Line::from(Span::styled(
                format!("    ↓ {} more", total - window_end),
                theme.dim,
            )));
        }

        lines
    } else if let Some(picker) = &app.ui_picker {
        // 服务端 UI picker 渲染（滑动窗口）
        let max_visible = 10usize;
        let total = picker.items.len();
        let selected = picker.selected.min(total.saturating_sub(1));
        let window_start = if total <= max_visible || selected < max_visible / 2 {
            0
        } else if selected >= total.saturating_sub(max_visible / 2) {
            total.saturating_sub(max_visible)
        } else {
            selected.saturating_sub(max_visible / 2)
        };
        let window_end = (window_start + max_visible).min(total);

        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(Span::styled(
            format!("  {} (↑↓ Enter Esc):", picker.message),
            theme.dim,
        )));
        if window_start > 0 {
            lines.push(Line::from(Span::styled(
                format!("    ↑ {} more", window_start),
                theme.dim,
            )));
        }
        for i in window_start..window_end {
            let item = &picker.items[i];
            let marker = if i == selected { "▸" } else { " " };
            let style = if i == selected {
                theme.popup_selected
            } else {
                theme.body
            };
            lines.push(Line::from(Span::styled(
                format!("  {marker} {item}"),
                style,
            )));
        }
        if window_end < total {
            lines.push(Line::from(Span::styled(
                format!("    ↓ {} more", total - window_end),
                theme.dim,
            )));
        }
        if total == 0 {
            lines.push(Line::from(Span::styled("    No options", theme.dim)));
        }
        lines
    } else if let Some(picker) = &app.session_picker {
        // Session picker 渲染（滑动窗口）
        let max_visible = 10usize;
        let total = picker.items.len();
        let selected = picker.selected.min(total.saturating_sub(1));
        let window_start = if total <= max_visible || selected < max_visible / 2 {
            0
        } else if selected >= total.saturating_sub(max_visible / 2) {
            total.saturating_sub(max_visible)
        } else {
            selected.saturating_sub(max_visible / 2)
        };
        let window_end = (window_start + max_visible).min(total);

        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(Span::styled(
            "  Select session (↑↓ Enter Esc):",
            theme.dim,
        )));
        // cwd 提示行：让用户清楚自己在看哪个目录的 session
        if !picker.cwd.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("    in {}", compact_path(&picker.cwd)),
                theme.dim,
            )));
        }
        if window_start > 0 {
            lines.push(Line::from(Span::styled(
                format!("    ↑ {} more", window_start),
                theme.dim,
            )));
        }
        let now = chrono::Utc::now();
        for i in window_start..window_end {
            let entry = &picker.items[i];
            let marker = if i == selected { "▸" } else { " " };
            let id_short = entry.session_id.get(..8).unwrap_or(&entry.session_id);
            let rel = store::session_picker::format_relative_time(&entry.last_active_at, now);
            // 时间列右对齐到 4 个字符宽，缺失时间用空格占位以保持对齐。
            let time_col = format!("{:>4}", rel);
            let prefix = format!("  {marker} {id_short}  ");
            let suffix = format!("  {}", entry.title);
            let style = if i == selected {
                theme.popup_selected
            } else {
                theme.body
            };
            lines.push(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(time_col, theme.dim),
                Span::styled(suffix, style),
            ]));
        }
        if window_end < total {
            lines.push(Line::from(Span::styled(
                format!("    ↓ {} more", total - window_end),
                theme.dim,
            )));
        }
        if total == 0 {
            lines.push(Line::from(Span::styled(
                "    No other sessions in this project",
                theme.dim,
            )));
        }
        lines
    } else {
        Vec::new()
    };

    let session = app
        .active_session_id
        .as_deref()
        .map(|id| id.get(..8).unwrap_or(id))
        .unwrap_or("none");
    let model = if app.model_name.is_empty() {
        "model: pending"
    } else {
        &app.model_name
    };
    let cwd = if app.working_dir.is_empty() {
        "cwd pending".to_string()
    } else {
        compact_path(&app.working_dir)
    };
    let hints = if app.is_streaming {
        "Esc stop"
    } else {
        "Enter send · /help"
    };
    // 拼接插件注册的状态栏项（按 key 字母序，排除空值）
    let extension_status: String = app
        .status_items
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(_, v)| v.as_str())
        .collect::<Vec<_>>()
        .join(" · ");
    let footer_text = if extension_status.is_empty() {
        format!("  {model} · {cwd} · {session}   {hints}")
    } else {
        format!("  [{extension_status}] {model} · {cwd} · {session}   {hints}")
    };
    let footer_line = Line::from(Span::styled(footer_text, theme.footer));

    Panel {
        composer_lines,
        slash_lines,
        status_line,
        footer_line,
        cursor_col,
        cursor_row,
    }
}

/// 计算 panel 需要的总行数（composer + slash palette + status + footer）。
fn panel_total_height(panel: &Panel) -> u16 {
    let composer = panel.composer_lines.len().max(1) as u16;
    let slash = panel.slash_lines.len() as u16;
    let status = 1u16;
    let footer = 1u16;
    composer + slash + status + footer
}

fn render_panel(frame: &mut custom_terminal::Frame<'_>, panel: &Panel) {
    use ratatui::{
        layout::{Constraint, Direction, Layout},
        text::{Line, Span, Text},
        widgets::Paragraph,
    };

    let area = frame.area();
    if area.height < 3 {
        return;
    }

    // Layout: [composer(N), slash_palette(?), status(1), footer(1)]
    let footer_height = 1u16;
    let status_height = 1u16;
    let slash_height = panel.slash_lines.len() as u16;
    let composer_height = panel.composer_lines.len().max(1) as u16;

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(composer_height),
            Constraint::Length(slash_height),
            Constraint::Length(status_height),
            Constraint::Length(footer_height),
        ])
        .split(area);

    // Composer
    let text = Text::from(panel.composer_lines.clone());
    frame.render_widget(Paragraph::new(text), layout[0]);

    // Cursor position within the composer area.
    let cx = layout[0].x + panel.cursor_col.min(layout[0].width.saturating_sub(1));
    let cy = layout[0].y + panel.cursor_row.min(layout[0].height.saturating_sub(1));
    frame.set_cursor_position((cx, cy));

    // Slash palette
    if !panel.slash_lines.is_empty() {
        let slash_text = Text::from(panel.slash_lines.clone());
        frame.render_widget(Paragraph::new(slash_text), layout[1]);
    }

    // Status
    frame.render_widget(Paragraph::new(panel.status_line.clone()), layout[2]);

    // Footer
    frame.render_widget(Paragraph::new(panel.footer_line.clone()), layout[3]);
}

fn compact_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let parts: Vec<_> = normalized.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() <= 3 {
        return normalized;
    }
    let root = if normalized.contains(":/") {
        parts.first().copied().unwrap_or_default()
    } else if normalized.starts_with('/') {
        ""
    } else {
        parts.first().copied().unwrap_or_default()
    };
    let tail = &parts[parts.len().saturating_sub(2)..];
    if root.is_empty() {
        format!("/.../{}", tail.join("/"))
    } else {
        format!("{root}/.../{}", tail.join("/"))
    }
}

fn normalize_paste(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}
