//! Session 句柄 — 带存储能力的会话操作入口。

use std::sync::Arc;

use astrcode_core::{
    event::{Event, EventPayload},
    extension::{ChildToolPolicy, ExtensionEvent},
    llm::LlmMessage,
    prompt::SystemPromptInput,
    storage::{
        CompactSnapshotInput, EventStore, SessionReadModel, StorageError, ToolResultArtifactInput,
        ToolResultArtifactReader, ToolResultArtifactRef, ToolResultArtifactSlice,
    },
    types::*,
};
use astrcode_kernel::ToolRegistry;
use astrcode_support::{hash::hex_fingerprint, perf_snapshot, shell::resolve_shell};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::{
    payload::{
        TURN_FINISH_ABORTED, agent_run_completed_payload, compact_boundary_payload,
        session_continued_from_compaction_payload, system_prompt_configured_payload,
        turn_completed_payload,
    },
    session_runtime::SessionRuntimeState,
    session_runtime_services::SessionRuntimeServices,
    tool_exec::interrupted_tool_result,
    turn_context::{SharedTurnContext, TurnError},
    turn_handle::TurnHandle,
    turn_runner::{RunTurnResult, TurnLoop, run_turn},
};

// ── Session struct & lifecycle ──

/// 创建 session 所需的参数集合。
#[derive(Clone)]
pub struct SessionCreateParams {
    pub store: Arc<dyn EventStore>,
    pub sid: SessionId,
    pub working_dir: String,
    pub model_id: String,
    pub parent: Option<SessionId>,
    pub tool_policy: Option<ChildToolPolicy>,
    pub source_extension: Option<String>,
    pub runtime: Arc<SessionRuntimeState>,
    pub caps: Arc<SessionRuntimeServices>,
}

/// 会话句柄 — 带存储能力的会话操作入口。
///
/// 字段语义：
/// - `runtime`：进程内瞬态资源（工具表、file_obs、event_tx）。broadcast 在 runtime 上而不是 Session
///   上：同 sid 多次 `Session::open` / `clone` 仍共享同一个
///   broadcast，订阅者一处订阅就能看到所有实例上发出的事件。
/// - `caps`：跨 session 共享的基础设施（LLM、扩展、上下文组装器、配置）。
///
/// `Clone` 是廉价的 Arc clone，可以自由复制。
#[derive(Clone)]
pub struct Session {
    pub(crate) id: SessionId,
    pub(crate) store: Arc<dyn EventStore>,
    pub(crate) runtime: Arc<SessionRuntimeState>,
    pub(crate) caps: Arc<SessionRuntimeServices>,
}

impl Session {
    /// 用调用方指定的 sid 创建会话。
    ///
    /// **注意**：`runtime` 必须由调用方保证「同 sid 唯一」，否则同 sid 的不同 Session
    /// 实例会有不同的 broadcast、不同的工具表、不同的 event_tx，订阅者只能看到自己那份
    /// 实例上发出的事件。生产路径走 `SessionManager`，由其内部的 `runtime_states` HashMap
    /// 保证唯一；CLI / 测试若直接调本入口须自行维护一份 sid→runtime 映射，或接受隔离语义。
    pub async fn create_with_params(params: SessionCreateParams) -> Result<Self, SessionError> {
        params
            .store
            .create_session(
                &params.sid,
                &params.working_dir,
                &params.model_id,
                params.parent.as_ref(),
                params.tool_policy.as_ref(),
                params.source_extension.as_deref(),
            )
            .await?;
        if let Some(policy) = &params.tool_policy {
            params.runtime.apply_child_tool_policy(Some(policy.clone()));
        }
        Ok(Self {
            id: params.sid,
            store: params.store,
            runtime: params.runtime,
            caps: params.caps,
        })
    }

    /// 用调用方指定的 sid 创建会话（参数展开版，兼容旧调用点）。
    #[allow(clippy::too_many_arguments)]
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
        Self::create_with_params(SessionCreateParams {
            store,
            sid,
            working_dir: working_dir.to_string(),
            model_id: model_id.to_string(),
            parent: parent.cloned(),
            tool_policy: tool_policy.cloned(),
            source_extension: source_extension.map(str::to_string),
            runtime,
            caps,
        })
        .await
    }

    /// 从磁盘恢复已有会话并附带运行时/能力/事件广播。
    pub async fn open(
        store: Arc<dyn EventStore>,
        id: SessionId,
        runtime: Arc<SessionRuntimeState>,
        caps: Arc<SessionRuntimeServices>,
    ) -> Result<Self, SessionError> {
        store.open_session(&id).await?;
        if runtime.child_tool_policy().is_none() {
            let model = store.session_read_model(&id).await?;
            if let Some(policy) = model.tool_policy {
                runtime.apply_child_tool_policy(Some(policy));
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

    pub fn runtime(&self) -> &SessionRuntimeState {
        &self.runtime
    }

    pub fn runtime_arc(&self) -> Arc<SessionRuntimeState> {
        Arc::clone(&self.runtime)
    }

    pub fn caps(&self) -> &SessionRuntimeServices {
        &self.caps
    }

    pub fn caps_arc(&self) -> Arc<SessionRuntimeServices> {
        Arc::clone(&self.caps)
    }

    pub async fn session_store_dir(&self) -> Option<std::path::PathBuf> {
        self.store.session_store_dir(&self.id).await.ok().flatten()
    }

    pub fn subscribe(&self) -> tokio::sync::mpsc::Receiver<Arc<Event>> {
        self.runtime.subscribe()
    }

    pub(crate) fn resolve_shell_name() -> String {
        resolve_shell().name
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

/// 会话操作中可能出现的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("Extension error: {0}")]
    Extension(#[from] astrcode_core::extension::ExtensionError),
    #[error("invalid session cursor (expected u64 event seq): {0}")]
    InvalidCursor(Cursor),
}

// ── Storage operations ──

impl Session {
    pub async fn read_model(&self) -> Result<SessionReadModel, SessionError> {
        Ok(self.store.session_read_model(&self.id).await?)
    }

    pub async fn provider_messages(&self) -> Result<Vec<LlmMessage>, SessionError> {
        Ok(self.store.session_provider_messages(&self.id).await?)
    }

    pub async fn current_system_prompt(&self) -> Result<Option<String>, SessionError> {
        Ok(self.store.session_system_prompt(&self.id).await?)
    }

    pub async fn visible_user_message_count(&self) -> Result<usize, SessionError> {
        Ok(self
            .store
            .session_visible_user_message_count(&self.id)
            .await?)
    }

    pub async fn latest_cursor(&self) -> Result<Option<Cursor>, SessionError> {
        Ok(self.store.latest_cursor(&self.id).await?)
    }

    pub async fn checkpoint(&self, cursor: &Cursor) -> Result<(), SessionError> {
        Ok(self.store.checkpoint(&self.id, cursor).await?)
    }

    pub async fn write_compact_snapshot(
        &self,
        snapshot: CompactSnapshotInput,
    ) -> Result<Option<String>, SessionError> {
        Ok(self
            .store
            .write_compact_snapshot(&self.id, snapshot)
            .await?)
    }

    pub async fn write_tool_artifact(
        &self,
        artifact: ToolResultArtifactInput,
    ) -> Result<ToolResultArtifactRef, SessionError> {
        Ok(self
            .store
            .write_tool_result_artifact(&self.id, artifact)
            .await?)
    }
}

/// 将持久化 cursor 解析为 compaction 基线 event seq。
///
/// 无 cursor 时返回 0（新 session）。cursor 存在但非 u64 时返回 [`SessionError::InvalidCursor`]。
pub(crate) fn parse_base_event_seq(cursor: Option<Cursor>) -> Result<u64, SessionError> {
    match cursor {
        None => Ok(0),
        Some(cursor) => cursor
            .parse::<u64>()
            .map_err(|_| SessionError::InvalidCursor(cursor)),
    }
}

// ── Event emission ──

impl Session {
    pub async fn append_event(&self, event: Event) -> Result<Event, SessionError> {
        let stored = self.store.append_event(event).await?;
        self.runtime.fanout(stored.clone());
        perf_snapshot::capture_event("session.append_event", &stored);
        Ok(stored)
    }

    pub async fn emit_live(&self, turn_id: Option<&TurnId>, payload: EventPayload) {
        let event = Event::new(self.id.clone(), turn_id.cloned(), payload);
        perf_snapshot::capture_event("session.emit_live", &event);
        self.runtime.fanout(event);
    }

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

    pub async fn emit_lifecycle(&self, event: ExtensionEvent) -> Result<(), SessionError> {
        let model = self.read_model().await?;
        emit_lifecycle_for_read_model(&self.caps, &self.id, &model, event).await
    }

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
}

/// 发射 session 生命周期事件，不要求构造完整 [`Session`]。
pub async fn emit_lifecycle_for_read_model(
    caps: &SessionRuntimeServices,
    session_id: &SessionId,
    model: &SessionReadModel,
    event: ExtensionEvent,
) -> Result<(), SessionError> {
    let ctx = SharedTurnContext::from_read_model(session_id, model).lifecycle_ctx();
    caps.extension_runner().emit_lifecycle(event, ctx).await?;
    Ok(())
}

// ── Tool & runtime init ──

impl Session {
    pub async fn refresh_tools(&self, working_dir: &str) -> Arc<ToolRegistry> {
        let tool_policy = self.runtime.child_tool_policy();
        let registry = crate::session_setup::build_tool_registry_snapshot(
            self.caps.extension_runner(),
            self.caps.tool_packs(),
            working_dir,
            tool_policy.as_ref(),
        )
        .await;
        let registry = Arc::new(registry);
        self.runtime.install_tool_registry(Arc::clone(&registry));
        registry
    }

    pub async fn initialize_runtime(&self, working_dir: &str) -> Result<(), SessionError> {
        self.refresh_tools(working_dir).await;
        self.refresh_prompt(working_dir, None, None).await?;
        Ok(())
    }

    pub async fn ensure_runtime_ready(&self) -> Result<(), SessionError> {
        let state = self.read_model().await?;
        if self
            .runtime
            .loaded_tool_registry()
            .list_definitions()
            .is_empty()
        {
            self.refresh_tools(&state.working_dir).await;
        }
        if state.system_prompt.is_none() {
            self.refresh_prompt(&state.working_dir, None, None).await?;
        }
        Ok(())
    }
}

// ── System prompt ──

pub(crate) fn normalize_extra_system_prompt(extra_system_prompt: Option<&str>) -> Option<String> {
    extra_system_prompt.and_then(|prompt| {
        let trimmed = prompt.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

impl Session {
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
        let is_subagent = match cached_state {
            Some(state) => state.parent_session_id.is_some(),
            None => self.read_model().await?.parent_session_id.is_some(),
        };
        let (text, fingerprint) = self
            .build_cached_system_prompt(
                working_dir,
                model_id,
                resolved_extra.as_deref(),
                is_subagent,
            )
            .await?;

        if stored_fingerprint == Some(fingerprint.as_str()) {
            self.runtime.update_prompt_extra(resolved_extra);
            return Ok(false);
        }

        self.runtime.update_prompt_extra(resolved_extra.clone());
        self.emit_durable(
            None,
            system_prompt_configured_payload(text, fingerprint, resolved_extra),
        )
        .await?;
        Ok(true)
    }

    async fn resolve_extra_system_prompt(
        &self,
        extra_system_prompt: Option<&str>,
        cached_state: Option<&SessionReadModel>,
    ) -> Result<Option<String>, SessionError> {
        if extra_system_prompt.is_some() {
            return Ok(normalize_extra_system_prompt(extra_system_prompt));
        }
        if let Some(extra) = self.runtime.prompt_extra() {
            return Ok(Some(extra));
        }
        Ok(match cached_state {
            Some(state) => state.extra_system_prompt.clone(),
            None => self.read_model().await?.extra_system_prompt,
        })
    }

    pub(crate) async fn build_cached_system_prompt(
        &self,
        working_dir: &str,
        model_id: &str,
        resolved_extra: Option<&str>,
        is_subagent: bool,
    ) -> Result<(String, String), SessionError> {
        let prompt_files = self
            .caps
            .prompt_file_provider()
            .load(working_dir, !is_subagent)
            .await;
        let tools_with_meta = self
            .runtime
            .loaded_tool_registry()
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
        .await?;
        let prompt_input = SystemPromptInput {
            working_dir: working_dir.to_string(),
            os: std::env::consts::OS.into(),
            shell: Self::resolve_shell_name(),
            gh_cli_available: astrcode_support::shell::is_gh_cli_available(),
            identity: prompt_files.identity,
            user_rules: prompt_files.user_rules,
            project_rules: prompt_files.project_rules,
            tools,
            tool_prompt_metadata: ext_data.merged_tool_metadata,
            extension_blocks: ext_data.extension_blocks,
            extra_instructions: resolved_extra.map(str::to_string),
        };
        let text = self
            .caps
            .prompt_provider()
            .assemble(prompt_input)
            .await
            .system_prompt
            .unwrap_or_default();
        let fingerprint = hex_fingerprint(text.as_bytes());
        Ok((text, fingerprint))
    }
}

// ── Compact boundary ──

impl Session {
    /// 在同一条 session log 上追加 compact 边界（**不**分配新 `session_id`）。
    ///
    /// `continued_session_id` 与 `SessionContinuedFromCompaction.parent_session_id` 均为
    /// `self.id`。子 agent 与主 session 共用此路径；勿假设 compact 会产生 leaf session。
    #[allow(clippy::too_many_arguments)]
    pub async fn append_compact_boundary(
        &self,
        system_prompt: String,
        fingerprint: String,
        extra_system_prompt: Option<String>,
        trigger_name: String,
        compaction: astrcode_core::context::CompactResult,
        base_event_seq: u64,
        strategy: astrcode_core::extension::CompactStrategy,
    ) -> Result<Vec<Event>, SessionError> {
        // compact 语义：冻结 base_event_seq 之前的历史前缀。
        // 即使 compact 计算期间有新事件写入，也必须以 base_event_seq 作为边界标记，
        // 后续 replay 会将这些新事件归类为 tail delta 追加，不覆盖它们。
        let cursor = base_event_seq.to_string();
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
        if let Some(cursor) = self.latest_cursor().await? {
            self.checkpoint(&cursor).await?;
        }
        Ok(events)
    }
}

// ── Child session ──
// spawn_child 与 AgentSessionSpawned 事件。
// 完成等待、终态写入、回收与通知由 `astrcode-server::child_session` 编排。

impl Session {
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
    ) -> Result<Self, SessionError> {
        let primary_llm = primary_llm_for_model_id(&self.caps, model_id);
        let child_runtime = Arc::new(SessionRuntimeState::new(
            primary_llm,
            self.caps.small_llm(),
            model_id.to_string(),
        ));
        if extra_system_prompt.is_some() {
            child_runtime.update_prompt_extra(extra_system_prompt);
        }
        let parent_working_dir = self.read_model().await?.working_dir;
        let parent_registry = self.runtime.loaded_tool_registry();
        if parent_working_dir == working_dir && !parent_registry.list_definitions().is_empty() {
            let child_registry = parent_registry.clone_with_child_policy(tool_policy.as_ref());
            child_runtime.install_tool_registry(Arc::new(child_registry));
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

/// 子 session 的 turn 使用 `SessionModelBinding.llm`；当目标 model_id 为小模型时选用 small
/// provider。
fn primary_llm_for_model_id(
    caps: &SessionRuntimeServices,
    model_id: &str,
) -> Arc<dyn astrcode_core::llm::LlmProvider> {
    let effective = caps.read_effective();
    if model_id == effective.small_llm.model_id && model_id != effective.llm.model_id {
        caps.small_llm()
    } else {
        caps.llm()
    }
}

// ── Turn submission ──

impl Session {
    async fn emit_turn_start_events(
        &self,
        text: &str,
        attachments: &[astrcode_core::message_attachment::MessageAttachment],
        turn_id: &TurnId,
    ) -> Result<(), TurnError> {
        self.emit_durable(Some(turn_id), EventPayload::TurnStarted)
            .await?;
        self.emit_durable(
            Some(turn_id),
            EventPayload::UserMessage {
                message_id: new_message_id(),
                text: text.to_string(),
                attachments: attachments.to_vec(),
            },
        )
        .await?;
        self.emit_live(Some(turn_id), EventPayload::AgentRunStarted)
            .await;
        Ok(())
    }

    async fn apply_user_message_envelope(
        &self,
        text: String,
        attachments: &[astrcode_core::message_attachment::MessageAttachment],
        turn_id: &TurnId,
    ) -> Result<String, TurnError> {
        let state = self.read_model().await?;
        let original_text = text.clone();
        let ctx = astrcode_core::extension::UserMessageEnvelopeContext {
            session_id: self.id.to_string(),
            turn_id: turn_id.to_string(),
            working_dir: state.working_dir.clone(),
            model: astrcode_core::config::ModelSelection::simple(state.model_id),
            text,
            attachments: attachments.to_vec(),
            session_store_dir: self.session_store_dir().await,
        };
        match self
            .caps()
            .extension_runner_arc()
            .emit_user_message_envelope(ctx)
            .await?
        {
            astrcode_core::extension::UserMessageEnvelopeResult::Allow => Ok(original_text),
            astrcode_core::extension::UserMessageEnvelopeResult::ReplaceText { text } => Ok(text),
            astrcode_core::extension::UserMessageEnvelopeResult::AppendText { text } => {
                let mut combined = original_text;
                if !combined.is_empty() && !text.is_empty() {
                    combined.push_str("\n\n");
                }
                combined.push_str(&text);
                Ok(combined)
            },
            astrcode_core::extension::UserMessageEnvelopeResult::Block { reason } => {
                Err(TurnError::InputBlocked { reason })
            },
        }
    }

    async fn prepare_turn_runner(&self) -> Result<TurnLoop, TurnError> {
        let model = self.runtime.model_binding();
        if let Err(e) = self.update_model_id(model.model_id()).await {
            tracing::warn!(session_id = %self.id, error = %e, "failed to update session model_id");
        }

        let pre_state = self.read_model().await?;
        let working_dir = pre_state.working_dir.clone();

        if self
            .runtime
            .loaded_tool_registry()
            .list_definitions()
            .is_empty()
        {
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

        let session_state = if prompt_changed {
            // refresh_prompt 可能写入了 durable event，需重读 projection。
            self.read_model().await?
        } else {
            pre_state
        };
        let session_store_dir = self.session_store_dir().await;
        let cancellation_token = CancellationToken::new();
        TurnLoop::new_with_llm(
            self.clone(),
            &session_state,
            session_store_dir,
            Arc::clone(&model.llm),
            cancellation_token,
        )
    }

    async fn run_and_finalize_turn(
        session: Session,
        mut agent: TurnLoop,
        text: String,
        turn_id: TurnId,
        cancellation_token: CancellationToken,
        completion_tx: oneshot::Sender<RunTurnResult>,
    ) {
        let result = run_turn(&mut agent, &text, &turn_id).await;
        let finish_reason = match &result.output {
            Ok(out) => out.finish_reason.clone(),
            Err(TurnError::Aborted) => TURN_FINISH_ABORTED.into(),
            Err(_) => "error".into(),
        };
        let pending_error = match (&result.output, result.emitted_error) {
            (Err(TurnError::Aborted), _) => None,
            (Err(e), false) => Some(e.to_string()),
            _ => None,
        };
        let aborted = matches!(result.output, Err(TurnError::Aborted));

        if aborted {
            emit_aborted_turn_context(&session, &turn_id).await;
        }
        if let Some(error_msg) = pending_error {
            if let Err(e) = session
                .emit_durable(
                    Some(&turn_id),
                    EventPayload::ErrorOccurred {
                        code: -32603,
                        message: error_msg,
                        recoverable: false,
                    },
                )
                .await
            {
                tracing::error!(
                    session_id = %session.id(),
                    turn_id = %turn_id,
                    error = %e,
                    "CRITICAL: failed to persist ErrorOccurred; session may need stale repair on restart"
                );
            }
        }
        if let Err(e) = session
            .emit_durable(
                Some(&turn_id),
                turn_completed_payload(finish_reason.clone()),
            )
            .await
        {
            tracing::error!(
                session_id = %session.id(),
                turn_id = %turn_id,
                error = %e,
                "CRITICAL: failed to persist TurnCompleted; session may need stale repair on restart"
            );
        }
        session
            .emit_live(Some(&turn_id), agent_run_completed_payload(finish_reason))
            .await;
        cancellation_token.cancel();
        let _ = completion_tx.send(result);
    }

    pub async fn submit(
        &self,
        text: String,
        attachments: Vec<astrcode_core::message_attachment::MessageAttachment>,
        turn_id: TurnId,
    ) -> Result<TurnHandle, TurnError> {
        let text = self
            .apply_user_message_envelope(text, &attachments, &turn_id)
            .await?;
        self.emit_turn_start_events(&text, &attachments, &turn_id)
            .await?;
        let agent = self.prepare_turn_runner().await?;
        let cancellation_token = agent.cancellation_token();
        let (completion_tx, completion_rx) = oneshot::channel();
        let turn_id_for_task = turn_id.clone();
        let session_for_completion = self.clone();
        let cancellation_for_task = cancellation_token.clone();
        let join = tokio::spawn(async move {
            Self::run_and_finalize_turn(
                session_for_completion,
                agent,
                text,
                turn_id_for_task,
                cancellation_for_task,
                completion_tx,
            )
            .await;
        });

        Ok(TurnHandle::new(
            turn_id,
            join,
            cancellation_token,
            completion_rx,
        ))
    }
}

async fn emit_aborted_turn_context(session: &Session, turn_id: &TurnId) {
    match session.read_model().await {
        Ok(state) => {
            if let Err(e) = emit_interrupted_tool_results(session, &state, Some(turn_id)).await {
                tracing::warn!(
                    session_id = %session.id(),
                    turn_id = %turn_id,
                    error = %e,
                    "failed to settle pending tool calls after abort"
                );
            }
        },
        Err(e) => {
            tracing::warn!(
                session_id = %session.id(),
                turn_id = %turn_id,
                error = %e,
                "failed to read session state after abort"
            );
        },
    }

    if let Err(e) = emit_turn_aborted_context(session, Some(turn_id)).await {
        tracing::warn!(
            session_id = %session.id(),
            turn_id = %turn_id,
            error = %e,
            "failed to write turn-aborted provider context"
        );
    }
}

pub async fn emit_interrupted_tool_results(
    session: &Session,
    state: &SessionReadModel,
    turn_id: Option<&TurnId>,
) -> Result<usize, SessionError> {
    let mut emitted = 0;
    for pending in state.tool_calls_needing_interruption() {
        let result = interrupted_tool_result(
            pending.call_id.clone(),
            &pending.tool_name,
            std::time::Duration::ZERO,
        );
        session
            .emit_durable(
                turn_id,
                EventPayload::ToolCallCompleted {
                    call_id: pending.call_id.into(),
                    tool_name: pending.tool_name,
                    result,
                    arguments: String::new(),
                    arguments_json: None,
                },
            )
            .await?;
        emitted += 1;
    }
    Ok(emitted)
}

pub async fn emit_turn_aborted_context(
    session: &Session,
    turn_id: Option<&TurnId>,
) -> Result<(), SessionError> {
    session
        .emit_durable(turn_id, EventPayload::TurnAbortedContext)
        .await?;
    Ok(())
}
