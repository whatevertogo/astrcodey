//! 子会话派生器 — 当扩展返回 `RunSession` 时的处理逻辑。
//!
//! [`ServerSessionSpawner`] 创建子会话、用子 Agent 执行一轮对话，
//! 将事件持久化到子会话存储，并经由 [`ProgressTx`] 将关键进展
//! 转译为 [`ToolOutputDelta`] 实时反馈给父会话的 TUI。
//!
//! 最大嵌套深度由 [`MAX_AGENT_DEPTH`] 控制；同层级可并发生成任意多个子 agent。

/// 子 agent 最大嵌套深度（root=0, child=1, grandchild=2）。
// TODO: 可配置
const MAX_AGENT_DEPTH: usize = 2;

use std::sync::Arc;

use astrcode_context::context_assembler::LlmContextAssembler;
use astrcode_core::{
    event::{Event, EventPayload, ToolOutputStream},
    types::{SessionId, ToolCallId, TurnId, new_background_task_id, new_message_id, new_turn_id},
};
use astrcode_extensions::{
    runner::ExtensionRunner,
    runtime::{SpawnRequest, SpawnResult},
};
use astrcode_session::{
    EventBus, Session, SessionServices, TurnError, TurnOutput, TurnRunner,
    agent_turn_completed_payloads, agent_turn_failed_payloads, agent_turn_started_payloads,
    background::{BackgroundTaskCompletion, BackgroundTaskManager, complete_background_task},
    run_turn,
};
use parking_lot::Mutex as StdMutex;
use tokio::sync::{Mutex, mpsc};

/// 服务器端的会话派生器，实现 `SessionSpawner` trait。
///
/// 当扩展返回 `ExtensionToolOutcome::RunSession` 时，
/// 扩展运行器通过此派生器创建子会话并运行 Agent 回合。
pub(crate) struct ServerSessionSpawner {
    pub(crate) config: Arc<crate::config_manager::ConfigManager>,
    pub(crate) context_assembler: Arc<LlmContextAssembler>,
    pub(crate) background_tasks: Arc<StdMutex<BackgroundTaskManager>>,
    pub(crate) session_manager: Arc<crate::session_manager::SessionManager>,
    pub(crate) extension_runner: Arc<ExtensionRunner>,
    pub(crate) agent_session_control: crate::bootstrap::AgentSessionControlSlot,
}

// ─── spawn() 入口与准备阶段 ────────────────────────────────────────────

/// `prepare_child_session()` 的产出，传给 `spawn_sync` / `spawn_async`。
struct PreparedChild {
    parent_session: Arc<Session>,
    child_session: Arc<Session>,
    child_turn_id: TurnId,
    child_name: String,
    user_prompt: String,
    tool_call_id: Option<String>,
    model_id: String,
    agent: TurnRunner,
    progress: ProgressTx,
    current_child_sid: Arc<Mutex<SessionId>>,
}

#[async_trait::async_trait]
impl astrcode_extensions::runtime::SessionSpawner for ServerSessionSpawner {
    async fn spawn(
        &self,
        parent_session_id: &str,
        request: SpawnRequest,
    ) -> Result<SpawnResult, String> {
        let wait_for_result = request.wait_for_result;
        let prepared = self
            .prepare_child_session(parent_session_id, request)
            .await?;
        if wait_for_result {
            self.spawn_sync(prepared).await
        } else {
            self.spawn_async(prepared).await
        }
    }
}

impl ServerSessionSpawner {
    /// 沿 parent 链向上遍历，计算当前 session 的嵌套深度。
    ///
    /// root session depth=0，每向上一级 parent +1。
    async fn session_depth(&self, session_id: &SessionId) -> Result<usize, String> {
        let mut depth = 0;
        let mut current = session_id.clone();
        loop {
            let model = self
                .session_manager
                .read_model(&current)
                .await
                .map_err(|e| format!("read session: {e}"))?;
            match model.parent_session_id {
                Some(parent) => {
                    depth += 1;
                    current = parent;
                },
                None => break,
            }
        }
        Ok(depth)
    }

    /// 共享准备阶段：创建子会话、构建 prompt、初始化 TurnRunner。
    async fn prepare_child_session(
        &self,
        parent_session_id: &str,
        request: SpawnRequest,
    ) -> Result<PreparedChild, String> {
        let parent_session_id = SessionId::from(parent_session_id);
        let tool_call_id = request.tool_call_id.clone();
        let progress = ProgressTx::new(request.tool_call_id, request.event_tx);
        let child_name = request.name.clone();
        let user_prompt = request.user_prompt.clone();
        let model_id = match request.model_preference.clone() {
            Some(model) => model,
            None => {
                let parent = self
                    .session_manager
                    .open(parent_session_id.clone())
                    .await
                    .map_err(|e| format!("open parent session: {e}"))?;
                parent
                    .read_model()
                    .await
                    .map_err(|e| format!("parent session {parent_session_id} not found: {e}"))?
                    .model_id
            },
        };

        let depth = self.session_depth(&parent_session_id).await?;
        if depth >= MAX_AGENT_DEPTH {
            return Err(format!(
                "已达最大 agent 嵌套深度 ({MAX_AGENT_DEPTH})，无法继续创建子 agent"
            ));
        }

        let child_session = self
            .session_manager
            .create_child(&request.working_dir, &model_id, &parent_session_id)
            .await
            .map_err(|e| format!("create child session: {e}"))?;

        let parent_session = Arc::new(
            self.session_manager
                .open(parent_session_id.clone())
                .await
                .map_err(|e| format!("open parent: {e}"))?,
        );

        parent_session
            .append_event(Event::new(
                parent_session_id.clone(),
                None,
                EventPayload::AgentSessionSpawned {
                    child_session_id: child_session.id().clone(),
                    agent_name: child_name.clone(),
                    task: user_prompt.clone(),
                },
            ))
            .await
            .map_err(|e| format!("append parent spawn event: {e}"))?;

        let child_sid = child_session.id().clone();
        let child_arc = Arc::new(child_session);

        let child_turn_id = new_turn_id();

        let tool_registry = self
            .session_manager
            .refresh_tool_registry(&child_sid, &request.working_dir)
            .await;
        let (system_prompt, fingerprint) = self
            .session_manager
            .build_system_prompt_snapshot(
                &child_sid,
                &request.working_dir,
                &model_id,
                &tool_registry,
                Some(&request.system_prompt),
            )
            .await
            .map_err(|e| format!("build child system prompt: {e}"))?;

        append_child_payload(
            child_arc.as_ref(),
            None,
            EventPayload::SystemPromptConfigured {
                text: system_prompt.clone(),
                fingerprint,
            },
        )
        .await?;

        append_child_progress_payloads(
            child_arc.as_ref(),
            &progress,
            Some(&child_turn_id),
            agent_turn_started_payloads(new_message_id(), user_prompt.clone()),
        )
        .await?;
        progress.emit(
            ToolOutputStream::Stdout,
            format!("child agent '{child_name}' started: {child_sid} using {model_id}\n"),
        );

        let current_child_sid = Arc::new(Mutex::new(child_sid.clone()));
        let child_bg_final_sid = Arc::clone(&current_child_sid);

        let (child_bg_result_tx, mut child_bg_result_rx) =
            mpsc::unbounded_channel::<BackgroundTaskCompletion>();

        let child_bg_session = Arc::clone(&child_arc);
        let child_bg_progress = progress.clone();
        let child_bg_turn_id = child_turn_id.clone();
        let handle = tokio::spawn(async move {
            while let Some(completion) = child_bg_result_rx.recv().await {
                let _sid = child_bg_final_sid.lock().await.clone();
                let (tool_call_event, bg_event) = completion.into_events();
                let _ = append_child_payload(
                    child_bg_session.as_ref(),
                    Some(&child_bg_turn_id),
                    tool_call_event,
                )
                .await;
                let _ = append_child_payload(
                    child_bg_session.as_ref(),
                    Some(&child_bg_turn_id),
                    bg_event.clone(),
                )
                .await;
                child_bg_progress.forward(&bg_event);
            }
        });
        tokio::spawn(async move {
            if let Err(e) = handle.await {
                tracing::error!("child background result forwarder panicked: {e}");
            }
        });

        let agent_session_control = self.agent_session_control.read().clone();
        let agent = TurnRunner::new(
            SessionServices::new(
                self.config.read_llm_provider(),
                Arc::clone(&tool_registry),
                Arc::clone(&self.extension_runner),
                Arc::clone(&self.context_assembler),
                Arc::clone(&child_arc),
                Arc::clone(&self.background_tasks),
                self.session_manager.file_observation_store(&child_sid),
            )
            .with_background_result_tx(child_bg_result_tx)
            .with_agent_session_control(agent_session_control),
        )
        .await
        .map_err(|e| format!("create child turn runner: {e}"))?;

        Ok(PreparedChild {
            parent_session,
            child_session: child_arc,
            child_turn_id,
            child_name,
            user_prompt,
            tool_call_id,
            model_id,
            agent,
            progress,
            current_child_sid,
        })
    }

    // ─── 同步路径 ────────────────────────────────────────────────────

    /// 阻塞等待子 Agent 完成并返回结果。
    async fn spawn_sync(&self, p: PreparedChild) -> Result<SpawnResult, String> {
        let PreparedChild {
            parent_session,
            child_session,
            child_turn_id,
            user_prompt,
            agent,
            progress,
            current_child_sid,
            ..
        } = p;

        let (output, emitted_error, final_child_sid) = drive_child_agent_turn(
            &agent,
            &user_prompt,
            Arc::clone(&child_session),
            current_child_sid,
            child_turn_id.clone(),
            progress.clone(),
        )
        .await;

        match output {
            Ok(output) => {
                append_child_progress_payloads(
                    child_session.as_ref(),
                    &progress,
                    Some(&child_turn_id),
                    agent_turn_completed_payloads(output.finish_reason.clone()),
                )
                .await?;
                parent_session
                    .append_event(Event::new(
                        parent_session.id().clone(),
                        None,
                        EventPayload::AgentSessionCompleted {
                            child_session_id: child_session.id().clone(),
                            final_session_id: final_child_sid.clone(),
                            summary: one_line_summary(&output.text),
                        },
                    ))
                    .await
                    .map_err(|e| format!("append parent completion event: {e}"))?;
                Ok(SpawnResult {
                    content: output.text,
                    child_session_id: final_child_sid.into_string(),
                    background_task_id: None,
                })
            },
            Err(e) => {
                append_child_progress_payloads(
                    child_session.as_ref(),
                    &progress,
                    Some(&child_turn_id),
                    agent_turn_failed_payloads(
                        (!emitted_error).then(|| e.to_string()),
                        "error".into(),
                    ),
                )
                .await?;
                progress.emit(
                    ToolOutputStream::Stderr,
                    format!("child agent error: {e}\n"),
                );
                parent_session
                    .append_event(Event::new(
                        parent_session.id().clone(),
                        None,
                        EventPayload::AgentSessionFailed {
                            child_session_id: child_session.id().clone(),
                            final_session_id: final_child_sid.clone(),
                            error: e.to_string(),
                        },
                    ))
                    .await
                    .map_err(|e| format!("append parent failure event: {e}"))?;
                Ok(SpawnResult {
                    content: format!("child agent error: {e}"),
                    child_session_id: final_child_sid.into_string(),
                    background_task_id: None,
                })
            },
        }
    }

    // ─── 异步路径 ────────────────────────────────────────────────────

    /// 启动子 Agent 后立即返回占位结果，Agent 在后台完成。
    async fn spawn_async(&self, p: PreparedChild) -> Result<SpawnResult, String> {
        let PreparedChild {
            parent_session,
            child_session,
            child_turn_id,
            child_name,
            user_prompt,
            tool_call_id,
            model_id,
            agent,
            progress,
            current_child_sid,
        } = p;

        let task_id = new_background_task_id();
        let task_id_str = task_id.to_string();

        let parent_sid = parent_session.id().clone();
        let register_sid = parent_sid.clone();
        let parent_arc = parent_session;
        let watcher_bg_tasks = Arc::clone(&self.background_tasks);
        let watcher_task_id = task_id.clone();
        let watcher_progress = progress.clone();
        let tool_call_id_for_result = tool_call_id.clone();
        let cti_for_completion = child_turn_id.clone();
        let drive_session = Arc::clone(&child_session);
        let watcher_child_session = Arc::clone(&child_session);
        let drive_current_child_sid = Arc::clone(&current_child_sid);
        let drive_child_turn_id = child_turn_id.clone();
        let drive_progress = progress.clone();
        let async_child_sid = child_session.id().clone();
        let return_child_sid = child_session.id().clone();

        let agent_handle = tokio::spawn(async move {
            let (output, emitted_error, final_child_sid) = drive_child_agent_turn(
                &agent,
                &user_prompt,
                drive_session,
                drive_current_child_sid,
                drive_child_turn_id,
                drive_progress,
            )
            .await;

            let result_content = match &output {
                Ok(o) => {
                    let _ = append_child_progress_payloads(
                        watcher_child_session.as_ref(),
                        &watcher_progress,
                        Some(&cti_for_completion),
                        agent_turn_completed_payloads(o.finish_reason.clone()),
                    )
                    .await;
                    let _ = parent_arc
                        .append_event(Event::new(
                            parent_sid.clone(),
                            None,
                            EventPayload::AgentSessionCompleted {
                                child_session_id: async_child_sid.clone(),
                                final_session_id: final_child_sid.clone(),
                                summary: one_line_summary(&o.text),
                            },
                        ))
                        .await;
                    o.text.clone()
                },
                Err(e) => {
                    let _ = append_child_progress_payloads(
                        watcher_child_session.as_ref(),
                        &watcher_progress,
                        Some(&cti_for_completion),
                        agent_turn_failed_payloads(
                            (!emitted_error).then(|| e.to_string()),
                            "error".into(),
                        ),
                    )
                    .await;
                    watcher_progress.emit(
                        ToolOutputStream::Stderr,
                        format!("child agent error: {e}\n"),
                    );
                    let _ = parent_arc
                        .append_event(Event::new(
                            parent_sid.clone(),
                            None,
                            EventPayload::AgentSessionFailed {
                                child_session_id: async_child_sid.clone(),
                                final_session_id: final_child_sid.clone(),
                                error: e.to_string(),
                            },
                        ))
                        .await;
                    format!("child agent error: {e}")
                },
            };

            if let Some(call_id) = &tool_call_id_for_result {
                let mut meta = std::collections::BTreeMap::new();
                meta.insert(
                    "task_id".into(),
                    serde_json::json!(watcher_task_id.to_string()),
                );

                let result = astrcode_core::tool::ToolResult {
                    call_id: call_id.clone(),
                    content: result_content,
                    is_error: output.is_err(),
                    error: output.as_ref().err().map(|e| e.to_string()),
                    metadata: meta,
                    duration_ms: None,
                };
                let _ = parent_arc
                    .append_event(Event::new(
                        parent_sid.clone(),
                        None,
                        EventPayload::ToolCallCompleted {
                            call_id: ToolCallId::from(call_id.as_str()),
                            tool_name: "agent".into(),
                            result,
                        },
                    ))
                    .await;
            }

            complete_background_task(&watcher_bg_tasks, &watcher_task_id);
        });

        // 异步 agent 只有一个 handle；注册一个空 watcher 以复用现有取消接口。
        // TODO: 前端 UI 入口设计中——用户如何取消异步 agent（ESC / 任务面板）
        let dummy_watcher = tokio::spawn(async {});
        self.background_tasks.lock().register(
            task_id.clone(),
            register_sid,
            agent_handle,
            dummy_watcher,
        );

        progress.emit(
            ToolOutputStream::Stdout,
            format!(
                "async child agent '{child_name}' launched: {return_child_sid} using {model_id}\n"
            ),
        );

        Ok(SpawnResult {
            content: format!(
                "异步 agent 已启动。完成后结果将在下一轮对话中可用。\nagent: {child_name} | \
                 session: {return_child_sid}"
            ),
            child_session_id: return_child_sid.into_string(),
            background_task_id: Some(task_id_str),
        })
    }
}

// ─── 子 Agent 驱动 ────────────────────────────────────────────────────

/// 子会话 EventBus：持久化事件到子会话并转发进度给父会话。
struct ChildEventBus {
    child_session: Arc<Session>,
    child_turn_id: TurnId,
    progress: ProgressTx,
}

#[async_trait::async_trait]
impl EventBus for ChildEventBus {
    async fn emit(
        &self,
        _session_id: &SessionId,
        _turn_id: Option<&TurnId>,
        payload: EventPayload,
    ) {
        let _ = append_child_progress_payload(
            &self.child_session,
            &self.progress,
            Some(&self.child_turn_id),
            payload,
        )
        .await;
    }
}

async fn drive_child_agent_turn(
    agent: &TurnRunner,
    user_prompt: &str,
    child_session: Arc<Session>,
    current_child_sid: Arc<Mutex<SessionId>>,
    child_turn_id: TurnId,
    progress: ProgressTx,
) -> (Result<TurnOutput, TurnError>, bool, SessionId) {
    let initial_sid = current_child_sid.lock().await.clone();
    let bus = ChildEventBus {
        child_session,
        child_turn_id: child_turn_id.clone(),
        progress,
    };
    let result = run_turn(agent, user_prompt, None, &child_turn_id, &bus).await;
    (result.output, result.emitted_error, initial_sid)
}

// ─── 进度转发 ─────────────────────────────────────────────────────────

/// 将子 agent 事件转发为父级工具调用的 [`ToolOutputDelta`] 进度事件。
#[derive(Clone)]
struct ProgressTx {
    call_id: Option<String>,
    tx: Option<mpsc::UnboundedSender<EventPayload>>,
}

impl ProgressTx {
    fn new(call_id: Option<String>, tx: Option<mpsc::UnboundedSender<EventPayload>>) -> Self {
        Self { call_id, tx }
    }

    fn emit(&self, stream: ToolOutputStream, delta: impl Into<String>) {
        let Some(call_id) = &self.call_id else { return };
        let Some(tx) = &self.tx else { return };
        let delta = delta.into();
        if delta.is_empty() {
            return;
        }
        let _ = tx.send(EventPayload::ToolOutputDelta {
            call_id: call_id.clone().into(),
            stream,
            delta,
        });
    }

    fn forward(&self, payload: &EventPayload) {
        if let Some((stream, delta)) = child_progress_delta(payload) {
            self.emit(stream, delta);
        }
    }
}

// ─── 子会话事件持久化 ──────────────────────────────────────────────────

async fn append_child_payload(
    session: &Session,
    child_turn_id: Option<&TurnId>,
    payload: EventPayload,
) -> Result<(), String> {
    if payload.is_durable() {
        session
            .append_event(Event::new(
                session.id().clone(),
                child_turn_id.cloned(),
                payload,
            ))
            .await
            .map_err(|e| format!("append child event: {e}"))?;
    }
    Ok(())
}

async fn append_child_progress_payload(
    session: &Session,
    progress: &ProgressTx,
    child_turn_id: Option<&TurnId>,
    payload: EventPayload,
) -> Result<(), String> {
    progress.forward(&payload);
    if payload.is_durable() {
        append_child_payload(session, child_turn_id, payload).await?;
    }
    Ok(())
}

async fn append_child_progress_payloads<I>(
    session: &Session,
    progress: &ProgressTx,
    child_turn_id: Option<&TurnId>,
    payloads: I,
) -> Result<(), String>
where
    I: IntoIterator<Item = EventPayload>,
{
    for payload in payloads {
        append_child_progress_payload(session, progress, child_turn_id, payload).await?;
    }
    Ok(())
}

fn child_progress_delta(payload: &EventPayload) -> Option<(ToolOutputStream, String)> {
    match payload {
        EventPayload::AssistantMessageStarted { .. } => {
            Some((ToolOutputStream::Stdout, "child assistant started\n".into()))
        },
        EventPayload::AssistantTextDelta { delta, .. } => {
            if delta.is_empty() {
                None
            } else {
                Some((ToolOutputStream::Stdout, delta.clone()))
            }
        },
        EventPayload::AssistantMessageCompleted { text, .. } => {
            let summary = one_line_summary(text);
            if summary.is_empty() {
                None
            } else {
                Some((
                    ToolOutputStream::Stdout,
                    format!("child assistant completed: {summary}\n"),
                ))
            }
        },
        EventPayload::ToolCallStarted { tool_name, .. } => Some((
            ToolOutputStream::Stdout,
            format!("child tool started: {tool_name}\n"),
        )),
        EventPayload::ToolOutputDelta { stream, delta, .. } => {
            Some((*stream, format!("child tool output: {delta}")))
        },
        EventPayload::ToolCallCompleted {
            tool_name, result, ..
        } => {
            let stream = if result.is_error {
                ToolOutputStream::Stderr
            } else {
                ToolOutputStream::Stdout
            };
            let detail = one_line_summary(result.error.as_deref().unwrap_or(&result.content));
            let suffix = if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            };
            Some((
                stream,
                format!("child tool completed: {tool_name}{suffix}\n"),
            ))
        },
        EventPayload::ErrorOccurred { message, .. } => Some((
            ToolOutputStream::Stderr,
            format!("child error: {message}\n"),
        )),
        EventPayload::TurnCompleted { finish_reason } => Some((
            ToolOutputStream::Stdout,
            format!("child turn completed: {finish_reason}\n"),
        )),
        _ => None,
    }
}

fn one_line_summary(text: &str) -> String {
    crate::http::compact_inline(text, 159)
}

// ─── 测试 ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use astrcode_context::context_assembler::LlmContextAssembler;
    use astrcode_core::{
        llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
        storage::EventStore,
        tool::ToolDefinition,
    };
    use astrcode_extensions::{
        runner::ExtensionRunner,
        runtime::{SessionSpawner, SpawnRequest},
    };
    use astrcode_storage::in_memory::InMemoryEventStore;
    use parking_lot::RwLock;

    use super::*;

    struct ToolThenTextLlm {
        call_count: AtomicUsize,
    }

    struct StaticTextLlm {
        text: &'static str,
    }

    #[async_trait::async_trait]
    impl LlmProvider for StaticTextLlm {
        async fn generate(
            &self,
            _messages: Vec<LlmMessage>,
            _tools: Vec<ToolDefinition>,
        ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
            let (tx, rx) = mpsc::unbounded_channel();
            let _ = tx.send(LlmEvent::ContentDelta {
                delta: self.text.into(),
            });
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "stop".into(),
            });
            Ok(rx)
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: 200000,
                max_output_tokens: 1024,
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for ToolThenTextLlm {
        async fn generate(
            &self,
            _messages: Vec<LlmMessage>,
            _tools: Vec<ToolDefinition>,
        ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
            let call = self.call_count.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = mpsc::unbounded_channel();
            match call {
                0 => {
                    let _ = tx.send(LlmEvent::ToolCallStart {
                        call_id: "missing-tool-call".into(),
                        name: "missingTool".into(),
                        arguments: "{}".into(),
                    });
                    let _ = tx.send(LlmEvent::Done {
                        finish_reason: "tool_calls".into(),
                    });
                },
                _ => {
                    let _ = tx.send(LlmEvent::ContentDelta {
                        delta: "leaf ok".into(),
                    });
                    let _ = tx.send(LlmEvent::Done {
                        finish_reason: "stop".into(),
                    });
                },
            }
            Ok(rx)
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: 200000,
                max_output_tokens: 1024,
            }
        }
    }

    fn test_config_manager(
        llm_provider: Arc<dyn LlmProvider>,
    ) -> Arc<crate::config_manager::ConfigManager> {
        use astrcode_core::config::{EffectiveConfig, LlmSettings, OpenAiApiMode};
        Arc::new(crate::config_manager::ConfigManager::new(
            Arc::new(astrcode_storage::config_store::FileConfigStore::new(
                std::path::PathBuf::from("target/test-config.json"),
            )),
            Default::default(),
            EffectiveConfig {
                llm: LlmSettings {
                    provider_kind: "mock".into(),
                    base_url: String::new(),
                    api_key: String::new(),
                    api_mode: OpenAiApiMode::ChatCompletions,
                    model_id: "mock".into(),
                    max_tokens: 1024,
                    context_limit: 1024,
                    connect_timeout_secs: 1,
                    read_timeout_secs: 1,
                    max_retries: 0,
                    retry_base_delay_ms: 0,
                    temperature: None,
                    supports_prompt_cache_key: false,
                    prompt_cache_retention: None,
                    reasoning: false,
                    reasoning_split: false,
                },
                context: Default::default(),
            },
            llm_provider,
        ))
    }

    fn test_spawner(store: Arc<dyn EventStore>, llm: Arc<ToolThenTextLlm>) -> ServerSessionSpawner {
        let settings = astrcode_context::ContextSettings {
            compact_threshold_percent: 0.0,
            ..Default::default()
        };
        let llm_provider: Arc<dyn LlmProvider> = llm;
        let config = test_config_manager(llm_provider);
        let extension_runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));
        let session_manager = Arc::new(crate::session_manager::SessionManager::new(
            Arc::clone(&store),
            Arc::clone(&config),
            Arc::clone(&extension_runner),
            Arc::new(astrcode_session::SessionRuntimeRegistry::default()),
            Default::default(),
        ));
        ServerSessionSpawner {
            config,
            context_assembler: Arc::new(LlmContextAssembler::new(settings)),
            background_tasks: Default::default(),
            session_manager,
            extension_runner,
            agent_session_control: Arc::new(RwLock::new(None)),
        }
    }

    #[tokio::test]
    async fn spawned_session_runs_agent_loop_and_records_events() {
        let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
        let parent = Session::create(store.clone(), ".", "mock", None)
            .await
            .unwrap();
        let llm = Arc::new(ToolThenTextLlm {
            call_count: AtomicUsize::new(0),
        });
        let spawner = test_spawner(Arc::clone(&store), Arc::clone(&llm));
        let (progress_tx, _progress_rx) = mpsc::unbounded_channel();

        let result = spawner
            .spawn(
                parent.id().as_str(),
                SpawnRequest {
                    name: "nested".into(),
                    system_prompt: "nested extra prompt".into(),
                    user_prompt: "current nested prompt".into(),
                    working_dir: ".".into(),
                    model_preference: Some("mock".into()),
                    tool_call_id: Some("tool-call-1".into()),
                    event_tx: Some(progress_tx),
                    wait_for_result: true,
                },
            )
            .await
            .unwrap();
        let child_session_id = SessionId::from(result.child_session_id.clone());

        assert_eq!(result.content, "leaf ok");
        assert!(llm.call_count.load(Ordering::SeqCst) >= 2);

        let child_session = Session::open(store.clone(), child_session_id.clone())
            .await
            .unwrap();
        let child = child_session.read_model().await.unwrap();
        assert!(
            child.messages.iter().any(|message| {
                message
                    .content
                    .iter()
                    .any(|content| matches!(content, astrcode_core::llm::LlmContent::Text { text } if text.contains("leaf ok")))
            }),
            "agent response should be persisted to the child session"
        );

        // 父 session 应记录派生的子 Agent
        let parent_model = parent.read_model().await.unwrap();
        assert_eq!(parent_model.agent_sessions.len(), 1);
        assert_eq!(parent_model.agent_sessions[0].agent_name, "nested");
        assert_eq!(parent_model.agent_sessions[0].task, "current nested prompt");
    }

    #[tokio::test]
    async fn spawned_session_uses_latest_llm_provider() {
        let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
        let parent = Session::create(store.clone(), ".", "mock", None)
            .await
            .unwrap();
        let initial_provider: Arc<dyn LlmProvider> = Arc::new(StaticTextLlm { text: "old" });
        let config = test_config_manager(initial_provider);
        let extension_runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));
        let session_manager = Arc::new(crate::session_manager::SessionManager::new(
            Arc::clone(&store),
            Arc::clone(&config),
            Arc::clone(&extension_runner),
            Arc::new(astrcode_session::SessionRuntimeRegistry::default()),
            Default::default(),
        ));
        let spawner = ServerSessionSpawner {
            config: Arc::clone(&config),
            context_assembler: Arc::new(LlmContextAssembler::new(Default::default())),
            background_tasks: Default::default(),
            session_manager,
            extension_runner,
            agent_session_control: Arc::new(RwLock::new(None)),
        };
        config.set_llm_provider(Arc::new(StaticTextLlm { text: "new" }));

        let result = spawner
            .spawn(
                parent.id().as_str(),
                SpawnRequest {
                    name: "nested".into(),
                    system_prompt: "nested extra prompt".into(),
                    user_prompt: "current nested prompt".into(),
                    working_dir: ".".into(),
                    model_preference: Some("mock".into()),
                    tool_call_id: None,
                    event_tx: None,
                    wait_for_result: true,
                },
            )
            .await
            .unwrap();

        assert_eq!(result.content, "new");
    }

    #[tokio::test]
    async fn async_spawn_returns_immediately_with_background_task_id() {
        let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
        let parent = Session::create(store.clone(), ".", "mock", None)
            .await
            .unwrap();
        let llm = Arc::new(StaticTextLlm {
            text: "async result",
        });
        let llm_provider: Arc<dyn LlmProvider> = llm;
        let background_tasks: Arc<StdMutex<BackgroundTaskManager>> = Default::default();
        let config = test_config_manager(llm_provider);
        let extension_runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));
        let session_manager = Arc::new(crate::session_manager::SessionManager::new(
            Arc::clone(&store),
            Arc::clone(&config),
            Arc::clone(&extension_runner),
            Arc::new(astrcode_session::SessionRuntimeRegistry::default()),
            Arc::clone(&background_tasks),
        ));
        let spawner = ServerSessionSpawner {
            config,
            context_assembler: Arc::new(LlmContextAssembler::new(Default::default())),
            background_tasks: Arc::clone(&background_tasks),
            session_manager,
            extension_runner,
            agent_session_control: Arc::new(RwLock::new(None)),
        };

        let result = spawner
            .spawn(
                parent.id().as_str(),
                SpawnRequest {
                    name: "async-agent".into(),
                    system_prompt: "do work".into(),
                    user_prompt: "run async task".into(),
                    working_dir: ".".into(),
                    model_preference: Some("mock".into()),
                    tool_call_id: Some("call-async-1".into()),
                    event_tx: None,
                    wait_for_result: false,
                },
            )
            .await
            .unwrap();

        // 立即返回，带有 background_task_id
        assert!(
            result.background_task_id.is_some(),
            "async spawn should return background_task_id"
        );
        assert!(
            result.content.contains("异步 agent 已启动"),
            "async spawn placeholder should mention launch"
        );

        let child_sid = SessionId::from(result.child_session_id.clone());

        // 给后台任务一点时间完成
        tokio::time::sleep(Duration::from_millis(200)).await;

        // 后台任务应已完成
        {
            let bg = background_tasks.lock();
            let active = bg.list_active(parent.id());
            assert!(active.is_empty(), "background task should be cleaned up");
        }

        // 子会话应存在
        let child_session = Session::open(store.clone(), child_sid.clone())
            .await
            .unwrap();
        let child = child_session.read_model().await.unwrap();
        assert!(
            child.messages.iter().any(|message| {
                message
                    .content
                    .iter()
                    .any(|content| matches!(content, astrcode_core::llm::LlmContent::Text { text } if text.contains("async result")))
            }),
            "async agent response should be persisted to child session"
        );
        let child_events = store.replay_events(&child_sid).await.unwrap();
        assert!(
            child_events
                .iter()
                .any(|event| matches!(event.payload, EventPayload::TurnCompleted { .. })),
            "async child completion should be persisted to the child session"
        );

        // 父会话应有 AgentSessionCompleted 事件
        let parent_model = parent.read_model().await.unwrap();
        assert_eq!(parent_model.agent_sessions.len(), 1);
        assert_eq!(
            parent_model.agent_sessions[0].status,
            astrcode_core::storage::AgentSessionStatus::Completed
        );
        let parent_events = store.replay_events(parent.id()).await.unwrap();
        assert!(
            parent_events
                .iter()
                .all(|event| !matches!(event.payload, EventPayload::TurnCompleted { .. })),
            "async child completion must not be persisted to the parent session"
        );
    }
}
