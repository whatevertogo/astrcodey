//! s5r 扩展 E2E guest — 使用 Worker SDK（manifest 与 handler 一体注册）。

use std::{
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
    time::Duration,
};

use astrcode_extension_sdk::{
    WireErrorCode,
    builder::tool,
    extension::{
        CustomEventDeclaration, CustomEventDelivery, ExtensionHttpDispatchRequest,
        ExtensionHttpMethod, ExtensionHttpResponse, ExtensionHttpRoute,
    },
    s5r::{CallContinuation, ErrorPayload, HandlerEffect, HandlerResult},
    tool::ExecutionMode,
};
use astrcode_extension_worker::worker_prelude::*;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

static PIPELINE_STEP_1_CALLS: AtomicU32 = AtomicU32::new(0);
static PIPELINE_STEP_2_CALLS: AtomicU32 = AtomicU32::new(0);
static PIPELINE_TOOL_CALLS: AtomicU32 = AtomicU32::new(0);
static PIPELINE_LLM_OK: AtomicBool = AtomicBool::new(false);
static PARALLEL_ACTIVE: AtomicU32 = AtomicU32::new(0);
static PARALLEL_PEAK: AtomicU32 = AtomicU32::new(0);

const EXT_ID: &str = "s5r-guest-demo";

fn no_resources() -> ToolPlannerFn {
    tool_planner(|_| async { Ok(ToolPlan::default()) })
}

fn host_resource(resource: HostResource) -> ToolPlannerFn {
    tool_planner(move |_| async move { Ok(ToolPlan::new([ResourceAccess::host(resource)])) })
}

fn read_probe() -> ToolPlannerFn {
    tool_planner(|ctx| async move {
        Ok(ToolPlan::new([ResourceAccess::read_file(
            ctx.working_dir().join("probe.txt"),
        )]))
    })
}

fn workspace_text(output: HostWorkspaceReadOutput) -> Result<String, ErrorPayload> {
    match output {
        HostWorkspaceReadOutput::Text { content, .. } => Ok(content),
        HostWorkspaceReadOutput::Image { .. } | HostWorkspaceReadOutput::Binary { .. } => {
            Err(ErrorPayload::new(
                WireErrorCode::InvalidResponse,
                "probe.txt must be UTF-8 text",
            ))
        },
    }
}

#[derive(Deserialize)]
struct GreetArgs {
    name: String,
}

#[derive(Deserialize)]
struct AddArgs {
    a: i64,
    b: i64,
}

#[derive(Deserialize)]
struct AskLlmArgs {
    prompt: String,
}

#[derive(Deserialize)]
struct PreToolInput {
    tool_name: String,
    tool_input: Value,
}

#[derive(Deserialize)]
struct PipelineStepInput {
    step: u64,
}

#[derive(Deserialize)]
struct ParallelReadArgs {
    delay_ms: u64,
}

struct ParallelCallGuard;

impl ParallelCallGuard {
    fn enter() -> Self {
        let active = PARALLEL_ACTIVE.fetch_add(1, Ordering::SeqCst) + 1;
        PARALLEL_PEAK.fetch_max(active, Ordering::SeqCst);
        Self
    }
}

impl Drop for ParallelCallGuard {
    fn drop(&mut self) {
        PARALLEL_ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("s5r guest failed: {} ({})", error.message, error.code);
        std::process::exit(1);
    }
}

async fn run() -> Result<(), ErrorPayload> {
    let mut worker = Worker::new(EXT_ID, "0.1.0");
    worker
        .capability(ExtensionCapability::SmallModel)
        .capability(ExtensionCapability::EmitCustomEvents)
        .capability(ExtensionCapability::WorkspaceRead)
        .capability(ExtensionCapability::SessionInspect)
        .capability(ExtensionCapability::PublicHttp)
        .capability(ExtensionCapability::PublicHttpDispatch)
        .capability(ExtensionCapability::ToolIntercept)
        .custom_event(CustomEventDeclaration {
            event_type: "s5r_guest.probe".into(),
            schema_version: 1,
            delivery: CustomEventDelivery::SessionDurable,
            max_payload_bytes: 4096,
        });

    worker.tool(
        tool("ping")
            .description("Returns pong")
            .parameters(json!({ "type": "object", "properties": {} }))
            .build(),
        no_resources(),
        tool_handler(|_ctx| async { Ok(tool_text("pong", false)) }),
    )?;

    worker.tool(
        tool("call_context")
            .description("Return the host-attributed tool call context")
            .parameters(json!({ "type": "object", "properties": {} }))
            .build(),
        no_resources(),
        tool_handler(|ctx| async move {
            Ok(tool_text(
                json!({
                    "extension_id": ctx.extension_id(),
                    "session_id": ctx.session_id(),
                    "turn_id": ctx.turn_id(),
                    "tool_call_id": ctx.tool_call_id(),
                    "working_dir": ctx.working_dir().to_string_lossy(),
                })
                .to_string(),
                false,
            ))
        }),
    )?;

    worker.tool(
        tool("session_state_roundtrip")
            .description("Write and read extension-scoped session state")
            .parameters(json!({ "type": "object", "properties": {} }))
            .build(),
        host_resource(HostResource::Session),
        tool_handler(|_ctx| async move {
            HostClient::session_state()
                .write(HostSessionStateWriteRequest {
                    key: "typed-probe".into(),
                    content: "state-roundtrip-ok".into(),
                })
                .await?;
            let output = HostClient::session_state()
                .read(HostSessionStateReadRequest {
                    key: "typed-probe".into(),
                })
                .await?;
            Ok(tool_text(
                output.content.unwrap_or_else(|| "missing".into()),
                false,
            ))
        }),
    )?;

    worker.http_route(
        ExtensionHttpRoute::public(ExtensionHttpMethod::Post, "/s5r-probe/{id}"),
        http_handler(|request, _ctx| async move {
            let status =
                if request.path_params.get("id").map(String::as_str) == Some("invalid-status") {
                    99
                } else {
                    202
                };
            Ok(ExtensionHttpResponse::json(
                status,
                json!({
                    "id": request.path_params.get("id"),
                    "query": request.query,
                    "body": request.body,
                }),
            ))
        }),
    )?;

    worker.tool(
        tool("greet")
            .description("Greet")
            .parameters(json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }))
            .build(),
        no_resources(),
        tool_handler_args(|args: GreetArgs, _ctx| async move {
            Ok(tool_text(format!("hello, {}!", args.name), false))
        }),
    )?;

    worker.tool(
        tool("add")
            .description("Add")
            .parameters(json!({
                "type": "object",
                "properties": {
                    "a": { "type": "integer" },
                    "b": { "type": "integer" }
                },
                "required": ["a", "b"]
            }))
            .build(),
        no_resources(),
        tool_handler_args(|args: AddArgs, _ctx| async move {
            Ok(tool_text(
                format!("{} + {} = {}", args.a, args.b, args.a + args.b),
                false,
            ))
        }),
    )?;

    worker.tool(
        tool("ask_llm")
            .description("Ask small LLM")
            .parameters(json!({
                "type": "object",
                "properties": { "prompt": { "type": "string" } },
                "required": ["prompt"]
            }))
            .build(),
        host_resource(HostResource::Model),
        tool_handler_args(|args: AskLlmArgs, _ctx| async move {
            let output = HostClient::models()
                .small_chat(vec![LlmMessage::user(args.prompt)])
                .await?;
            Ok(tool_text(output.content, false))
        }),
    )?;

    worker.tool(
        tool("pipeline_tool_step")
            .description("Pipeline tool continuation probe")
            .parameters(json!({ "type": "object" }))
            .build(),
        no_resources(),
        tool_handler(|ctx| async move {
            assert_eq!(ctx.session_id(), "e2e-session");
            assert_eq!(ctx.working_dir(), std::path::Path::new("/tmp"));
            PIPELINE_TOOL_CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(tool_text("pipeline tool complete", false))
        }),
    )?;

    worker.tool(
        tool("pipeline_status")
            .description("Pipeline status")
            .parameters(json!({ "type": "object" }))
            .build(),
        no_resources(),
        tool_handler(|_ctx| async move {
            let step_1_calls = PIPELINE_STEP_1_CALLS.load(Ordering::SeqCst);
            let step_2_calls = PIPELINE_STEP_2_CALLS.load(Ordering::SeqCst);
            let tool_calls = PIPELINE_TOOL_CALLS.load(Ordering::SeqCst);
            let llm_ok = PIPELINE_LLM_OK.load(Ordering::SeqCst);
            Ok(tool_text(
                format!(
                    "step_1_calls={step_1_calls} step_2_calls={step_2_calls} \
                     tool_calls={tool_calls} llm_ok={llm_ok}"
                ),
                false,
            ))
        }),
    )?;

    worker.tool(
        tool("read_workspace")
            .description("Read probe.txt")
            .parameters(json!({ "type": "object" }))
            .build(),
        read_probe(),
        tool_handler(|_ctx| async move {
            let output = HostClient::workspace()
                .read(HostWorkspaceReadRequest {
                    path: "probe.txt".into(),
                    max_bytes: None,
                    line_offset: 0,
                    line_limit: None,
                })
                .await?;
            Ok(tool_text(
                format!("read probe.txt: {}", workspace_text(output)?),
                false,
            ))
        }),
    )?;

    worker.tool(
        tool("parallel_read_workspace")
            .description("Read probe.txt with a bounded parallel handler")
            .parameters(json!({
                "type": "object",
                "properties": { "delay_ms": { "type": "integer", "minimum": 0 } },
                "required": ["delay_ms"]
            }))
            .execution_mode(ExecutionMode::Parallel)
            .build(),
        read_probe(),
        tool_handler_args(|args: ParallelReadArgs, _ctx| async move {
            let _active = ParallelCallGuard::enter();
            tokio::time::sleep(Duration::from_millis(args.delay_ms)).await;
            let output = HostClient::workspace()
                .read(HostWorkspaceReadRequest {
                    path: "probe.txt".into(),
                    max_bytes: None,
                    line_offset: 0,
                    line_limit: None,
                })
                .await?;
            let peak = PARALLEL_PEAK.load(Ordering::SeqCst);
            Ok(tool_text(
                format!("content={} peak={peak}", workspace_text(output)?),
                false,
            ))
        }),
    )?;

    worker.tool(
        tool("inspect_sessions")
            .description("List host-visible sessions")
            .parameters(json!({ "type": "object" }))
            .build(),
        host_resource(HostResource::Session),
        tool_handler(|_ctx| async move {
            let output = HostClient::session_inspect().list().await?;
            Ok(tool_text(
                format!("session_count={}", output.sessions.len()),
                false,
            ))
        }),
    )?;

    worker.tool(
        tool("dispatch_public_http")
            .description("Dispatch to another extension's public HTTP route")
            .parameters(json!({ "type": "object" }))
            .build(),
        host_resource(HostResource::ExtensionHttp),
        tool_handler(|_ctx| async move {
            let forged = astrcode_extension_worker::testing::invoke_host(
                "astrcode.extension.http.public",
                json!({
                    "method": "POST",
                    "path": "/dispatch-target/42",
                    "path_params": { "id": "forged" },
                    "query": "source=forged",
                    "body": { "from": "forged" }
                }),
            )
            .await;
            if !matches!(&forged, Err(error) if error.code == WireErrorCode::InvalidInput.as_str())
            {
                return Err(ErrorPayload::new(
                    WireErrorCode::InvalidInput,
                    format!("host accepted forged path params: {forged:?}"),
                ));
            }
            let response = HostClient::extension_http()
                .dispatch_public(
                    ExtensionHttpDispatchRequest::new(
                        ExtensionHttpMethod::Post,
                        "/dispatch-target/42",
                    )
                    .query("source=s5r")
                    .json_body(json!({ "from": "guest" })),
                )
                .await?;
            Ok(tool_text(response.body.to_string(), response.status >= 400))
        }),
    )?;

    worker.tool(
        tool("slow")
            .description("Slow tool for cancel E2E")
            .parameters(json!({ "type": "object" }))
            .build(),
        no_resources(),
        tool_handler(|ctx| async move {
            for _ in 0..200 {
                if ctx.cancel_token().is_cancelled() {
                    return Ok(tool_text("cancelled", true));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Ok(tool_text("done", false))
        }),
    )?;

    worker.command(
        command("demo")
            .description("Demo slash command")
            .arguments(json!({ "type": "string" }))
            .requires_idle(true)
            .argument_completions(true)
            .priority(17)
            .availability(CommandAvailability::InteractiveOnly)
            .build(),
        command_handler(|ctx| async move {
            let data = match ctx.invocation() {
                WorkerCommandInvocation::Complete { cursor } => json!({
                    "items": [{
                        "label": ctx.argument(),
                        "insert_text": format!("{}-value", ctx.argument()),
                        "detail": cursor.to_string()
                    }],
                    "truncated": false
                }),
                WorkerCommandInvocation::Execute => json!({
                    "kind": "display",
                    "content": format!("s5r guest {} works!", ctx.command_name()),
                    "is_error": false
                }),
            };
            Ok(HandlerResult::effect(HandlerEffect::Ok, data))
        }),
    )?;

    worker.hook(
        LifecycleEvent::PreToolUse,
        HookMode::Blocking,
        hook_handler_args(|input: PreToolInput, _ctx| async move {
            if input.tool_name == "emit_hook_probe" {
                // This probe must inherit the active hook's request-scoped event context.
                HostClient::events()
                    .emit(HostEventEmitRequest {
                        event_type: "s5r_guest.probe".into(),
                        schema_version: 1,
                        payload: json!({ "from": "pre_tool_use" }),
                    })
                    .await?;
                return Ok(HandlerResult::ok());
            }
            if input.tool_name == "bash" {
                let cmd = input.tool_input["command"].as_str().unwrap_or("");
                if cmd.contains("rm -rf") {
                    return Ok(HandlerResult::effect(
                        HandlerEffect::Block,
                        json!({ "reason": "dangerous rm -rf blocked by s5r-guest-demo" }),
                    ));
                }
            }
            Ok(HandlerResult::ok())
        }),
    )?;

    worker.hook(
        LifecycleEvent::TurnEnd,
        HookMode::NonBlocking,
        hook_handler(|_ctx| async {
            Ok(HandlerResult {
                effect: HandlerEffect::Ok,
                data: Value::Null,
                continuations: vec![CallContinuation::Hook {
                    on: "pipeline_step".into(),
                    input: json!({ "step": 1 }),
                }],
            })
        }),
    )?;

    worker.continuation_hook_handler(
        "pipeline_step",
        continuation_handler_args(|input: PipelineStepInput, _ctx| async move {
            match input.step {
                1 => {
                    PIPELINE_STEP_1_CALLS.fetch_add(1, Ordering::SeqCst);
                    Ok(HandlerResult {
                        effect: HandlerEffect::Ok,
                        data: Value::Null,
                        continuations: vec![CallContinuation::Hook {
                            on: "pipeline_step".into(),
                            input: json!({ "step": 2 }),
                        }],
                    })
                },
                2 => {
                    PIPELINE_STEP_2_CALLS.fetch_add(1, Ordering::SeqCst);
                    let mut stream = HostClient::models()
                        .small_chat_events(vec![LlmMessage::user("continuation pipeline")])
                        .await?;
                    let mut saw_started = false;
                    let mut content = String::new();
                    let mut completed = false;
                    while let Some(event) = stream.next().await {
                        match event {
                            ModelStreamEvent::Started => saw_started = true,
                            ModelStreamEvent::ContentDelta { content: delta } => {
                                content.push_str(&delta);
                            },
                            ModelStreamEvent::Completed { .. } => completed = true,
                            ModelStreamEvent::Failed { error } => {
                                return Err(ErrorPayload {
                                    code: error.code,
                                    message: error.message,
                                    hint: error.hint,
                                    retryable: error.retryable,
                                    details: error.details,
                                });
                            },
                            _ => {},
                        }
                    }
                    if !saw_started || content != "mock-llm-response" || !completed {
                        return Err(ErrorPayload::new(
                            WireErrorCode::InvalidResponse,
                            "incremental model stream did not preserve event order",
                        ));
                    }
                    PIPELINE_LLM_OK.store(true, Ordering::SeqCst);
                    Ok(HandlerResult {
                        effect: HandlerEffect::Ok,
                        data: Value::Null,
                        continuations: vec![CallContinuation::Tool {
                            name: "pipeline_tool_step".into(),
                            input: json!({}),
                        }],
                    })
                },
                _ => Err(ErrorPayload::new(
                    WireErrorCode::InvalidRequest,
                    format!("unknown pipeline step: {}", input.step),
                )),
            }
        }),
    )?;

    worker.run_stdio().await
}
