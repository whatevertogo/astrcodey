//! Session history, control, and inspection capabilities.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::Arc,
};

use astrcode_core::{
    event::{DurableEventPayload, StoredEvent},
    llm::LlmTokenUsage,
    tool::{
        CreateRootSessionRequest as CoreCreateRootSessionRequest, CreateSessionRequest,
        SessionAccessPair, SessionApiError, SessionDeliveryOutcome, SessionOperations,
        SessionToolSelection, SubmitTurnRequest,
    },
    types::SessionId,
};
use astrcode_extension_sdk::{
    host::{
        HOST_ERROR_CODE_BACKEND_UNAVAILABLE, HOST_ERROR_CODE_CONTEXT_UNAVAILABLE,
        HOST_ERROR_CODE_INVALID_INPUT, HOST_ERROR_CODE_PERMISSION_DENIED,
        HOST_ERROR_CODE_SERIALIZATION_FAILED, HostAcknowledgement, HostConfigureSessionToolsOutput,
        HostConfigureSessionToolsRequest, HostSessionCancelOutput, HostSessionDeliveryOutput,
        HostSessionExecutionView, HostSessionInputRequest, HostSessionProviderMessagesOutput,
        HostSessionSummariesOutput, HostSessionSummary, HostSessionTokenUsage,
        HostSessionTokenUsageOutput, HostSessionTranscript, HostSessionTranscriptMessage,
    },
    s5r::ErrorPayload,
    session::{
        HostCreateSessionOutput, HostCreateSessionRequest, HostRecycleSessionRequest,
        HostRootSubmitTurnRequest, HostSessionEvent, HostSessionEventsPageOutput,
        HostSessionEventsPageRequest, HostSessionReactivateOutput, HostSessionStateOutput,
        HostSessionTargetRequest, HostSubmitTurnOutput, HostSubmitTurnRequest,
        SessionLifecycleStateDto, SessionToolSelectionDto,
    },
    session_inspect::{HostSessionInspectRequest, SessionHistorySnapshotOutput},
};
use astrcode_storage::{EventReader, SessionReader, StorageError};
use serde_json::Value;

use super::{InvokeContext, capability::SessionCapability, session_inspect};

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
        capability: SessionCapability,
        input: Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        // Session operations vary widely in state size; boxing the selected branch avoids one
        // oversized enum-like future for the entire surface.
        match capability {
            SessionCapability::ReadEvents => Box::pin(self.read_events(&input, ctx)).await,
            SessionCapability::RootCreate => Box::pin(create_root_session(&input, ctx)).await,
            SessionCapability::RootState => Box::pin(self.root_session_state(&input, ctx)).await,
            SessionCapability::RootSubmitTurn => Box::pin(self.submit_root_turn(&input, ctx)).await,
            SessionCapability::Create => Box::pin(create_session(&input, ctx)).await,
            SessionCapability::ConfigureTools => Box::pin(configure_tools(&input, ctx)).await,
            SessionCapability::SubmitTurn => Box::pin(submit_turn(&input, ctx)).await,
            SessionCapability::InterruptAndSubmit => {
                Box::pin(interrupt_and_submit(&input, ctx)).await
            },
            SessionCapability::Inject => Box::pin(inject_input(&input, ctx)).await,
            SessionCapability::CancelTurn => Box::pin(cancel_turn(&input, ctx)).await,
            SessionCapability::ExecutionView => Box::pin(execution_view(&input, ctx)).await,
            SessionCapability::Dispose => Box::pin(dispose_session(&input, ctx)).await,
            SessionCapability::Reactivate => Box::pin(reactivate_session(&input, ctx)).await,
            SessionCapability::State => Box::pin(session_state(&input, ctx)).await,
            SessionCapability::HistoryList => Box::pin(self.history_list(&input, ctx)).await,
            SessionCapability::HistoryProviderMessages => {
                Box::pin(self.history_provider_messages(&input, ctx)).await
            },
            SessionCapability::HistorySnapshot => {
                Box::pin(self.history_snapshot(&input, ctx)).await
            },
            SessionCapability::HistoryTokenUsage => {
                Box::pin(self.history_token_usage(&input, ctx)).await
            },
            SessionCapability::HistoryTranscript => {
                Box::pin(self.history_transcript(&input, ctx)).await
            },
            SessionCapability::InspectList => Box::pin(self.inspect_list(&input)).await,
            SessionCapability::InspectSnapshot => Box::pin(self.inspect_snapshot(&input)).await,
            SessionCapability::InspectReadModel => Box::pin(self.inspect_read_model(&input)).await,
            SessionCapability::InspectProviderMessages => {
                Box::pin(self.inspect_provider_messages(&input)).await
            },
        }
    }

    pub(super) fn has_event_reader(&self) -> bool {
        self.event_reader.is_some()
    }

    pub(super) fn has_session_reader(&self) -> bool {
        self.session_reader.is_some()
    }

    async fn read_events(&self, input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
        let reader = self.event_reader.as_ref().ok_or_else(|| {
            ErrorPayload::new(
                HOST_ERROR_CODE_BACKEND_UNAVAILABLE,
                "session_read not configured",
            )
        })?;
        let request =
            parse_wire_request::<HostSessionEventsPageRequest>(input, "session.read_events")?;
        if !(1..=MAX_READ_EVENTS_LIMIT).contains(&request.limit) {
            return Err(ErrorPayload::new(
                HOST_ERROR_CODE_INVALID_INPUT,
                format!("limit must be between 1 and {MAX_READ_EVENTS_LIMIT}"),
            ));
        }
        let caller_session_id = required_history_context(ctx)?;
        let access = SessionAccessPair::new(caller_session_id, &request.session_id);
        authorize_history_target(ctx.session_ops.as_deref(), &access).await?;

        let session_id = astrcode_core::types::SessionId::new(&access.target_session_id);
        let mut events = reader
            .replay_from_limited(&session_id, &request.cursor, request.limit + 1)
            .await
            .map_err(event_read_error)?;
        let has_more = events.len() > request.limit;
        events.truncate(request.limit);
        let next_cursor = events
            .last()
            .map(|event| event.seq.to_string())
            .unwrap_or(request.cursor);
        let events = events
            .into_iter()
            .map(host_session_event)
            .collect::<Result<Vec<_>, _>>()?;
        serialize_wire_response(
            HostSessionEventsPageOutput {
                events,
                next_cursor,
                has_more,
            },
            "session.read_events",
        )
    }

    async fn inspect_list(&self, input: &Value) -> Result<Value, ErrorPayload> {
        require_empty_object(input, "session.inspect.list")?;
        let reader = self.session_reader()?;
        session_inspect::list(reader).await
    }

    async fn inspect_snapshot(&self, input: &Value) -> Result<Value, ErrorPayload> {
        let session_id = inspect_session_id(input, "session.inspect.snapshot")?;
        let reader = self.session_reader()?;
        session_inspect::snapshot(reader, session_id).await
    }

    async fn inspect_read_model(&self, input: &Value) -> Result<Value, ErrorPayload> {
        let session_id = inspect_session_id(input, "session.inspect.read_model")?;
        let reader = self.session_reader()?;
        session_inspect::read_model(reader, session_id).await
    }

    async fn inspect_provider_messages(&self, input: &Value) -> Result<Value, ErrorPayload> {
        let session_id = inspect_session_id(input, "session.inspect.provider_messages")?;
        let reader = self.session_reader()?;
        session_inspect::provider_messages(reader, session_id).await
    }

    async fn history_snapshot(
        &self,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let request =
            parse_wire_request::<HostSessionTargetRequest>(input, "session.history.snapshot")?;
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
                    .map_err(history_storage_error)?,
            ),
            Err(error) => return Err(storage_error(error)),
        };
        serialize_wire_response(
            SessionHistorySnapshotOutput {
                lifecycle,
                read_model: session_inspect::read_model_dto((*model).clone()),
            },
            "session.history.snapshot",
        )
    }

    async fn history_list(
        &self,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let caller_session_id = required_history_context(ctx)?.to_owned();
        if !input.as_object().is_some_and(serde_json::Map::is_empty) {
            return Err(ErrorPayload::new(
                HOST_ERROR_CODE_INVALID_INPUT,
                "session.history.list expects an empty object",
            ));
        }
        let reader = self.session_reader()?;
        let summaries = reader
            .list_session_summaries()
            .await
            .map_err(history_storage_error)?;
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
                    "read_failed",
                    format!("duplicate session summary for {}", summary.session_id),
                ));
            }
        }
        let visible = visible_history_sessions(
            &astrcode_core::types::SessionId::new(caller_session_id),
            &parents,
        )?;
        let mut sessions = Vec::new();
        for summary in summaries {
            if !visible.contains(&summary.session_id) {
                continue;
            }
            sessions.push(HostSessionSummary {
                session_id: summary.session_id,
                parent_session_id: summary.parent_session_id,
                source_extension: summary.source_extension,
                working_dir: summary.working_dir,
                model_id: summary.model_id,
                created_at: summary.created_at,
                updated_at: summary.updated_at,
                latest_cursor: summary.latest_cursor,
            });
        }
        serialize_wire_response(
            HostSessionSummariesOutput { sessions },
            "session.history.list",
        )
    }

    async fn history_transcript(
        &self,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let request =
            parse_wire_request::<HostSessionTargetRequest>(input, "session.history.transcript")?;
        let access = history_access_from_target(request, ctx)?;
        let reader = self.session_reader()?;
        authorize_history_target(ctx.session_ops.as_deref(), &access).await?;
        let model = history_read_model(&reader, &access.target_session_id).await?;
        let messages = model
            .transcript
            .messages
            .iter()
            .filter(|message| extension_visible_message(&message.message))
            .map(|message| HostSessionTranscriptMessage {
                message: message.message.clone().into(),
                source: message.source.clone(),
            })
            .collect();
        serialize_wire_response(
            HostSessionTranscript {
                session_id: model.identity.session_id.clone(),
                messages,
            },
            "session.history.transcript",
        )
    }

    async fn history_provider_messages(
        &self,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let request = parse_wire_request::<HostSessionTargetRequest>(
            input,
            "session.history.provider_messages",
        )?;
        let access = history_access_from_target(request, ctx)?;
        let reader = self.session_reader()?;
        authorize_history_target(ctx.session_ops.as_deref(), &access).await?;
        let model = history_read_model(&reader, &access.target_session_id).await?;
        let messages = astrcode_core::llm::provider_visible_messages(
            model
                .transcript
                .messages
                .iter()
                .map(|message| message.message.clone())
                .collect(),
        );
        serialize_wire_response(
            HostSessionProviderMessagesOutput {
                session_id: model.identity.session_id.clone(),
                messages: messages.into_iter().map(Into::into).collect(),
            },
            "session.history.provider_messages",
        )
    }

    async fn history_token_usage(
        &self,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let request =
            parse_wire_request::<HostSessionTargetRequest>(input, "session.history.token_usage")?;
        let access = history_access_from_target(request, ctx)?;
        let reader = self.event_reader.as_ref().ok_or_else(|| {
            ErrorPayload::new(
                HOST_ERROR_CODE_BACKEND_UNAVAILABLE,
                "session event reader not configured",
            )
        })?;
        authorize_history_target(ctx.session_ops.as_deref(), &access).await?;
        let session_id = astrcode_core::types::SessionId::new(access.target_session_id);
        let events = reader
            .replay_events(&session_id)
            .await
            .map_err(history_storage_error)?;
        let mut total_tokens = 0u64;
        let mut saw_usage = false;
        let mut model_context_window = None;
        for event in events {
            if let DurableEventPayload::TokenUsageRecorded {
                usage,
                model_context_window: window,
            } = event.event.payload
            {
                if let Some(tokens) = non_cached_token_count(&usage) {
                    total_tokens = total_tokens.saturating_add(tokens);
                    saw_usage = true;
                }
                model_context_window = Some(window);
            }
        }
        serialize_wire_response(
            HostSessionTokenUsageOutput {
                usage: saw_usage.then_some(HostSessionTokenUsage {
                    total_tokens,
                    model_context_window,
                }),
            },
            "session.history.token_usage",
        )
    }

    async fn submit_root_turn(
        &self,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let wire_request =
            parse_wire_request::<HostRootSubmitTurnRequest>(input, "session.root.submit_turn")?;
        if ctx.on_peer_io_thread && wire_request.wait_for_result {
            return Err(ErrorPayload::new(
                "invalid_request",
                "wait_for_result cannot be used from peer synchronous host invokes (deadlock \
                 risk); set wait_for_result to false",
            ));
        }
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
        serialize_wire_response(
            HostSubmitTurnOutput::from(result),
            "session.root.submit_turn",
        )
    }

    async fn root_session_state(
        &self,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let request = parse_wire_request::<HostSessionTargetRequest>(input, "session.root.state")?;
        let reader = self.session_reader()?;
        let ops = required_session_ops(ctx)?;
        let extension_id = required_extension_id(ctx)?.to_owned();
        authorize_owned_root(&reader, &request.target_session_id, &extension_id).await?;
        let access = SessionAccessPair::same(&request.target_session_id);
        let state = ops
            .session_state(access.as_access())
            .await
            .map_err(session_api_error)?;
        serialize_wire_response(
            HostSessionStateOutput::from_state(state),
            "session.root.state",
        )
    }

    fn session_reader(&self) -> Result<Arc<dyn SessionReader>, ErrorPayload> {
        self.session_reader.as_ref().map(Arc::clone).ok_or_else(|| {
            ErrorPayload::new(
                HOST_ERROR_CODE_BACKEND_UNAVAILABLE,
                "session_read not configured",
            )
        })
    }
}

fn visible_history_sessions(
    caller_session_id: &astrcode_core::types::SessionId,
    parents: &HashMap<astrcode_core::types::SessionId, Option<astrcode_core::types::SessionId>>,
) -> Result<HashSet<astrcode_core::types::SessionId>, ErrorPayload> {
    let mut resolved = HashSet::with_capacity(parents.len());
    for session_id in parents.keys() {
        if resolved.contains(session_id) {
            continue;
        }
        let mut current = session_id.clone();
        let mut path = Vec::new();
        let mut visited = HashSet::new();
        loop {
            if resolved.contains(&current) {
                break;
            }
            if !visited.insert(current.clone()) {
                return Err(ErrorPayload::new(
                    "read_failed",
                    format!("session parent chain contains a cycle at {current}"),
                ));
            }
            path.push(current.clone());
            let Some(parent) = parents.get(&current).and_then(Option::as_ref) else {
                break;
            };
            current = parent.clone();
        }
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
            HOST_ERROR_CODE_CONTEXT_UNAVAILABLE,
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
        HOST_ERROR_CODE_PERMISSION_DENIED,
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
    match reader.session_read_model(&session_id).await {
        Ok(model) => Ok(model),
        Err(StorageError::NotFound(_)) => reader
            .recycled_session_read_model(&session_id)
            .await
            .map_err(history_storage_error),
        Err(error) => Err(history_storage_error(error)),
    }
}

fn extension_visible_message(message: &astrcode_core::llm::LlmMessage) -> bool {
    !astrcode_context::is_compact_summary_message(message)
}

fn non_cached_token_count(usage: &LlmTokenUsage) -> Option<u64> {
    match (usage.input_tokens, usage.output_tokens) {
        (Some(input), Some(output)) => Some(
            input
                .saturating_sub(usage.cached_input_tokens.unwrap_or_default())
                .saturating_add(output),
        ),
        _ => usage
            .total_tokens
            .map(|total| total.saturating_sub(usage.cached_input_tokens.unwrap_or_default()))
            .or_else(|| {
                let input = usage.input_tokens.map(|input| {
                    input.saturating_sub(usage.cached_input_tokens.unwrap_or_default())
                });
                (input.is_some() || usage.output_tokens.is_some()).then(|| {
                    input
                        .unwrap_or_default()
                        .saturating_add(usage.output_tokens.unwrap_or_default())
                })
            }),
    }
}

fn history_storage_error(error: StorageError) -> ErrorPayload {
    match error {
        StorageError::InvalidId(message) => {
            ErrorPayload::new(HOST_ERROR_CODE_INVALID_INPUT, message)
        },
        StorageError::NotFound(session_id) => ErrorPayload::new(
            "session_not_found",
            format!("session not found: {session_id}"),
        ),
        StorageError::Unsupported(message) => ErrorPayload::new("unsupported", message),
        error => ErrorPayload::new("read_failed", error.to_string()).retryable(true),
    }
}

async fn create_root_session(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    require_empty_object(input, "session.root.create")?;
    let ops = required_session_ops(ctx)?;
    let request = CoreCreateRootSessionRequest {
        working_dir: ctx.working_dir.clone().ok_or_else(|| {
            ErrorPayload::new(
                HOST_ERROR_CODE_CONTEXT_UNAVAILABLE,
                "session.root.create requires a workspace-scoped call context",
            )
        })?,
        source_extension: Some(required_extension_id(ctx)?.to_owned()),
    };
    let handle = ops
        .create_root_session(request)
        .await
        .map_err(session_api_error)?;
    serialize_wire_response(HostCreateSessionOutput::from(handle), "session.root.create")
}

async fn authorize_owned_root(
    reader: &Arc<dyn SessionReader>,
    target_session_id: &str,
    extension_id: &str,
) -> Result<(), ErrorPayload> {
    let session_id = astrcode_core::types::SessionId::new(target_session_id);
    let model = match reader.session_read_model(&session_id).await {
        Ok(model) => model,
        Err(StorageError::NotFound(_)) => reader
            .recycled_session_read_model(&session_id)
            .await
            .map_err(root_session_read_error)?,
        Err(error) => return Err(root_session_read_error(error)),
    };
    if model.identity.parent.is_none()
        && model.identity.source_extension.as_deref() == Some(extension_id)
    {
        return Ok(());
    }
    Err(ErrorPayload::new(
        HOST_ERROR_CODE_PERMISSION_DENIED,
        "target session is not a top-level session owned by the calling extension",
    ))
}

fn root_session_read_error(error: StorageError) -> ErrorPayload {
    match error {
        StorageError::NotFound(session_id) => ErrorPayload::new(
            "session_not_found",
            format!("session not found: {session_id}"),
        ),
        error => storage_error(error),
    }
}

fn event_read_error(error: StorageError) -> ErrorPayload {
    match error {
        StorageError::InvalidId(message) => {
            ErrorPayload::new(HOST_ERROR_CODE_INVALID_INPUT, message)
        },
        StorageError::NotFound(session_id) => ErrorPayload::new(
            "session_not_found",
            format!("session not found: {session_id}"),
        ),
        error => ErrorPayload::new("read_failed", error.to_string()),
    }
}

fn required_extension_id(ctx: &InvokeContext) -> Result<&str, ErrorPayload> {
    (!ctx.extension_id.is_empty())
        .then_some(ctx.extension_id.as_str())
        .ok_or_else(|| {
            ErrorPayload::new(
                HOST_ERROR_CODE_CONTEXT_UNAVAILABLE,
                "host operation requires an attributed extension context",
            )
        })
}

fn host_session_event(stored: StoredEvent) -> Result<HostSessionEvent, ErrorPayload> {
    let StoredEvent { seq, event } = stored;
    let timestamp = serde_json::to_value(event.timestamp).map_err(|error| {
        ErrorPayload::new(
            HOST_ERROR_CODE_SERIALIZATION_FAILED,
            format!("failed to serialize session event timestamp: {error}"),
        )
    })?;
    let timestamp = timestamp.as_str().map(str::to_owned).ok_or_else(|| {
        ErrorPayload::new(
            HOST_ERROR_CODE_SERIALIZATION_FAILED,
            "session event timestamp did not serialize as a string",
        )
    })?;
    let payload = serde_json::to_value(event.payload).map_err(|error| {
        ErrorPayload::new(
            HOST_ERROR_CODE_SERIALIZATION_FAILED,
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

async fn create_session(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let ops = required_session_ops(ctx)?;
    let wire_request =
        parse_wire_request::<HostCreateSessionRequest>(input, "session.control.create")?;
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
        ErrorPayload::new(HOST_ERROR_CODE_INVALID_INPUT, "parent session_id required")
    })?;
    let handle = ops
        .create_session(&parent, request)
        .await
        .map_err(session_api_error)?;
    serialize_wire_response(
        HostCreateSessionOutput::from(handle),
        "session.control.create",
    )
}

async fn configure_tools(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let request = parse_wire_request::<HostConfigureSessionToolsRequest>(
        input,
        "session.control.configure_tools",
    )?;
    let ops = required_session_ops(ctx)?;
    let access = session_access_from_id(request.session_id, ctx)?;
    let selection = map_tool_selection(request.selection, "selection")?;
    let effective = ops
        .configure_tools(access.as_access(), selection)
        .await
        .map_err(session_api_error)?;
    serialize_wire_response(
        HostConfigureSessionToolsOutput {
            selection: effective.into(),
        },
        "session.control.configure_tools",
    )
}

async fn submit_turn(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let wire_request =
        parse_wire_request::<HostSubmitTurnRequest>(input, "session.control.submit_turn")?;
    if ctx.on_peer_io_thread && wire_request.wait_for_result {
        return Err(ErrorPayload::new(
            "invalid_request",
            "wait_for_result cannot be used from peer synchronous host invokes (deadlock risk); \
             set wait_for_result to false",
        ));
    }
    let ops = required_session_ops(ctx)?;
    let caller = ctx.session_id.clone().ok_or_else(|| {
        ErrorPayload::new(HOST_ERROR_CODE_INVALID_INPUT, "caller session_id required")
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
    serialize_wire_response(
        HostSubmitTurnOutput::from(result),
        "session.control.submit_turn",
    )
}

async fn inject_input(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let request =
        parse_wire_request::<HostSessionInputRequest>(input, "session.control.inject_or_start")?;
    let ops = required_session_ops(ctx)?;
    let access = session_access_from_id(request.target_session_id, ctx)?;
    let content = non_empty_session_content(request.content)?;
    let outcome = ops
        .inject_message(access.as_access(), content)
        .await
        .map_err(session_api_error)?;
    serialize_wire_response(
        session_delivery_outcome(outcome),
        "session.control.inject_or_start",
    )
}

async fn interrupt_and_submit(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let request = parse_wire_request::<HostSessionInputRequest>(
        input,
        "session.control.interrupt_and_submit",
    )?;
    let ops = required_session_ops(ctx)?;
    let access = session_access_from_id(request.target_session_id, ctx)?;
    let content = non_empty_session_content(request.content)?;
    let outcome = ops
        .interrupt_and_submit(access.as_access(), content)
        .await
        .map_err(session_api_error)?;
    serialize_wire_response(
        session_delivery_outcome(outcome),
        "session.control.interrupt_and_submit",
    )
}

async fn cancel_turn(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let request =
        parse_wire_request::<HostSessionTargetRequest>(input, "session.control.cancel_turn")?;
    let ops = required_session_ops(ctx)?;
    let access = session_access_from_target(request, ctx)?;
    let cancelled = ops
        .cancel_turn(access.as_access())
        .await
        .map_err(session_api_error)?;
    serialize_wire_response(
        HostSessionCancelOutput { cancelled },
        "session.control.cancel_turn",
    )
}

async fn execution_view(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let request =
        parse_wire_request::<HostSessionTargetRequest>(input, "session.control.execution_view")?;
    let ops = required_session_ops(ctx)?;
    let access = session_access_from_target(request, ctx)?;
    let view = ops
        .execution_view(access.as_access())
        .await
        .map_err(session_api_error)?;
    serialize_wire_response(
        HostSessionExecutionView {
            phase: view.phase.into(),
            active_turn_id: view.active_turn_id,
            queued_inputs: view.queued_inputs,
        },
        "session.control.execution_view",
    )
}

async fn dispose_session(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let request =
        parse_wire_request::<HostRecycleSessionRequest>(input, "session.control.dispose")?;
    let ops = required_session_ops(ctx)?;
    let access = SessionAccessPair::new(
        ctx.session_id.clone().ok_or_else(|| {
            ErrorPayload::new(
                HOST_ERROR_CODE_CONTEXT_UNAVAILABLE,
                "caller session_id required",
            )
        })?,
        request.session_id,
    );
    ops.recycle_session(access.as_access())
        .await
        .map_err(session_api_error)?;
    serialize_wire_response(HostAcknowledgement::accepted(), "session.control.dispose")
}

async fn session_state(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let request = parse_wire_request::<HostSessionTargetRequest>(input, "session.control.state")?;
    let ops = required_session_ops(ctx)?;
    let access = session_access_from_target(request, ctx)?;
    let state = ops
        .session_state(access.as_access())
        .await
        .map_err(session_api_error)?;
    serialize_wire_response(
        HostSessionStateOutput::from_state(state),
        "session.control.state",
    )
}

async fn reactivate_session(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let request =
        parse_wire_request::<HostSessionTargetRequest>(input, "session.control.reactivate")?;
    let target_session_id = request.target_session_id.clone();
    let ops = required_session_ops(ctx)?;
    let access = session_access_from_target(request, ctx)?;
    let result = ops
        .reactivate_session(access.as_access())
        .await
        .map_err(session_api_error)?;
    serialize_wire_response(
        HostSessionReactivateOutput::from_result(target_session_id, result),
        "session.control.reactivate",
    )
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
    ctx.session_ops.as_deref().ok_or_else(|| {
        ErrorPayload::new(
            HOST_ERROR_CODE_BACKEND_UNAVAILABLE,
            "session_ops not available",
        )
    })
}

fn session_access_from_id(
    target_session_id: String,
    ctx: &InvokeContext,
) -> Result<SessionAccessPair, ErrorPayload> {
    let caller = ctx.session_id.as_deref().ok_or_else(|| {
        ErrorPayload::new(HOST_ERROR_CODE_INVALID_INPUT, "caller session_id required")
    })?;
    if target_session_id.is_empty() {
        return Err(ErrorPayload::new(
            HOST_ERROR_CODE_INVALID_INPUT,
            "target session id must not be empty",
        ));
    }
    Ok(SessionAccessPair::new(caller, target_session_id))
}

fn session_access_from_target(
    request: HostSessionTargetRequest,
    ctx: &InvokeContext,
) -> Result<SessionAccessPair, ErrorPayload> {
    session_access_from_id(request.target_session_id, ctx)
}

fn history_access_from_target(
    request: HostSessionTargetRequest,
    ctx: &InvokeContext,
) -> Result<SessionAccessPair, ErrorPayload> {
    let caller = required_history_context(ctx)?;
    Ok(SessionAccessPair::new(caller, request.target_session_id))
}

fn session_api_error(error: SessionApiError) -> ErrorPayload {
    let code = match &error {
        SessionApiError::NotFound(_) => "session_not_found",
        SessionApiError::PermissionDenied(_) => HOST_ERROR_CODE_PERMISSION_DENIED,
        SessionApiError::SessionBusy(_) => "session_busy",
        SessionApiError::MaxDepthExceeded { .. } => "max_depth_exceeded",
        SessionApiError::Unsupported(_) => "unsupported",
        SessionApiError::Internal(_) => "internal_error",
    };
    ErrorPayload::new(code, error.to_string())
}

pub(super) fn storage_error(error: StorageError) -> ErrorPayload {
    let retryable = error.is_retryable();
    let code = match &error {
        StorageError::NotFound(_) => "session_not_found",
        StorageError::AlreadyExists(_) => "session_already_exists",
        StorageError::InvalidId(_) => HOST_ERROR_CODE_INVALID_INPUT,
        StorageError::Unsupported(_) => "unsupported",
        StorageError::Io(_) => "storage_io_error",
        StorageError::Serialization(_)
        | StorageError::InvalidEvent(_)
        | StorageError::CorruptLog(_) => "corrupt_session_data",
        StorageError::LockError(_) => "storage_lock_error",
    };
    ErrorPayload::new(code, error.to_string()).retryable(retryable)
}

fn non_empty_session_content(content: String) -> Result<String, ErrorPayload> {
    if content.is_empty() {
        Err(ErrorPayload::new(
            HOST_ERROR_CODE_INVALID_INPUT,
            "content must not be empty",
        ))
    } else {
        Ok(content)
    }
}

fn inspect_session_id(input: &Value, capability: &str) -> Result<SessionId, ErrorPayload> {
    let request = parse_wire_request::<HostSessionInspectRequest>(input, capability)?;
    Ok(SessionId::new(request.session_id))
}

fn require_empty_object(input: &Value, capability: &str) -> Result<(), ErrorPayload> {
    if input.as_object().is_some_and(serde_json::Map::is_empty) {
        Ok(())
    } else {
        Err(ErrorPayload::new(
            HOST_ERROR_CODE_INVALID_INPUT,
            format!("{capability} expects an empty object"),
        ))
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

fn validated_tool_names(tools: Vec<String>, field: &str) -> Result<Vec<String>, ErrorPayload> {
    let tools = tools
        .into_iter()
        .map(|tool| {
            let tool = tool.trim();
            if tool.is_empty() {
                Err(ErrorPayload::new(
                    HOST_ERROR_CODE_INVALID_INPUT,
                    format!("{field} must contain non-empty strings"),
                ))
            } else {
                Ok(tool.to_owned())
            }
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    Ok(tools.into_iter().collect())
}

fn parse_wire_request<'de, T>(input: &'de Value, capability: &str) -> Result<T, ErrorPayload>
where
    T: serde::Deserialize<'de>,
{
    T::deserialize(input).map_err(|error| {
        ErrorPayload::new(
            HOST_ERROR_CODE_INVALID_INPUT,
            format!("invalid {capability} request: {error}"),
        )
    })
}

fn serialize_wire_response<T>(output: T, capability: &str) -> Result<Value, ErrorPayload>
where
    T: serde::Serialize,
{
    serde_json::to_value(output).map_err(|error| {
        ErrorPayload::new(
            HOST_ERROR_CODE_SERIALIZATION_FAILED,
            format!("failed to serialize {capability} response: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_cached_token_count_handles_complete_and_partial_usage() {
        let cases = [
            (
                LlmTokenUsage {
                    input_tokens: Some(100),
                    cached_input_tokens: Some(20),
                    cache_creation_input_tokens: None,
                    output_tokens: Some(20),
                    reasoning_output_tokens: Some(5),
                    total_tokens: Some(120),
                    source: None,
                },
                Some(100),
            ),
            (
                LlmTokenUsage {
                    cached_input_tokens: Some(20),
                    reasoning_output_tokens: Some(5),
                    total_tokens: Some(120),
                    ..Default::default()
                },
                Some(100),
            ),
            (
                LlmTokenUsage {
                    input_tokens: Some(100),
                    cached_input_tokens: Some(20),
                    ..Default::default()
                },
                Some(80),
            ),
            (
                LlmTokenUsage {
                    output_tokens: Some(20),
                    ..Default::default()
                },
                Some(20),
            ),
            (LlmTokenUsage::default(), None),
        ];

        for (usage, expected) in cases {
            assert_eq!(non_cached_token_count(&usage), expected, "usage: {usage:?}");
        }
    }

    #[test]
    fn history_visibility_uses_lineage_and_rejects_cycles() {
        let id = astrcode_core::types::SessionId::new;
        let parents = HashMap::from([
            (id("root"), None),
            (id("child"), Some(id("root"))),
            (id("grandchild"), Some(id("child"))),
            (id("sibling"), Some(id("other-root"))),
        ]);

        assert_eq!(
            visible_history_sessions(&id("root"), &parents).expect("valid lineage"),
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
                .expect_err("corrupt lineage must not be exposed");
            assert_eq!(error.code, "read_failed");
        }
    }

    #[test]
    fn session_api_errors_keep_stable_wire_codes() {
        let cases = [
            (
                SessionApiError::NotFound("child".into()),
                "session_not_found",
            ),
            (
                SessionApiError::PermissionDenied("unrelated session".into()),
                "permission_denied",
            ),
            (
                SessionApiError::SessionBusy("active turn".into()),
                "session_busy",
            ),
            (
                SessionApiError::MaxDepthExceeded { current: 4, max: 4 },
                "max_depth_exceeded",
            ),
            (
                SessionApiError::Unsupported("operation".into()),
                "unsupported",
            ),
            (
                SessionApiError::internal_msg("backend failed"),
                "internal_error",
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(session_api_error(error).code, expected);
        }
    }
}
