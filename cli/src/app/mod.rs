mod coordinator;
mod reducer;

use std::{
    env, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use astrcode_client::{
    AstrcodeClient, ClientConfig, ClientError, ClientTransport,
    ConversationSlashCandidatesResponseDto, ConversationSnapshotResponseDto,
    ConversationStreamItem, CurrentModelInfoDto, ModeSummaryDto, ModelOptionDto,
    PromptSubmitResponse, ReqwestTransport, SessionListItem, SessionModeStateDto,
};
use clap::Parser;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste,
        Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent,
        MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use ratatui::backend::CrosstermBackend;
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{self, MissedTickBehavior},
};

use crate::{
    bottom_pane::{BottomPaneState, SurfaceLayout, render_bottom_pane},
    capability::TerminalCapabilities,
    chat::ChatSurfaceState,
    command::{fuzzy_contains, palette_action},
    launcher::{LaunchOptions, Launcher, LauncherSession, SystemManagedServer},
    state::{CliState, PaletteState, PaneFocus, StreamRenderMode},
    tui::TuiRuntime,
    ui::{CodexTheme, overlay::render_browser_overlay},
};

#[derive(Debug, Parser)]
#[command(name = "astrcode-cli")]
#[command(about = "Astrcode 的正式 terminal frontend")]
struct CliArgs {
    #[arg(long)]
    server_origin: Option<String>,
    #[arg(long)]
    token: Option<String>,
    #[arg(long)]
    working_dir: Option<PathBuf>,
    #[arg(long)]
    run_info_path: Option<PathBuf>,
    #[arg(long)]
    server_binary: Option<PathBuf>,
}

#[derive(Debug)]
struct SnapshotLoadedAction {
    session_id: String,
    result: Result<ConversationSnapshotResponseDto, ClientError>,
}

#[derive(Debug)]
enum Action {
    Tick,
    Key(KeyEvent),
    Paste(String),
    Resize {
        width: u16,
        height: u16,
    },
    Mouse(MouseEvent),
    Quit,
    SessionsRefreshed(Result<Vec<SessionListItem>, ClientError>),
    SessionCreated(Result<SessionListItem, ClientError>),
    SnapshotLoaded(Box<SnapshotLoadedAction>),
    StreamBatch {
        session_id: String,
        items: Vec<ConversationStreamItem>,
    },
    SlashCandidatesLoaded {
        query: String,
        result: Result<ConversationSlashCandidatesResponseDto, ClientError>,
    },
    CurrentModelLoaded(Result<CurrentModelInfoDto, ClientError>),
    ModesLoaded(Result<Vec<ModeSummaryDto>, ClientError>),
    ModelOptionsLoaded {
        query: String,
        result: Result<Vec<ModelOptionDto>, ClientError>,
    },
    PromptSubmitted {
        session_id: String,
        result: Result<PromptSubmitResponse, ClientError>,
    },
    ModelSelectionSaved {
        profile_name: String,
        model: String,
        result: Result<(), ClientError>,
    },
    CompactRequested {
        session_id: String,
        result: Result<astrcode_client::CompactSessionResponse, ClientError>,
    },
    SessionModeLoaded {
        session_id: String,
        result: Result<SessionModeStateDto, ClientError>,
    },
    ModeSwitched {
        session_id: String,
        requested_mode_id: String,
        result: Result<SessionModeStateDto, ClientError>,
    },
}

pub async fn run_from_env() -> Result<()> {
    let args = CliArgs::parse();
    let launcher = Launcher::new();
    let working_dir = resolve_working_dir(args.working_dir)?;
    let launch_options = LaunchOptions {
        server_origin: args.server_origin,
        bootstrap_token: args.token,
        working_dir: Some(working_dir.clone()),
        run_info_path: args.run_info_path,
        server_binary: args.server_binary,
        ..LaunchOptions::default()
    };
    let launcher_session = launcher.resolve(launch_options).await?;
    run_app(launcher_session).await
}

async fn run_app(launcher_session: LauncherSession<SystemManagedServer>) -> Result<()> {
    let mut launcher_session = launcher_session;
    let connection = launcher_session.connection().clone();
    let debug_tap = launcher_session
        .managed_server_mut()
        .map(|server| server.debug_tap());
    let client = AstrcodeClient::new(ClientConfig::new(connection.origin.clone()));
    let capabilities = TerminalCapabilities::detect();
    client
        .exchange_auth(connection.bootstrap_token.clone())
        .await
        .context("exchange auth with astrcode-server failed")?;

    let (actions_tx, actions_rx) = mpsc::unbounded_channel();
    let mut controller = AppController::new(
        client,
        CliState::new(
            connection.origin,
            connection.working_dir.clone(),
            capabilities,
        ),
        debug_tap,
        AppControllerChannels::new(actions_tx.clone(), actions_rx),
    );

    controller.refresh_current_model().await;
    controller.refresh_modes().await;
    controller.refresh_model_options(String::new()).await;
    controller.bootstrap().await?;

    let terminal_result = run_terminal_loop(&mut controller, actions_tx.clone()).await;

    controller.stop_background_tasks();
    let shutdown_result = launcher_session.shutdown().await;

    terminal_result?;
    shutdown_result?;
    Ok(())
}

async fn run_terminal_loop(
    controller: &mut AppController,
    actions_tx: mpsc::UnboundedSender<Action>,
) -> Result<()> {
    let terminal_guard = TerminalRestoreGuard::enter(controller.state.shell.capabilities)?;
    let stdout = io::stdout();

    let backend = CrosstermBackend::new(stdout);
    let mut runtime = TuiRuntime::with_backend(backend).context("create TUI runtime failed")?;

    let input_handle = InputHandle::spawn(actions_tx.clone());
    let tick_handle = spawn_tick_loop(actions_tx);

    let loop_result = run_event_loop(controller, &mut runtime).await;

    input_handle.stop();
    tick_handle.stop().await;
    runtime
        .terminal_mut()
        .show_cursor()
        .context("show cursor failed")?;
    drop(terminal_guard);

    loop_result
}

async fn run_event_loop(
    controller: &mut AppController,
    runtime: &mut TuiRuntime<CrosstermBackend<io::Stdout>>,
) -> Result<()> {
    redraw(controller, runtime).context("initial draw failed")?;
    controller.state.render.take_frame_dirty();

    while let Some(action) = controller.actions_rx.recv().await {
        controller.handle_action(action).await?;
        if controller.state.render.take_frame_dirty() {
            redraw(controller, runtime).context("redraw failed")?;
        }
        if controller.should_quit {
            break;
        }
    }

    Ok(())
}

fn redraw(
    controller: &mut AppController,
    runtime: &mut TuiRuntime<CrosstermBackend<io::Stdout>>,
) -> Result<()> {
    let size = runtime.screen_size().context("read terminal size failed")?;
    let theme = CodexTheme::new(controller.state.shell.capabilities);
    let mut chat = controller
        .chat_surface
        .build_frame(&controller.state, &theme, size.width);
    runtime.stage_history_lines(
        std::mem::take(&mut chat.history_lines)
            .into_iter()
            .map(|line| crate::ui::history_line_to_ratatui(line, &theme)),
    );
    let pane = BottomPaneState::from_cli(&controller.state, &chat, &theme, size.width);
    let layout = SurfaceLayout::new(size, controller.state.render.active_overlay, &pane);
    runtime
        .draw(
            layout.viewport_height(),
            controller.state.render.active_overlay.is_open(),
            |frame, area| {
                if controller.state.render.active_overlay.is_open() {
                    render_browser_overlay(frame, &controller.state, &theme);
                } else {
                    render_bottom_pane(frame, area, &controller.state, &pane, &theme);
                }
            },
        )
        .context("draw CLI surface failed")?;
    Ok(())
}

#[derive(Clone, Default)]
struct SharedStreamPacer {
    inner: Arc<std::sync::Mutex<StreamPacerState>>,
}

#[derive(Default)]
struct StreamPacerState {
    mode: StreamRenderMode,
    pending_chunks: usize,
    oldest_chunk_at: Option<std::time::Instant>,
}

impl SharedStreamPacer {
    fn note_enqueued(&self, count: usize) {
        let mut state = self.inner.lock().expect("stream pacer lock poisoned");
        if count == 0 {
            return;
        }
        if state.pending_chunks == 0 {
            state.oldest_chunk_at = Some(std::time::Instant::now());
        }
        state.pending_chunks += count;
    }

    fn note_consumed(&self, count: usize) {
        let mut state = self.inner.lock().expect("stream pacer lock poisoned");
        state.pending_chunks = state.pending_chunks.saturating_sub(count);
        if state.pending_chunks == 0 {
            state.oldest_chunk_at = None;
        }
    }

    fn update_mode(&self) -> (StreamRenderMode, usize, Duration) {
        let mut state = self.inner.lock().expect("stream pacer lock poisoned");
        let oldest = state
            .oldest_chunk_at
            .map(|instant| instant.elapsed())
            .unwrap_or(Duration::ZERO);
        state.mode = if state.pending_chunks >= 8 || oldest >= Duration::from_millis(200) {
            StreamRenderMode::CatchUp
        } else {
            StreamRenderMode::Smooth
        };
        (state.mode, state.pending_chunks, oldest)
    }

    fn mode(&self) -> StreamRenderMode {
        self.inner.lock().expect("stream pacer lock poisoned").mode
    }

    fn reset(&self) {
        let mut state = self.inner.lock().expect("stream pacer lock poisoned");
        *state = StreamPacerState::default();
    }
}

struct AppController<T = ReqwestTransport> {
    client: AstrcodeClient<T>,
    state: CliState,
    chat_surface: ChatSurfaceState,
    debug_tap: Option<crate::launcher::DebugLogTap>,
    actions_tx: mpsc::UnboundedSender<Action>,
    actions_rx: mpsc::UnboundedReceiver<Action>,
    pending_session_id: Option<String>,
    pending_bootstrap_session_refresh: bool,
    stream_task: Option<JoinHandle<()>>,
    stream_pacer: SharedStreamPacer,
    should_quit: bool,
}

struct AppControllerChannels {
    tx: mpsc::UnboundedSender<Action>,
    rx: mpsc::UnboundedReceiver<Action>,
}

impl AppControllerChannels {
    fn new(tx: mpsc::UnboundedSender<Action>, rx: mpsc::UnboundedReceiver<Action>) -> Self {
        Self { tx, rx }
    }
}

struct TerminalRestoreGuard {
    capabilities: TerminalCapabilities,
}

impl TerminalRestoreGuard {
    fn enter(capabilities: TerminalCapabilities) -> Result<Self> {
        enable_raw_mode().context("enable raw mode failed")?;
        let mut stdout = io::stdout();
        if capabilities.bracketed_paste {
            execute!(stdout, EnableBracketedPaste).context("enable bracketed paste failed")?;
        }
        Ok(Self { capabilities })
    }
}

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        if self.capabilities.bracketed_paste {
            let _ = execute!(stdout, DisableBracketedPaste);
        }
        let _ = execute!(stdout, DisableMouseCapture);
    }
}

impl<T> AppController<T>
where
    T: ClientTransport + 'static,
{
    fn new(
        client: AstrcodeClient<T>,
        state: CliState,
        debug_tap: Option<crate::launcher::DebugLogTap>,
        channels: AppControllerChannels,
    ) -> Self {
        Self {
            client,
            state,
            chat_surface: ChatSurfaceState::default(),
            debug_tap,
            actions_tx: channels.tx,
            actions_rx: channels.rx,
            pending_session_id: None,
            pending_bootstrap_session_refresh: false,
            stream_task: None,
            stream_pacer: SharedStreamPacer::default(),
            should_quit: false,
        }
    }

    async fn bootstrap(&mut self) -> Result<()> {
        // bootstrap 故意复用异步命令链路，避免在初始化阶段维护一套单独的 /new 创建路径。
        self.pending_bootstrap_session_refresh = true;
        self.execute_command(crate::command::Command::New).await;
        Ok(())
    }

    fn stop_background_tasks(&mut self) {
        if let Some(stream_task) = self.stream_task.take() {
            stream_task.abort();
        }
    }

    async fn consume_bootstrap_refresh(&mut self) {
        if self.pending_bootstrap_session_refresh {
            self.pending_bootstrap_session_refresh = false;
            self.refresh_sessions().await;
        }
    }

    async fn handle_action(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Tick => {
                if let Some(debug_tap) = &self.debug_tap {
                    for line in debug_tap.drain() {
                        self.state.push_debug_line(line);
                    }
                }
                let (mode, pending, oldest) = self.stream_pacer.update_mode();
                self.state.set_stream_mode(mode, pending, oldest);
                self.state.advance_thinking_playback();
            },
            Action::Quit => self.should_quit = true,
            Action::Resize { width, height } => self.state.note_terminal_resize(width, height),
            Action::Mouse(mouse) => self.handle_mouse(mouse),
            Action::Key(key) => self.handle_key(key).await?,
            Action::Paste(text) => self.handle_paste(text).await?,
            Action::SessionsRefreshed(result) => match result {
                Ok(sessions) => {
                    self.state.update_sessions(sessions);
                    self.refresh_resume_palette();
                },
                Err(error) => self.apply_status_error(error),
            },
            Action::SessionCreated(result) => match result {
                Ok(session) => {
                    let session_id = session.session_id.clone();
                    let mut sessions = self.state.conversation.sessions.clone();
                    sessions.retain(|existing| existing.session_id != session_id);
                    sessions.push(session);
                    sessions.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
                    self.state.update_sessions(sessions);
                    self.begin_session_hydration(session_id).await;
                },
                Err(error) => {
                    self.apply_status_error(error);
                    self.consume_bootstrap_refresh().await;
                },
            },
            Action::SnapshotLoaded(payload) => {
                let SnapshotLoadedAction { session_id, result } = *payload;
                if !self.pending_session_matches(session_id.as_str()) {
                    return Ok(());
                }
                match result {
                    Ok(snapshot) => {
                        self.pending_session_id = None;
                        self.chat_surface.reset();
                        self.state.activate_snapshot(snapshot);
                        self.state
                            .set_status(format!("attached to session {}", session_id));
                        self.open_stream_for_active_session().await;
                        self.consume_bootstrap_refresh().await;
                    },
                    Err(error) => {
                        self.pending_session_id = None;
                        self.apply_hydration_error(error);
                        self.consume_bootstrap_refresh().await;
                    },
                }
            },
            Action::StreamBatch { session_id, items } => {
                let batch_len = items.len();
                if !self.active_session_matches(session_id.as_str()) {
                    self.stream_pacer.note_consumed(batch_len);
                    return Ok(());
                }
                for item in items {
                    self.apply_stream_event(session_id.as_str(), item).await;
                }
                self.stream_pacer.note_consumed(batch_len);
                let (mode, pending, oldest) = self.stream_pacer.update_mode();
                self.state.set_stream_mode(mode, pending, oldest);
            },
            Action::SlashCandidatesLoaded { query, result } => {
                let PaletteState::Slash(palette) = &self.state.interaction.palette else {
                    return Ok(());
                };
                if palette.query != query {
                    return Ok(());
                }

                match result {
                    Ok(candidates) => {
                        let items = slash_candidates_with_local_commands(
                            &candidates.items,
                            &self.state.shell.available_modes,
                            query.as_str(),
                        );
                        self.state.set_slash_query(query, items);
                    },
                    Err(error) => self.apply_status_error(error),
                }
            },
            Action::CurrentModelLoaded(result) => match result {
                Ok(current_model) => self.state.update_current_model(current_model),
                Err(error) => self.apply_status_error(error),
            },
            Action::ModesLoaded(result) => match result {
                Ok(modes) => self.state.update_modes(modes),
                Err(error) => self.apply_status_error(error),
            },
            Action::ModelOptionsLoaded { query, result } => match result {
                Ok(model_options) => {
                    self.state.update_model_options(model_options.clone());
                    if let PaletteState::Model(palette) = &self.state.interaction.palette {
                        if palette.query == query {
                            self.state.set_model_query(
                                query,
                                filter_model_options(&model_options, palette.query.as_str()),
                            );
                        }
                    }
                },
                Err(error) => self.apply_status_error(error),
            },
            Action::PromptSubmitted { session_id, result } => {
                if !self.active_session_matches(session_id.as_str()) {
                    return Ok(());
                }
                match result {
                    Ok(_response) => self.state.set_status("ready"),
                    Err(error) => self.apply_status_error(error),
                }
            },
            Action::ModelSelectionSaved {
                profile_name,
                model,
                result,
            } => match result {
                Ok(()) => {
                    let provider_kind = self
                        .state
                        .shell
                        .model_options
                        .iter()
                        .find(|option| option.profile_name == profile_name && option.model == model)
                        .map(|option| option.provider_kind.clone())
                        .unwrap_or_else(|| "unknown".to_string());
                    self.state.update_current_model(CurrentModelInfoDto {
                        profile_name,
                        model: model.clone(),
                        provider_kind,
                    });
                    self.state.set_status(format!("ready · model {model}"));
                    self.refresh_current_model().await;
                },
                Err(error) => self.apply_status_error(error),
            },
            Action::CompactRequested { session_id, result } => {
                if !self.active_session_matches(session_id.as_str()) {
                    return Ok(());
                }
                match result {
                    Ok(response) => {
                        self.state.set_status(response.message);
                    },
                    Err(error) => self.apply_status_error(error),
                }
            },
            Action::SessionModeLoaded { session_id, result } => {
                if !self.active_session_matches(session_id.as_str()) {
                    return Ok(());
                }
                match result {
                    Ok(mode) => {
                        let available = self
                            .state
                            .shell
                            .available_modes
                            .iter()
                            .map(|summary| summary.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        if available.is_empty() {
                            self.state
                                .set_status(format!("mode {}", mode.current_mode_id));
                        } else {
                            self.state.set_status(format!(
                                "mode {} · available: {}",
                                mode.current_mode_id, available
                            ));
                        }
                    },
                    Err(error) => self.apply_status_error(error),
                }
            },
            Action::ModeSwitched {
                session_id,
                requested_mode_id,
                result,
            } => {
                if !self.active_session_matches(session_id.as_str()) {
                    return Ok(());
                }
                match result {
                    Ok(mode) => {
                        self.state.set_status(format!(
                            "mode {} · next turn will use {}",
                            mode.current_mode_id, requested_mode_id
                        ));
                        self.refresh_modes().await;
                    },
                    Err(error) => self.apply_status_error(error),
                }
            },
        }
        Ok(())
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Ok(());
        }

        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q'))
        {
            self.should_quit = true;
            return Ok(());
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('t')) {
            self.state.toggle_browser();
            return Ok(());
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('o')) {
            return Ok(());
        }

        if self.state.interaction.browser.open {
            match key.code {
                KeyCode::Esc => {
                    self.state.toggle_browser();
                },
                KeyCode::Home => self.state.browser_first(),
                KeyCode::End => self.state.browser_last(),
                KeyCode::Up => self.state.browser_prev(1),
                KeyCode::Down => self.state.browser_next(1),
                KeyCode::PageUp => self.state.browser_prev(5),
                KeyCode::PageDown => self.state.browser_next(5),
                KeyCode::Enter => self.state.toggle_selected_cell_expanded(),
                _ => {},
            }
            return Ok(());
        }

        match key.code {
            KeyCode::Esc if self.state.interaction.has_palette() => {
                self.state.close_palette();
            },
            KeyCode::Esc => {},
            KeyCode::Left => {
                self.state.move_cursor_left();
            },
            KeyCode::Right => {
                self.state.move_cursor_right();
            },
            KeyCode::Home => {
                self.state.move_cursor_home();
            },
            KeyCode::End => {
                self.state.move_cursor_end();
            },
            KeyCode::BackTab => {
                if !matches!(self.state.interaction.palette, PaletteState::Closed) {
                    return Ok(());
                }
                self.state.interaction.set_focus(PaneFocus::Composer);
                self.state.render.mark_dirty();
            },
            KeyCode::Tab => {
                if !matches!(self.state.interaction.palette, PaletteState::Closed) {
                    return Ok(());
                }
                self.state.interaction.set_focus(PaneFocus::Composer);
                self.state.render.mark_dirty();
            },
            KeyCode::Up if !matches!(self.state.interaction.palette, PaletteState::Closed) => {
                self.state.palette_prev();
            },
            KeyCode::Up => {},
            KeyCode::Down if !matches!(self.state.interaction.palette, PaletteState::Closed) => {
                self.state.palette_next();
            },
            KeyCode::Down => {},
            KeyCode::PageUp | KeyCode::PageDown => {},
            KeyCode::Enter => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    if matches!(self.state.interaction.palette, PaletteState::Closed)
                        && matches!(self.state.interaction.pane_focus, PaneFocus::Composer)
                    {
                        self.state.insert_newline();
                    }
                } else if let Some(selection) = self.state.selected_palette() {
                    self.execute_palette_action(palette_action(selection))
                        .await?;
                } else {
                    match self.state.interaction.pane_focus {
                        PaneFocus::Composer => self.submit_current_input().await,
                        PaneFocus::Palette | PaneFocus::Browser => {},
                    }
                }
            },
            KeyCode::Backspace => {
                if matches!(self.state.interaction.palette, PaletteState::Closed) {
                    self.state.pop_input();
                } else {
                    self.state.pop_input();
                    self.refresh_palette_query().await;
                }
            },
            KeyCode::Delete => {
                self.state.delete_input();
                if !matches!(self.state.interaction.palette, PaletteState::Closed) {
                    self.refresh_palette_query().await;
                }
            },
            KeyCode::Char(ch) => {
                if !matches!(self.state.interaction.palette, PaletteState::Closed) {
                    self.state.push_input(ch);
                    self.refresh_palette_query().await;
                } else {
                    self.state.push_input(ch);
                    if ch == '/' {
                        let query = self.slash_query_for_current_input();
                        self.open_slash_palette(query).await;
                    }
                }
            },
            _ => {},
        }

        Ok(())
    }

    async fn handle_paste(&mut self, text: String) -> Result<()> {
        self.state.append_input(text.as_str());
        if !matches!(self.state.interaction.palette, PaletteState::Closed) {
            self.refresh_palette_query().await;
        }
        Ok(())
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {},
            MouseEventKind::Down(_) => {
                let _ = mouse;
                self.state.interaction.set_focus(PaneFocus::Composer);
                self.state.render.mark_dirty();
            },
            _ => {},
        }
    }

    fn active_session_matches(&self, session_id: &str) -> bool {
        self.state.conversation.active_session_id.as_deref() == Some(session_id)
    }

    fn pending_session_matches(&self, session_id: &str) -> bool {
        self.pending_session_id.as_deref() == Some(session_id)
    }
}

struct InputHandle {
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl InputHandle {
    fn spawn(actions_tx: mpsc::UnboundedSender<Action>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let join = thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                if event::poll(Duration::from_millis(100)).unwrap_or(false) {
                    match event::read() {
                        Ok(CrosstermEvent::Key(key)) => {
                            if actions_tx.send(Action::Key(key)).is_err() {
                                break;
                            }
                        },
                        Ok(CrosstermEvent::Paste(text)) => {
                            if actions_tx.send(Action::Paste(text)).is_err() {
                                break;
                            }
                        },
                        Ok(CrosstermEvent::Mouse(mouse)) => {
                            if actions_tx.send(Action::Mouse(mouse)).is_err() {
                                break;
                            }
                        },
                        Ok(CrosstermEvent::Resize(width, height)) => {
                            if actions_tx.send(Action::Resize { width, height }).is_err() {
                                break;
                            }
                        },
                        Ok(_) => {},
                        Err(_) => {
                            let _ = actions_tx.send(Action::Quit);
                            break;
                        },
                    }
                }
            }
        });

        Self {
            stop,
            join: Some(join),
        }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn spawn_tick_loop(actions_tx: mpsc::UnboundedSender<Action>) -> TickHandle {
    TickHandle::spawn(actions_tx)
}

struct TickHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl TickHandle {
    fn spawn(actions_tx: mpsc::UnboundedSender<Action>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let join = tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_millis(250));
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                if actions_tx.send(Action::Tick).is_err() {
                    break;
                }
            }
        });
        Self {
            stop,
            join: Some(join),
        }
    }

    async fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }
}

fn resolve_working_dir(cli_value: Option<PathBuf>) -> Result<PathBuf> {
    match cli_value {
        Some(path) => Ok(path),
        None => env::current_dir().context("resolve current working directory failed"),
    }
}

fn required_working_dir(state: &CliState) -> Result<&Path> {
    state
        .shell
        .working_dir
        .as_deref()
        .context("working directory is required for /new")
}

fn filter_resume_sessions(sessions: &[SessionListItem], query: &str) -> Vec<SessionListItem> {
    let mut items = sessions
        .iter()
        .filter(|session| {
            fuzzy_contains(
                query,
                [
                    session.session_id.clone(),
                    session.title.clone(),
                    session.display_name.clone(),
                    session.working_dir.clone(),
                ],
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    items.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    items
}

fn slash_candidates_with_local_commands(
    candidates: &[astrcode_client::ConversationSlashCandidateDto],
    modes: &[astrcode_client::ModeSummaryDto],
    query: &str,
) -> Vec<astrcode_client::ConversationSlashCandidateDto> {
    let mut merged = candidates.to_vec();
    let model_candidate = astrcode_client::ConversationSlashCandidateDto {
        id: "model".to_string(),
        title: "/model".to_string(),
        description: "选择当前已配置的模型".to_string(),
        keywords: vec!["model".to_string(), "profile".to_string()],
        action_kind: astrcode_client::ConversationSlashActionKindDto::ExecuteCommand,
        action_value: "/model".to_string(),
    };

    if !merged
        .iter()
        .any(|candidate| candidate.id == model_candidate.id)
        && fuzzy_contains(
            query,
            [
                model_candidate.id.clone(),
                model_candidate.title.clone(),
                model_candidate.description.clone(),
            ],
        )
    {
        merged.push(model_candidate);
    }

    let mode_candidate = astrcode_client::ConversationSlashCandidateDto {
        id: "mode".to_string(),
        title: "/mode".to_string(),
        description: "查看或切换当前 session 的治理 mode".to_string(),
        keywords: vec![
            "mode".to_string(),
            "governance".to_string(),
            "plan".to_string(),
            "review".to_string(),
            "code".to_string(),
        ],
        action_kind: astrcode_client::ConversationSlashActionKindDto::ExecuteCommand,
        action_value: "/mode".to_string(),
    };
    if !merged
        .iter()
        .any(|candidate| candidate.id == mode_candidate.id)
        && fuzzy_contains(
            query,
            [
                mode_candidate.id.clone(),
                mode_candidate.title.clone(),
                mode_candidate.description.clone(),
            ],
        )
    {
        merged.push(mode_candidate);
    }

    for mode in modes {
        let candidate = astrcode_client::ConversationSlashCandidateDto {
            id: format!("mode:{}", mode.id),
            title: format!("/mode {}", mode.id),
            description: format!("切换到 {} · {}", mode.name, mode.description),
            keywords: vec![
                "mode".to_string(),
                "governance".to_string(),
                mode.id.clone(),
                mode.name.clone(),
            ],
            action_kind: astrcode_client::ConversationSlashActionKindDto::ExecuteCommand,
            action_value: format!("/mode {}", mode.id),
        };
        if !merged.iter().any(|existing| existing.id == candidate.id)
            && fuzzy_contains(
                query,
                [
                    candidate.id.clone(),
                    candidate.title.clone(),
                    candidate.description.clone(),
                ],
            )
        {
            merged.push(candidate);
        }
    }

    merged
}

fn filter_model_options(options: &[ModelOptionDto], query: &str) -> Vec<ModelOptionDto> {
    let mut items = options
        .iter()
        .filter(|option| {
            fuzzy_contains(
                query,
                [
                    option.model.clone(),
                    option.profile_name.clone(),
                    option.provider_kind.clone(),
                ],
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    items.sort_by(|left, right| left.model.cmp(&right.model));
    items
}

fn slash_query_from_input(input: &str) -> String {
    let trimmed = input.trim();
    let command = trimmed.trim_start_matches('/');
    command
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

fn resume_query_from_input(input: &str) -> String {
    input
        .trim()
        .strip_prefix("/resume")
        .map(str::trim)
        .unwrap_or_default()
        .to_string()
}

fn model_query_from_input(input: &str) -> String {
    input
        .trim()
        .strip_prefix("/model")
        .map(str::trim)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use astrcode_client::{
        ClientTransport, PhaseDto, SseEvent, TransportError, TransportMethod, TransportRequest,
        TransportResponse,
    };
    use async_trait::async_trait;
    use serde_json::json;
    use tokio::{sync::mpsc, time::timeout};

    use super::*;
    use crate::{
        capability::{ColorLevel, GlyphMode, TerminalCapabilities},
        command::Command,
    };

    fn session(
        session_id: &str,
        working_dir: &str,
        title: &str,
        updated_at: &str,
    ) -> SessionListItem {
        SessionListItem {
            session_id: session_id.to_string(),
            working_dir: working_dir.to_string(),
            display_name: title.to_string(),
            title: title.to_string(),
            created_at: updated_at.to_string(),
            updated_at: updated_at.to_string(),
            parent_session_id: None,
            parent_storage_seq: None,
            phase: PhaseDto::Idle,
        }
    }

    fn ascii_capabilities() -> TerminalCapabilities {
        TerminalCapabilities {
            color: ColorLevel::None,
            glyphs: GlyphMode::Ascii,
            alt_screen: false,
            mouse: false,
            bracketed_paste: false,
        }
    }

    #[test]
    fn model_query_from_input_extracts_optional_filter() {
        assert_eq!(super::model_query_from_input("/model"), "");
        assert_eq!(super::model_query_from_input("/model claude"), "claude");
    }

    #[test]
    fn slash_candidates_with_local_commands_includes_model_entry() {
        let items = super::slash_candidates_with_local_commands(&[], &[], "model");
        assert!(items.iter().any(|item| item.id == "model"));
    }

    #[derive(Debug)]
    enum MockCall {
        Request {
            expected: TransportRequest,
            result: Result<TransportResponse, TransportError>,
        },
        Stream {
            expected: TransportRequest,
            events: Vec<Result<SseEvent, TransportError>>,
        },
    }

    #[derive(Debug, Default, Clone)]
    struct MockTransport {
        calls: Arc<Mutex<VecDeque<MockCall>>>,
    }

    impl MockTransport {
        fn push(&self, call: MockCall) {
            self.calls
                .lock()
                .expect("mock lock poisoned")
                .push_back(call);
        }

        fn assert_consumed(&self) {
            assert!(
                self.calls.lock().expect("mock lock poisoned").is_empty(),
                "all mocked transport calls should be consumed"
            );
        }
    }

    #[async_trait]
    impl ClientTransport for MockTransport {
        async fn execute(
            &self,
            request: TransportRequest,
        ) -> Result<TransportResponse, TransportError> {
            let Some(MockCall::Request { expected, result }) =
                self.calls.lock().expect("mock lock poisoned").pop_front()
            else {
                panic!("expected request call");
            };
            assert_eq!(request, expected);
            result
        }

        async fn open_sse(
            &self,
            request: TransportRequest,
            buffer: usize,
        ) -> Result<tokio::sync::mpsc::Receiver<Result<SseEvent, TransportError>>, TransportError>
        {
            let Some(MockCall::Stream { expected, events }) =
                self.calls.lock().expect("mock lock poisoned").pop_front()
            else {
                panic!("expected stream call");
            };
            assert_eq!(request, expected);
            let (sender, receiver) = mpsc::channel(buffer.max(1));
            tokio::spawn(async move {
                for event in events {
                    let _ = sender.send(event).await;
                }
            });
            Ok(receiver)
        }
    }

    fn client_with_transport(transport: MockTransport) -> AstrcodeClient<MockTransport> {
        AstrcodeClient::with_transport(
            ClientConfig {
                origin: "http://localhost:5529".to_string(),
                api_token: Some("session-token".to_string()),
                api_token_expires_at_ms: None,
                stream_buffer: 8,
            },
            transport,
        )
    }

    fn snapshot_response(session_id: &str, title: &str) -> TransportResponse {
        TransportResponse {
            status: 200,
            body: json!({
                "sessionId": session_id,
                "sessionTitle": title,
                "cursor": format!("cursor:{session_id}"),
                "phase": "idle",
                "control": {
                    "phase": "idle",
                    "canSubmitPrompt": true,
                    "canRequestCompact": true,
                    "compactPending": false,
                    "compacting": false,
                    "currentModeId": "default"
                },
                "blocks": [{
                    "kind": "assistant",
                    "id": format!("assistant:{session_id}"),
                    "status": "complete",
                    "markdown": format!("hydrated {session_id}")
                }],
                "childSummaries": [],
                "slashCandidates": [],
                "banner": null
            })
            .to_string(),
        }
    }

    async fn handle_next_action<T>(controller: &mut AppController<T>)
    where
        T: ClientTransport + 'static,
    {
        let action = timeout(Duration::from_millis(200), controller.actions_rx.recv())
            .await
            .expect("pending action should arrive")
            .expect("action channel should stay open");
        controller
            .handle_action(action)
            .await
            .expect("handling queued action should succeed");
    }

    #[test]
    fn resume_filter_matches_title_and_working_dir() {
        let sessions = vec![
            session(
                "s1",
                "D:/repo-a",
                "terminal-read-model",
                "2026-04-15T10:00:00Z",
            ),
            session("s2", "D:/other", "other", "2026-04-15T12:00:00Z"),
        ];

        assert_eq!(filter_resume_sessions(&sessions, "terminal").len(), 1);
        assert_eq!(filter_resume_sessions(&sessions, "repo-a").len(), 1);
    }

    #[tokio::test]
    async fn bootstrap_creates_fresh_session_instead_of_restoring_existing_one() {
        let transport = MockTransport::default();
        let existing = session("session-old", "D:/repo-a", "old", "2026-04-15T10:00:00Z");
        let created = session("session-new", "D:/repo-a", "new", "2026-04-15T12:30:00Z");

        transport.push(MockCall::Request {
            expected: TransportRequest {
                method: TransportMethod::Post,
                url: "http://localhost:5529/api/sessions".to_string(),
                auth_token: Some("session-token".to_string()),
                query: Vec::new(),
                json_body: Some(json!({
                    "workingDir": "D:/repo-a"
                })),
            },
            result: Ok(TransportResponse {
                status: 201,
                body: serde_json::to_string(&created).expect("session should serialize"),
            }),
        });
        transport.push(MockCall::Request {
            expected: TransportRequest {
                method: TransportMethod::Get,
                url: "http://localhost:5529/api/v1/conversation/sessions/session-new/snapshot"
                    .to_string(),
                auth_token: Some("session-token".to_string()),
                query: Vec::new(),
                json_body: None,
            },
            result: Ok(snapshot_response("session-new", "new")),
        });
        transport.push(MockCall::Stream {
            expected: TransportRequest {
                method: TransportMethod::Get,
                url: "http://localhost:5529/api/v1/conversation/sessions/session-new/stream"
                    .to_string(),
                auth_token: Some("session-token".to_string()),
                query: vec![("cursor".to_string(), "cursor:session-new".to_string())],
                json_body: None,
            },
            events: Vec::new(),
        });
        transport.push(MockCall::Request {
            expected: TransportRequest {
                method: TransportMethod::Get,
                url: "http://localhost:5529/api/sessions".to_string(),
                auth_token: Some("session-token".to_string()),
                query: Vec::new(),
                json_body: None,
            },
            result: Ok(TransportResponse {
                status: 200,
                body: serde_json::to_string(&vec![created.clone(), existing])
                    .expect("sessions should serialize"),
            }),
        });

        let (actions_tx, actions_rx) = mpsc::unbounded_channel();
        let mut controller = AppController::new(
            client_with_transport(transport.clone()),
            CliState::new(
                "http://localhost:5529".to_string(),
                Some(PathBuf::from("D:/repo-a")),
                ascii_capabilities(),
            ),
            None,
            AppControllerChannels::new(actions_tx, actions_rx),
        );
        controller.state.update_sessions(vec![session(
            "session-old",
            "D:/repo-a",
            "old",
            "2026-04-15T10:00:00Z",
        )]);

        controller
            .bootstrap()
            .await
            .expect("bootstrap should succeed");
        handle_next_action(&mut controller).await;
        handle_next_action(&mut controller).await;
        handle_next_action(&mut controller).await;

        assert_eq!(
            controller.state.conversation.active_session_id.as_deref(),
            Some("session-new")
        );
        assert!(
            controller
                .state
                .conversation
                .sessions
                .iter()
                .any(|session| session.session_id == "session-new"),
            "bootstrap should attach the freshly created session"
        );
        transport.assert_consumed();
    }

    #[tokio::test]
    async fn bootstrap_create_failure_surfaces_error_without_attaching_old_session() {
        let transport = MockTransport::default();
        let existing = session("session-old", "D:/repo-a", "old", "2026-04-15T10:00:00Z");

        transport.push(MockCall::Request {
            expected: TransportRequest {
                method: TransportMethod::Post,
                url: "http://localhost:5529/api/sessions".to_string(),
                auth_token: Some("session-token".to_string()),
                query: Vec::new(),
                json_body: Some(json!({
                    "workingDir": "D:/repo-a"
                })),
            },
            result: Err(TransportError::Http {
                status: 500,
                body: json!({
                    "code": "transport_unavailable",
                    "message": "create failed"
                })
                .to_string(),
            }),
        });
        transport.push(MockCall::Request {
            expected: TransportRequest {
                method: TransportMethod::Get,
                url: "http://localhost:5529/api/sessions".to_string(),
                auth_token: Some("session-token".to_string()),
                query: Vec::new(),
                json_body: None,
            },
            result: Ok(TransportResponse {
                status: 200,
                body: serde_json::to_string(&vec![existing.clone()])
                    .expect("sessions should serialize"),
            }),
        });

        let (actions_tx, actions_rx) = mpsc::unbounded_channel();
        let mut controller = AppController::new(
            client_with_transport(transport.clone()),
            CliState::new(
                "http://localhost:5529".to_string(),
                Some(PathBuf::from("D:/repo-a")),
                ascii_capabilities(),
            ),
            None,
            AppControllerChannels::new(actions_tx, actions_rx),
        );
        controller.state.update_sessions(vec![existing]);

        controller
            .bootstrap()
            .await
            .expect("bootstrap should succeed");
        handle_next_action(&mut controller).await;
        handle_next_action(&mut controller).await;

        assert_eq!(controller.state.conversation.active_session_id, None);
        assert!(controller.state.interaction.status.is_error);
        assert_eq!(controller.state.interaction.status.message, "create failed");
        transport.assert_consumed();
    }

    #[tokio::test]
    async fn submitting_prompt_restores_transcript_tail_follow_mode() {
        let transport = MockTransport::default();
        transport.push(MockCall::Request {
            expected: TransportRequest {
                method: TransportMethod::Post,
                url: "http://localhost:5529/api/sessions/session-1/prompts".to_string(),
                auth_token: Some("session-token".to_string()),
                query: Vec::new(),
                json_body: Some(json!({
                    "text": "hello"
                })),
            },
            result: Ok(TransportResponse {
                status: 202,
                body: json!({
                    "status": "accepted",
                    "sessionId": "session-1",
                    "turnId": "turn-1"
                })
                .to_string(),
            }),
        });

        let (actions_tx, actions_rx) = mpsc::unbounded_channel();
        let mut controller = AppController::new(
            client_with_transport(transport.clone()),
            CliState::new(
                "http://localhost:5529".to_string(),
                Some(PathBuf::from("D:/repo-a")),
                ascii_capabilities(),
            ),
            None,
            AppControllerChannels::new(actions_tx, actions_rx),
        );
        controller.state.conversation.active_session_id = Some("session-1".to_string());
        controller.state.replace_input("hello");

        controller.submit_current_input().await;

        handle_next_action(&mut controller).await;
        assert_eq!(controller.state.interaction.status.message, "ready");
        transport.assert_consumed();
    }

    #[tokio::test]
    async fn submitting_skill_slash_sends_structured_skill_invocation() {
        let transport = MockTransport::default();
        transport.push(MockCall::Request {
            expected: TransportRequest {
                method: TransportMethod::Post,
                url: "http://localhost:5529/api/sessions/session-1/prompts".to_string(),
                auth_token: Some("session-token".to_string()),
                query: Vec::new(),
                json_body: Some(json!({
                    "text": "修复失败测试",
                    "skillInvocation": {
                        "skillId": "review",
                        "userPrompt": "修复失败测试"
                    }
                })),
            },
            result: Ok(TransportResponse {
                status: 202,
                body: json!({
                    "status": "accepted",
                    "sessionId": "session-1",
                    "turnId": "turn-2"
                })
                .to_string(),
            }),
        });

        let (actions_tx, actions_rx) = mpsc::unbounded_channel();
        let mut controller = AppController::new(
            client_with_transport(transport.clone()),
            CliState::new(
                "http://localhost:5529".to_string(),
                Some(PathBuf::from("D:/repo-a")),
                ascii_capabilities(),
            ),
            None,
            AppControllerChannels::new(actions_tx, actions_rx),
        );
        controller.state.conversation.active_session_id = Some("session-1".to_string());
        controller.state.conversation.slash_candidates =
            vec![astrcode_client::ConversationSlashCandidateDto {
                id: "review".to_string(),
                title: "Review".to_string(),
                description: "review skill".to_string(),
                keywords: vec!["review".to_string()],
                action_kind: astrcode_client::ConversationSlashActionKindDto::InsertText,
                action_value: "/review".to_string(),
            }];
        controller
            .state
            .replace_input("/review 修复失败测试".to_string());

        controller.submit_current_input().await;

        handle_next_action(&mut controller).await;
        assert_eq!(controller.state.interaction.status.message, "ready");
        transport.assert_consumed();
    }

    #[tokio::test]
    async fn end_to_end_acceptance_covers_resume_compact_skill_and_single_active_stream_switch() {
        let transport = MockTransport::default();
        let session_one = session("session-1", "D:/repo-a", "repo-a", "2026-04-15T10:00:00Z");
        let session_two = session("session-2", "D:/repo-b", "repo-b", "2026-04-15T12:00:00Z");

        transport.push(MockCall::Request {
            expected: TransportRequest {
                method: TransportMethod::Get,
                url: "http://localhost:5529/api/v1/conversation/sessions/session-1/snapshot"
                    .to_string(),
                auth_token: Some("session-token".to_string()),
                query: Vec::new(),
                json_body: None,
            },
            result: Ok(snapshot_response("session-1", "repo-a")),
        });
        transport.push(MockCall::Stream {
            expected: TransportRequest {
                method: TransportMethod::Get,
                url: "http://localhost:5529/api/v1/conversation/sessions/session-1/stream"
                    .to_string(),
                auth_token: Some("session-token".to_string()),
                query: vec![("cursor".to_string(), "cursor:session-1".to_string())],
                json_body: None,
            },
            events: Vec::new(),
        });
        transport.push(MockCall::Request {
            expected: TransportRequest {
                method: TransportMethod::Get,
                url:
                    "http://localhost:5529/api/v1/conversation/sessions/session-1/slash-candidates"
                        .to_string(),
                auth_token: Some("session-token".to_string()),
                query: vec![("q".to_string(), "review".to_string())],
                json_body: None,
            },
            result: Ok(TransportResponse {
                status: 200,
                body: json!({
                    "items": [{
                        "id": "review",
                        "title": "Review skill",
                        "description": "插入 review skill",
                        "keywords": ["review"],
                        "actionKind": "insert_text",
                        "actionValue": "/review"
                    }]
                })
                .to_string(),
            }),
        });
        transport.push(MockCall::Request {
            expected: TransportRequest {
                method: TransportMethod::Post,
                url: "http://localhost:5529/api/sessions/session-1/compact".to_string(),
                auth_token: Some("session-token".to_string()),
                query: Vec::new(),
                json_body: Some(json!({
                    "control": {
                        "manualCompact": true
                    }
                })),
            },
            result: Ok(TransportResponse {
                status: 202,
                body: json!({
                    "accepted": true,
                    "deferred": false,
                    "message": "手动 compact 已执行。"
                })
                .to_string(),
            }),
        });
        transport.push(MockCall::Request {
            expected: TransportRequest {
                method: TransportMethod::Get,
                url: "http://localhost:5529/api/sessions".to_string(),
                auth_token: Some("session-token".to_string()),
                query: Vec::new(),
                json_body: None,
            },
            result: Ok(TransportResponse {
                status: 200,
                body: serde_json::to_string(&vec![session_one.clone(), session_two.clone()])
                    .expect("sessions should serialize"),
            }),
        });
        transport.push(MockCall::Request {
            expected: TransportRequest {
                method: TransportMethod::Get,
                url: "http://localhost:5529/api/v1/conversation/sessions/session-2/snapshot"
                    .to_string(),
                auth_token: Some("session-token".to_string()),
                query: Vec::new(),
                json_body: None,
            },
            result: Ok(snapshot_response("session-2", "repo-b")),
        });
        transport.push(MockCall::Stream {
            expected: TransportRequest {
                method: TransportMethod::Get,
                url: "http://localhost:5529/api/v1/conversation/sessions/session-2/stream"
                    .to_string(),
                auth_token: Some("session-token".to_string()),
                query: vec![("cursor".to_string(), "cursor:session-2".to_string())],
                json_body: None,
            },
            events: Vec::new(),
        });

        let (actions_tx, actions_rx) = mpsc::unbounded_channel();
        let mut controller = AppController::new(
            client_with_transport(transport.clone()),
            CliState::new(
                "http://localhost:5529".to_string(),
                Some(PathBuf::from("D:/repo-a")),
                ascii_capabilities(),
            ),
            None,
            AppControllerChannels::new(actions_tx, actions_rx),
        );
        controller
            .state
            .update_sessions(vec![session_one.clone(), session_two.clone()]);

        controller
            .begin_session_hydration("session-1".to_string())
            .await;
        handle_next_action(&mut controller).await;
        assert_eq!(
            controller.state.conversation.active_session_id.as_deref(),
            Some("session-1")
        );
        assert_eq!(
            controller.state.conversation.transcript.len(),
            1,
            "session one should hydrate one transcript block"
        );

        controller.state.replace_input("/review".to_string());
        controller.open_slash_palette("review".to_string()).await;
        handle_next_action(&mut controller).await;
        let PaletteState::Slash(palette) = &controller.state.interaction.palette else {
            panic!("skill command should open slash palette");
        };
        assert_eq!(palette.query, "review");
        assert_eq!(palette.items.len(), 1);

        controller.execute_command(Command::Compact).await;
        handle_next_action(&mut controller).await;
        assert_eq!(
            controller.state.interaction.status.message,
            "手动 compact 已执行。"
        );

        controller
            .execute_command(Command::Resume {
                query: Some("repo-b".to_string()),
            })
            .await;
        let PaletteState::Resume(resume) = &controller.state.interaction.palette else {
            panic!("resume command should open resume palette");
        };
        assert_eq!(resume.query, "repo-b");
        handle_next_action(&mut controller).await;
        let selection = controller
            .state
            .selected_palette()
            .expect("resume palette should keep a selection");
        controller
            .execute_palette_action(palette_action(selection))
            .await
            .expect("resume selection should switch session");
        handle_next_action(&mut controller).await;
        assert_eq!(
            controller.state.conversation.active_session_id.as_deref(),
            Some("session-2")
        );
        assert!(
            controller
                .state
                .conversation
                .transcript
                .iter()
                .any(|block| matches!(
                    block,
                    astrcode_client::ConversationBlockDto::Assistant(block)
                        if block.id == "assistant:session-2"
                )),
            "session two snapshot should replace transcript"
        );

        let transcript_before = controller.state.conversation.transcript.clone();
        controller
            .handle_action(Action::StreamBatch {
                session_id: "session-1".to_string(),
                items: vec![ConversationStreamItem::Delta(Box::new(
                    astrcode_client::ConversationStreamEnvelopeDto {
                        session_id: "session-1".to_string(),
                        cursor: astrcode_client::ConversationCursorDto("cursor:old".to_string()),
                        step_progress: Default::default(),
                        delta: astrcode_client::ConversationDeltaDto::AppendBlock {
                            block: astrcode_client::ConversationBlockDto::Assistant(
                                astrcode_client::ConversationAssistantBlockDto {
                                    id: "assistant:stale".to_string(),
                                    turn_id: None,
                                    status: astrcode_client::ConversationBlockStatusDto::Complete,
                                    markdown: "stale".to_string(),
                                    step_index: None,
                                },
                            ),
                        },
                    },
                ))],
            })
            .await
            .expect("stale batch should be ignored");
        assert_eq!(
            controller.state.conversation.transcript, transcript_before,
            "single active stream mode should ignore deltas from inactive sessions"
        );

        transport.assert_consumed();
    }
}
