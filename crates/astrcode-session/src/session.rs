//! Session 句柄 — 带存储能力的会话操作入口。
//!
//! Session 是系统唯一的持久事实来源。所有关键状态变化以不可变事件
//! 写入持久层，任何时刻都可通过事件日志和快照重建 session 状态。
//!
//! 内部 runtime 通过此类型操作会话。

use std::sync::Arc;

use astrcode_core::{
    config::ModelSelection,
    event::{Event, EventPayload},
    extension::{ChildToolPolicy, ExtensionEvent, LifecycleContext},
    prompt::SystemPromptInput,
    storage::{
        CompactSnapshotInput, EventStore, SessionReadModel, StorageError, ToolResultArtifactInput,
        ToolResultArtifactReader, ToolResultArtifactRef, ToolResultArtifactSlice,
    },
    types::*,
};
use astrcode_support::{hash::hex_fingerprint, perf_snapshot, shell::resolve_shell};
use tokio::sync::mpsc;

use crate::{
    background::{BackgroundTaskCompletion, spawn_background_forwarder},
    child_turn::ChildTurnGuard,
    payload::{
        compact_boundary_payload, session_continued_from_compaction_payload,
        system_prompt_configured_payload,
    },
    session_runtime::SessionRuntimeState,
    session_runtime_services::SessionRuntimeServices,
    turn_context::TurnError,
    turn_handle::TurnHandle,
    turn_runner::{RunTurnResult, TurnRunner, run_turn},
};

fn normalize_extra_system_prompt(extra_system_prompt: Option<&str>) -> Option<String> {
    extra_system_prompt.and_then(|prompt| {
        let trimmed = prompt.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

/// 会话句柄 — 带存储能力的会话操作入口。
///
/// 字段语义：
/// - `runtime`：进程内瞬态资源（工具表、file_obs、bg_tasks、event_tx）。 broadcast 在 runtime
///   上而不是 Session 上：同 sid 多次 `Session::open` / `clone` 仍共享同一个
///   broadcast，订阅者一处订阅就能看到所有实例发出的事件。
/// - `caps`：跨 session 共享的基础设施（LLM、扩展、上下文组装器、配置）。
///
/// `Clone` 是廉价的 Arc clone，可以自由复制。
#[derive(Clone)]
pub struct Session {
    id: SessionId,
    store: Arc<dyn EventStore>,
    runtime: Arc<SessionRuntimeState>,
    caps: Arc<SessionRuntimeServices>,
}

impl Session {
    /// 用调用方指定的 sid 创建会话。
    ///
    /// **注意**：`runtime` 必须由调用方保证「同 sid 唯一」，否则同 sid 的不同 Session
    /// 实例会有不同的 broadcast、不同的工具表、不同的 bg_tasks，订阅者只能看到自己那份
    /// 实例上发出的事件。生产路径走 `SessionManager`，由其内部的 `runtime_states` HashMap
    /// 保证唯一；CLI / 测试若直接调本入口须自行维护一份 sid→runtime 映射，或接受隔离语义。
    #[allow(clippy::too_many_arguments)] // 构造函数要持有完整依赖图
    pub async fn create_with_id(
        store: Arc<dyn EventStore>,
        sid: SessionId,
        working_dir: &str,
        model_id: &str,
        parent: Option<&SessionId>,
        tool_policy: Option<&ChildToolPolicy>,
        source_extension: Option<&str>,
        runtime: Arc<SessionRuntimeState>,
        caps: Arc<SessionRuntimeServices>,
    ) -> Result<Self, SessionError> {
        store
            .create_session(
                &sid,
                working_dir,
                model_id,
                parent,
                tool_policy,
                source_extension,
            )
            .await?;
        // tool_policy 走 event log 持久化；同步注入 runtime 让首次 refresh_tools 立刻生效。
        if let Some(policy) = tool_policy {
            runtime.set_tool_policy(Some(policy.clone()));
        }
        Ok(Self {
            id: sid,
            store,
            runtime,
            caps,
        })
    }

    /// 从磁盘恢复已有会话并附带运行时/能力/事件广播。
    ///
    /// 同 sid 的并发 `open` 必须共享 `runtime`——参见 `create_with_id` 的同条警告。
    ///
    /// resume 时从 projection 读 `tool_policy` 并写回 runtime，让 `refresh_tools`
    /// 重建工具表时与首次创建一致。父子 session 走同一条路：根 session 的 policy
    /// 是 `None`，子 session 的 policy 来自 spawn 时写入的 `SessionStarted`。
    pub async fn open(
        store: Arc<dyn EventStore>,
        id: SessionId,
        runtime: Arc<SessionRuntimeState>,
        caps: Arc<SessionRuntimeServices>,
    ) -> Result<Self, SessionError> {
        store.open_session(&id).await?;
        // 优先信任 runtime 中已存在的 policy（spawn_child 注入路径），
        // 仅在 runtime 为空时从 projection 回填——避免覆盖最新的进程内状态。
        if runtime.tool_policy().is_none() {
            let model = store.session_read_model(&id).await?;
            if let Some(policy) = model.tool_policy {
                runtime.set_tool_policy(Some(policy));
            }
        }
        Ok(Self {
            id,
            store,
            runtime,
            caps,
        })
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    pub fn runtime(&self) -> &Arc<SessionRuntimeState> {
        &self.runtime
    }

    pub fn caps(&self) -> &Arc<SessionRuntimeServices> {
        &self.caps
    }

    /// 返回本 session 在存储层中的真实目录路径。
    pub async fn session_store_dir(&self) -> Option<std::path::PathBuf> {
        self.store.session_store_dir(&self.id).await.ok().flatten()
    }

    /// 订阅本 session 的事件 fan-out 通道。
    ///
    /// 同 sid 不同 Session 实例订阅的是同一份 EventFanout（在 runtime 上），
    /// 因此一个订阅者能看到任何实例发出的事件。
    pub fn subscribe(&self) -> mpsc::Receiver<Event> {
        self.runtime.subscribe()
    }

    // ─── 事件操作 ──────────────────────────────────────────────────────

    /// 追加持久事件到事件日志，分配递增序号。
    pub async fn append_event(&self, event: Event) -> Result<Event, SessionError> {
        let stored = self.store.append_event(event).await?;
        self.runtime.fanout(stored.clone());
        perf_snapshot::capture_event("session.append_event", &stored);
        Ok(stored)
    }

    /// 发射只 fanout、不持久化的 live 事件。Infallible。
    pub async fn emit_live(&self, turn_id: Option<&TurnId>, payload: EventPayload) {
        let event = Event::new(self.id.clone(), turn_id.cloned(), payload);
        perf_snapshot::capture_event("session.emit_live", &event);
        self.runtime.fanout(event);
    }

    /// 持久化 durable 事件后 fanout。持久化失败返回 Err，调用方决定是否中止。
    pub async fn emit_durable(
        &self,
        turn_id: Option<&TurnId>,
        payload: EventPayload,
    ) -> Result<Event, SessionError> {
        let event = Event::new(self.id.clone(), turn_id.cloned(), payload);
        let stored = self.store.append_event(event).await?;
        self.runtime.fanout(stored.clone());
        perf_snapshot::capture_event("session.emit_durable", &stored);
        Ok(stored)
    }

    /// 发射 session 生命周期事件。
    pub async fn emit_lifecycle(&self, event: ExtensionEvent) -> Result<(), SessionError> {
        let model = self.read_model().await?;
        Self::emit_lifecycle_for_read_model(&self.caps, &self.id, &model, event).await
    }

    /// 发射 session 生命周期事件，不要求构造完整 [`Session`]。
    pub async fn emit_lifecycle_for_read_model(
        caps: &SessionRuntimeServices,
        session_id: &SessionId,
        model: &SessionReadModel,
        event: ExtensionEvent,
    ) -> Result<(), SessionError> {
        caps.extension_runner()
            .emit_lifecycle(
                event,
                LifecycleContext {
                    session_id: session_id.to_string(),
                    working_dir: model.working_dir.clone(),
                    model: ModelSelection::simple(model.model_id.clone()),
                    extension_event_sink: None,
                    last_exchange: None,
                },
            )
            .await?;
        Ok(())
    }

    /// 更新会话使用的模型标识。
    ///
    /// 仅在 model_id 与当前值不同时写入 `ModelIdChanged` 事件，避免冗余事件。
    pub async fn update_model_id(&self, model_id: &str) -> Result<Option<Event>, SessionError> {
        let current = self.read_model().await?;
        if current.model_id == model_id {
            return Ok(None);
        }
        self.append_event(Event::new(
            self.id.clone(),
            None,
            EventPayload::ModelIdChanged {
                model_id: model_id.to_string(),
            },
        ))
        .await
        .map(Some)
    }

    /// 返回会话读模型。
    pub async fn read_model(&self) -> Result<SessionReadModel, SessionError> {
        Ok(self.store.session_read_model(&self.id).await?)
    }

    /// 返回当前 system_prompt，只读单个字段避免 clone 整个读模型。
    pub async fn current_system_prompt(&self) -> Result<Option<String>, SessionError> {
        Ok(self.store.session_system_prompt(&self.id).await?)
    }

    /// 返回最新 durable cursor。
    pub async fn latest_cursor(&self) -> Result<Option<Cursor>, SessionError> {
        Ok(self.store.latest_cursor(&self.id).await?)
    }

    /// 为当前 projection cursor 写入恢复 checkpoint。
    pub async fn checkpoint(&self, cursor: &Cursor) -> Result<(), SessionError> {
        Ok(self.store.checkpoint(&self.id, cursor).await?)
    }

    // ─── Artifact 操作 ─────────────────────────────────────────────────

    /// 写入 compact 前 transcript snapshot。
    pub async fn write_compact_snapshot(
        &self,
        snapshot: CompactSnapshotInput,
    ) -> Result<Option<String>, SessionError> {
        Ok(self
            .store
            .write_compact_snapshot(&self.id, snapshot)
            .await?)
    }

    /// 写入大工具结果 artifact。
    pub async fn write_tool_artifact(
        &self,
        artifact: ToolResultArtifactInput,
    ) -> Result<ToolResultArtifactRef, SessionError> {
        Ok(self
            .store
            .write_tool_result_artifact(&self.id, artifact)
            .await?)
    }

    // ─── 运行时刷新 ────────────────────────────────────────────────────

    /// 重建本 session 的工具表快照并写入 runtime。
    ///
    /// `Session` 总是带 runtime（由构造函数参数强制），所以本函数不会 panic。
    /// 调用时机：新建/恢复 session、扩展加载状态变化、运行时检测到 `tool_registry`
    /// 为空（首次 submit / resume）。
    pub async fn refresh_tools(
        &self,
        working_dir: &str,
    ) -> Arc<astrcode_tools::registry::ToolRegistry> {
        let caps = &self.caps;
        let runtime = &self.runtime;
        let timeout = caps.read_effective().agent.shell_timeout_secs;
        let tool_policy = runtime.tool_policy();
        let registry = crate::session_setup::build_tool_registry_snapshot(
            caps.extension_runner(),
            working_dir,
            timeout,
            tool_policy.as_ref(),
        )
        .await;
        let registry = Arc::new(registry);
        runtime.set_tool_registry(Arc::clone(&registry));
        registry
    }

    /// 重建本 session 的 system prompt，fingerprint 未变时跳过。
    ///
    /// 调用方必须先调用过 `refresh_tools` 或确认 runtime 已有工具表。
    /// 事件通过 `Session::emit` 写入：写 store + fanout 到 runtime 广播；
    /// 已 attach 的 ServerEventBus forwarder 会接续转发到客户端。
    ///
    /// `extra_system_prompt` 语义：
    /// - `Some(s)`：使用 s（空串视为清空）；调用方明确指定。
    /// - `None`：**保留当前** — 优先 runtime 内的 extra，其次 projection 中的 extra。 这样 handler
    ///   在不知道 session 是不是子会话的情况下传 `None`，不会误把 子会话的 extra prompt 抹成空。
    ///
    /// 返回 `true` 表示真的写了新 `SystemPromptConfigured` 事件，`false` 表示
    /// fingerprint 命中跳过。
    pub async fn refresh_prompt(
        &self,
        working_dir: &str,
        extra_system_prompt: Option<&str>,
        stored_fingerprint: Option<&str>,
    ) -> Result<bool, SessionError> {
        let model_id = self.runtime.model_id();
        self.refresh_prompt_with_state(
            working_dir,
            extra_system_prompt,
            stored_fingerprint,
            None,
            &model_id,
        )
        .await
    }

    /// 初始化当前 session 的运行时工具快照与 system prompt。
    pub async fn initialize_runtime(&self, working_dir: &str) -> Result<(), SessionError> {
        self.refresh_tools(working_dir).await;
        self.refresh_prompt(working_dir, None, None).await?;
        Ok(())
    }

    /// 确保恢复后的 session 具备运行 turn 所需的工具快照与 system prompt。
    pub async fn ensure_runtime_ready(&self) -> Result<(), SessionError> {
        let state = self.read_model().await?;
        if self.runtime.tool_registry().list_definitions().is_empty() {
            self.refresh_tools(&state.working_dir).await;
        }
        if state.system_prompt.is_none() {
            self.refresh_prompt(&state.working_dir, None, None).await?;
        }
        Ok(())
    }

    /// 解析调用方指定或已有的 extra prompt。
    ///
    /// `extra=None` 且 runtime/cached_state 都没有值时必须从 store 读取并传播错误；
    /// 否则一次存储抖动可能被误记成「extra 已清空」。
    async fn resolve_extra_system_prompt(
        &self,
        extra_system_prompt: Option<&str>,
        cached_state: Option<&SessionReadModel>,
    ) -> Result<Option<String>, SessionError> {
        if extra_system_prompt.is_some() {
            return Ok(normalize_extra_system_prompt(extra_system_prompt));
        }
        if let Some(extra) = self.runtime.extra_system_prompt() {
            return Ok(Some(extra));
        }
        Ok(match cached_state {
            Some(state) => state.extra_system_prompt.clone(),
            None => self.read_model().await?.extra_system_prompt,
        })
    }

    async fn build_cached_system_prompt(
        &self,
        working_dir: &str,
        model_id: &str,
        resolved_extra: Option<&str>,
    ) -> Result<(String, String), SessionError> {
        let prompt_files =
            astrcode_context::prompt_engine::load_system_prompt_files(working_dir).await;
        let tools_with_meta = self
            .runtime
            .tool_registry()
            .list_definitions_with_prompt_metadata();
        let tools: Vec<_> = tools_with_meta.iter().map(|(def, _)| def.clone()).collect();
        let tool_prompt_metadata = tools_with_meta
            .into_iter()
            .filter_map(|(def, meta)| meta.map(|m| (def.name, m)))
            .collect();
        let ext_data = crate::session_setup::collect_extension_prompt_data(
            self.caps.extension_runner(),
            self.id.as_str(),
            working_dir,
            model_id,
            &tools,
            tool_prompt_metadata,
        )
        .await
        .map_err(|e| SessionError::Other(format!("collect extension data: {e}")))?;
        let prompt_input = SystemPromptInput {
            working_dir: working_dir.to_string(),
            os: std::env::consts::OS.into(),
            shell: resolve_shell().name,
            identity: prompt_files.identity,
            user_rules: prompt_files.user_rules,
            project_rules: prompt_files.project_rules,
            tools,
            tool_prompt_metadata: ext_data.merged_tool_metadata,
            extension_blocks: ext_data.extension_blocks,
            extra_instructions: resolved_extra.map(str::to_string),
        };
        let stable_fingerprint =
            astrcode_context::prompt_engine::compute_stable_fingerprint(&prompt_input);

        let (text, fingerprint) = match self.runtime.cached_stable_prefix() {
            Some((cached_text, cached_fingerprint)) if cached_fingerprint == stable_fingerprint => {
                let dynamic = astrcode_context::prompt_engine::build_dynamic_suffix(&prompt_input);
                let text = if dynamic.is_empty() {
                    cached_text
                } else {
                    format!("{}\n\n{}", cached_text.trim(), dynamic.trim())
                };
                let fingerprint = hex_fingerprint(text.as_bytes());
                (text, fingerprint)
            },
            _ => {
                let text = astrcode_context::prompt_engine::build_system_prompt(&prompt_input);
                let fingerprint = hex_fingerprint(text.as_bytes());
                let stable_prefix =
                    astrcode_context::prompt_engine::build_stable_prefix(&prompt_input);
                self.runtime
                    .set_cached_stable_prefix(stable_prefix, stable_fingerprint);
                (text, fingerprint)
            },
        };
        Ok((text, fingerprint))
    }

    /// `refresh_prompt` 的内部版本；调用方可复用已读取的状态和模型绑定，
    /// 避免 turn 初始化期间重新观察到另一套配置。
    pub(crate) async fn refresh_prompt_with_state(
        &self,
        working_dir: &str,
        extra_system_prompt: Option<&str>,
        stored_fingerprint: Option<&str>,
        cached_state: Option<&SessionReadModel>,
        model_id: &str,
    ) -> Result<bool, SessionError> {
        let resolved_extra = self
            .resolve_extra_system_prompt(extra_system_prompt, cached_state)
            .await?;
        let (text, fingerprint) = self
            .build_cached_system_prompt(working_dir, model_id, resolved_extra.as_deref())
            .await?;

        if stored_fingerprint == Some(fingerprint.as_str()) {
            self.runtime.set_extra_system_prompt(resolved_extra);
            return Ok(false);
        }

        self.runtime.set_extra_system_prompt(resolved_extra.clone());
        self.emit_durable(
            None,
            system_prompt_configured_payload(text, fingerprint, resolved_extra),
        )
        .await?;
        Ok(true)
    }

    // ─── Turn 入口 ────────────────────────────────────────────────────

    async fn emit_turn_start_events(&self, text: &str, turn_id: &TurnId) {
        self.emit_durable(Some(turn_id), EventPayload::TurnStarted)
            .await
            .ok();
        self.emit_durable(
            Some(turn_id),
            EventPayload::UserMessage {
                message_id: new_message_id(),
                text: text.to_string(),
            },
        )
        .await
        .ok();
        self.emit_live(Some(turn_id), EventPayload::AgentRunStarted)
            .await;
    }

    async fn prepare_turn_runner(&self) -> Result<TurnRunner, TurnError> {
        let model = self.runtime.model_binding();
        if let Err(e) = self.update_model_id(model.model_id()).await {
            tracing::warn!(session_id = %self.id, error = %e, "failed to update session model_id");
        }

        let pre_state = self
            .read_model()
            .await
            .map_err(|e| TurnError::Internal(format!("read session: {e}")))?;
        let working_dir = pre_state.working_dir.clone();

        if self.runtime.tool_registry().list_definitions().is_empty() {
            self.refresh_tools(&working_dir).await;
        }

        let stored_fingerprint = pre_state.system_prompt_fingerprint.clone();
        let prompt_changed = match self
            .refresh_prompt_with_state(
                &working_dir,
                None,
                stored_fingerprint.as_deref(),
                Some(&pre_state),
                model.model_id(),
            )
            .await
        {
            Ok(changed) => changed,
            Err(e) => {
                tracing::warn!(session_id = %self.id, error = %e, "configure system prompt failed");
                false
            },
        };

        let (background_result_tx, background_result_rx) =
            mpsc::unbounded_channel::<BackgroundTaskCompletion>();
        let bg_session = Arc::new(self.clone());
        let _forwarder = spawn_background_forwarder(background_result_rx, bg_session);

        let session_state = if prompt_changed {
            self.read_model()
                .await
                .map_err(|e| TurnError::Internal(format!("re-read session: {e}")))?
        } else {
            pre_state
        };
        let session_store_dir = self.session_store_dir().await;
        TurnRunner::new_with_llm(
            Arc::new(self.clone()),
            &session_state,
            Some(background_result_tx),
            session_store_dir,
            Arc::clone(model.llm()),
        )
    }

    async fn run_and_finalize_turn(
        session: Arc<Self>,
        mut agent: TurnRunner,
        text: String,
        turn_id: TurnId,
        completion_tx: tokio::sync::oneshot::Sender<RunTurnResult>,
    ) {
        let result = run_turn(&mut agent, &text, &turn_id).await;
        let finish_reason = match &result.output {
            Ok(out) => out.finish_reason.clone(),
            Err(_) => "error".into(),
        };
        let pending_error = match (&result.output, result.emitted_error) {
            (Err(e), false) => Some(e.to_string()),
            _ => None,
        };

        // 先交付执行结果，再落终态事件；被 abort 时由 abort 路径写入终态。
        let _ = completion_tx.send(result);
        if let Some(error_msg) = pending_error {
            let _ = session
                .emit_durable(
                    Some(&turn_id),
                    EventPayload::ErrorOccurred {
                        code: -32603,
                        message: error_msg,
                        recoverable: false,
                    },
                )
                .await;
        }
        let _ = session
            .emit_durable(
                Some(&turn_id),
                EventPayload::TurnCompleted {
                    finish_reason: finish_reason.clone(),
                },
            )
            .await;
        session
            .emit_live(
                Some(&turn_id),
                EventPayload::AgentRunCompleted {
                    reason: finish_reason,
                },
            )
            .await;
    }

    /// 提交用户输入开始一轮 turn，返回运行句柄。
    ///
    /// 内部完成：准备工具表、同步 system prompt、
    /// 装配 `TurnRunner`、起后台任务监听后台工具结果，最后 spawn agent task。
    ///
    /// 事件通过 store + runtime 广播分发；订阅者通过 `Session::subscribe` 或
    /// `ServerEventBus::attach` 接收。
    ///
    /// 调用方负责持有 `TurnHandle` 直到完成或主动 abort；handle 析构会让 task 自然继续。
    pub async fn submit(&self, text: String, turn_id: TurnId) -> Result<TurnHandle, TurnError> {
        self.emit_turn_start_events(&text, &turn_id).await;
        let agent = self.prepare_turn_runner().await?;
        let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
        let turn_id_for_task = turn_id.clone();
        let session_for_completion = Arc::new(self.clone());
        let join = tokio::spawn(async move {
            Self::run_and_finalize_turn(
                session_for_completion,
                agent,
                text,
                turn_id_for_task,
                completion_tx,
            )
            .await;
        });

        Ok(TurnHandle::new(turn_id, join, completion_rx))
    }

    // ─── 子会话 ────────────────────────────────────────────────────────

    /// 派生子会话。
    ///
    /// 共享父 session 的 store / caps，独立的 runtime（独立工具表/file_obs/bg_tasks）。
    /// 父侧记录 `AgentSessionSpawned` 事件，子侧的 `extra_system_prompt` / `tool_policy`
    /// 注入子 runtime，在 `submit` 时被 `refresh_prompt` / `refresh_tools` 读取。
    ///
    /// 调用方拿到 child Session 后通常立刻调 `child.submit(...)` 启动 turn。
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_child(
        &self,
        working_dir: &str,
        model_id: &str,
        agent_name: String,
        task: String,
        extra_system_prompt: Option<String>,
        tool_policy: Option<ChildToolPolicy>,
        source_extension: Option<&str>,
        tool_call_id: ToolCallId,
    ) -> Result<Session, SessionError> {
        let child_runtime = Arc::new(SessionRuntimeState::new(
            self.caps.llm(),
            self.caps.small_llm(),
            model_id.to_string(),
        ));
        if extra_system_prompt.is_some() {
            child_runtime.set_extra_system_prompt(extra_system_prompt);
        }
        let parent_working_dir = self.read_model().await?.working_dir;
        let parent_registry = self.runtime.tool_registry();
        if parent_working_dir == working_dir && !parent_registry.list_definitions().is_empty() {
            let child_registry = parent_registry.clone_with_child_policy(tool_policy.as_ref());
            child_runtime.set_tool_registry(Arc::new(child_registry));
        }
        let child_sid = new_session_id();
        let child = Session::create_with_id(
            Arc::clone(&self.store),
            child_sid.clone(),
            working_dir,
            model_id,
            Some(&self.id),
            tool_policy.as_ref(),
            source_extension,
            child_runtime,
            Arc::clone(&self.caps),
        )
        .await?;

        self.append_event(Event::new(
            self.id.clone(),
            None,
            EventPayload::AgentSessionSpawned {
                child_session_id: child_sid,
                agent_name,
                task,
                tool_policy,
                tool_call_id,
            },
        ))
        .await?;
        Ok(child)
    }

    /// 消费已完成子 turn 的信号并返回已完成的 guards。
    ///
    /// 终态事件已由 `ChildTurnGuard` 后台任务写入，本方法只负责收集
    /// 并移除已完成的 guard。返回的 guards 供 server 层处理回收和通知。
    pub fn drain_completed_guards(&self) -> Vec<Arc<ChildTurnGuard>> {
        self.runtime.drain_completed()
    }
}

#[async_trait::async_trait]
impl ToolResultArtifactReader for Session {
    async fn read_tool_result_artifact_by_path(
        &self,
        _session_id: &SessionId,
        path: &str,
        char_offset: usize,
        max_chars: usize,
    ) -> Result<ToolResultArtifactSlice, StorageError> {
        self.store
            .read_tool_result_artifact_by_path(&self.id, path, char_offset, max_chars)
            .await
    }
}

// ─── Compact ────────────────────────────────────────────────────────

impl Session {
    #[allow(clippy::too_many_arguments)]
    pub async fn append_compact_boundary(
        &self,
        system_prompt: String,
        fingerprint: String,
        extra_system_prompt: Option<String>,
        trigger_name: String,
        compaction: astrcode_context::compaction::CompactResult,
        base_event_seq: u64,
        strategy: astrcode_core::extension::CompactStrategy,
    ) -> Result<Vec<Event>, SessionError> {
        let cursor = self.latest_cursor().await?.unwrap_or_else(|| "0".into());
        let extra_system_prompt = normalize_extra_system_prompt(extra_system_prompt.as_deref());
        let mut events = Vec::with_capacity(3);
        events.push(
            self.append_event(Event::new(
                self.id.clone(),
                None,
                compact_boundary_payload(
                    trigger_name,
                    &compaction,
                    self.id.clone(),
                    base_event_seq,
                    strategy,
                ),
            ))
            .await?,
        );
        events.push(
            self.append_event(Event::new(
                self.id.clone(),
                None,
                system_prompt_configured_payload(system_prompt, fingerprint, extra_system_prompt),
            ))
            .await?,
        );
        events.push(
            self.append_event(Event::new(
                self.id.clone(),
                None,
                session_continued_from_compaction_payload(self.id.clone(), cursor, &compaction),
            ))
            .await?,
        );
        // compact 后 system prompt 全量重建，清空稳定前缀缓存确保下一 turn 使用新值。
        self.runtime.invalidate_stable_prefix_cache();
        if let Some(cursor) = self.latest_cursor().await? {
            self.checkpoint(&cursor).await?;
        }
        Ok(events)
    }
} // ─── SessionError ───────────────────────────────────────────────────────

/// 会话操作中可能出现的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("Extension error: {0}")]
    Extension(#[from] astrcode_core::extension::ExtensionError),
    #[error("{0}")]
    Other(String),
}
