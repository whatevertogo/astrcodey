//! Session history, control, and inspection capabilities.

use std::sync::Arc;

use astrcode_core::{
    extension::ChildToolPolicy,
    storage::EventReader,
    tool::{
        CreateSessionRequest, SessionAccessPair, SessionDeliveryOutcome, SessionOperations,
        SubmitTurnRequest, SubmitTurnResult,
    },
};
use astrcode_extension_sdk::s5r::ErrorPayload;
use serde_json::{Value, json};

use super::{InvokeContext, block_on_async, capability::SessionCapability, session_inspect};

const MAX_READ_EVENTS_LIMIT: usize = 500;

pub(super) struct SessionGroup {
    reader: Option<Arc<dyn EventReader>>,
}

impl SessionGroup {
    pub(super) fn new(reader: Option<Arc<dyn EventReader>>) -> Self {
        Self { reader }
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
        let reader = self.reader.as_ref().ok_or_else(|| {
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
        let reader = self.reader()?;
        block_on_async(async move { session_inspect::list(reader).await })?
    }

    fn inspect_snapshot(&self, input: Value) -> Result<Value, ErrorPayload> {
        let reader = self.reader()?;
        block_on_async(async move { session_inspect::snapshot(reader, input).await })?
    }

    fn inspect_read_model(&self, input: Value) -> Result<Value, ErrorPayload> {
        let reader = self.reader()?;
        block_on_async(async move { session_inspect::read_model(reader, input).await })?
    }

    fn inspect_provider_messages(&self, input: Value) -> Result<Value, ErrorPayload> {
        let reader = self.reader()?;
        block_on_async(async move { session_inspect::provider_messages(reader, input).await })?
    }

    fn reader(&self) -> Result<Arc<dyn EventReader>, ErrorPayload> {
        self.reader
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
    let request = CreateSessionRequest {
        name: input["name"].as_str().unwrap_or("child").to_string(),
        working_dir: input["working_dir"].as_str().map(str::to_string),
        system_prompt: input["system_prompt"].as_str().map(str::to_string),
        model_preference: input["model_preference"].as_str().map(str::to_string),
        tool_policy: parse_child_tool_policy(input)?,
        source_extension: Some(ctx.extension_id.clone()),
        ephemeral: input["ephemeral"].as_bool().unwrap_or(false),
        tool_call_id: input["tool_call_id"].as_str().unwrap_or("").to_string(),
    };
    let parent = ctx
        .session_id
        .clone()
        .ok_or_else(|| ErrorPayload::new("invalid_input", "parent session_id required"))?;
    let ops = Arc::clone(ops);
    block_on_async(async move {
        ops.create_session(&parent, request)
            .await
            .map(|handle| json!({ "session_id": handle.session_id }))
            .map_err(|error| ErrorPayload::new("session_error", error.to_string()))
    })?
}

fn submit_turn(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let wait_for_result = input["wait_for_result"].as_bool().unwrap_or(true);
    if ctx.on_peer_io_thread && wait_for_result {
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
    let target_session_id = input["target_session_id"]
        .as_str()
        .ok_or_else(|| ErrorPayload::new("invalid_input", "target_session_id required"))?
        .to_string();
    let user_prompt = input["user_prompt"]
        .as_str()
        .ok_or_else(|| ErrorPayload::new("invalid_input", "user_prompt required"))?
        .to_string();
    let request = SubmitTurnRequest::for_child(caller, target_session_id, user_prompt)
        .wait_for_result(wait_for_result)
        .notify_parent_on_complete(
            input["notify_parent_on_complete"]
                .as_str()
                .map(str::to_string),
        )
        .recycle_on_complete(input["recycle_on_complete"].as_bool().unwrap_or(false))
        .tool_call_id(input["tool_call_id"].as_str().map(str::to_string));
    let ops = Arc::clone(ops);
    block_on_async(async move {
        ops.submit_turn(request)
            .await
            .map(submit_turn_result_json)
            .map_err(|error| ErrorPayload::new("session_error", error.to_string()))
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

fn submit_turn_result_json(result: SubmitTurnResult) -> Value {
    match result {
        SubmitTurnResult::Completed { content } => {
            json!({ "status": "completed", "content": content })
        },
        SubmitTurnResult::Backgrounded {
            task_id,
            session_id,
        } => json!({
            "status": "backgrounded",
            "task_id": task_id,
            "session_id": session_id
        }),
    }
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

fn parse_child_tool_policy(input: &Value) -> Result<Option<ChildToolPolicy>, ErrorPayload> {
    let Some(value) = input.get("tool_policy") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }

    let policy = serde_json::from_value::<ChildToolPolicy>(value.clone()).map_err(|error| {
        ErrorPayload::new("invalid_input", format!("invalid tool_policy: {error}"))
            .with_hint("expected {\"mode\":\"allow|deny\",\"tools\":[\"tool_name\"]}")
    })?;
    validate_child_tool_policy(&policy)?;
    Ok(Some(policy))
}

fn validate_child_tool_policy(policy: &ChildToolPolicy) -> Result<(), ErrorPayload> {
    let tools = match policy {
        ChildToolPolicy::Deny { tools } => tools,
        ChildToolPolicy::Allow { tools } if tools.is_empty() => {
            return Err(ErrorPayload::new(
                "invalid_input",
                "tool_policy allow mode requires at least one tool",
            ));
        },
        ChildToolPolicy::Allow { tools } => tools,
    };

    if tools.iter().any(|tool| tool.trim().is_empty()) {
        return Err(ErrorPayload::new(
            "invalid_input",
            "tool_policy tools must be non-empty strings",
        ));
    }
    Ok(())
}
