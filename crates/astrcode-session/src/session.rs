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
    storage::{
        CompactSnapshotInput, EventStore, SessionReadModel, StorageError, ToolResultArtifactInput,
        ToolResultArtifactReader, ToolResultArtifactRef, ToolResultArtifactSlice,
    },
    types::*,
};
use tokio::sync::{broadcast, mpsc};

use crate::{
    payload::{compact_boundary_payload, session_continued_from_compaction_payload},
    session_runtime::SessionRuntimeState,
    session_runtime_services::SessionRuntimeServices,
};

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

    /// 订阅本 session 的事件广播。
    ///
    /// 同 sid 不同 Session 实例订阅的是同一份 broadcast（在 runtime 上），
    /// 因此一个订阅者能看到任何实例发出的事件。
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.runtime.subscribe()
    }

    // ─── 事件操作 ──────────────────────────────────────────────────────

    /// 追加持久事件到事件日志，分配递增序号。
    pub async fn append_event(&self, event: Event) -> Result<Event, SessionError> {
        let stored = self.store.append_event(event).await?;
        self.runtime.fanout(stored.clone());
        Ok(stored)
    }

    /// 发射只 fanout、不持久化的 live 事件。Infallible。
    pub async fn emit_live(&self, turn_id: Option<&TurnId>, payload: EventPayload) {
        let event = Event::new(self.id.clone(), turn_id.cloned(), payload);
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
        self.refresh_prompt_with_state(working_dir, extra_system_prompt, stored_fingerprint, None)
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

    /// `refresh_prompt` 的内部版本，调用方可传入已读取的 `SessionReadModel` 避免内部
    /// 在 `extra=None` 路径再读一次 projection。`Session::submit` 走这个入口。
    ///
    /// 错误处理：`extra=None` 且 runtime/cached_state 都没有值时需要从 store 拉
    /// projection；如果存储层报错，本函数 **必须** 把错误向上传，而不是把 extra
    /// 视为 None 继续——否则一次瞬时存储抖动会被记成「extra 真的没了」并写入新的
    /// `SystemPromptConfigured` 事件覆盖原值。
    pub(crate) async fn refresh_prompt_with_state(
        &self,
        working_dir: &str,
        extra_system_prompt: Option<&str>,
        stored_fingerprint: Option<&str>,
        cached_state: Option<&SessionReadModel>,
    ) -> Result<bool, SessionError> {
        let caps = &self.caps;
        let runtime = &self.runtime;

        // 入口规范化：trim 后空串视为 None。后续整段按 Option<String> 处理，
        // 避免「Some("") vs None」的语义漂移和重复规范化。
        let explicit_extra: Option<String> = extra_system_prompt.and_then(|s| {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        });

        // None 语义 = 保留：先看 runtime，再看 projection。Some(_) 直接采用。
        // 注意 explicit_extra 用 `is_some()` 区分而非 unwrap_or_else，因为调用方
        // 显式传 Some("") 表示「清空 extra」（已 trim 成 None），需走 None 分支
        // 之外的「显式」路径——但这里 trim 后两者等价，所以直接复用。
        let resolved_extra: Option<String> = if extra_system_prompt.is_some() {
            // 调用方显式指定（含 Some("") → None 表示清空）
            explicit_extra
        } else {
            match runtime.extra_system_prompt() {
                Some(s) => Some(s),
                None => match cached_state {
                    Some(state) => state.extra_system_prompt.clone(),
                    // 关键：read_model 错误必须传播，不能 unwrap_or_default 默默吞掉
                    None => self.read_model().await?.extra_system_prompt,
                },
            }
        };

        let model_id = caps.read_effective().llm.model_id.clone();
        let prompt_files =
            astrcode_context::prompt_engine::load_system_prompt_files(working_dir).await;
        let registry = runtime.tool_registry();
        let tools_with_meta = registry.list_definitions_with_prompt_metadata();
        let tools: Vec<_> = tools_with_meta.iter().map(|(def, _)| def.clone()).collect();
        let tool_prompt_metadata = tools_with_meta
            .into_iter()
            .filter_map(|(def, meta)| meta.map(|m| (def.name, m)))
            .collect();

        let (text, fingerprint) = crate::session_setup::build_system_prompt_snapshot(
            crate::session_setup::SystemPromptSnapshotInput {
                extension_runner: caps.extension_runner(),
                session_id: self.id.as_str(),
                working_dir,
                model_id: &model_id,
                tools: &tools,
                extra_system_prompt: resolved_extra.as_deref(),
                tool_prompt_metadata,
                prompt_files,
            },
        )
        .await
        .map_err(|e| SessionError::Other(format!("build system prompt: {e}")))?;

        if stored_fingerprint == Some(fingerprint.as_str()) {
            runtime.set_extra_system_prompt(resolved_extra);
            return Ok(false);
        }

        runtime.set_extra_system_prompt(resolved_extra.clone());
        self.emit_durable(
            None,
            EventPayload::SystemPromptConfigured {
                text,
                fingerprint,
                extra_system_prompt: resolved_extra,
            },
        )
        .await?;
        Ok(true)
    }

    // ─── Turn 入口 ────────────────────────────────────────────────────

    /// 提交用户输入开始一轮 turn，返回运行句柄。
    ///
    /// 内部完成：刷新工具表（如未填充）、刷新 system prompt（如缺失）、
    /// 装配 `TurnRunner`、起后台任务监听后台工具结果，最后 spawn agent task。
    ///
    /// 事件通过 store + runtime 广播分发；订阅者通过 `Session::subscribe` 或
    /// `ServerEventBus::attach` 接收。
    ///
    /// 调用方负责持有 `TurnHandle` 直到完成或主动 abort；handle 析构会让 task 自然继续。
    pub async fn submit(
        &self,
        text: String,
        turn_id: TurnId,
    ) -> Result<crate::turn_handle::TurnHandle, crate::turn_context::TurnError> {
        use crate::{
            background::{BackgroundTaskCompletion, spawn_background_forwarder},
            turn_context::TurnError,
            turn_handle::TurnHandle,
            turn_runner::{TurnRunner, run_turn},
        };

        // 刷新 model_id 与 session 当前值不同时写入事件
        let model_id = self.caps.read_effective().llm.model_id.clone();
        if let Err(e) = self.update_model_id(&model_id).await {
            tracing::warn!(session_id = %self.id, error = %e, "failed to update session model_id");
        }

        // 第一次读 state：取 working_dir 和 fingerprint。projection 缓存在 store 内，
        // 同 session 多次 read_model 是 O(1) HashMap 查询，不会触发 replay。
        let pre_state = self
            .read_model()
            .await
            .map_err(|e| TurnError::Internal(format!("read session: {e}")))?;
        let working_dir = pre_state.working_dir.clone();

        // 工具表：runtime 未填充时构建快照
        if self.runtime.tool_registry().list_definitions().is_empty() {
            self.refresh_tools(&working_dir).await;
        }

        // system prompt：fingerprint 命中时跳过。`None` extra 表示保留——refresh_prompt
        // 内部用 cached_state 解析当前值，避免再读一次 projection。
        let stored_fingerprint = pre_state.system_prompt_fingerprint.clone();
        let prompt_changed = match self
            .refresh_prompt_with_state(
                &working_dir,
                None,
                stored_fingerprint.as_deref(),
                Some(&pre_state),
            )
            .await
        {
            Ok(changed) => changed,
            Err(e) => {
                tracing::warn!(session_id = %self.id, error = %e, "configure system prompt failed");
                false
            },
        };

        // 后台工具结果泵：把 BackgroundTaskCompletion 翻译成事件写入 session
        // （store + runtime 广播）。
        // `spawn_background_forwarder` 内部已经 `tokio::spawn`，丢弃返回的 JoinHandle
        // 让其在 rx 关闭（所有 sender drop）时自然结束；panic 时 task 终止，下一次
        // background 完成会因 sender 端 mpsc 已关闭而走 None 分支退出，不会泄漏。
        let (background_result_tx, background_result_rx) =
            mpsc::unbounded_channel::<BackgroundTaskCompletion>();
        let bg_session = Arc::new(self.clone());
        // TODO: 更好的后台处理？
        let _forwarder = spawn_background_forwarder(background_result_rx, bg_session, None);

        // refresh_prompt 写过事件时 projection 已变，需要 re-read；命中 fingerprint
        // 跳过的情况下 pre_state 仍然反映最新状态。
        let session_state = if prompt_changed {
            self.read_model()
                .await
                .map_err(|e| TurnError::Internal(format!("re-read session: {e}")))?
        } else {
            pre_state
        };
        let mut agent = TurnRunner::new(
            Arc::new(self.clone()),
            &session_state,
            Some(background_result_tx),
        )?;

        let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
        let turn_id_for_task = turn_id.clone();
        let join = tokio::spawn(async move {
            let result = run_turn(&mut agent, &text, &turn_id_for_task).await;
            let _ = completion_tx.send(result);
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
        let child_runtime = Arc::new(SessionRuntimeState::default());
        if extra_system_prompt.is_some() {
            child_runtime.set_extra_system_prompt(extra_system_prompt);
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
    pub async fn append_compact_boundary(
        &self,
        system_prompt: String,
        fingerprint: String,
        extra_system_prompt: Option<String>,
        trigger_name: String,
        compaction: astrcode_context::compaction::CompactResult,
    ) -> Result<Vec<Event>, SessionError> {
        let cursor = self.latest_cursor().await?.unwrap_or_else(|| "0".into());
        let extra_system_prompt = extra_system_prompt.and_then(|s| {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        });
        let mut events = Vec::with_capacity(3);
        events.push(
            self.append_event(Event::new(
                self.id.clone(),
                None,
                compact_boundary_payload(trigger_name, &compaction, self.id.clone()),
            ))
            .await?,
        );
        events.push(
            self.append_event(Event::new(
                self.id.clone(),
                None,
                EventPayload::SystemPromptConfigured {
                    text: system_prompt,
                    fingerprint,
                    extra_system_prompt,
                },
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
