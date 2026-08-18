//! Session history, control, and inspection capabilities.

use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    future::Future,
    sync::Arc,
};

use astrcode_core::{
    event::{DurableEventPayload, Phase, StoredEvent},
    llm::TranscriptMessageOrigin,
    session_lineage::{ParentChainWalkError, collect_parent_chain},
    tool::{
        CreateRootSessionRequest as CoreCreateRootSessionRequest, CreateSessionRequest,
        ForkSessionRequest, SessionAccessPair, SessionApiError, SessionDeliveryOutcome,
        SessionHandle, SessionLifecycleState, SessionOperations, SessionState,
        SessionToolSelection, SubmitTurnRequest, SubmitTurnResult,
    },
    types::SessionId,
};
use astrcode_extension_sdk::{
    host::{
        Acknowledgement, EmptyRequest, HostConfigureSessionToolsOutput,
        HostConfigureSessionToolsRequest, HostOperation, HostSessionCancelOutput,
        HostSessionDeliveryOutput, HostSessionExecutionView, HostSessionInputRequest,
        HostSessionProviderMessagesOutput, HostSessionSummariesOutput, HostSessionSummary,
        HostSessionTokenUsage, HostSessionTokenUsageOutput, HostSessionTranscript,
        HostSessionTranscriptMessage,
        internal::{HostOperationGroup, llm_message_to_wire, llm_messages_to_wire},
    },
    session::tool_selection_to_dto,
    wire::{
        ErrorPayload, WireErrorCode,
        session::{
            HostCreateRootSessionRequest, HostCreateSessionOutput, HostCreateSessionRequest,
            HostForkRootSessionRequest, HostRecycleSessionRequest, HostRootSubmitTurnRequest,
            HostSessionEvent, HostSessionEventsPageOutput, HostSessionEventsPageRequest,
            HostSessionReactivateOutput, HostSessionStateOutput, HostSessionTargetRequest,
            HostSubmitTurnOutput, HostSubmitTurnRequest, SessionLifecycleStateDto,
            SessionMessageOriginDto, SessionToolSelectionDto,
        },
        session_inspect::{
            HostSessionInspectRequest, SessionHistorySnapshotOutput, SessionInspectListOutput,
            SessionInspectProviderMessagesOutput, SessionInspectReadModelOutput,
            SessionInspectSnapshotOutput,
        },
    },
};
use astrcode_storage::{EventReader, SessionReader, StorageError};
use serde_json::Value;

use super::{
    InvokeContext, acknowledgement, backend_unavailable, dispatch, invalid_group_operation,
    session_inspect,
};

const MAX_READ_EVENTS_LIMIT: usize = 500;

pub(super) struct SessionGroup {
    event_reader: Option<Arc<dyn EventReader>>,
    session_reader: Option<Arc<dyn SessionReader>>,
}

impl SessionGroup {
    pub(super) fn new(
        event_reader: Option<Arc<dyn EventReader>>,
        session_reader: Option<Arc<dyn SessionReader>>,
    ) -> Self {
        Self {
            event_reader,
            session_reader,
        }
    }

    pub(super) async fn invoke(
        &self,
        operation: HostOperation,
        input: Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        // Session operations vary widely in state size; boxing the selected branch avoids one
        // oversized enum-like future for the entire surface.
        match operation {
            HostOperation::SessionReadEvents => {
                Box::pin(dispatch(operation, &input, |request| {
                    self.read_events(request, ctx)
                }))
                .await
            },
            HostOperation::SessionRootCreate => {
                Box::pin(dispatch(operation, &input, |request| {
                    create_root_session(operation, request, ctx)
                }))
                .await
            },
            HostOperation::SessionRootState => {
                Box::pin(dispatch(operation, &input, |request| {
                    self.root_session_state(request, ctx)
                }))
                .await
            },
            HostOperation::SessionRootSubmitTurn => {
                Box::pin(dispatch(operation, &input, |request| {
                    self.submit_root_turn(request, ctx)
                }))
                .await
            },
            HostOperation::SessionRootDispose => {
                Box::pin(dispatch(operation, &input, |request| {
                    self.dispose_root_session(request, ctx)
                }))
                .await
            },
            HostOperation::SessionRootFork => {
                Box::pin(dispatch(operation, &input, |request| {
                    self.fork_root_session(request, ctx)
                }))
                .await
            },
            HostOperation::SessionControlCreate => {
                Box::pin(dispatch(operation, &input, |request| {
                    create_session(request, ctx)
                }))
                .await
            },
            HostOperation::SessionControlConfigureTools => {
                Box::pin(dispatch(operation, &input, |request| {
                    configure_tools(request, ctx)
                }))
                .await
            },
            HostOperation::SessionControlSubmitTurn => {
                Box::pin(dispatch(operation, &input, |request| {
                    submit_turn(request, ctx)
                }))
                .await
            },
            HostOperation::SessionControlInterruptAndSubmit => {
                Box::pin(dispatch(operation, &input, |request| {
                    interrupt_and_submit(request, ctx)
                }))
                .await
            },
            HostOperation::SessionControlInjectOrStart => {
                Box::pin(dispatch(operation, &input, |request| {
                    inject_input(request, ctx)
                }))
                .await
            },
            HostOperation::SessionControlQueueOrStart => {
                Box::pin(dispatch(operation, &input, |request| {
                    queue_or_start_input(request, ctx)
                }))
                .await
            },
            HostOperation::SessionControlDeferContext => {
                Box::pin(dispatch(operation, &input, |request| {
                    defer_context(request, ctx)
                }))
                .await
            },
            HostOperation::SessionControlCancelTurn => {
                Box::pin(dispatch(operation, &input, |request| {
                    cancel_turn(request, ctx)
                }))
                .await
            },
            HostOperation::SessionControlExecutionView => {
                Box::pin(dispatch(operation, &input, |request| {
                    execution_view(request, ctx)
                }))
                .await
            },
            HostOperation::SessionControlDispose => {
                Box::pin(dispatch(operation, &input, |request| {
                    dispose_session(request, ctx)
                }))
                .await
            },
            HostOperation::SessionControlReactivate => {
                Box::pin(dispatch(operation, &input, |request| {
                    reactivate_session(request, ctx)
                }))
                .await
            },
            HostOperation::SessionControlState => {
                Box::pin(dispatch(operation, &input, |request| {
                    session_state(request, ctx)
                }))
                .await
            },
            HostOperation::SessionHistoryList => {
                Box::pin(dispatch(operation, &input, |_: EmptyRequest| {
                    self.history_list(ctx)
                }))
                .await
            },
            HostOperation::SessionHistoryProviderMessages => {
                Box::pin(dispatch(operation, &input, |request| {
                    self.history_provider_messages(request, ctx)
                }))
                .await
            },
            HostOperation::SessionHistorySnapshot => {
                Box::pin(dispatch(operation, &input, |request| {
                    self.history_snapshot(request, ctx)
                }))
                .await
            },
            HostOperation::SessionHistoryTokenUsage => {
                Box::pin(dispatch(operation, &input, |request| {
                    self.history_token_usage(request, ctx)
                }))
                .await
            },
            HostOperation::SessionHistoryTranscript => {
                Box::pin(dispatch(operation, &input, |request| {
                    self.history_transcript(request, ctx)
                }))
                .await
            },
            HostOperation::SessionInspectList => {
                Box::pin(dispatch(operation, &input, |_: EmptyRequest| {
                    self.inspect_list(operation)
                }))
                .await
            },
            HostOperation::SessionInspectSnapshot => {
                Box::pin(dispatch(operation, &input, |request| {
                    self.inspect_snapshot(operation, request)
                }))
                .await
            },
            HostOperation::SessionInspectReadModel => {
                Box::pin(dispatch(operation, &input, |request| {
                    self.inspect_read_model(operation, request)
                }))
                .await
            },
            HostOperation::SessionInspectProviderMessages => {
                Box::pin(dispatch(operation, &input, |request| {
                    self.inspect_provider_messages(operation, request)
                }))
                .await
            },
            _ => Err(invalid_group_operation(
                operation,
                HostOperationGroup::Session,
            )),
        }
    }

    pub(super) fn has_event_reader(&self) -> bool {
        self.event_reader.is_some()
    }

    pub(super) fn has_session_reader(&self) -> bool {
        self.session_reader.is_some()
    }

    async fn read_events(
        &self,
        request: HostSessionEventsPageRequest,
        ctx: &InvokeContext,
    ) -> Result<HostSessionEventsPageOutput, ErrorPayload> {
        let reader = self
            .event_reader
            .as_ref()
            .ok_or_else(|| backend_unavailable("session_read not configured"))?;
        if !(1..=MAX_READ_EVENTS_LIMIT).contains(&request.limit) {
            return Err(ErrorPayload::new(
                WireErrorCode::InvalidInput,
                format!("limit must be between 1 and {MAX_READ_EVENTS_LIMIT}"),
            ));
        }
        let caller_session_id = required_history_context(ctx)?;
        let access = SessionAccessPair::new(caller_session_id, &request.session_id);
        authorize_history_target(ctx.session_ops.as_deref(), &access).await?;

        let session_id = astrcode_core::types::SessionId::new(&access.target_session_id);
        let mut events = match request.cursor.as_ref() {
            Some(cursor) => {
                reader
                    .replay_from_active_or_recycled_limited(&session_id, cursor, request.limit + 1)
                    .await
            },
            None => {
                reader
                    .replay_from_start_active_or_recycled_limited(&session_id, request.limit + 1)
                    .await
            },
        }
        .map_err(storage_read_error)?;
        let has_more = events.len() > request.limit;
        events.truncate(request.limit);
        let next_cursor = events
            .last()
            .map(|event| event.seq.to_string())
            .or(request.cursor)
            .unwrap_or_else(|| "0".into());
        let events = events
            .into_iter()
            .map(host_session_event)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(HostSessionEventsPageOutput {
            events,
            next_cursor,
            has_more,
        })
    }

    async fn inspect_list(
        &self,
        operation: HostOperation,
    ) -> Result<SessionInspectListOutput, ErrorPayload> {
        let reader = self.session_reader()?;
        session_inspect::list(operation, reader).await
    }

    async fn inspect_snapshot(
        &self,
        operation: HostOperation,
        request: HostSessionInspectRequest,
    ) -> Result<SessionInspectSnapshotOutput, ErrorPayload> {
        let reader = self.session_reader()?;
        session_inspect::snapshot(operation, reader, SessionId::new(request.session_id)).await
    }

    async fn inspect_read_model(
        &self,
        operation: HostOperation,
        request: HostSessionInspectRequest,
    ) -> Result<SessionInspectReadModelOutput, ErrorPayload> {
        let reader = self.session_reader()?;
        session_inspect::read_model(operation, reader, SessionId::new(request.session_id)).await
    }

    async fn inspect_provider_messages(
        &self,
        operation: HostOperation,
        request: HostSessionInspectRequest,
    ) -> Result<SessionInspectProviderMessagesOutput, ErrorPayload> {
        let reader = self.session_reader()?;
        session_inspect::provider_messages(operation, reader, SessionId::new(request.session_id))
            .await
    }

    async fn history_snapshot(
        &self,
        request: HostSessionTargetRequest,
        ctx: &InvokeContext,
    ) -> Result<SessionHistorySnapshotOutput, ErrorPayload> {
        let access = history_access_from_target(request, ctx)?;
        let reader = self.session_reader()?;
        authorize_history_target(ctx.session_ops.as_deref(), &access).await?;

        let session_id = astrcode_core::types::SessionId::new(&access.target_session_id);
        let (lifecycle, model) = match reader.session_read_model(&session_id).await {
            Ok(model) => (SessionLifecycleStateDto::Active, model),
            Err(StorageError::NotFound(_)) => (
                SessionLifecycleStateDto::Recycled,
                reader
                    .recycled_session_read_model(&session_id)
                    .await
                    .map_err(storage_read_error)?,
            ),
            Err(error) => return Err(storage_error(error)),
        };
        Ok(SessionHistorySnapshotOutput {
            lifecycle,
            read_model: session_inspect::read_model_dto((*model).clone()),
        })
    }

    async fn history_list(
        &self,
        ctx: &InvokeContext,
    ) -> Result<HostSessionSummariesOutput, ErrorPayload> {
        let caller_session_id = required_history_context(ctx)?.to_owned();
        let reader = self.session_reader()?;
        let summaries = reader
            .list_all_session_summaries()
            .await
            .map_err(storage_read_error)?;
        let mut parents = HashMap::with_capacity(summaries.len());
        for summary in &summaries {
            if parents
                .insert(
                    summary.session_id.clone(),
                    summary.parent_session_id.clone(),
                )
                .is_some()
            {
                return Err(ErrorPayload::new(
                    WireErrorCode::ReadFailed,
                    format!("duplicate session summary for {}", summary.session_id),
                ));
            }
        }
        let visible = visible_history_sessions(
            &astrcode_core::types::SessionId::new(caller_session_id),
            &parents,
        )
        .await?;
        let mut sessions = Vec::new();
        for summary in summaries {
            if !visible.contains(&summary.session_id) {
                continue;
            }
            sessions.push(HostSessionSummary {
                session_id: summary.session_id.into_string(),
                parent_session_id: summary.parent_session_id.map(SessionId::into_string),
                source_extension: summary.source_extension,
                working_dir: summary.working_dir,
                model_id: summary.model_id,
                created_at: summary.created_at,
                updated_at: summary.updated_at,
                latest_cursor: summary.latest_cursor,
            });
        }
        Ok(HostSessionSummariesOutput { sessions })
    }

    async fn history_transcript(
        &self,
        request: HostSessionTargetRequest,
        ctx: &InvokeContext,
    ) -> Result<HostSessionTranscript, ErrorPayload> {
        let access = history_access_from_target(request, ctx)?;
        let reader = self.session_reader()?;
        authorize_history_target(ctx.session_ops.as_deref(), &access).await?;
        let model = history_read_model(&reader, &access.target_session_id).await?;
        let messages = model
            .model_context
            .messages
            .iter()
            .filter(|message| extension_visible_message(&message.message))
            .map(|message| HostSessionTranscriptMessage {
                message: llm_message_to_wire((*message.message).clone()),
                origin: message.origin.map(message_origin_dto),
            })
            .collect();
        Ok(HostSessionTranscript {
            session_id: model.identity.session_id.to_string(),
            messages,
        })
    }

    async fn history_provider_messages(
        &self,
        request: HostSessionTargetRequest,
        ctx: &InvokeContext,
    ) -> Result<HostSessionProviderMessagesOutput, ErrorPayload> {
        let access = history_access_from_target(request, ctx)?;
        let reader = self.session_reader()?;
        authorize_history_target(ctx.session_ops.as_deref(), &access).await?;
        let model = history_read_model(&reader, &access.target_session_id).await?;
        let messages = astrcode_core::llm::provider_visible_messages(
            model
                .model_context
                .messages
                .iter()
                .map(|message| (*message.message).clone())
                .collect(),
        );
        Ok(HostSessionProviderMessagesOutput {
            session_id: model.identity.session_id.to_string(),
            messages: llm_messages_to_wire(messages),
        })
    }

    async fn history_token_usage(
        &self,
        request: HostSessionTargetRequest,
        ctx: &InvokeContext,
    ) -> Result<HostSessionTokenUsageOutput, ErrorPayload> {
        let access = history_access_from_target(request, ctx)?;
        let reader = self
            .event_reader
            .as_ref()
            .ok_or_else(|| backend_unavailable("session event reader not configured"))?;
        authorize_history_target(ctx.session_ops.as_deref(), &access).await?;
        let session_id = astrcode_core::types::SessionId::new(access.target_session_id);
        let events = reader
            .replay_events_active_or_recycled(&session_id)
            .await
            .map_err(storage_read_error)?;
        let mut total_tokens = 0u64;
        let mut saw_usage = false;
        let mut model_context_window = None;
        for event in events {
            if let DurableEventPayload::TokenUsageRecorded {
                usage,
                model_context_window: window,
            } = event.event.payload
            {
                if let Some(tokens) = usage.non_cached_tokens() {
                    total_tokens = total_tokens.saturating_add(tokens);
                    saw_usage = true;
                }
                model_context_window = Some(window);
            }
        }
        Ok(HostSessionTokenUsageOutput {
            usage: saw_usage.then_some(HostSessionTokenUsage {
                total_tokens,
                model_context_window,
            }),
        })
    }

    async fn submit_root_turn(
        &self,
        wire_request: HostRootSubmitTurnRequest,
        ctx: &InvokeContext,
    ) -> Result<HostSubmitTurnOutput, ErrorPayload> {
        reject_wait_for_result_on_peer_thread(wire_request.wait_for_result, ctx)?;
        let reader = self.session_reader()?;
        let ops = required_session_ops(ctx)?;
        let extension_id = required_extension_id(ctx)?.to_owned();
        authorize_owned_root(&reader, &wire_request.target_session_id, &extension_id).await?;
        let request = SubmitTurnRequest::for_session(
            wire_request.target_session_id,
            wire_request.user_prompt,
        )
        .wait_for_result(wire_request.wait_for_result);
        let result = ops.submit_turn(request).await.map_err(session_api_error)?;
        Ok(submit_turn_output(result))
    }

    async fn root_session_state(
        &self,
        request: HostSessionTargetRequest,
        ctx: &InvokeContext,
    ) -> Result<HostSessionStateOutput, ErrorPayload> {
        let reader = self.session_reader()?;
        let ops = required_session_ops(ctx)?;
        let extension_id = required_extension_id(ctx)?.to_owned();
        authorize_owned_root(&reader, &request.target_session_id, &extension_id).await?;
        let access = SessionAccessPair::same(&request.target_session_id);
        let state = ops
            .session_state(access.as_access())
            .await
            .map_err(session_api_error)?;
        Ok(session_state_output(state))
    }

    async fn dispose_root_session(
        &self,
        request: HostSessionTargetRequest,
        ctx: &InvokeContext,
    ) -> Result<Acknowledgement, ErrorPayload> {
        let reader = self.session_reader()?;
        let ops = required_session_ops(ctx)?;
        let extension_id = required_extension_id(ctx)?.to_owned();
        authorize_owned_root(&reader, &request.target_session_id, &extension_id).await?;
        ops.recycle_session(SessionAccessPair::same(&request.target_session_id).as_access())
            .await
            .map_err(session_api_error)?;
        Ok(acknowledgement())
    }

    /// 把一个 session 分叉为归属调用扩展的新顶层会话。
    ///
    /// 授权:source 要么是调用扩展拥有的顶层会话(与其余 root 操作同一规则,
    /// 经 `authorize_owned_root`),要么就是本次调用的上下文会话——正在某个
    /// turn 内执行的扩展分叉它已在其中的对话。其余一律拒绝。产出的新 root
    /// 带 `source_extension` 归属,后续 submit/state/dispose 走既有授权。
    async fn fork_root_session(
        &self,
        request: HostForkRootSessionRequest,
        ctx: &InvokeContext,
    ) -> Result<HostCreateSessionOutput, ErrorPayload> {
        let reader = self.session_reader()?;
        let ops = required_session_ops(ctx)?;
        let extension_id = required_extension_id(ctx)?.to_owned();
        let owned_root = authorize_owned_root(&reader, &request.source_session_id, &extension_id)
            .await
            .is_ok();
        let context_session = ctx
            .session_id
            .as_deref()
            .is_some_and(|session_id| session_id == request.source_session_id);
        if !owned_root && !context_session {
            return Err(ErrorPayload::new(
                WireErrorCode::PermissionDenied,
                "fork source must be an owned top-level session or the current session",
            ));
        }
        let handle = ops
            .fork_session(ForkSessionRequest {
                source_session_id: request.source_session_id,
                at_cursor: request.at_cursor,
                source_extension: Some(extension_id),
            })
            .await
            .map_err(session_api_error)?;
        Ok(create_session_output(handle))
    }

    fn session_reader(&self) -> Result<Arc<dyn SessionReader>, ErrorPayload> {
        self.session_reader
            .as_ref()
            .map(Arc::clone)
            .ok_or_else(|| backend_unavailable("session_read not configured"))
    }
}

pub(super) fn message_origin_dto(origin: TranscriptMessageOrigin) -> SessionMessageOriginDto {
    match origin {
        TranscriptMessageOrigin::TurnAborted => SessionMessageOriginDto::TurnAborted,
        TranscriptMessageOrigin::ToolCallFailed => SessionMessageOriginDto::ToolCallFailed,
        TranscriptMessageOrigin::ToolCallCancelled => SessionMessageOriginDto::ToolCallCancelled,
    }
}

async fn visible_history_sessions(
    caller_session_id: &astrcode_core::types::SessionId,
    parents: &HashMap<astrcode_core::types::SessionId, Option<astrcode_core::types::SessionId>>,
) -> Result<HashSet<astrcode_core::types::SessionId>, ErrorPayload> {
    // 已确认无环的后缀无需重复遍历；新路径的环检测在 collect_parent_chain 内完成。
    let mut resolved = HashSet::with_capacity(parents.len());
    for session_id in parents.keys() {
        if resolved.contains(session_id) {
            continue;
        }
        let path = collect_parent_chain(session_id, |current: astrcode_core::types::SessionId| {
            let parent = if resolved.contains(&current) {
                None
            } else {
                parents.get(&current).and_then(Option::as_ref).cloned()
            };
            async move { Ok::<Option<astrcode_core::types::SessionId>, Infallible>(parent) }
        })
        .await
        .map_err(|error| match error {
            ParentChainWalkError::Cycle(cycle) => {
                ErrorPayload::new(WireErrorCode::ReadFailed, cycle.to_string())
            },
            ParentChainWalkError::Resolve(error) => match error {},
        })?;
        resolved.extend(path);
    }

    let mut children: HashMap<_, Vec<_>> = HashMap::new();
    for (session_id, parent_session_id) in parents {
        if let Some(parent_session_id) = parent_session_id {
            children
                .entry(parent_session_id)
                .or_default()
                .push(session_id);
        }
    }
    let mut visible = HashSet::new();
    let mut pending = vec![caller_session_id];
    while let Some(session_id) = pending.pop() {
        if !visible.insert((*session_id).clone()) {
            continue;
        }
        if let Some(descendants) = children.get(session_id) {
            pending.extend(descendants.iter().copied());
        }
    }
    Ok(visible)
}

fn required_history_context(ctx: &InvokeContext) -> Result<&str, ErrorPayload> {
    required_extension_id(ctx)?;
    ctx.session_id.as_deref().ok_or_else(|| {
        ErrorPayload::new(
            WireErrorCode::ContextUnavailable,
            "session history requires a session-scoped invoke context",
        )
    })
}

async fn authorize_history_target(
    ops: Option<&dyn SessionOperations>,
    access: &SessionAccessPair,
) -> Result<(), ErrorPayload> {
    if history_target_is_visible(ops, access).await? {
        return Ok(());
    }
    Err(ErrorPayload::new(
        WireErrorCode::PermissionDenied,
        "session history target is outside the caller session lineage",
    ))
}

async fn history_target_is_visible(
    ops: Option<&dyn SessionOperations>,
    access: &SessionAccessPair,
) -> Result<bool, ErrorPayload> {
    if access.caller_session_id == access.target_session_id {
        return Ok(true);
    }
    let Some(ops) = ops else {
        return Ok(false);
    };
    match ops.session_state(access.as_access()).await {
        Ok(_) => Ok(true),
        Err(SessionApiError::PermissionDenied(_) | SessionApiError::NotFound(_)) => Ok(false),
        Err(error) => Err(session_api_error(error)),
    }
}

async fn history_read_model(
    reader: &Arc<dyn SessionReader>,
    target_session_id: &str,
) -> Result<Arc<astrcode_session_projection::SessionReadModel>, ErrorPayload> {
    let session_id = astrcode_core::types::SessionId::new(target_session_id);
    reader
        .session_read_model_active_or_recycled(&session_id)
        .await
        .map_err(storage_read_error)
}

fn extension_visible_message(message: &astrcode_core::llm::LlmMessage) -> bool {
    !astrcode_core::compaction::is_compact_summary_message(message)
}

/// 只读会话 API 的 StorageError 映射：身份与能力类错误保持稳定码，其余读失败
/// 统一为 `read_failed`（可重试）。
fn storage_read_error(error: StorageError) -> ErrorPayload {
    match error {
        StorageError::InvalidId(message) => ErrorPayload::new(WireErrorCode::InvalidInput, message),
        StorageError::NotFound(session_id) => ErrorPayload::new(
            WireErrorCode::SessionNotFound,
            format!("session not found: {session_id}"),
        ),
        StorageError::Unsupported(message) => {
            ErrorPayload::new(WireErrorCode::Unsupported, message)
        },
        error => ErrorPayload::new(WireErrorCode::ReadFailed, error.to_string()).retryable(true),
    }
}

/// handler 上下文里 `wait_for_result` 会与 admission permit 形成互等:handler 持有 permit
/// (Sequential 模式占满该扩展全部 permit),而被等待的 turn 可能需要回调本扩展的 hook/tool
/// (需要 ≥1 个 permit)。后台调用（无父调用，`on_peer_io_thread == false`）不持有 permit,
/// 允许同步等待。两种 submit 入口共用该检查。
fn reject_wait_for_result_on_peer_thread(
    wait_for_result: bool,
    ctx: &InvokeContext,
) -> Result<(), ErrorPayload> {
    if ctx.on_peer_io_thread && wait_for_result {
        return Err(ErrorPayload::new(
            WireErrorCode::InvalidRequest,
            "wait_for_result cannot be used from peer synchronous host invokes (deadlock risk); \
             set wait_for_result to false",
        ));
    }
    Ok(())
}

async fn create_root_session(
    operation: HostOperation,
    request: HostCreateRootSessionRequest,
    ctx: &InvokeContext,
) -> Result<HostCreateSessionOutput, ErrorPayload> {
    let ops = required_session_ops(ctx)?;
    let (working_dir, explicit) = match request.working_dir {
        Some(dir) => (canonicalize_existing_dir(&dir)?, true),
        None => (
            ctx.working_dir.clone().ok_or_else(|| {
                ErrorPayload::new(
                    WireErrorCode::ContextUnavailable,
                    format!(
                        "{} requires a workspace-scoped call context",
                        operation.wire_name()
                    ),
                )
            })?,
            false,
        ),
    };
    let request = CoreCreateRootSessionRequest {
        working_dir: working_dir.clone(),
        source_extension: Some(required_extension_id(ctx)?.to_owned()),
        system_prompt: request.system_prompt,
        model_preference: request.model_preference,
        tool_selection: request
            .tool_selection
            .map(|selection| map_tool_selection(selection, "tool_selection"))
            .transpose()?,
    };
    let handle = ops
        .create_root_session(request)
        .await
        .map_err(session_api_error)?;
    if explicit {
        tracing::info!(
            extension_id = %ctx.extension_id,
            working_dir = %working_dir,
            session_id = %handle.session_id,
            "extension created root session with explicit working_dir"
        );
    }
    Ok(create_session_output(handle))
}

/// 显式 `working_dir` 的边界校验：解析为规范的绝对路径且必须已存在为目录。
fn canonicalize_existing_dir(dir: &str) -> Result<String, ErrorPayload> {
    let canonicalized = std::fs::canonicalize(dir).map_err(|error| {
        ErrorPayload::new(
            WireErrorCode::InvalidInput,
            format!("working_dir {dir} is not an existing directory: {error}"),
        )
    })?;
    if !canonicalized.is_dir() {
        return Err(ErrorPayload::new(
            WireErrorCode::InvalidInput,
            format!("working_dir {dir} is not a directory"),
        ));
    }
    Ok(canonicalized.to_string_lossy().into_owned())
}

async fn authorize_owned_root(
    reader: &Arc<dyn SessionReader>,
    target_session_id: &str,
    extension_id: &str,
) -> Result<(), ErrorPayload> {
    let session_id = astrcode_core::types::SessionId::new(target_session_id);
    let model = reader
        .session_read_model_active_or_recycled(&session_id)
        .await
        .map_err(storage_read_error)?;
    if model.identity.parent.is_none()
        && model.identity.source_extension.as_deref() == Some(extension_id)
    {
        return Ok(());
    }
    Err(ErrorPayload::new(
        WireErrorCode::PermissionDenied,
        "target session is not a top-level session owned by the calling extension",
    ))
}

fn required_extension_id(ctx: &InvokeContext) -> Result<&str, ErrorPayload> {
    (!ctx.extension_id.is_empty())
        .then_some(ctx.extension_id.as_str())
        .ok_or_else(|| {
            ErrorPayload::new(
                WireErrorCode::ContextUnavailable,
                "host operation requires an attributed extension context",
            )
        })
}

fn host_session_event(stored: StoredEvent) -> Result<HostSessionEvent, ErrorPayload> {
    let StoredEvent { seq, event } = stored;
    let timestamp = serde_json::to_value(event.timestamp).map_err(|error| {
        ErrorPayload::new(
            WireErrorCode::SerializationFailed,
            format!("failed to serialize session event timestamp: {error}"),
        )
    })?;
    let timestamp = timestamp.as_str().map(str::to_owned).ok_or_else(|| {
        ErrorPayload::new(
            WireErrorCode::SerializationFailed,
            "session event timestamp did not serialize as a string",
        )
    })?;
    let payload = serde_json::to_value(event.payload).map_err(|error| {
        ErrorPayload::new(
            WireErrorCode::SerializationFailed,
            format!("failed to serialize session event payload: {error}"),
        )
    })?;
    Ok(HostSessionEvent {
        seq,
        id: event.id.into_string(),
        session_id: event.session_id.into_string(),
        turn_id: event.turn_id.map(|turn_id| turn_id.into_string()),
        timestamp,
        payload,
    })
}

async fn create_session(
    wire_request: HostCreateSessionRequest,
    ctx: &InvokeContext,
) -> Result<HostCreateSessionOutput, ErrorPayload> {
    let ops = required_session_ops(ctx)?;
    let request = CreateSessionRequest {
        name: wire_request.name,
        system_prompt: wire_request.system_prompt,
        model_preference: wire_request.model_preference,
        tool_selection: wire_request
            .tool_selection
            .map(|selection| map_tool_selection(selection, "tool_selection"))
            .transpose()?,
        source_extension: Some(ctx.extension_id.clone()),
        ephemeral: wire_request.ephemeral,
        tool_call_id: ctx.tool_call_id.clone(),
    };
    let parent = ctx.session_id.clone().ok_or_else(|| {
        ErrorPayload::new(WireErrorCode::InvalidInput, "parent session_id required")
    })?;
    let handle = ops
        .create_session(&parent, request)
        .await
        .map_err(session_api_error)?;
    Ok(create_session_output(handle))
}

async fn configure_tools(
    request: HostConfigureSessionToolsRequest,
    ctx: &InvokeContext,
) -> Result<HostConfigureSessionToolsOutput, ErrorPayload> {
    let ops = required_session_ops(ctx)?;
    let access = session_access_from_id(request.session_id, ctx)?;
    let selection = map_tool_selection(request.selection, "selection")?;
    let effective = ops
        .configure_tools(access.as_access(), selection)
        .await
        .map_err(session_api_error)?;
    Ok(HostConfigureSessionToolsOutput {
        selection: tool_selection_to_dto(effective),
    })
}

async fn submit_turn(
    wire_request: HostSubmitTurnRequest,
    ctx: &InvokeContext,
) -> Result<HostSubmitTurnOutput, ErrorPayload> {
    reject_wait_for_result_on_peer_thread(wire_request.wait_for_result, ctx)?;
    let ops = required_session_ops(ctx)?;
    let caller = ctx.session_id.clone().ok_or_else(|| {
        ErrorPayload::new(WireErrorCode::InvalidInput, "caller session_id required")
    })?;
    let request = SubmitTurnRequest::for_child(
        caller,
        wire_request.target_session_id,
        wire_request.user_prompt,
    )
    .wait_for_result(wire_request.wait_for_result)
    .notify_parent_on_complete(wire_request.notify_parent_on_complete)
    .recycle_on_complete(wire_request.recycle_on_complete)
    .tool_call_id(ctx.tool_call_id.clone());
    let result = ops.submit_turn(request).await.map_err(session_api_error)?;
    Ok(submit_turn_output(result))
}

async fn inject_input(
    request: HostSessionInputRequest,
    ctx: &InvokeContext,
) -> Result<HostSessionDeliveryOutput, ErrorPayload> {
    deliver_session_input(ctx, request, |ops, access, content| async move {
        ops.inject_message(access.as_access(), content).await
    })
    .await
}

async fn queue_or_start_input(
    request: HostSessionInputRequest,
    ctx: &InvokeContext,
) -> Result<HostSessionDeliveryOutput, ErrorPayload> {
    deliver_session_input(ctx, request, |ops, access, content| async move {
        ops.queue_or_start(access.as_access(), content).await
    })
    .await
}

async fn defer_context(
    request: HostSessionInputRequest,
    ctx: &InvokeContext,
) -> Result<HostSessionDeliveryOutput, ErrorPayload> {
    deliver_session_input(ctx, request, |ops, access, content| async move {
        ops.defer_context(access.as_access(), content).await
    })
    .await
}

async fn interrupt_and_submit(
    request: HostSessionInputRequest,
    ctx: &InvokeContext,
) -> Result<HostSessionDeliveryOutput, ErrorPayload> {
    deliver_session_input(ctx, request, |ops, access, content| async move {
        ops.interrupt_and_submit(access.as_access(), content).await
    })
    .await
}

async fn cancel_turn(
    request: HostSessionTargetRequest,
    ctx: &InvokeContext,
) -> Result<HostSessionCancelOutput, ErrorPayload> {
    session_target_call(
        ctx,
        request.target_session_id,
        |ops, access| async move { ops.cancel_turn(access.as_access()).await },
        |cancelled| HostSessionCancelOutput { cancelled },
    )
    .await
}

async fn execution_view(
    request: HostSessionTargetRequest,
    ctx: &InvokeContext,
) -> Result<HostSessionExecutionView, ErrorPayload> {
    session_target_call(
        ctx,
        request.target_session_id,
        |ops, access| async move { ops.execution_view(access.as_access()).await },
        |view| HostSessionExecutionView {
            phase: phase_output(view.phase),
            active_turn_id: view.active_turn_id,
            queued_inputs: view.queued_inputs,
        },
    )
    .await
}

async fn dispose_session(
    request: HostRecycleSessionRequest,
    ctx: &InvokeContext,
) -> Result<Acknowledgement, ErrorPayload> {
    let ops = required_session_ops(ctx)?;
    let access = SessionAccessPair::new(
        ctx.session_id.clone().ok_or_else(|| {
            ErrorPayload::new(
                WireErrorCode::ContextUnavailable,
                "caller session_id required",
            )
        })?,
        request.session_id,
    );
    ops.recycle_session(access.as_access())
        .await
        .map_err(session_api_error)?;
    Ok(acknowledgement())
}

async fn session_state(
    request: HostSessionTargetRequest,
    ctx: &InvokeContext,
) -> Result<HostSessionStateOutput, ErrorPayload> {
    session_target_call(
        ctx,
        request.target_session_id,
        |ops, access| async move { ops.session_state(access.as_access()).await },
        session_state_output,
    )
    .await
}

async fn reactivate_session(
    request: HostSessionTargetRequest,
    ctx: &InvokeContext,
) -> Result<HostSessionReactivateOutput, ErrorPayload> {
    let target_session_id = request.target_session_id.clone();
    session_target_call(
        ctx,
        request.target_session_id,
        |ops, access| async move { ops.reactivate_session(access.as_access()).await },
        move |result| HostSessionReactivateOutput {
            session_id: target_session_id,
            reactivated: result.reactivated,
        },
    )
    .await
}

/// `session.control` 目标型操作的固定管线：取 session_ops、按 target session 提取访问对、
/// 调用给定 ops 方法并映射为契约输出。ops 方法与输出映射由调用点给出。
/// `call` 收 owned `Arc` 与 `SessionAccessPair`：借用形式要求 higher-ranked 生命周期，
/// 而方法引用/闭包无法对其泛化；拥有所有权的签名让返回的 future 类型与生命周期解耦。
async fn session_target_call<T, Output, Fut>(
    ctx: &InvokeContext,
    target_session_id: String,
    call: impl FnOnce(Arc<dyn SessionOperations>, SessionAccessPair) -> Fut,
    map_output: impl FnOnce(T) -> Output,
) -> Result<Output, ErrorPayload>
where
    Fut: Future<Output = Result<T, SessionApiError>>,
{
    let ops = required_session_ops_arc(ctx)?;
    let access = session_access_from_id(target_session_id, ctx)?;
    let result = call(ops, access).await.map_err(session_api_error)?;
    Ok(map_output(result))
}

/// 输入投递类操作（`inject_or_start`/`queue_or_start`/`defer_context`/`interrupt_and_submit`）
/// 共用的投递管线：校验顺序、访问对与 delivery outcome 映射完全一致，仅 ops 方法不同。
/// owned 参数的原因同 `session_target_call`。
async fn deliver_session_input<Fut>(
    ctx: &InvokeContext,
    request: HostSessionInputRequest,
    deliver: impl FnOnce(Arc<dyn SessionOperations>, SessionAccessPair, String) -> Fut,
) -> Result<HostSessionDeliveryOutput, ErrorPayload>
where
    Fut: Future<Output = Result<SessionDeliveryOutcome, SessionApiError>>,
{
    let ops = required_session_ops_arc(ctx)?;
    let access = session_access_from_id(request.target_session_id, ctx)?;
    let content = non_empty_session_content(request.content)?;
    let outcome = deliver(ops, access, content)
        .await
        .map_err(session_api_error)?;
    Ok(session_delivery_outcome(outcome))
}

fn session_delivery_outcome(outcome: SessionDeliveryOutcome) -> HostSessionDeliveryOutput {
    match outcome {
        SessionDeliveryOutcome::Started { turn_id } => {
            HostSessionDeliveryOutput::Started { turn_id }
        },
        SessionDeliveryOutcome::Injected { turn_id } => {
            HostSessionDeliveryOutput::Injected { turn_id }
        },
        SessionDeliveryOutcome::Queued { queue_len } => {
            HostSessionDeliveryOutput::Queued { queue_len }
        },
    }
}

fn required_session_ops(ctx: &InvokeContext) -> Result<&dyn SessionOperations, ErrorPayload> {
    ctx.session_ops
        .as_deref()
        .ok_or_else(|| backend_unavailable("session_ops not available"))
}

fn required_session_ops_arc(
    ctx: &InvokeContext,
) -> Result<Arc<dyn SessionOperations>, ErrorPayload> {
    ctx.session_ops
        .clone()
        .ok_or_else(|| backend_unavailable("session_ops not available"))
}

fn session_access_from_id(
    target_session_id: String,
    ctx: &InvokeContext,
) -> Result<SessionAccessPair, ErrorPayload> {
    let caller = ctx.session_id.as_deref().ok_or_else(|| {
        ErrorPayload::new(WireErrorCode::InvalidInput, "caller session_id required")
    })?;
    if target_session_id.is_empty() {
        return Err(ErrorPayload::new(
            WireErrorCode::InvalidInput,
            "target session id must not be empty",
        ));
    }
    Ok(SessionAccessPair::new(caller, target_session_id))
}

fn history_access_from_target(
    request: HostSessionTargetRequest,
    ctx: &InvokeContext,
) -> Result<SessionAccessPair, ErrorPayload> {
    let caller = required_history_context(ctx)?;
    Ok(SessionAccessPair::new(caller, request.target_session_id))
}

fn session_api_error(error: SessionApiError) -> ErrorPayload {
    super::wire_payload(error)
}

pub(super) fn storage_error(error: StorageError) -> ErrorPayload {
    super::wire_payload(error)
}

fn non_empty_session_content(content: String) -> Result<String, ErrorPayload> {
    if content.is_empty() {
        Err(ErrorPayload::new(
            WireErrorCode::InvalidInput,
            "content must not be empty",
        ))
    } else {
        Ok(content)
    }
}

fn map_tool_selection(
    selection: SessionToolSelectionDto,
    field: &str,
) -> Result<SessionToolSelection, ErrorPayload> {
    match selection {
        SessionToolSelectionDto::All { except } => Ok(SessionToolSelection::All {
            except: validated_tool_names(except, &format!("{field}.except"))?,
        }),
        SessionToolSelectionDto::Only { names } => Ok(SessionToolSelection::Only {
            names: validated_tool_names(names, &format!("{field}.names"))?,
        }),
    }
}

fn create_session_output(handle: SessionHandle) -> HostCreateSessionOutput {
    HostCreateSessionOutput {
        session_id: handle.session_id,
    }
}

fn submit_turn_output(result: SubmitTurnResult) -> HostSubmitTurnOutput {
    match result {
        SubmitTurnResult::Completed { content } => HostSubmitTurnOutput::Completed { content },
        SubmitTurnResult::Backgrounded {
            task_id,
            session_id,
        } => HostSubmitTurnOutput::Backgrounded {
            task_id,
            session_id,
        },
    }
}

fn session_state_output(state: SessionState) -> HostSessionStateOutput {
    HostSessionStateOutput {
        lifecycle: match state.lifecycle {
            SessionLifecycleState::Active => SessionLifecycleStateDto::Active,
            SessionLifecycleState::Recycled => SessionLifecycleStateDto::Recycled,
        },
        phase: phase_output(state.phase),
        active_turn_id: state.active_turn_id,
        queued_inputs: state.queued_inputs,
        message_count: state.message_count,
    }
}

pub(super) fn phase_output(phase: Phase) -> astrcode_extension_sdk::wire::session::SessionPhaseDto {
    use astrcode_extension_sdk::wire::session::SessionPhaseDto;

    match phase {
        Phase::Idle => SessionPhaseDto::Idle,
        Phase::Thinking => SessionPhaseDto::Thinking,
        Phase::Streaming => SessionPhaseDto::Streaming,
        Phase::CallingTool => SessionPhaseDto::CallingTool,
        Phase::Compacting => SessionPhaseDto::Compacting,
        Phase::Error => SessionPhaseDto::Error,
    }
}

fn validated_tool_names(tools: Vec<String>, field: &str) -> Result<Vec<String>, ErrorPayload> {
    astrcode_core::tool::validated_tool_names(tools).map_err(|_| {
        ErrorPayload::new(
            WireErrorCode::InvalidInput,
            format!("{field} must contain non-empty strings"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn history_visibility_uses_lineage_and_rejects_cycles() {
        let id = astrcode_core::types::SessionId::new;
        let parents = HashMap::from([
            (id("root"), None),
            (id("child"), Some(id("root"))),
            (id("grandchild"), Some(id("child"))),
            (id("sibling"), Some(id("other-root"))),
        ]);

        assert_eq!(
            visible_history_sessions(&id("root"), &parents)
                .await
                .expect("valid lineage"),
            HashSet::from([id("root"), id("child"), id("grandchild")])
        );

        for cycle in [
            HashMap::from([(id("root"), Some(id("root")))]),
            HashMap::from([
                (id("root"), Some(id("child"))),
                (id("child"), Some(id("root"))),
            ]),
            HashMap::from([
                (id("cycle-a"), Some(id("cycle-b"))),
                (id("cycle-b"), Some(id("cycle-a"))),
            ]),
        ] {
            let error = visible_history_sessions(&id("root"), &cycle)
                .await
                .expect_err("corrupt lineage must not be exposed");
            assert_eq!(error.code_enum(), Some(WireErrorCode::ReadFailed));
        }
    }

    #[test]
    fn session_api_errors_keep_stable_wire_codes() {
        let cases = [
            (
                SessionApiError::NotFound("child".into()),
                WireErrorCode::SessionNotFound,
            ),
            (
                SessionApiError::PermissionDenied("unrelated session".into()),
                WireErrorCode::PermissionDenied,
            ),
            (
                SessionApiError::SessionBusy("active turn".into()),
                WireErrorCode::SessionBusy,
            ),
            (
                SessionApiError::NoActiveTurn("child".into()),
                WireErrorCode::NoActiveTurn,
            ),
            (
                SessionApiError::MaxDepthExceeded { current: 4, max: 4 },
                WireErrorCode::MaxDepthExceeded,
            ),
            (
                SessionApiError::Unsupported("operation".into()),
                WireErrorCode::Unsupported,
            ),
            (
                SessionApiError::internal_msg("backend failed"),
                WireErrorCode::InternalError,
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(session_api_error(error).code_enum(), Some(expected));
        }
    }
}
