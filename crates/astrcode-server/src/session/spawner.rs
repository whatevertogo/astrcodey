//! 子会话派生器 — 当扩展返回 `RunSession` 时的处理逻辑。
//!
//! [`ServerSessionSpawner`] 创建子会话、用子 Agent 执行一轮对话，
//! 将事件持久化到子会话存储，并经由 [`ProgressTx`] 将关键进展
//! 转译为 [`ToolOutputDelta`] 实时反馈给父会话的 TUI。

use std::sync::Arc;

use astrcode_context::manager::LlmContextAssembler;
use astrcode_core::{
    event::{Event, EventPayload, ToolOutputStream},
    llm::LlmProvider,
    types::{SessionId, ToolCallId, TurnId, new_background_task_id, new_message_id, new_turn_id},
};
use astrcode_extensions::{
    runner::ExtensionRunner,
    runtime::{SpawnRequest, SpawnResult},
};
use parking_lot::{Mutex as StdMutex, RwLock};
use tokio::sync::{Mutex, mpsc};

use super::{
    SameSessionCompactionInput, SessionManager, agent_turn_completed_payloads,
    agent_turn_failed_payloads, agent_turn_started_payloads, append_same_session_compaction,
};
use crate::{
    agent::{
        AgentError, AgentLoop, AgentServices, AgentSignal, AgentTurnOutput,
        AutoCompactFailureTracker, BackgroundTaskManager, background::complete_background_task,
        compact::compact_trigger_name, drive_agent, tool_types::BackgroundTaskCompletion,
    },
    bootstrap::{
        SystemPromptSnapshotInput, build_system_prompt_snapshot_with_files,
        build_tool_registry_snapshot, load_system_prompt_files, prompt_fingerprint,
    },
};


/// 服务器端的会话派生器，实现 `SessionSpawner` trait。
///
/// 当扩展返回 `ExtensionToolOutcome::RunSession` 时，
/// 扩展运行器通过此派生器创建子会话并运行 Agent 回合。
pub(crate) struct ServerSessionSpawner {
    pub(crate) session_manager: Arc<SessionManager>,
    pub(crate) llm_provider: Arc<RwLock<Arc<dyn LlmProvider>>>,
    pub(crate) context_assembler: Arc<LlmContextAssembler>,
    pub(crate) auto_compact_failures: Arc<AutoCompactFailureTracker>,
    pub(crate) background_tasks: Arc<StdMutex<BackgroundTaskManager>>,
    pub(crate) extension_runner: Arc<ExtensionRunner>,
    // bind 时从 effective.llm 快照，不会随配置热更新变化。
    // TODO: 如需配置热更新生效，改为持有 RwLock<EffectiveConfig> 引用，在 spawn 时动态读取。
    pub(crate) read_timeout_secs: u64,
}

#[async_trait::async_trait]
impl astrcode_extensions::runtime::SessionSpawner for ServerSessionSpawner {
    async fn spawn(
        &self,
        parent_session_id: &str,
        request: SpawnRequest,
    ) -> Result<SpawnResult, String> {
        let parent_session_id = SessionId::from(parent_session_id);
        let tool_call_id_clone = request.tool_call_id.clone();
        let progress = ProgressTx::new(request.tool_call_id, request.event_tx);
        let child_name = request.name.clone();
        let user_prompt = request.user_prompt.clone();
        let model_id = match request.model_preference.clone() {
            Some(model) => model,
            None => {
                self.session_manager
                    .read_model(&parent_session_id)
                    .await
                    .map_err(|e| format!("parent session {parent_session_id} not found: {e}"))?
                    .model_id
            },
        };

        let create_event = self
            .session_manager
            .create(&request.working_dir, &model_id, Some(&parent_session_id))
            .await
            .map_err(|e| format!("create child session: {e}"))?;

        let child_sid = create_event.session_id.clone();

        // 向父 session 记录派生关系
        self.session_manager
            .append_event(Event::new(
                parent_session_id.clone(),
                None,
                EventPayload::AgentSessionSpawned {
                    child_session_id: child_sid.clone(),
                    agent_name: child_name.clone(),
                    task: user_prompt.clone(),
                },
            ))
            .await
            .map_err(|e| format!("append parent spawn event: {e}"))?;

        let child_turn_id = new_turn_id();

        let registry_fut = build_tool_registry_snapshot(
            &self.extension_runner,
            &request.working_dir,
            self.read_timeout_secs,
        );
        let prompt_files_fut = load_system_prompt_files(&request.working_dir);
        let (tool_registry, prompt_files) = tokio::join!(registry_fut, prompt_files_fut);

        let prompt_tools_with_meta = tool_registry.list_definitions_with_prompt_metadata();
        let prompt_tools: Vec<_> = prompt_tools_with_meta
            .iter()
            .map(|(def, _)| def.clone())
            .collect();
        let tool_prompt_metadata = prompt_tools_with_meta
            .into_iter()
            .filter_map(|(def, meta)| meta.map(|m| (def.name, m)))
            .collect();
        let (system_prompt, fingerprint) =
            build_system_prompt_snapshot_with_files(SystemPromptSnapshotInput {
                extension_runner: &self.extension_runner,
                session_id: child_sid.as_str(),
                working_dir: &request.working_dir,
                model_id: &model_id,
                tools: &prompt_tools,
                extra_system_prompt: Some(&request.system_prompt),
                tool_prompt_metadata,
                prompt_files,
            })
            .await
            .map_err(|e| format!("build child system prompt: {e}"))?;

        append_child_payload(
            self.session_manager.as_ref(),
            &child_sid,
            None,
            EventPayload::SystemPromptConfigured {
                text: system_prompt.clone(),
                fingerprint,
            },
        )
        .await?;

        append_child_progress_payloads(
            self.session_manager.as_ref(),
            &progress,
            &child_sid,
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

        // 子会话的后台任务完成通道。
        // watcher 通过此通道发送 BackgroundTaskCompletion，
        // 下面的 spawned task 将其转为事件持久化到子会话存储。
        // 注意：需要共享 current_child_sid 以便 compact continuation 后
        // 也能正确定位最终的 leaf session。
        let (child_bg_result_tx, mut child_bg_result_rx) =
            mpsc::unbounded_channel::<BackgroundTaskCompletion>();

        let child_bg_sm = Arc::clone(&self.session_manager);
        let child_bg_progress = progress.clone();
        let child_bg_turn_id = child_turn_id.clone();
        tokio::spawn(async move {
            while let Some(completion) = child_bg_result_rx.recv().await {
                let sid = child_bg_final_sid.lock().await.clone();
                // 持久化 ToolCallCompleted 到子会话
                if let Err(e) = append_child_payload(
                    child_bg_sm.as_ref(),
                    &sid,
                    Some(&child_bg_turn_id),
                    completion.to_tool_call_completed(),
                )
                .await
                {
                    tracing::warn!(session_id = %sid, error = %e, "failed to persist ToolCallCompleted for background task");
                }
                // BackgroundTaskCompleted 是 live UI 状态；append_child_payload 会跳过持久化。
                let bg_event = completion.to_background_task_completed();
                if let Err(e) = append_child_payload(
                    child_bg_sm.as_ref(),
                    &sid,
                    Some(&child_bg_turn_id),
                    bg_event.clone(),
                )
                .await
                {
                    tracing::warn!(session_id = %sid, error = %e, "failed to persist BackgroundTaskCompleted");
                }
                child_bg_progress.forward(&bg_event);
            }
        });

        let agent = AgentLoop::new(
            child_sid.clone(),
            request.working_dir.clone(),
            system_prompt.clone(),
            model_id.clone(),
            AgentServices {
                llm: self.read_llm_provider(),
                tool_registry: Arc::clone(&tool_registry),
                extension_runner: Arc::clone(&self.extension_runner),
                context_assembler: Arc::clone(&self.context_assembler),
                session_manager: Arc::clone(&self.session_manager),
                auto_compact_failures: Arc::clone(&self.auto_compact_failures),
                background_result_tx: Some(child_bg_result_tx),
                background_tasks: Arc::clone(&self.background_tasks),
            },
        );

        // ── 异步路径：tokio::spawn 后立即返回占位结果 ────────────────
        if !request.wait_for_result {
            let task_id = new_background_task_id();
            let task_id_str = task_id.to_string();

            let parent_sid = parent_session_id.clone();
            let parent_sm = Arc::clone(&self.session_manager);
            let watcher_bg_tasks = Arc::clone(&self.background_tasks);
            let watcher_task_id = task_id.clone();
            let watcher_progress = progress.clone();
            let tool_call_id_for_result = tool_call_id_clone.clone();
            let cti_for_completion = child_turn_id.clone();
            let drive_session_manager = Arc::clone(&self.session_manager);
            let drive_current_child_sid = Arc::clone(&current_child_sid);
            let drive_child_turn_id = child_turn_id.clone();
            let drive_progress = progress.clone();
            let drive_system_prompt = system_prompt.clone();
            let async_child_sid = child_sid.clone();
            let return_child_sid = child_sid.clone();

            let agent_handle = tokio::spawn(async move {
                let (output, emitted_error, final_child_sid) = drive_child_agent_turn(
                    &agent,
                    &user_prompt,
                    drive_session_manager,
                    drive_current_child_sid,
                    drive_child_turn_id,
                    drive_progress,
                    drive_system_prompt,
                )
                .await;

                let result_content = match &output {
                    Ok(o) => {
                        let _ = append_child_progress_payloads(
                            parent_sm.as_ref(),
                            &watcher_progress,
                            &final_child_sid,
                            Some(&cti_for_completion),
                            agent_turn_completed_payloads(o.finish_reason.clone()),
                        )
                        .await;
                        let _ = parent_sm
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
                            parent_sm.as_ref(),
                            &watcher_progress,
                            &final_child_sid,
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
                        let _ = parent_sm
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

                // 向父 session 追加 ToolCallCompleted，让下一轮 projection 可见
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
                    let _ = parent_sm
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
                parent_session_id.clone(),
                agent_handle,
                dummy_watcher,
            );

            // 向父 session 记录异步派生关系
            progress.emit(
                ToolOutputStream::Stdout,
                format!(
                    "async child agent '{child_name}' launched: {return_child_sid} using \
                     {model_id}\n"
                ),
            );

            return Ok(SpawnResult {
                content: format!(
                    "异步 agent 已启动。完成后结果将在下一轮对话中可用。\nagent: {child_name} | \
                     session: {return_child_sid}"
                ),
                child_session_id: return_child_sid.into_string(),
                background_task_id: Some(task_id_str),
            });
        }

        // ── 同步路径：阻塞等待子 agent 完成 ───────────────────────
        let (output, emitted_error, final_child_sid) = drive_child_agent_turn(
            &agent,
            &user_prompt,
            Arc::clone(&self.session_manager),
            current_child_sid,
            child_turn_id.clone(),
            progress.clone(),
            system_prompt.clone(),
        )
        .await;

        match output {
            Ok(output) => {
                append_child_progress_payloads(
                    self.session_manager.as_ref(),
                    &progress,
                    &final_child_sid,
                    Some(&child_turn_id),
                    agent_turn_completed_payloads(output.finish_reason.clone()),
                )
                .await?;
                // 向父会话记录子 Agent 完成状态
                self.session_manager
                    .append_event(Event::new(
                        parent_session_id.clone(),
                        None,
                        EventPayload::AgentSessionCompleted {
                            child_session_id: child_sid.clone(),
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
                    self.session_manager.as_ref(),
                    &progress,
                    &final_child_sid,
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
                // 向父会话记录子 Agent 失败状态
                self.session_manager
                    .append_event(Event::new(
                        parent_session_id.clone(),
                        None,
                        EventPayload::AgentSessionFailed {
                            child_session_id: child_sid.clone(),
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
}

impl ServerSessionSpawner {
    fn read_llm_provider(&self) -> Arc<dyn LlmProvider> {
        self.llm_provider.read().clone()
    }
}

async fn drive_child_agent_turn(
    agent: &AgentLoop,
    user_prompt: &str,
    session_manager: Arc<SessionManager>,
    current_child_sid: Arc<Mutex<SessionId>>,
    child_turn_id: TurnId,
    progress: ProgressTx,
    system_prompt: String,
) -> (Result<AgentTurnOutput, AgentError>, bool, SessionId) {
    let signal_child_sid = Arc::clone(&current_child_sid);
    let (output, emitted_error) = drive_agent(agent, user_prompt, Vec::new(), move |signal| {
        let session_manager = Arc::clone(&session_manager);
        let current_child_sid = Arc::clone(&signal_child_sid);
        let child_turn_id = child_turn_id.clone();
        let progress = progress.clone();
        let system_prompt = system_prompt.clone();
        async move {
            match signal {
                AgentSignal::Event(payload) => {
                    let sid = current_child_sid.lock().await.clone();
                    let _ = append_child_progress_payload(
                        &session_manager,
                        &progress,
                        &sid,
                        Some(&child_turn_id),
                        payload.clone(),
                    )
                    .await;
                },
                AgentSignal::AutoCompact {
                    trigger,
                    compaction,
                    reply,
                } => {
                    let parent_sid = current_child_sid.lock().await.clone();
                    let result = append_same_session_compaction(
                        &session_manager,
                        SameSessionCompactionInput {
                            session_id: parent_sid.clone(),
                            system_prompt_fingerprint: prompt_fingerprint(&system_prompt),
                            system_prompt,
                            trigger_name: compact_trigger_name(trigger).into(),
                            compaction,
                        },
                    )
                    .await
                    .map(|_| parent_sid.clone());
                    if result.is_ok() {
                        progress.emit(
                            ToolOutputStream::Stdout,
                            format!("child agent compacted: {parent_sid}\n"),
                        );
                    }
                    let _ = reply.send(result);
                },
            }
        }
    })
    .await;
    let final_child_sid = current_child_sid.lock().await.clone();
    (output, emitted_error, final_child_sid)
}

/// 沿 `parent_session_id` 链路向上遍历，计算从根会话到指定会话的嵌套深度。
///
/// 根会话（无 `parent_session_id`）深度为 0，其子会话深度为 1，以此类推。
/// 将子 agent 事件转发为父级工具调用的 [`ToolOutputDelta`] 进度事件。
///
/// 持有父级工具调用 ID 和事件发送通道。`emit` 发送字符串消息，
/// `forward` 将子 agent 事件自动转译为对应的进度描述。
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

async fn append_child_payload(
    session_manager: &SessionManager,
    child_sid: &SessionId,
    child_turn_id: Option<&TurnId>,
    payload: EventPayload,
) -> Result<(), String> {
    if payload.is_durable() {
        session_manager
            .append_event(Event::new(
                child_sid.clone(),
                child_turn_id.cloned(),
                payload,
            ))
            .await
            .map_err(|e| format!("append child event: {e}"))?;
    }
    Ok(())
}

async fn append_child_progress_payload(
    session_manager: &SessionManager,
    progress: &ProgressTx,
    child_sid: &SessionId,
    child_turn_id: Option<&TurnId>,
    payload: EventPayload,
) -> Result<(), String> {
    progress.forward(&payload);
    if payload.is_durable() {
        append_child_payload(session_manager, child_sid, child_turn_id, payload).await?;
    }
    Ok(())
}

async fn append_child_progress_payloads<I>(
    session_manager: &SessionManager,
    progress: &ProgressTx,
    child_sid: &SessionId,
    child_turn_id: Option<&TurnId>,
    payloads: I,
) -> Result<(), String>
where
    I: IntoIterator<Item = EventPayload>,
{
    for payload in payloads {
        append_child_progress_payload(session_manager, progress, child_sid, child_turn_id, payload)
            .await?;
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

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use astrcode_context::manager::LlmContextAssembler;
    use astrcode_core::{
        llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
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
            // Call 0 asks for a missing tool (tool execution fails, growing
            // messages); call 1 returns the final text.
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

    fn test_spawner(
        session_manager: Arc<SessionManager>,
        llm: Arc<ToolThenTextLlm>,
    ) -> ServerSessionSpawner {
        let settings = astrcode_context::ContextSettings {
            compact_threshold_percent: 0.0,
            ..Default::default()
        };
        let llm_provider: Arc<dyn LlmProvider> = llm;
        ServerSessionSpawner {
            session_manager,
            llm_provider: Arc::new(RwLock::new(llm_provider)),
            context_assembler: Arc::new(LlmContextAssembler::new(settings)),
            auto_compact_failures: Arc::new(AutoCompactFailureTracker::default()),
            background_tasks: Default::default(),
            extension_runner: Arc::new(ExtensionRunner::new(Duration::from_secs(1))),
            read_timeout_secs: 1,
        }
    }

    #[tokio::test]
    async fn spawned_session_runs_agent_loop_and_records_events() {
        let session_manager = Arc::new(SessionManager::new(Arc::new(InMemoryEventStore::new())));
        let parent = session_manager.create(".", "mock", None).await.unwrap();
        let llm = Arc::new(ToolThenTextLlm {
            call_count: AtomicUsize::new(0),
        });
        let spawner = test_spawner(Arc::clone(&session_manager), Arc::clone(&llm));
        let (progress_tx, _progress_rx) = mpsc::unbounded_channel();

        let result = spawner
            .spawn(
                parent.session_id.as_str(),
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

        let child = session_manager.read_model(&child_session_id).await.unwrap();
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
        let parent_model = session_manager
            .read_model(&parent.session_id)
            .await
            .unwrap();
        assert_eq!(parent_model.agent_sessions.len(), 1);
        assert_eq!(parent_model.agent_sessions[0].agent_name, "nested");
        assert_eq!(parent_model.agent_sessions[0].task, "current nested prompt");
    }

    #[tokio::test]
    async fn spawned_session_uses_latest_llm_provider() {
        let session_manager = Arc::new(SessionManager::new(Arc::new(InMemoryEventStore::new())));
        let parent = session_manager.create(".", "mock", None).await.unwrap();
        let initial_provider: Arc<dyn LlmProvider> = Arc::new(StaticTextLlm { text: "old" });
        let llm_provider = Arc::new(RwLock::new(initial_provider));
        let spawner = ServerSessionSpawner {
            session_manager,
            llm_provider: Arc::clone(&llm_provider),
            context_assembler: Arc::new(LlmContextAssembler::new(Default::default())),
            auto_compact_failures: Arc::new(AutoCompactFailureTracker::default()),
            background_tasks: Default::default(),
            extension_runner: Arc::new(ExtensionRunner::new(Duration::from_secs(1))),
            read_timeout_secs: 1,
        };
        *llm_provider.write() = Arc::new(StaticTextLlm { text: "new" });

        let result = spawner
            .spawn(
                parent.session_id.as_str(),
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
        let session_manager = Arc::new(SessionManager::new(Arc::new(InMemoryEventStore::new())));
        let parent = session_manager.create(".", "mock", None).await.unwrap();
        let llm = Arc::new(StaticTextLlm {
            text: "async result",
        });
        let llm_provider: Arc<dyn LlmProvider> = llm;
        let background_tasks: Arc<StdMutex<BackgroundTaskManager>> = Default::default();
        let spawner = ServerSessionSpawner {
            session_manager: Arc::clone(&session_manager),
            llm_provider: Arc::new(RwLock::new(llm_provider)),
            context_assembler: Arc::new(LlmContextAssembler::new(Default::default())),
            auto_compact_failures: Arc::new(AutoCompactFailureTracker::default()),
            background_tasks: Arc::clone(&background_tasks),
            extension_runner: Arc::new(ExtensionRunner::new(Duration::from_secs(1))),
            read_timeout_secs: 1,
        };

        let result = spawner
            .spawn(
                parent.session_id.as_str(),
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
            let active = bg.list_active(&parent.session_id);
            assert!(active.is_empty(), "background task should be cleaned up");
        }

        // 子会话应存在
        let child = session_manager.read_model(&child_sid).await.unwrap();
        assert!(
            child.messages.iter().any(|message| {
                message
                    .content
                    .iter()
                    .any(|content| matches!(content, astrcode_core::llm::LlmContent::Text { text } if text.contains("async result")))
            }),
            "async agent response should be persisted to child session"
        );

        // 父会话应有 AgentSessionCompleted 事件
        let parent_model = session_manager
            .read_model(&parent.session_id)
            .await
            .unwrap();
        assert_eq!(parent_model.agent_sessions.len(), 1);
        assert_eq!(
            parent_model.agent_sessions[0].status,
            astrcode_core::storage::AgentSessionStatus::Completed
        );
    }
}
