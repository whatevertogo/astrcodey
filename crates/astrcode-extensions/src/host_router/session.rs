//! Session history, control, and inspection capabilities.

use std::{collections::BTreeSet, sync::Arc};

use astrcode_core::tool::{
    CreateSessionRequest, SessionAccessPair, SessionDeliveryOutcome, SessionOperations,
    SessionToolSelection, SubmitTurnRequest,
};
use astrcode_extension_sdk::{
    s5r::ErrorPayload,
    session::{
        HostCreateSessionOutput, HostCreateSessionRequest, HostSubmitTurnOutput,
        HostSubmitTurnRequest, SessionToolSelectionDto,
    },
};
use astrcode_storage::{EventReader, SessionReader};
use serde_json::{Value, json};

use super::{InvokeContext, block_on_async, capability::SessionCapability, session_inspect};

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

    pub(super) fn invoke(
        &self,
        capability: SessionCapability,
        input: Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        match capability {
            SessionCapability::ReadEvents => self.read_events(&input, ctx),
            SessionCapability::Create => create_session(&input, ctx),
            SessionCapability::ConfigureTools => configure_tools(&input, ctx),
            SessionCapability::SubmitTurn => submit_turn(&input, ctx),
            SessionCapability::InterruptAndSubmit => interrupt_and_submit(&input, ctx),
            SessionCapability::Inject => inject_input(&input, ctx),
            SessionCapability::CancelTurn => cancel_turn(&input, ctx),
            SessionCapability::ExecutionView => execution_view(&input, ctx),
            SessionCapability::Dispose => dispose_session(&input, ctx),
            SessionCapability::InspectList => self.inspect_list(),
            SessionCapability::InspectSnapshot => self.inspect_snapshot(input),
            SessionCapability::InspectReadModel => self.inspect_read_model(input),
            SessionCapability::InspectProviderMessages => self.inspect_provider_messages(input),
        }
    }

    fn read_events(&self, input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
        let reader = self.event_reader.as_ref().ok_or_else(|| {
            ErrorPayload::new("backend_unavailable", "session_read not configured")
        })?;
        let target_session_id = input["session_id"]
            .as_str()
            .ok_or_else(|| ErrorPayload::new("invalid_input", "session_id required"))?;
        let limit = input["limit"]
            .as_u64()
            .unwrap_or(100)
            .clamp(1, MAX_READ_EVENTS_LIMIT as u64) as usize;
        let caller_session_id = ctx.session_id.as_deref().ok_or_else(|| {
            ErrorPayload::new(
                "invalid_input",
                "caller session_id required in invoke context",
            )
        })?;
        let reader = Arc::clone(reader);
        let access = SessionAccessPair::new(caller_session_id, target_session_id);
        let ops = ctx.session_ops.as_ref().map(Arc::clone);
        block_on_async(async move {
            if let Some(ops) = ops {
                ops.query_session(access.as_access())
                    .await
                    .map_err(|error| ErrorPayload::new("permission_denied", error.to_string()))?;
            } else if access.caller_session_id != access.target_session_id {
                return Err(ErrorPayload::new(
                    "permission_denied",
                    "session_history read is limited to the caller session without session_control",
                ));
            }

            let session_id = astrcode_core::types::SessionId::new(&access.target_session_id);
            reader
                .replay_events(&session_id)
                .await
                .map(|events| {
                    let events = events.into_iter().take(limit).collect::<Vec<_>>();
                    json!({ "events": events })
                })
                .map_err(|error| ErrorPayload::new("read_failed", error.to_string()))
        })?
    }

    fn inspect_list(&self) -> Result<Value, ErrorPayload> {
        let reader = self.session_reader()?;
        block_on_async(async move { session_inspect::list(reader).await })?
    }

    fn inspect_snapshot(&self, input: Value) -> Result<Value, ErrorPayload> {
        let reader = self.session_reader()?;
        block_on_async(async move { session_inspect::snapshot(reader, input).await })?
    }

    fn inspect_read_model(&self, input: Value) -> Result<Value, ErrorPayload> {
        let reader = self.session_reader()?;
        block_on_async(async move { session_inspect::read_model(reader, input).await })?
    }

    fn inspect_provider_messages(&self, input: Value) -> Result<Value, ErrorPayload> {
        let reader = self.session_reader()?;
        block_on_async(async move { session_inspect::provider_messages(reader, input).await })?
    }

    fn session_reader(&self) -> Result<Arc<dyn SessionReader>, ErrorPayload> {
        self.session_reader
            .as_ref()
            .map(Arc::clone)
            .ok_or_else(|| ErrorPayload::new("backend_unavailable", "session_read not configured"))
    }
}

fn create_session(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let ops = ctx.session_ops.as_ref().ok_or_else(|| {
        ErrorPayload::new(
            "backend_unavailable",
            "session_ops not available in context",
        )
    })?;
    let wire_request =
        parse_wire_request::<HostCreateSessionRequest>(input, "session.control.create")?;
    let request = CreateSessionRequest {
        name: wire_request.name,
        working_dir: wire_request.working_dir,
        system_prompt: wire_request.system_prompt,
        model_preference: wire_request.model_preference,
        tool_selection: wire_request
            .tool_selection
            .map(|selection| map_tool_selection(selection, "tool_selection"))
            .transpose()?,
        source_extension: Some(ctx.extension_id.clone()),
        ephemeral: wire_request.ephemeral,
        tool_call_id: wire_request.tool_call_id.unwrap_or_default(),
    };
    let parent = ctx
        .session_id
        .clone()
        .ok_or_else(|| ErrorPayload::new("invalid_input", "parent session_id required"))?;
    let ops = Arc::clone(ops);
    block_on_async(async move {
        let handle = ops
            .create_session(&parent, request)
            .await
            .map_err(|error| ErrorPayload::new("session_error", error.to_string()))?;
        serialize_wire_response(
            HostCreateSessionOutput::from(handle),
            "session.control.create",
        )
    })?
}

fn configure_tools(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let ops = required_session_ops(ctx)?;
    let access = session_access_from_input(input, ctx)?;
    let selection = parse_session_tool_selection(input)?;
    block_on_async(async move {
        ops.configure_tools(access.as_access(), selection)
            .await
            .map(|effective| json!({ "selection": SessionToolSelectionDto::from(effective) }))
            .map_err(|error| ErrorPayload::new("session_error", error.to_string()))
    })?
}

fn submit_turn(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let wire_request =
        parse_wire_request::<HostSubmitTurnRequest>(input, "session.control.submit_turn")?;
    if ctx.on_peer_io_thread && wire_request.wait_for_result {
        return Err(ErrorPayload::new(
            "invalid_request",
            "wait_for_result cannot be used from peer synchronous host invokes (deadlock risk); \
             set wait_for_result to false",
        ));
    }
    let ops = ctx.session_ops.as_ref().ok_or_else(|| {
        ErrorPayload::new(
            "backend_unavailable",
            "session_ops not available in context",
        )
    })?;
    let caller = ctx
        .session_id
        .clone()
        .ok_or_else(|| ErrorPayload::new("invalid_input", "caller session_id required"))?;
    let request = SubmitTurnRequest::for_child(
        caller,
        wire_request.target_session_id,
        wire_request.user_prompt,
    )
    .wait_for_result(wire_request.wait_for_result)
    .notify_parent_on_complete(wire_request.notify_parent_on_complete)
    .recycle_on_complete(wire_request.recycle_on_complete)
    .tool_call_id(wire_request.tool_call_id);
    let ops = Arc::clone(ops);
    block_on_async(async move {
        let result = ops
            .submit_turn(request)
            .await
            .map_err(|error| ErrorPayload::new("session_error", error.to_string()))?;
        serialize_wire_response(
            HostSubmitTurnOutput::from(result),
            "session.control.submit_turn",
        )
    })?
}

fn inject_input(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let ops = required_session_ops(ctx)?;
    let access = session_access_from_input(input, ctx)?;
    let content = required_session_content(input)?;
    block_on_async(async move {
        ops.inject_message(access.as_access(), content)
            .await
            .map(session_delivery_outcome_json)
            .map_err(|error| ErrorPayload::new("session_error", error.to_string()))
    })?
}

fn interrupt_and_submit(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let ops = required_session_ops(ctx)?;
    let access = session_access_from_input(input, ctx)?;
    let content = required_session_content(input)?;
    block_on_async(async move {
        ops.interrupt_and_submit(access.as_access(), content)
            .await
            .map(session_delivery_outcome_json)
            .map_err(|error| ErrorPayload::new("session_error", error.to_string()))
    })?
}

fn cancel_turn(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let ops = required_session_ops(ctx)?;
    let access = session_access_from_input(input, ctx)?;
    block_on_async(async move {
        ops.cancel_turn(access.as_access())
            .await
            .map(|()| json!({ "ok": true }))
            .map_err(|error| ErrorPayload::new("session_error", error.to_string()))
    })?
}

fn execution_view(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let ops = required_session_ops(ctx)?;
    let access = session_access_from_input(input, ctx)?;
    block_on_async(async move {
        ops.execution_view(access.as_access())
            .await
            .map(|view| {
                json!({
                    "phase": view.phase,
                    "active_turn_id": view.active_turn_id,
                    "queued_inputs": view.queued_inputs,
                })
            })
            .map_err(|error| ErrorPayload::new("session_error", error.to_string()))
    })?
}

fn dispose_session(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let ops = ctx.session_ops.as_ref().ok_or_else(|| {
        ErrorPayload::new(
            "backend_unavailable",
            "session_ops not available in context",
        )
    })?;
    let session_id = input["session_id"]
        .as_str()
        .ok_or_else(|| ErrorPayload::new("invalid_input", "session_id required"))?;
    let ops = Arc::clone(ops);
    let access = SessionAccessPair::new(
        ctx.session_id
            .clone()
            .ok_or_else(|| ErrorPayload::new("invalid_input", "caller session_id required"))?,
        session_id,
    );
    block_on_async(async move {
        ops.recycle_session(access.as_access())
            .await
            .map(|()| json!({ "ok": true }))
            .map_err(|error| ErrorPayload::new("session_error", error.to_string()))
    })?
}

fn session_delivery_outcome_json(outcome: SessionDeliveryOutcome) -> Value {
    match outcome {
        SessionDeliveryOutcome::Started { turn_id } => {
            json!({ "status": "started", "turn_id": turn_id })
        },
        SessionDeliveryOutcome::Injected { turn_id } => {
            json!({ "status": "injected", "turn_id": turn_id })
        },
        SessionDeliveryOutcome::Queued { queue_len } => {
            json!({ "status": "queued", "queue_len": queue_len })
        },
    }
}

fn required_session_ops(ctx: &InvokeContext) -> Result<Arc<dyn SessionOperations>, ErrorPayload> {
    ctx.session_ops
        .as_ref()
        .map(Arc::clone)
        .ok_or_else(|| ErrorPayload::new("backend_unavailable", "session_ops not available"))
}

fn session_access_from_input(
    input: &Value,
    ctx: &InvokeContext,
) -> Result<SessionAccessPair, ErrorPayload> {
    let caller = ctx
        .session_id
        .as_deref()
        .ok_or_else(|| ErrorPayload::new("invalid_input", "caller session_id required"))?;
    let target = input
        .get("target_session_id")
        .or_else(|| input.get("session_id"))
        .and_then(Value::as_str)
        .filter(|target| !target.is_empty())
        .ok_or_else(|| ErrorPayload::new("invalid_input", "target_session_id required"))?;
    Ok(SessionAccessPair::new(caller, target))
}

fn required_session_content(input: &Value) -> Result<String, ErrorPayload> {
    input
        .get("content")
        .or_else(|| input.get("user_prompt"))
        .and_then(Value::as_str)
        .filter(|content| !content.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ErrorPayload::new("invalid_input", "content required"))
}

fn parse_session_tool_selection(input: &Value) -> Result<SessionToolSelection, ErrorPayload> {
    let value = input
        .get("selection")
        .ok_or_else(|| ErrorPayload::new("invalid_input", "selection required"))?;
    parse_tool_selection_value(value, "selection")
}

fn parse_tool_selection_value(
    value: &Value,
    field: &str,
) -> Result<SessionToolSelection, ErrorPayload> {
    let selection =
        serde_json::from_value::<SessionToolSelectionDto>(value.clone()).map_err(|error| {
            ErrorPayload::new("invalid_input", format!("invalid {field}: {error}")).with_hint(
                "expected {\"mode\":\"all\",\"except\":[]} or {\"mode\":\"only\",\"names\":[]}",
            )
        })?;
    map_tool_selection(selection, field)
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
                    "invalid_input",
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
            "invalid_input",
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
            "serialization_failed",
            format!("failed to serialize {capability} response: {error}"),
        )
    })
}
