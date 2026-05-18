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

use astrcode_core::{
    event::{Event, EventPayload, ToolOutputStream},
    types::{SessionId, ToolCallId, new_background_task_id, new_message_id, new_turn_id},
};
use astrcode_extensions::runtime::{SpawnRequest, SpawnResult};
use astrcode_session::{
    EventSink, Session, agent_turn_completed_payloads, agent_turn_failed_payloads,
    agent_turn_started_payloads, background::complete_background_task,
};
use tokio::sync::mpsc;

/// 服务器端的会话派生器，实现 `SessionSpawner` trait。
///
/// 当扩展返回 `ExtensionToolOutcome::RunSession` 时，
/// 扩展运行器通过此派生器创建子会话并运行 Agent 回合。
pub(crate) struct ServerSessionSpawner {
    pub(crate) session_manager: Arc<crate::session_manager::SessionManager>,
}

// ─── spawn() 入口 ─────────────────────────────────────────────────────

#[async_trait::async_trait]
impl astrcode_extensions::runtime::SessionSpawner for ServerSessionSpawner {
    async fn spawn(
        &self,
        parent_session_id: &str,
        request: SpawnRequest,
    ) -> Result<SpawnResult, String> {
        let wait_for_result = request.wait_for_result;
        let (child_session, parent_session, child_turn_id, user_prompt, progress) =
            self.prepare(parent_session_id, request).await?;
        if wait_for_result {
            self.spawn_sync(
                child_session,
                parent_session,
                child_turn_id,
                user_prompt,
                progress,
            )
            .await
        } else {
            self.spawn_async(
                child_session,
                parent_session,
                child_turn_id,
                user_prompt,
                progress,
            )
            .await
        }
    }
}

impl ServerSessionSpawner {
    /// 沿 parent 链向上遍历，计算当前 session 的嵌套深度。
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

    /// 创建子会话，注入 extra_system_prompt，追加父 session 派生事件。
    async fn prepare(
        &self,
        parent_session_id: &str,
        request: SpawnRequest,
    ) -> Result<
        (
            Arc<Session>,
            Arc<Session>,
            astrcode_core::types::TurnId,
            String,
            ProgressTx,
        ),
        String,
    > {
        let parent_session_id = SessionId::from(parent_session_id);
        let progress = ProgressTx::new(request.tool_call_id, request.event_tx);
        let child_name = request.name.clone();
        let user_prompt = request.user_prompt.clone();

        let parent_session = Arc::new(
            self.session_manager
                .open(parent_session_id.clone())
                .await
                .map_err(|e| format!("open parent: {e}"))?,
        );

        let model_id = match request.model_preference {
            Some(model) => model,
            None => {
                parent_session
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

        // Session::spawn_child 内部完成 create + 注入 extra_system_prompt + 追加
        // AgentSessionSpawned。
        let child_session = parent_session
            .spawn_child(
                &request.working_dir,
                &model_id,
                child_name.clone(),
                user_prompt.clone(),
                Some(request.system_prompt),
            )
            .await
            .map_err(|e| format!("spawn child session: {e}"))?;

        let child_sid = child_session.id().clone();
        let child_turn_id = new_turn_id();
        let child_arc = Arc::new(child_session);

        // 追加 TurnStarted + UserMessage 到子 session（通过 child.emit 写 store + fanout，
        // ChildProgressSink 在后续 submit 路径里负责转发到父 progress）。
        let user_prompt_for_submit = request.user_prompt.clone();
        for payload in agent_turn_started_payloads(new_message_id(), user_prompt) {
            // 这两条事件先于 submit 写入；progress 转发由 spawn_sync/async 内的 sink 接管。
            // 这里手动转发一次，避免 prepare 阶段的事件被父 progress 漏掉。
            progress.forward(&payload);
            child_arc.emit(Some(&child_turn_id), payload).await;
        }
        progress.emit(
            ToolOutputStream::Stdout,
            format!("child agent '{child_name}' started: {child_sid} using {model_id}\n"),
        );

        Ok((
            child_arc,
            parent_session,
            child_turn_id,
            user_prompt_for_submit,
            progress,
        ))
    }

    // ─── 同步路径 ────────────────────────────────────────────────────

    async fn spawn_sync(
        &self,
        child_session: Arc<Session>,
        parent_session: Arc<Session>,
        child_turn_id: astrcode_core::types::TurnId,
        user_prompt: String,
        progress: ProgressTx,
    ) -> Result<SpawnResult, String> {
        let child_sid = child_session.id().clone();
        let sink: Arc<dyn EventSink> = Arc::new(ChildProgressSink {
            progress: progress.clone(),
        });

        let handle = child_session
            .submit(user_prompt, child_turn_id.clone(), Some(sink))
            .await
            .map_err(|e| format!("child submit: {e}"))?;

        let result = handle.wait().await;
        let (output, emitted_error) = match result {
            Some(r) => (r.output, r.emitted_error),
            None => (
                Err(astrcode_session::TurnError::Internal(
                    "child turn task panicked".into(),
                )),
                false,
            ),
        };

        let content = finalize_child_turn(
            &child_session,
            &parent_session,
            &child_turn_id,
            &output,
            emitted_error,
            &progress,
        )
        .await?;
        Ok(SpawnResult {
            content,
            child_session_id: child_sid.into_string(),
            background_task_id: None,
        })
    }

    // ─── 异步路径 ────────────────────────────────────────────────────

    async fn spawn_async(
        &self,
        child_session: Arc<Session>,
        parent_session: Arc<Session>,
        child_turn_id: astrcode_core::types::TurnId,
        user_prompt: String,
        progress: ProgressTx,
    ) -> Result<SpawnResult, String> {
        let child_sid = child_session.id().clone();
        let return_child_sid = child_sid.clone();
        let task_id = new_background_task_id();
        let task_id_str = task_id.to_string();
        // 后台 agent 注册到父 session 的 runtime bg_tasks——父被删除/重置时
        // 子 agent task 跟着一起被清。在 spawn 闭包之外预留一份 parent_sid 副本，
        // 因为 parent_session 本身会被 `async move` 捕获。
        let register_sid = parent_session.id().clone();
        let parent_bg_tasks = parent_session.runtime().background_tasks();
        let watcher_bg_tasks = Arc::clone(&parent_bg_tasks);
        let watcher_task_id = task_id.clone();
        let parent_sid = register_sid.clone();

        let agent_handle = tokio::spawn(async move {
            let sink: Arc<dyn EventSink> = Arc::new(ChildProgressSink {
                progress: progress.clone(),
            });

            let result = match child_session
                .submit(user_prompt, child_turn_id.clone(), Some(sink))
                .await
            {
                Ok(handle) => handle.wait().await,
                Err(e) => {
                    progress.emit(
                        ToolOutputStream::Stderr,
                        format!("child submit error: {e}\n"),
                    );
                    let _ = parent_session
                        .append_event(Event::new(
                            parent_sid.clone(),
                            None,
                            EventPayload::AgentSessionFailed {
                                child_session_id: child_sid.clone(),
                                final_session_id: child_sid.clone(),
                                error: e.to_string(),
                            },
                        ))
                        .await;
                    complete_background_task(&watcher_bg_tasks, &watcher_task_id);
                    return;
                },
            };

            let (output, emitted_error) = match result {
                Some(r) => (r.output, r.emitted_error),
                None => (
                    Err(astrcode_session::TurnError::Internal(
                        "child turn task panicked".into(),
                    )),
                    false,
                ),
            };

            let result_content = finalize_child_turn(
                &child_session,
                &parent_session,
                &child_turn_id,
                &output,
                emitted_error,
                &progress,
            )
            .await
            .unwrap_or_else(|e| format!("finalize child turn failed: {e}"));

            if let Some(call_id) = progress.call_id() {
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
                let _ = parent_session
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

        let dummy_watcher = tokio::spawn(async {});
        parent_bg_tasks
            .lock()
            .register(task_id.clone(), register_sid, agent_handle, dummy_watcher);

        Ok(SpawnResult {
            content: format!(
                "异步 agent 已启动。完成后结果将在下一轮对话中可用。\nsession: {return_child_sid}"
            ),
            child_session_id: return_child_sid.into_string(),
            background_task_id: Some(task_id_str),
        })
    }
}

// ─── 子 Agent 驱动 ────────────────────────────────────────────────────

/// 子 turn 终态写入与父子事件汇报。
///
/// 把子 session 的完成/失败 payloads 写入子事件日志、把摘要事件写到父 session、
/// 顺带通知父侧的 progress 进度通道。返回结果文本（成功时是 LLM 文本，失败时是
/// 格式化的错误描述），调用方据此构造 `SpawnResult.content`。
async fn finalize_child_turn(
    child_session: &Session,
    parent_session: &Session,
    child_turn_id: &astrcode_core::types::TurnId,
    output: &Result<astrcode_session::TurnOutput, astrcode_session::TurnError>,
    emitted_error: bool,
    progress: &ProgressTx,
) -> Result<String, String> {
    let child_sid = child_session.id().clone();
    let parent_sid = parent_session.id().clone();
    match output {
        Ok(out) => {
            for payload in agent_turn_completed_payloads(out.finish_reason.clone()) {
                if let Err(e) = child_session
                    .append_event(Event::new(
                        child_sid.clone(),
                        Some(child_turn_id.clone()),
                        payload,
                    ))
                    .await
                {
                    tracing::error!(
                        session_id = %child_sid,
                        error = %e,
                        "append child completion event failed",
                    );
                }
            }
            progress.emit(
                ToolOutputStream::Stdout,
                format!("child turn completed: {}\n", out.finish_reason),
            );
            parent_session
                .append_event(Event::new(
                    parent_sid,
                    None,
                    EventPayload::AgentSessionCompleted {
                        child_session_id: child_sid.clone(),
                        final_session_id: child_sid,
                        summary: one_line_summary(&out.text),
                    },
                ))
                .await
                .map_err(|e| format!("append parent completion event: {e}"))?;
            Ok(out.text.clone())
        },
        Err(e) => {
            for payload in
                agent_turn_failed_payloads((!emitted_error).then(|| e.to_string()), "error".into())
            {
                if let Err(append_err) = child_session
                    .append_event(Event::new(
                        child_sid.clone(),
                        Some(child_turn_id.clone()),
                        payload,
                    ))
                    .await
                {
                    tracing::error!(
                        session_id = %child_sid,
                        error = %append_err,
                        "append child failure event failed",
                    );
                }
            }
            progress.emit(
                ToolOutputStream::Stderr,
                format!("child agent error: {e}\n"),
            );
            parent_session
                .append_event(Event::new(
                    parent_sid,
                    None,
                    EventPayload::AgentSessionFailed {
                        child_session_id: child_sid.clone(),
                        final_session_id: child_sid,
                        error: e.to_string(),
                    },
                ))
                .await
                .map_err(|e| format!("append parent failure event: {e}"))?;
            Ok(format!("child agent error: {e}"))
        },
    }
}

/// 子会话 EventSink：把子事件翻译成父 session 的 progress 通道（lossless mpsc）。
///
/// 持久化由 `Session::emit` 自己负责，本 sink 只做单向 progress 转发。
struct ChildProgressSink {
    progress: ProgressTx,
}

#[async_trait::async_trait]
impl EventSink for ChildProgressSink {
    async fn on_event(&self, event: &astrcode_core::event::Event) {
        self.progress.forward(&event.payload);
    }
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

    fn call_id(&self) -> Option<&String> {
        self.call_id.as_ref()
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
        types::new_session_id,
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

    fn test_capabilities(
        config: &Arc<crate::config_manager::ConfigManager>,
        extension_runner: Arc<ExtensionRunner>,
        context_assembler: Arc<LlmContextAssembler>,
    ) -> Arc<astrcode_session::Capabilities> {
        let caps = Arc::new(astrcode_session::Capabilities::new(
            config.read_llm_provider(),
            extension_runner,
            context_assembler,
            config.read_effective().clone(),
        ));
        config.attach_capabilities(Arc::clone(&caps));
        caps
    }

    fn test_spawner(store: Arc<dyn EventStore>, llm: Arc<ToolThenTextLlm>) -> ServerSessionSpawner {
        let settings = astrcode_context::ContextSettings {
            compact_threshold_percent: 0.0,
            ..Default::default()
        };
        let llm_provider: Arc<dyn LlmProvider> = llm;
        let config = test_config_manager(llm_provider);
        let extension_runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));
        let context_assembler = Arc::new(LlmContextAssembler::new(settings));
        let capabilities = test_capabilities(
            &config,
            Arc::clone(&extension_runner),
            Arc::clone(&context_assembler),
        );
        let session_manager = Arc::new(crate::session_manager::SessionManager::new(
            Arc::clone(&store),
            Arc::clone(&config),
            Arc::clone(&extension_runner),
            capabilities,
        ));
        ServerSessionSpawner { session_manager }
    }

    #[tokio::test]
    async fn spawned_session_runs_agent_loop_and_records_events() {
        let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
        let parent_id = new_session_id();
        store
            .create_session(&parent_id, ".", "mock", None)
            .await
            .unwrap();
        let llm = Arc::new(ToolThenTextLlm {
            call_count: AtomicUsize::new(0),
        });
        let spawner = test_spawner(Arc::clone(&store), Arc::clone(&llm));
        let (progress_tx, _progress_rx) = mpsc::unbounded_channel();

        let result = spawner
            .spawn(
                parent_id.as_str(),
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

        let child = store.session_read_model(&child_session_id).await.unwrap();
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
        let parent_model = store.session_read_model(&parent_id).await.unwrap();
        assert_eq!(parent_model.agent_sessions.len(), 1);
        assert_eq!(parent_model.agent_sessions[0].agent_name, "nested");
        assert_eq!(parent_model.agent_sessions[0].task, "current nested prompt");
    }

    #[tokio::test]
    async fn spawned_session_uses_latest_llm_provider() {
        let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
        let parent_id = new_session_id();
        store
            .create_session(&parent_id, ".", "mock", None)
            .await
            .unwrap();
        let initial_provider: Arc<dyn LlmProvider> = Arc::new(StaticTextLlm { text: "old" });
        let config = test_config_manager(initial_provider);
        let extension_runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));
        let context_assembler = Arc::new(LlmContextAssembler::new(Default::default()));
        let capabilities = test_capabilities(
            &config,
            Arc::clone(&extension_runner),
            Arc::clone(&context_assembler),
        );
        let session_manager = Arc::new(crate::session_manager::SessionManager::new(
            Arc::clone(&store),
            Arc::clone(&config),
            Arc::clone(&extension_runner),
            capabilities,
        ));
        let spawner = ServerSessionSpawner { session_manager };
        config.set_llm_provider(Arc::new(StaticTextLlm { text: "new" }));

        let result = spawner
            .spawn(
                parent_id.as_str(),
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
        let parent_id = new_session_id();
        store
            .create_session(&parent_id, ".", "mock", None)
            .await
            .unwrap();
        let llm = Arc::new(StaticTextLlm {
            text: "async result",
        });
        let llm_provider: Arc<dyn LlmProvider> = llm;
        let config = test_config_manager(llm_provider);
        let extension_runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));
        let context_assembler = Arc::new(LlmContextAssembler::new(Default::default()));
        let capabilities = test_capabilities(
            &config,
            Arc::clone(&extension_runner),
            Arc::clone(&context_assembler),
        );
        let session_manager = Arc::new(crate::session_manager::SessionManager::new(
            Arc::clone(&store),
            Arc::clone(&config),
            Arc::clone(&extension_runner),
            capabilities,
        ));
        // 通过 session_manager.open 拿到与 spawner 共享的 parent_session runtime；
        // spawner 的 spawn_async 会向同一份 bg_tasks 注册子 agent。
        let parent_session = session_manager.open(parent_id.clone()).await.unwrap();
        let parent_bg_tasks = parent_session.runtime().background_tasks();
        let spawner = ServerSessionSpawner {
            session_manager: Arc::clone(&session_manager),
        };

        let result = spawner
            .spawn(
                parent_id.as_str(),
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
            let bg = parent_bg_tasks.lock();
            let active = bg.list_active(&parent_id);
            assert!(active.is_empty(), "background task should be cleaned up");
        }

        // 子会话应存在
        let child = store.session_read_model(&child_sid).await.unwrap();
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
        let parent_model = store.session_read_model(&parent_id).await.unwrap();
        assert_eq!(parent_model.agent_sessions.len(), 1);
        assert_eq!(
            parent_model.agent_sessions[0].status,
            astrcode_core::storage::AgentSessionStatus::Completed
        );
        let parent_events = store.replay_events(&parent_id).await.unwrap();
        assert!(
            parent_events
                .iter()
                .all(|event| !matches!(event.payload, EventPayload::TurnCompleted { .. })),
            "async child completion must not be persisted to the parent session"
        );
    }
}
