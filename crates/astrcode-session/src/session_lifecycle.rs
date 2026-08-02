//! Session lifecycle service — creation, open, and child session saga.

use std::{collections::HashSet, panic::AssertUnwindSafe, sync::Arc};

use astrcode_core::{
    event::{DurableEvent, DurableEventPayload, ParentSessionRef, SessionStarted},
    tool::SessionToolSelection,
    types::*,
};
use astrcode_extension_sdk::extension::ExtensionEvent;
use astrcode_session_projection::SessionReadModel;
use futures_util::FutureExt;

use crate::{
    payload::agent_session_failed_payload,
    session::{Session, SessionCreateParams},
    session_error::SessionError,
    session_runtime::SessionRuntimeState,
    session_runtime_services::SessionRuntimeServices,
    session_state::SessionStateSource,
};

impl Session {
    /// 使用 runtime 已绑定的 session id 创建会话。
    ///
    /// **注意**：`runtime` 必须由调用方保证「同 sid 唯一」，否则同 sid 的不同 Session
    /// 实例会有不同的工具缓存与审批状态。生产路径通过 [`crate::SessionResourceStore`]
    /// 保证唯一；直接调用本入口的测试或嵌入方需维持相同约束。
    pub async fn create_with_params(mut params: SessionCreateParams) -> Result<Self, SessionError> {
        let state_source = SessionStateSource::new(params.runtime.store().clone());
        params.tool_selection = resolve_initial_tool_selection(
            &state_source,
            params.parent_session_id.as_ref(),
            params.tool_selection.as_ref(),
        )
        .await?;
        Self::create_persisted(params, state_source).await
    }

    async fn create_persisted(
        params: SessionCreateParams,
        state_source: SessionStateSource,
    ) -> Result<Self, SessionError> {
        let session = Self {
            state_source,
            runtime: Arc::clone(&params.runtime),
            runtime_services: Arc::clone(&params.runtime_services),
        };
        let model_id = params.model_id;
        let initial_system_prompt = match params.initial_system_prompt {
            Some(prompt) => prompt,
            None => {
                session
                    .prepare_initial_system_prompt(
                        &params.working_dir,
                        &model_id,
                        params.parent_session_id.as_ref(),
                        params.tool_selection.as_ref(),
                        params.source_extension.as_deref(),
                        params.extra_system_prompt.as_deref(),
                    )
                    .await?
            },
        };
        let started = DurableEvent::session(
            session.id().clone(),
            DurableEventPayload::SessionStarted(SessionStarted {
                working_dir: params.working_dir,
                model_id,
                parent: params
                    .parent_session_id
                    .map(|session_id| ParentSessionRef { session_id }),
                tool_selection: params.tool_selection.unwrap_or_default(),
                source_extension: params.source_extension,
                initial_system_prompt,
            }),
        );
        session
            .runtime
            .event_sink()
            .create(session.runtime.store().clone(), started)
            .await?;
        Ok(session)
    }

    /// 从磁盘恢复已有会话并附带运行时服务和事件广播。
    pub async fn open(
        runtime: Arc<SessionRuntimeState>,
        runtime_services: Arc<SessionRuntimeServices>,
    ) -> Result<Self, SessionError> {
        runtime.store().open_session(runtime.session_id()).await?;
        Ok(Self {
            state_source: SessionStateSource::new(runtime.store().clone()),
            runtime,
            runtime_services,
        })
    }
}

async fn resolve_initial_tool_selection(
    state_source: &SessionStateSource,
    parent_session_id: Option<&SessionId>,
    requested: Option<&SessionToolSelection>,
) -> Result<Option<SessionToolSelection>, SessionError> {
    let Some(parent_session_id) = parent_session_id else {
        return Ok(SessionToolSelection::intersect(None, requested));
    };
    let parent = state_source.read_model(parent_session_id).await?;
    let parent_selection =
        resolve_effective_tool_selection(state_source, parent_session_id, &parent).await?;
    Ok(SessionToolSelection::intersect(
        parent_selection.as_ref(),
        requested,
    ))
}

pub(crate) async fn resolve_effective_tool_selection(
    state_source: &SessionStateSource,
    session_id: &SessionId,
    model: &SessionReadModel,
) -> Result<Option<SessionToolSelection>, SessionError> {
    let mut visited = HashSet::from([session_id.clone()]);
    let mut selection = Some(model.identity.tool_selection.clone());
    let mut parent_session_id = model
        .identity
        .parent
        .as_ref()
        .map(|parent| parent.session_id.clone());

    while let Some(parent_id) = parent_session_id {
        if !visited.insert(parent_id.clone()) {
            return Err(SessionError::ParentCycle {
                session_id: parent_id,
            });
        }
        let parent = state_source.read_model(&parent_id).await?;
        selection = SessionToolSelection::intersect(
            Some(&parent.identity.tool_selection),
            selection.as_ref(),
        );
        parent_session_id = parent
            .identity
            .parent
            .as_ref()
            .map(|parent| parent.session_id.clone());
    }

    Ok(selection)
}

/// 创建子会话所需的参数。
pub struct SpawnChildParams {
    pub working_dir: String,
    pub model_id: String,
    pub agent_name: String,
    pub task: String,
    pub extra_system_prompt: Option<String>,
    pub tool_selection: Option<SessionToolSelection>,
    pub source_extension: Option<String>,
    pub tool_call_id: ToolCallId,
}

impl Session {
    pub async fn spawn_child(&self, mut params: SpawnChildParams) -> Result<Self, SessionError> {
        params.tool_selection = resolve_initial_tool_selection(
            &self.state_source,
            Some(self.id()),
            params.tool_selection.as_ref(),
        )
        .await?;
        let child_session_id = new_session_id();
        let publication = self
            .runtime
            .event_sink()
            .defer_publication(child_session_id.clone())?;
        match AssertUnwindSafe(self.create_child_transaction(params, child_session_id.clone()))
            .catch_unwind()
            .await
        {
            Ok(Ok(child)) => {
                publication.commit();
                Ok(child)
            },
            Ok(Err(error)) => Err(error),
            Err(_) => {
                let cause = SessionError::CreationTask(
                    "child session creation transaction panicked".into(),
                );
                let compensation = AssertUnwindSafe(
                    self.compensate_panicked_child_creation(&child_session_id, &cause),
                )
                .catch_unwind()
                .await;
                if compensation.is_err() {
                    tracing::warn!(
                        parent_session_id = %self.id(),
                        child_session_id = %child_session_id,
                        "child creation panic compensation panicked"
                    );
                }
                Err(cause)
            },
        }
    }

    /// 子会话 runtime 在 shared resource store 中的唯一实例。
    fn child_runtime(&self, child_sid: &SessionId) -> Arc<SessionRuntimeState> {
        let child_store = self.runtime.store().clone();
        self.runtime_services
            .session_resources()
            .resources_for(child_sid, || {
                Arc::new(SessionRuntimeState::new_with_event_sink(
                    child_sid.clone(),
                    child_store,
                    self.runtime.event_sink_arc(),
                ))
            })
    }

    async fn create_child_transaction(
        &self,
        params: SpawnChildParams,
        child_sid: SessionId,
    ) -> Result<Self, SessionError> {
        let SpawnChildParams {
            working_dir,
            model_id,
            agent_name,
            task,
            extra_system_prompt,
            tool_selection,
            source_extension,
            tool_call_id,
        } = params;
        let child_runtime = self.child_runtime(&child_sid);
        let creation = child_runtime.begin_creation();
        let child = match Session::create_persisted(
            SessionCreateParams {
                working_dir,
                model_id,
                parent_session_id: Some(self.id().clone()),
                tool_selection: tool_selection.clone(),
                source_extension,
                extra_system_prompt,
                initial_system_prompt: None,
                runtime: Arc::clone(&child_runtime),
                runtime_services: Arc::clone(&self.runtime_services),
            },
            self.state_source.clone(),
        )
        .await
        {
            Ok(child) => child,
            Err(error) => {
                if matches!(&error, SessionError::EventPublish(_)) {
                    if let Err(compensation_error) =
                        self.discard_failed_child_runtime(&child_runtime).await
                    {
                        tracing::warn!(
                            parent_session_id = %self.id(),
                            child_session_id = %child_sid,
                            error = %error,
                            compensation_error = %compensation_error,
                            "failed to fully compensate child session creation"
                        );
                    }
                } else {
                    self.runtime_services
                        .session_resources()
                        .cleanup(&child_sid);
                }
                return Err(error);
            },
        };

        // 父链接是 child 进入父投影的持久可见边界；初始化失败时不能留下 Running 链接。
        if let Err(error) = child
            .ensure_lifecycle_initialized(ExtensionEvent::SessionStart)
            .await
        {
            self.compensate_failed_child_creation(&child, &child_sid, &error, false)
                .await;
            return Err(error);
        }

        if let Err(error) = child.sync_durable_events().await {
            self.compensate_failed_child_creation(&child, &child_sid, &error, false)
                .await;
            return Err(error);
        }

        if let Err(error) = self
            .emit_durable(
                None,
                DurableEventPayload::AgentSessionSpawned {
                    child_session_id: child_sid.clone(),
                    agent_name,
                    task,
                    tool_selection,
                    tool_call_id,
                },
            )
            .await
        {
            self.compensate_failed_child_creation(&child, &child_sid, &error, false)
                .await;
            return Err(error);
        }
        if let Err(error) = self.sync_durable_events().await {
            self.compensate_failed_child_creation(&child, &child_sid, &error, true)
                .await;
            return Err(error);
        }
        creation.commit();
        Ok(child)
    }

    async fn compensate_panicked_child_creation(
        &self,
        child_session_id: &SessionId,
        cause: &SessionError,
    ) {
        let child_runtime = self.child_runtime(child_session_id);
        let child = match Session::open(
            Arc::clone(&child_runtime),
            Arc::clone(&self.runtime_services),
        )
        .await
        {
            Ok(child) => child,
            Err(_) => {
                if let Err(error) = self.discard_failed_child_runtime(&child_runtime).await {
                    tracing::warn!(
                        parent_session_id = %self.id(),
                        child_session_id = %child_session_id,
                        error = %cause,
                        compensation_error = %error,
                        "failed to fully compensate panicked child session creation"
                    );
                }
                return;
            },
        };
        let parent_linked = match self.read_model().await {
            Ok(parent) => parent.agent_sessions.iter().any(|link| {
                link.child_session_id == *child_session_id
                    && link.status == astrcode_session_projection::AgentSessionStatus::Running
            }),
            Err(error) => {
                tracing::warn!(
                    parent_session_id = %self.id(),
                    child_session_id = %child_session_id,
                    error = %error,
                    "could not inspect parent link during child creation compensation"
                );
                true
            },
        };
        self.compensate_failed_child_creation(&child, child_session_id, cause, parent_linked)
            .await;
    }

    async fn compensate_failed_child_creation(
        &self,
        child: &Self,
        child_session_id: &SessionId,
        cause: &SessionError,
        parent_linked: bool,
    ) {
        // 补偿错误只进日志（下方 join 成一条 warn），不需要结构化错误类型：
        // String 让各阶段错误可以直接 format 进去，保持补偿链简单。
        let mut compensation_errors = Vec::new();
        let mut parent_link_settled = !parent_linked;
        if parent_linked {
            let terminal = match self
                .emit_durable(
                    None,
                    agent_session_failed_payload(
                        child_session_id.clone(),
                        format!("child creation did not commit: {cause}"),
                    ),
                )
                .await
            {
                Ok(_) => self.sync_durable_events().await,
                Err(error) => Err(error),
            };
            match terminal {
                Ok(()) => parent_link_settled = true,
                Err(error) => {
                    compensation_errors.push(format!("settle parent child link: {error}"));
                },
            }
        }
        // 第三方 hook 可能 panic（这是跨进程/插件边界的代码）；子会话创建是
        // "要么全部持久化、要么全部补偿"的事务，即使 shutdown hook panic 也必须
        // 继续走补偿路径，所以用 catch_unwind 把 panic 折叠成一条记录项，
        // 而不是让 panic 中断补偿链。
        match AssertUnwindSafe(child.emit_lifecycle(ExtensionEvent::SessionShutdown))
            .catch_unwind()
            .await
        {
            Ok(Ok(())) => {},
            Ok(Err(error)) => {
                compensation_errors.push(format!("run child shutdown hooks: {error}"));
            },
            Err(_) => compensation_errors.push("child shutdown hooks panicked".into()),
        }
        if parent_link_settled {
            if let Err(error) = child.discard_failed_creation().await {
                compensation_errors.push(error);
            }
        }

        if !compensation_errors.is_empty() {
            tracing::warn!(
                parent_session_id = %self.id(),
                child_session_id = %child_session_id,
                error = %cause,
                compensation_error = %compensation_errors.join("; "),
                "failed to fully compensate child session creation"
            );
        }
    }

    async fn discard_failed_creation(&self) -> Result<(), String> {
        self.discard_failed_child_runtime(&self.runtime).await
    }

    /// 丢弃未提交成功的子会话 runtime（释放事件 lane + 删除持久化会话）。
    ///
    /// 错误以 `String` 汇总返回：所有调用方都只把它写进 warn 日志（补偿路径没有
    /// 重试或结构化处理），String 足以承载各阶段的错误信息。
    async fn discard_failed_child_runtime(
        &self,
        child_runtime: &SessionRuntimeState,
    ) -> Result<(), String> {
        let release_result = child_runtime
            .event_sink()
            .release(child_runtime.store().as_ref(), child_runtime.session_id())
            .await;
        let delete_result = child_runtime
            .store()
            .delete_session(child_runtime.session_id())
            .await;
        if delete_result.is_ok() {
            self.runtime_services
                .session_resources()
                .cleanup(child_runtime.session_id());
        }

        let mut errors = Vec::new();
        if let Err(error) = release_result {
            errors.push(format!("release event lane: {error}"));
        }
        if let Err(error) = delete_result {
            errors.push(format!("delete persisted session: {error}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}
