//! Agent 子会话编排器。
//!
//! 负责创建子会话并把父子关系事件、child turn 请求交给对应 actor；
//! 自己不拥有任何 session 的 turn 状态或 durable 写入权限。
//!
//! 最大嵌套深度由 [`MAX_AGENT_DEPTH`] 控制；同层级可并发生成任意多个子 agent。

/// 子 agent 最大嵌套深度（root=0, child=1, grandchild=2）。
// TODO: 可配置
const MAX_AGENT_DEPTH: usize = 2;

use std::sync::Arc;

use astrcode_core::{
    event::{EventPayload, ToolOutputStream},
    types::{SessionId, ToolCallId, new_background_task_id},
};
use astrcode_extensions::runtime::{SpawnRequest, SpawnResult};
use astrcode_session::{
    Session,
    background::{BackgroundTaskManager, complete_background_task},
};
use parking_lot::Mutex as StdMutex;
use tokio::sync::mpsc;

/// 服务器端的 agent 子会话编排器，实现 `SessionSpawner` trait。
///
/// 当扩展返回 `ExtensionToolOutcome::RunSession` 时，
/// 扩展运行器通过此派生器创建子会话并运行 Agent 回合。
pub(crate) struct AgentSessionCoordinator {
    pub(crate) background_tasks: Arc<StdMutex<BackgroundTaskManager>>,
    pub(crate) session_directory: Arc<crate::session::directory::SessionDirectory>,
    pub(crate) session_bootstrapper: Arc<crate::session::bootstrapper::SessionBootstrapper>,
    pub(crate) session_supervisor:
        Arc<parking_lot::RwLock<Option<Arc<crate::session::SessionSupervisor>>>>,
}

// ─── spawn() 入口与准备阶段 ────────────────────────────────────────────

/// `prepare_child_session()` 的产出，传给 `spawn_sync` / `spawn_async`。
struct PreparedChild {
    parent_session: Arc<Session>,
    child_session: Arc<Session>,
    child_name: String,
    user_prompt: String,
    tool_call_id: Option<String>,
    model_id: String,
    progress: ProgressTx,
}

#[async_trait::async_trait]
impl astrcode_extensions::runtime::SessionSpawner for AgentSessionCoordinator {
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

impl AgentSessionCoordinator {
    fn supervisor(&self) -> Result<Arc<crate::session::SessionSupervisor>, String> {
        self.session_supervisor
            .read()
            .clone()
            .ok_or_else(|| "session supervisor not bound".to_string())
    }

    /// 沿 parent 链向上遍历，计算当前 session 的嵌套深度。
    ///
    /// root session depth=0，每向上一级 parent +1。
    async fn session_depth(&self, session_id: &SessionId) -> Result<usize, String> {
        let mut depth = 0;
        let mut current = session_id.clone();
        loop {
            let model = self
                .session_directory
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
                    .session_directory
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
            .session_directory
            .create_child(&request.working_dir, &model_id, &parent_session_id)
            .await
            .map_err(|e| format!("create child session: {e}"))?;

        let parent_session = Arc::new(
            self.session_directory
                .open(parent_session_id.clone())
                .await
                .map_err(|e| format!("open parent: {e}"))?,
        );

        let child_sid = child_session.id().clone();
        let child_arc = Arc::new(child_session);

        let tool_registry = self
            .session_bootstrapper
            .refresh_tool_registry(&child_sid, &request.working_dir)
            .await;
        let (system_prompt, fingerprint) = self
            .session_bootstrapper
            .build_system_prompt_snapshot(
                &child_sid,
                &request.working_dir,
                &model_id,
                &tool_registry,
                Some(&request.system_prompt),
            )
            .await
            .map_err(|e| format!("build child system prompt: {e}"))?;

        let supervisor = self
            .session_supervisor
            .read()
            .clone()
            .ok_or_else(|| "session supervisor not bound".to_string())?;
        let parent_handle = supervisor.handle_for(&parent_session_id);
        let child_handle = supervisor.handle_for(&child_sid);
        parent_handle
            .emit_session_event(
                parent_session_id.clone(),
                None,
                EventPayload::AgentSessionSpawned {
                    child_session_id: child_sid.clone(),
                    agent_name: child_name.clone(),
                    task: user_prompt.clone(),
                },
            )
            .await
            .map_err(|e| format!("append parent spawn event: {e}"))?;
        child_handle
            .emit_session_event(
                child_sid.clone(),
                None,
                EventPayload::SystemPromptConfigured {
                    text: system_prompt.clone(),
                    fingerprint,
                },
            )
            .await
            .map_err(|e| format!("append child prompt event: {e}"))?;
        progress.emit(
            ToolOutputStream::Stdout,
            format!("child agent '{child_name}' started: {child_sid} using {model_id}\n"),
        );

        Ok(PreparedChild {
            parent_session,
            child_session: child_arc,
            child_name,
            user_prompt,
            tool_call_id,
            model_id,
            progress,
        })
    }

    // ─── 同步路径 ────────────────────────────────────────────────────

    /// 阻塞等待子 Agent 完成并返回结果。
    async fn spawn_sync(&self, p: PreparedChild) -> Result<SpawnResult, String> {
        let PreparedChild {
            parent_session,
            child_session,
            user_prompt,
            ..
        } = p;
        let child_sid = child_session.id().clone();
        let supervisor = self.supervisor()?;
        let child_handle = supervisor.handle_for(&child_sid);
        let parent_handle = supervisor.handle_for(parent_session.id());
        let (_turn_id, completion_rx) = child_handle
            .submit_prompt_with_completion(child_sid.clone(), user_prompt)
            .await
            .map_err(|e| format!("start child turn: {e}"))?;
        match completion_rx
            .await
            .map_err(|_| "child turn completion channel closed".to_string())?
        {
            crate::session::TurnCompletion::Completed { .. } => {
                let text = read_last_assistant_text(child_session.as_ref()).await?;
                parent_handle
                    .emit_session_event(
                        parent_session.id().clone(),
                        None,
                        EventPayload::AgentSessionCompleted {
                            child_session_id: child_sid.clone(),
                            final_session_id: child_sid.clone(),
                            summary: one_line_summary(&text),
                        },
                    )
                    .await
                    .map_err(|e| format!("append parent completion event: {e}"))?;
                Ok(SpawnResult {
                    content: text,
                    child_session_id: child_sid.into_string(),
                    background_task_id: None,
                })
            },
            crate::session::TurnCompletion::Failed { error } => {
                parent_handle
                    .emit_session_event(
                        parent_session.id().clone(),
                        None,
                        EventPayload::AgentSessionFailed {
                            child_session_id: child_sid.clone(),
                            final_session_id: child_sid.clone(),
                            error: error.clone(),
                        },
                    )
                    .await
                    .map_err(|e| format!("append parent failure event: {e}"))?;
                Ok(SpawnResult {
                    content: format!("child agent error: {error}"),
                    child_session_id: child_sid.into_string(),
                    background_task_id: None,
                })
            },
            crate::session::TurnCompletion::Aborted => Ok(SpawnResult {
                content: "child agent was aborted".into(),
                child_session_id: child_sid.into_string(),
                background_task_id: None,
            }),
        }
    }

    // ─── 异步路径 ────────────────────────────────────────────────────

    /// 启动子 Agent 后立即返回占位结果，Agent 在后台完成。
    async fn spawn_async(&self, p: PreparedChild) -> Result<SpawnResult, String> {
        let PreparedChild {
            parent_session,
            child_session,
            child_name,
            user_prompt,
            tool_call_id,
            model_id,
            progress,
        } = p;

        let task_id = new_background_task_id();
        let task_id_str = task_id.to_string();

        let parent_sid = parent_session.id().clone();
        let register_sid = parent_sid.clone();
        let parent_sid = parent_session.id().clone();
        let watcher_bg_tasks = Arc::clone(&self.background_tasks);
        let watcher_task_id = task_id.clone();
        let tool_call_id_for_result = tool_call_id.clone();
        let async_child_sid = child_session.id().clone();
        let return_child_sid = child_session.id().clone();
        let supervisor = self.supervisor()?;
        let child_handle = supervisor.handle_for(&async_child_sid);
        let parent_handle = supervisor.handle_for(&parent_sid);

        let agent_handle = tokio::spawn(async move {
            let completion = match child_handle
                .submit_prompt_with_completion(async_child_sid.clone(), user_prompt)
                .await
            {
                Ok((_turn_id, rx)) => rx.await.ok(),
                Err(error) => Some(crate::session::TurnCompletion::Failed {
                    error: error.to_string(),
                }),
            };
            let (result_content, is_error) = match completion {
                Some(crate::session::TurnCompletion::Completed { .. }) => {
                    let text = read_last_assistant_text(child_session.as_ref())
                        .await
                        .unwrap_or_default();
                    let _ = parent_handle
                        .emit_session_event(
                            parent_sid.clone(),
                            None,
                            EventPayload::AgentSessionCompleted {
                                child_session_id: async_child_sid.clone(),
                                final_session_id: async_child_sid.clone(),
                                summary: one_line_summary(&text),
                            },
                        )
                        .await;
                    (text, false)
                },
                Some(crate::session::TurnCompletion::Failed { error }) => {
                    let _ = parent_handle
                        .emit_session_event(
                            parent_sid.clone(),
                            None,
                            EventPayload::AgentSessionFailed {
                                child_session_id: async_child_sid.clone(),
                                final_session_id: async_child_sid.clone(),
                                error: error.clone(),
                            },
                        )
                        .await;
                    (format!("child agent error: {error}"), true)
                },
                _ => ("child agent was aborted".into(), true),
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
                    is_error,
                    error: is_error.then(|| "child agent failed".into()),
                    metadata: meta,
                    duration_ms: None,
                };
                let _ = parent_handle
                    .emit_session_event(
                        parent_sid.clone(),
                        None,
                        EventPayload::ToolCallCompleted {
                            call_id: ToolCallId::from(call_id.as_str()),
                            tool_name: "agent".into(),
                            result,
                        },
                    )
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

async fn read_last_assistant_text(session: &Session) -> Result<String, String> {
    let state = session
        .read_model()
        .await
        .map_err(|e| format!("read child session: {e}"))?;
    Ok(state
        .messages
        .iter()
        .rev()
        .find_map(|message| {
            (message.role == astrcode_core::llm::LlmRole::Assistant).then(|| {
                message.content.iter().find_map(|content| match content {
                    astrcode_core::llm::LlmContent::Text { text } => Some(text.clone()),
                    _ => None,
                })
            })?
        })
        .unwrap_or_default())
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

    fn bind_test_supervisor(
        store: Arc<dyn EventStore>,
        config: Arc<crate::config_manager::ConfigManager>,
        extension_runner: Arc<ExtensionRunner>,
        session_directory: Arc<crate::session::directory::SessionDirectory>,
        session_bootstrapper: Arc<crate::session::bootstrapper::SessionBootstrapper>,
        background_tasks: Arc<StdMutex<BackgroundTaskManager>>,
        slot: &Arc<parking_lot::RwLock<Option<Arc<crate::session::SessionSupervisor>>>>,
    ) {
        let runtime = Arc::new(crate::bootstrap::ServerRuntime {
            event_store: store,
            config_manager: config,
            context_assembler: Arc::new(
                astrcode_context::context_assembler::LlmContextAssembler::new(Default::default()),
            ),
            background_tasks,
            session_directory,
            session_bootstrapper,
            extension_runner,
            session_supervisor: Arc::clone(slot),
            shutdown_token: tokio_util::sync::CancellationToken::new(),
        });
        let (tx, _) = tokio::sync::broadcast::channel(16);
        let event_publisher = Arc::new(crate::events::ClientEventPublisher::new(tx));
        *slot.write() = Some(Arc::new(crate::session::SessionSupervisor::new(
            runtime,
            event_publisher,
        )));
    }

    fn test_spawner(
        store: Arc<dyn EventStore>,
        llm: Arc<ToolThenTextLlm>,
    ) -> AgentSessionCoordinator {
        let llm_provider: Arc<dyn LlmProvider> = llm;
        let config = test_config_manager(llm_provider);
        let extension_runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));
        let runtime_registry = Arc::new(astrcode_session::SessionRuntimeRegistry::default());
        let session_directory = Arc::new(crate::session::directory::SessionDirectory::new(
            Arc::clone(&store),
            Arc::clone(&config),
            Arc::clone(&extension_runner),
            Arc::clone(&runtime_registry),
            Default::default(),
        ));
        let session_bootstrapper =
            Arc::new(crate::session::bootstrapper::SessionBootstrapper::new(
                Arc::clone(&config),
                Arc::clone(&extension_runner),
                runtime_registry,
            ));
        let session_supervisor = Arc::new(parking_lot::RwLock::new(None));
        bind_test_supervisor(
            Arc::clone(&store),
            Arc::clone(&config),
            Arc::clone(&extension_runner),
            Arc::clone(&session_directory),
            Arc::clone(&session_bootstrapper),
            Default::default(),
            &session_supervisor,
        );
        AgentSessionCoordinator {
            background_tasks: Default::default(),
            session_directory,
            session_bootstrapper,
            session_supervisor,
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
        let runtime_registry = Arc::new(astrcode_session::SessionRuntimeRegistry::default());
        let session_directory = Arc::new(crate::session::directory::SessionDirectory::new(
            Arc::clone(&store),
            Arc::clone(&config),
            Arc::clone(&extension_runner),
            Arc::clone(&runtime_registry),
            Default::default(),
        ));
        let session_bootstrapper =
            Arc::new(crate::session::bootstrapper::SessionBootstrapper::new(
                Arc::clone(&config),
                Arc::clone(&extension_runner),
                runtime_registry,
            ));
        let session_supervisor = Arc::new(parking_lot::RwLock::new(None));
        bind_test_supervisor(
            Arc::clone(&store),
            Arc::clone(&config),
            Arc::clone(&extension_runner),
            Arc::clone(&session_directory),
            Arc::clone(&session_bootstrapper),
            Default::default(),
            &session_supervisor,
        );
        let spawner = AgentSessionCoordinator {
            background_tasks: Default::default(),
            session_directory,
            session_bootstrapper,
            session_supervisor,
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
        let runtime_registry = Arc::new(astrcode_session::SessionRuntimeRegistry::default());
        let session_directory = Arc::new(crate::session::directory::SessionDirectory::new(
            Arc::clone(&store),
            Arc::clone(&config),
            Arc::clone(&extension_runner),
            Arc::clone(&runtime_registry),
            Arc::clone(&background_tasks),
        ));
        let session_bootstrapper =
            Arc::new(crate::session::bootstrapper::SessionBootstrapper::new(
                Arc::clone(&config),
                Arc::clone(&extension_runner),
                runtime_registry,
            ));
        let session_supervisor = Arc::new(parking_lot::RwLock::new(None));
        bind_test_supervisor(
            Arc::clone(&store),
            Arc::clone(&config),
            Arc::clone(&extension_runner),
            Arc::clone(&session_directory),
            Arc::clone(&session_bootstrapper),
            Arc::clone(&background_tasks),
            &session_supervisor,
        );
        let spawner = AgentSessionCoordinator {
            background_tasks: Arc::clone(&background_tasks),
            session_directory,
            session_bootstrapper,
            session_supervisor,
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
